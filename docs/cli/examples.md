# Recipes

Runnable sample files for every engine live in [`examples/`](../../examples/).

## Smoke-test an API before merging

```yaml
# smoke.test.yaml
steps:
  - name: health
    use: std/http@v1
    with: { url: "https://staging.example.com/health" }
    check: { status: 200, duration_ms_lt: 300 }
```

```yaml
# smoke.config.yaml
vus: 5
duration: 30s
```

```sh
perfscale run -f smoke.test.yaml -c smoke.config.yaml
```

## Login → authenticated request (chained steps)

```yaml
steps:
  - name: login
    use: std/http@v1
    with:
      method: POST
      url: https://api.example.com/login
      body: { user: demo, password: demo }
    check: { status: 200 }
    outputs: login

  - name: profile
    use: std/http@v1
    with:
      url: https://api.example.com/me
      headers:
        authorization: "Bearer ${{ login.body }}"
    check: { status: 200, body_contains: "demo" }

  - use: std/sleep@v1
    with: { ms: 500 }
```

## Reuse an existing k6 script

```sh
perfscale run --k6 load-tests/checkout.js
```

Load configuration (VUs, stages, thresholds) stays in the script's `options`
block — perfscale streams the output and returns k6's exit code semantics.

## Reuse an existing locustfile

```sh
perfscale run --locust locustfile.py --host https://target.example.com -c load.config.yaml
```

`vus`/`duration` from the config map to `--users`/`--spawn-rate`/`--run-time`.
After the run, locust's CSV stats are converted to the same summary block the
other engines print.

## Collect results from several terminals / machines

```sh
# terminal 1 — collector
perfscale serve --port 7999

# terminals 2..N — each run reports in
perfscale run -f test.yaml -c config.yaml --report http://collector-host:7999
```

Or bake the collector into the config so the flag isn't needed:

```yaml
# config.yaml
vus: 10
duration: 5m
report:
  url: http://collector-host:7999
```

## CI (GitHub Actions)

```yaml
- name: Load smoke test
  run: |
    curl -fsSL -o perfscale https://github.com/Perfscale/perfscale/releases/latest/download/perfscale-linux-amd64
    chmod +x perfscale
    ./perfscale run -f smoke.test.yaml -c smoke.config.yaml | tee load-report.txt
    ! grep -q 'http_req_failed........: 100.00%' load-report.txt
```

The run exits `0` even when checks fail (see
[exit code semantics](commands.md#exit-code-semantics)) — gate on the summary
content, as above, when you want failures to break the build.
