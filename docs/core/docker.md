# Running perfscale in Docker

perfscale publishes ready-to-run images to the GitHub Container Registry on
every release — no install, no Rust toolchain, no runner binaries to manage.
Mount your scenarios into the container and run:

```sh
docker run --rm -v "$PWD:/work" -w /work ghcr.io/perfscale/perfscale:latest \
  run -f test.yaml -c config.yaml
```

Everything after the image name is passed straight to the `perfscale` CLI
(the image entrypoint is `perfscale`), so every command and flag works the
same as a local install: `run`, `lint`, `serve`, `man`, `--help`.

## Flavors

| Image | Contents | Use it for |
|---|---|---|
| `ghcr.io/perfscale/perfscale:<version>` | perfscale only (~50 MB) | Native step-engine scenarios (`-f`/`-c`) |
| `ghcr.io/perfscale/perfscale:<version>-full` | perfscale + k6 (~140 MB) | The above, plus `--k6` scripts |

Both are multi-arch (`linux/amd64` and `linux/arm64`) and are tagged
`X.Y.Z`, `X.Y`, and `latest` (the `-full` flavor uses the same tags with a
`-full` suffix). Pin an exact version in CI — treat `latest` as a
convenience for local use.

Locust and JMeter are **not** in either image (they pull in a full
Python/JRE stack). Build on top of the image if you need them — see
[Extending the image](#extending-the-image).

## Mounting scenarios

The container runs as a non-root user (`perfscale`, uid 10001) with no
working directory of its own, so the standard pattern is a bind mount:

```sh
# Scenarios live in the current directory
docker run --rm -v "$PWD:/work" -w /work ghcr.io/perfscale/perfscale:latest \
  run -f test.yaml -c config.yaml

# Read-only mount is fine for running tests
docker run --rm -v "$PWD:/work:ro" -w /work ghcr.io/perfscale/perfscale:latest \
  lint test.yaml
```

The bind mount is owned by your host user, so anything perfscale *writes*
(`--summary-export`, `std/file-write@v1`) lands as the container's uid
10001. To keep files owned by you, run with your own uid and a writable
mount:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest run -f test.yaml -c config.yaml \
  --summary-export summary.json
```

Secrets and configuration go through the same mechanisms as a local install
— pass environment variables with `-e` (`-e API_TOKEN=...`) and reference
them as `${{ env.API_TOKEN }}` in the scenario.

## k6 scripts (full flavor)

The `-full` image ships k6, so existing scripts run without any host
install:

```sh
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-full run --k6 script.js
```

## In CI

### GitHub Actions

```yaml
jobs:
  load:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - name: Run load test
        run: |
          docker run --rm -v "$PWD:/work" -w /work \
            ghcr.io/perfscale/perfscale:0.17.0 \
            run -f load.test.yaml -c load.config.yaml
```

Threshold gates (`std/thresholds@v1`) make the container exit non-zero on
SLO violations, so the step fails the job the same way a local run would.

### Kubernetes

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: perfscale-load
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: perfscale
          image: ghcr.io/perfscale/perfscale:0.17.0
          args: ["run", "-f", "/scenarios/load.test.yaml", "-c", "/scenarios/load.config.yaml"]
          volumeMounts:
            - name: scenarios
              mountPath: /scenarios
      volumes:
        - name: scenarios
          configMap:
            name: perfscale-scenarios
```

## Extending the image

Need JMeter, locust, or extra tooling? Use the image as a base — the binary
stays put, you add what you need:

```dockerfile
FROM ghcr.io/perfscale/perfscale:0.17.0

USER root
RUN apk add --no-cache openjdk17-jre-headless curl && \
    curl -fsSL https://dlcdn.apache.org//jmeter/binaries/apache-jmeter-5.6.3.tgz \
      | tar xz -C /opt && \
    ln -s /opt/apache-jmeter-5.6.3/bin/jmeter /usr/local/bin/jmeter
USER perfscale
```

## Verifying a release image

Images are built from the same static musl binaries published on GitHub
Releases, and the release workflow smoke-tests both flavors (a real
native-engine scenario in slim, `k6 version` in full) before they are
visible. To double-check locally:

```sh
docker run --rm ghcr.io/perfscale/perfscale:0.17.0 --version
docker run --rm --entrypoint k6 ghcr.io/perfscale/perfscale:0.17.0-full version
```

## Limits

- **Locust and JMeter are not bundled** — extend the image (above) or use
  the platform binaries.
- **`perfscale serve` needs a published port**: add `-p 7999:7999` and POST
  reports to the container's address, not `localhost`.
- **File actions are container-scoped**: `std/file-read@v1`/`std/file-write@v1`
  see the container filesystem — bind-mount what the scenario needs.
