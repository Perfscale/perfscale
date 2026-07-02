# perfscale documentation

perfscale is a single CLI for running load tests with [k6](https://k6.io),
[locust](https://locust.io), or perfscale's own native step engine — plus a
tiny local dev server for collecting results.

## Start here

- [Getting started](getting-started.md) — install, first run, first results
- [YAML reference](yaml-reference.md) — the `test.yaml` / `config.yaml` formats

## CLI (`perfscale` binary)

- [Commands](cli/commands.md) — `run`, `serve`, and `bench` reference
- [Recipes](cli/examples.md) — copy-paste examples for common workflows
- [Benchmarks](benchmarks.md) — engine comparison methodology & CI runs

## Core (`perfscale-core` library)

- [Architecture](core/architecture.md) — how the pieces fit together
- [Runners](core/runners.md) — k6, locust, and the native engine
- [Actions](core/actions.md) — `std/http`, `std/check`, `std/sleep`, `std/log`

## For contributors

- [Repository README](../README.md) — layout, local development, release builds
- [Examples](../examples/) — runnable sample files for each engine
- [JSON Schemas](../schema/) — generated schemas for editor autocomplete
