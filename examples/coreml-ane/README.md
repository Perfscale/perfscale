# Core ML / ANE (Neural Engine) load example

Generate real **Neural Engine** load on an Apple Silicon Mac and watch it in
perfscale's GPU metrics: the sidecar (`ane_load.py`) builds a small
convolutional network with [coremltools](https://github.com/apple/coremltools)'
MIL builder — no model file to download — and loops predictions with
`compute_units=ALL`, which is what routes conv nets onto the ANE. The run
samples the SoC with `gpu.source: powermetrics`, so `ane_power_w` climbs
while the sidecar works (≈+2 W over baseline on an M2 Pro with the default
net at ~1 450 inferences/s, alongside `cpu_power_w` / `package_power_w`).

> Note: Python's data stack (pandas, Anaconda, NumPy) does **not** touch the
> Neural Engine — `coremltools` is Apple's driver for Core ML, and Core ML is
> the only way user code reaches the ANE. Whether a given model uses the ANE
> depends on its ops and compute units; conv/MLProgram models with
> `compute_units=ALL` do, tiny sklearn-style models stay on the CPU.

## Setup (once)

```sh
cd examples/coreml-ane
python3 -m venv .venv
.venv/bin/pip install coremltools numpy

# the powermetrics source needs root (see docs/core/gpu.md)
echo "$USER ALL=(root) NOPASSWD: /usr/bin/powermetrics" | sudo tee /etc/sudoers.d/powermetrics
```

## Run

```sh
perfscale run -f test.yaml -c config.yaml --summary-export ane-run.json
```

`config.yaml` starts the sidecar in `before:` (failing fast if the inference
loop never comes up — without ANE load the run proves nothing) and stops it
in `after:`. `test.yaml` just keeps the run open; point its steps at your own
service to correlate ANE power with real request load.

## Reading the result

- `ane_power_w` rises when the sidecar starts and falls when it stops — that
  delta is the ANE signal. (The absolute baseline is not zero on every chip:
  powermetrics models several watts of ANE draw at idle on M2 Pro/Max, so
  judge the delta, not the floor.)
- `ane_power_w` flat at its baseline during the run → the ANE is not
  engaged: the model is CPU/GPU-bound (check its ops) or was loaded without
  `compute_units=ALL`.
- `ane_power_w` up while `gpu_utilization_pct` and `gpu_power_w` stay flat →
  the load really is on the Neural Engine, not the GPU — the two are
  separate engines on the SoC.
- Sweep the load by editing the net (`channels`, `--size`) or running
  N sidecars (`for i in $(seq N); do .venv/bin/python ane_load.py & done; wait`
  as the `before:` command) and watch where inferences/sec stops scaling —
  that is the chip's ANE ceiling for this workload.
- Use your own model: `.venv/bin/python ane_load.py --model path/to/model.mlpackage`.

## Files

- `ane_load.py` — the sidecar: MIL-built conv net (default) or any
  `.mlpackage` (`--model`), infinite prediction loop, rate line every 5 s.
- `config.yaml` — run config: `gpu.source: powermetrics`, sidecar lifecycle.
- `test.yaml` — placeholder steps keeping the run open.
