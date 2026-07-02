# Getting started

## Install

Download a binary for your platform from
[GitHub Releases](https://github.com/Perfscale/perfscale/releases):

| Platform | Artifact |
|---|---|
| Linux x86_64 | `perfscale-linux-amd64` (static) |
| Linux ARM64 | `perfscale-linux-arm64` (static) |
| macOS Apple Silicon | `perfscale-darwin-arm64` |
| macOS Intel | `perfscale-darwin-amd64` |
| Windows x86_64 | `perfscale-windows-amd64.exe` |
| Windows ARM64 | `perfscale-windows-arm64.exe` |

```sh
# Linux/macOS example
curl -fsSL -o perfscale https://github.com/Perfscale/perfscale/releases/latest/download/perfscale-linux-amd64
chmod +x perfscale
```

Verify with `sha256sums.txt` from the same release. Or build from source:

```sh
cargo build --release -p perfscale-cli
# binary at target/release/perfscale
```

perfscale itself has no runtime dependencies. The external engines are
optional and only needed if you use them:

- **k6** — [installation guide](https://k6.io/docs/get-started/installation/)
- **locust** — `pip install locust`
- **native engine** — built in, nothing to install

## First run (no external tools needed)

Create `test.yaml`:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/Perfscale/perfscale/main/schema/test.schema.json
steps:
  - name: homepage
    use: std/http@v1
    with:
      method: GET
      url: https://httpbin.org/get
    check:
      status: 200
    outputs: resp

  - name: log status
    use: std/log@v1
    with:
      message: "got status ${{ resp.status }}"
```

Create `config.yaml`:

```yaml
vus: 5
duration: 30s
```

Run it:

```sh
perfscale run -f test.yaml -c config.yaml
```

You'll see live per-request output, then a k6-compatible summary:

```text
vus....................: 5 min=1 max=5
iterations..............: 142 4.73/s
http_req_duration......: avg=213.40ms p(50)=201ms p(90)=280ms p(95)=310ms p(99)=352ms min=180ms max=390ms
http_req_failed........: 0.00%
http_reqs..............: 142 4.73/s
```

## Running k6 or locust scripts

Already have scripts? Point perfscale at them — output lands in the same
unified summary format regardless of engine:

```sh
perfscale run --k6 script.js
perfscale run --locust locustfile.py --host https://target.example.com
```

For locust, `-c config.yaml` maps `vus`/`duration` onto locust's
`--users`/`--spawn-rate`/`--run-time`.

## Collecting results from multiple runs

Start the local dev server in one terminal:

```sh
perfscale serve            # listens on :7999
```

Then report runs to it from anywhere:

```sh
perfscale run -f test.yaml -c config.yaml --report http://localhost:7999
```

The server prints each incoming summary batch. Only the metric summary is
forwarded — never the full per-iteration log.

## Next steps

- [YAML reference](yaml-reference.md) — every field of both file formats
- [CLI commands](cli/commands.md) — all flags
- [Recipes](cli/examples.md) — CI usage, presets, multi-step scenarios
