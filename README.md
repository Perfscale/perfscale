# perfscale

A single CLI for running load tests with k6, locust, or perfscale's own
native step engine — plus a tiny local dev server for collecting the
results.

**[Documentation](docs/README.md)** — [getting started](docs/getting-started.md) ·
[YAML reference](docs/yaml-reference.md) · [CLI commands](docs/cli/commands.md) ·
[recipes](docs/cli/examples.md) · [architecture](docs/core/architecture.md)

## Stack

- Rust, tokio (async runtime)
- clap (CLI parsing)
- axum (`perfscale serve`)
- serde / serde_yaml / serde_json (test & config files)
- schemars + jsonschema (YAML schema generation & validation)
- k6 / locust (external binaries, invoked as subprocesses — not bundled)

## How it works

1. `perfscale run` picks exactly one engine from `--k6`, `--locust`, or `-f` (native).
2. k6 and locust runs shell out to an existing `k6`/`locust` installation and stream
   their stdout/stderr live; locust's `--csv` output is parsed into a k6-compatible
   summary at the end so all three engines report in the same shape.
3. Native (`-f test.yaml -c config.yaml`) runs perfscale's own step engine — no
   external binary required — executing `std/http`, `std/check`, `std/sleep`, and
   `std/log` actions across N virtual users for a given duration.
4. `--report <url>` optionally POSTs the aggregated summary to a `perfscale serve`
   instance running locally, for a shared view across multiple `run` invocations.
5. `perfscale serve` is a minimal HTTP receiver + console printer — a stand-in dev
   dashboard, not a control-plane.

## Commands

| Command | Description |
|---|---|
| `perfscale run --k6 <file.js>` | Run a k6 script |
| `perfscale run --locust <file.py> [--host <url>]` | Run a locust file headless |
| `perfscale run -f <test.yaml> -c <config.yaml>` | Run a native step-engine test |
| `perfscale run ... --report <url>` | Also forward the summary to `perfscale serve` |
| `perfscale run ... --quiet` | Drop per-request output (errors and the final summary still print) |
| `perfscale run ... --summary-export <file>` | Write the parsed summary + run metadata as JSON or Markdown (repeatable) |
| `perfscale serve [--port 7999]` | Start the local metrics receiver |
| `perfscale serve --tls` | Same, over self-signed HTTPS — a local TLS load-test target |

See [`examples/`](examples/) for a working test/config pair per engine, and
[`schema/`](schema/) for the generated JSON Schemas (used for editor
autocomplete via a `# yaml-language-server: $schema=...` modeline).
Full reference lives in [`docs/`](docs/README.md).

## Repository layout

```
crates/
  perfscale-core/   generic engine: k6 & locust runners, native step engine, YAML/schema
  perfscale-cli/    bin `perfscale`: clap CLI, run/serve commands
docs/               user & contributor documentation (markdown)
examples/           sample test/config/script files for each engine
schema/             generated JSON Schemas for test.yaml / config.yaml
docker/             dev & release Dockerfiles
```

## Local development

Prerequisites: Rust (see `rust-toolchain` version below), and optionally `k6`
and `locust` on `PATH` if you want to exercise those engines end to end.

```sh
cargo build
cargo test
cargo run -p perfscale-cli -- run -f examples/hello.test.yaml -c examples/hello.config.yaml
```

Regenerate `schema/*.json` after changing `TestDef`/`Step`/`RunConfig`/`ConfigFile`:

```sh
cargo run -p perfscale-core --example gen_schema
```

## Release binaries

Tagged pushes (`vX.Y.Z`) build binaries for every desktop platform and
publish them to GitHub Releases (with `sha256sums.txt`) via
`.github/workflows/release.yml`:

| Artifact | Platform |
|---|---|
| `perfscale-linux-amd64` | Linux x86_64 (static musl) |
| `perfscale-linux-arm64` | Linux aarch64 (static musl) |
| `perfscale-darwin-arm64` | macOS Apple Silicon |
| `perfscale-darwin-amd64` | macOS Intel |
| `perfscale-windows-amd64.exe` | Windows x86_64 |
| `perfscale-windows-arm64.exe` | Windows ARM64 |

For local Linux builds without a musl toolchain there is also
`docker/release.Dockerfile`:

```sh
docker buildx build --platform linux/amd64 -f docker/release.Dockerfile --output type=local,dest=./dist .
```

## Environment variables

| Name | Description |
|---|---|
| `RUST_LOG` | `tracing` filter, e.g. `RUST_LOG=debug` |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
