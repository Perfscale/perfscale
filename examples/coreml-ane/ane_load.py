#!/usr/bin/env python3
"""Core ML inference loop — an ANE (Neural Engine) load generator.

Builds a small convolutional network with coremltools' MIL builder (no
model download needed) and runs predictions in a loop, printing the
inference rate every few seconds. Core ML schedules conv nets onto the
Neural Engine when the model is loaded with ComputeUnit.ALL, so a
perfscale run with `gpu.source: powermetrics` shows a non-zero
`ane_power_w` while this script works.

Usage:
    python3 ane_load.py [--model path/to/model.mlpackage] [--size 112]

Requires: pip install coremltools numpy  (a venv is fine)
"""

import argparse
import time

import numpy as np
import coremltools as ct
from coremltools.converters.mil import Builder as mb


def build_conv_net(size: int) -> ct.models.MLModel:
    """A small conv stack, built in MIL — no model file to download.

    Convolutional nets are what the Core ML scheduler routes to the ANE;
    ~5M multiply-adds per layer at 112x112 keeps the NPU visibly busy
    without starving the CPU the load test itself runs on.
    """
    channels = [32, 48, 64, 64, 96, 96]
    rng = np.random.default_rng(42)

    @mb.program(input_specs=[mb.TensorSpec(shape=(1, 3, size, size))])
    def prog(x):
        for ch in channels:
            w = mb.const(
                val=(rng.standard_normal((ch, x.shape[1], 3, 3)) * 0.05).astype(
                    np.float32
                )
            )
            x = mb.conv(x=x, weight=w, strides=[1, 1], pad_type="same")
            x = mb.relu(x=x)
        return mb.reduce_mean(x=x, axes=[2, 3], keep_dims=True)

    return ct.convert(
        prog,
        convert_to="mlprogram",
        compute_units=ct.ComputeUnit.ALL,
        minimum_deployment_target=ct.target.macOS13,
    )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--model",
        help="load this .mlpackage instead of the built-in conv net "
        "(use its first input as-is)",
    )
    ap.add_argument("--size", type=int, default=112, help="square input edge (built-in net)")
    ap.add_argument("--report-every", type=float, default=5.0, help="seconds between rate lines")
    args = ap.parse_args()

    rng = np.random.default_rng(0)
    if args.model:
        model = ct.models.MLModel(args.model, compute_units=ct.ComputeUnit.ALL)
        inp = model.get_spec().description.input[0]
        in_name = inp.name
        shape = [int(d) for d in inp.type.multiArrayType.shape]
        img = rng.random(shape, dtype=np.float32)
    else:
        model = build_conv_net(args.size)
        in_name = "x"
        img = rng.random((1, 3, args.size, args.size), dtype=np.float32)

    print(f"ane_load: model ready (input {in_name}{list(img.shape)}), inference loop...", flush=True)
    n = 0
    t0 = t_last = time.monotonic()
    while True:
        model.predict({in_name: img})
        n += 1
        now = time.monotonic()
        if now - t_last >= args.report_every:
            print(f"ane_load: {n} inferences, {n / (now - t0):.1f} inf/s avg", flush=True)
            t_last = now


if __name__ == "__main__":
    main()
