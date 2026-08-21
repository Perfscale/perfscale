# CI pipelines

perfscale runs headless by design, so it drops straight into a CI pipeline:
the run summary goes to stdout, the exit code gates the job, and
`--summary-export` writes a machine-readable JSON (or a Markdown table for
the job summary UI) for downstream steps.

## GitHub Actions

The official composite action —
[**Perfscale/github-action**](https://github.com/Perfscale/github-action) —
installs a pinned perfscale release (checksum-verified against the release's
`sha256sums.txt`), runs your test, renders the metric table into the **job
summary**, and packs the summary + metrics into a zip artifact:

```yaml
- uses: Perfscale/github-action@v1
  id: loadtest
  with:
    file: test.yaml        # native engine; or: k6 / locust / jmeter
    config: config.yaml
- uses: actions/upload-artifact@v4
  with:
    name: perfscale-output
    path: ${{ steps.loadtest.outputs.output-file }}
```

One input per engine — `k6`, `locust`, `jmeter`, or `file` (native) — maps to
the matching `perfscale run` flag. Engine binaries stay the workflow's
responsibility (the action installs perfscale itself): k6/locust are
one-package installs, JMeter needs Java (preinstalled on GitHub-hosted
runners) plus the distribution tarball — the action's README has a copy-paste
install step.

Useful outputs for gating: `summary-json` (requests, RPS, latency
percentiles, error rate + engine/VUs/duration metadata) and `exit-code`:

```yaml
- name: Gate on p95
  run: |
    p95=$(jq '.summary.p95_ms' "${{ steps.loadtest.outputs.summary-json }}")
    awk "BEGIN { exit !(${p95} < 500) }" || { echo '::error::p95 over budget'; exit 1; }
```

**Live demo:** [Perfscale/perfscale-demo](https://github.com/Perfscale/perfscale-demo)
is a public playground repo — every push runs the native and JMeter engines
against a local target and publishes the job-summary tables and artifacts, so
you can see the output without setting anything up.

## GitLab CI

For GitLab there is a templates repo —
[**Perfscale/gitlab-ci**](https://github.com/Perfscale/gitlab-ci) — with
ready-made `.gitlab-ci.yml` includes and examples for the same engines.

## Any other CI

No plugin needed — install the binary and run:

```sh
curl -fsSL https://github.com/Perfscale/perfscale/releases/latest/download/perfscale-linux-amd64 \
  -o perfscale && chmod +x perfscale
./perfscale run -f test.yaml -c config.yaml \
  --summary-export summary.json
```

Exit codes mirror the CLI: `0` — the run completed (individual failed
requests/checks don't change that — gate on `summary.json` instead), `1` —
the run could not execute, `2` — invalid arguments. One deliberate exception:
a `std/thresholds@v1` gate violated at `severity: fail` fails the process
after the summary export, so CI turns red on SLO breaches.
