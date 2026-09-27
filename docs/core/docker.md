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
| `ghcr.io/perfscale/perfscale:<version>-k6` | + k6 | Existing k6 scripts (`--k6`) |
| `ghcr.io/perfscale/perfscale:<version>-jmeter` | + JMeter (headless JRE) | Existing `.jmx` plans (`--jmeter`) |
| `ghcr.io/perfscale/perfscale:<version>-locust` | + locust (Python) | Existing locust files (`--locust`) |
| `ghcr.io/perfscale/perfscale:<version>-full` | + k6, JMeter, locust | Everything |

All flavors are multi-arch (`linux/amd64` and `linux/arm64`) and are tagged
`X.Y.Z`, `X.Y`, and `latest` (runner flavors use the same tags with their
suffix: `latest-k6`, `latest-jmeter`, `latest-locust`, `latest-full`).
Runner versions are pinned in the image (k6, JMeter, locust — see
`docker/image.Dockerfile` for the exact pins). Pin an exact perfscale
version in CI — treat `latest` as a convenience for local use.

Need a different tool stack? Build on top of any flavor — see
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

## WASM libraries

`${alias.fn(...)}` libraries (RFC 005) work in the container — WASM support
is always compiled into the engine. What to mount depends on the source:

**Local components** (`use: ./libs/fixer-ids.wasm`) need nothing extra:
paths resolve relative to the declaring file, so the usual `$PWD:/work`
mount covers them as long as the relative layout is preserved. A library
granted `capabilities: [fs]` gets a read-only preopen of the run's
`fs_root` — by default the config file's directory, i.e. inside `/work`.

**Remote components** (`https://…`, `git+…`) are fetched only by
`perfscale install`, which writes `perfscale.lock` next to the config (in
the mount) but the artifact cache to the host's `~/.cache/perfscale`. Keep
the cache inside the mounted directory so host and container agree:

```sh
# host: install into a project-local cache (perfscale.lock lands next to the config)
PERFSCALE_CACHE_DIR="$PWD/.perfscale-cache" perfscale install test.yaml config.yaml

# container: same cache dir, fully offline run
docker run --rm -v "$PWD:/work" -w /work \
  -e PERFSCALE_CACHE_DIR=/work/.perfscale-cache \
  ghcr.io/perfscale/perfscale:latest run -f test.yaml -c config.yaml
```

`install` also *burns* (AOT-precompiles) each library into the cache, so
containerized runs skip per-run compilation too — the cache directory must
be shared between the install and the run, as above.

**Standalone binaries** — `perfscale burn` embeds a scenario's libraries
into a copy of the binary, so nothing has to be mounted besides the YAML:

```sh
# on the host (match the target architecture), then ship one file:
perfscale burn -f test.yaml -c config.yaml -o perfscale+libs
```

## External runners (k6 / JMeter / locust)

The runner flavors ship the matching engine, so existing scripts and plans
run without any host install:

```sh
# k6
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-k6 run --k6 script.js

# JMeter (.jmx plans)
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-jmeter run --jmeter plan.jmx

# locust
docker run --rm -v "$PWD:/work" -w /work \
  ghcr.io/perfscale/perfscale:latest-locust run --locust locustfile.py --host https://example.com
```

The `-full` image carries all three — use it when one job runs several
engines, or when you don't want to think about which flavor a script needs.

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

Need extra tooling the flavors don't carry — JMeter plugins, custom CA
certificates, another k6 build? Use any flavor as a base — the binaries
stay put, you add what you need:

```dockerfile
FROM ghcr.io/perfscale/perfscale:0.17.0-jmeter

USER root
# Example: JMeter plugins land in lib/ext of the installation
RUN curl -fsSL -o /opt/apache-jmeter-5.6.3/lib/ext/jpgc-casutg.jar \
      https://repo1.maven.org/maven2/kg/apc/jmeter-plugins-casutg/2.10/jmeter-plugins-casutg-2.10.jar
USER perfscale
```

## Verifying a release image

Images are built from the same static musl binaries published on GitHub
Releases, and the release workflow smoke-tests every flavor before it goes
live: a real native-engine scenario in slim, and each runner binary
starting (`k6 version`, `jmeter --version`, `locust --version`) in its
flavor and in `-full`. To double-check locally:

```sh
docker run --rm ghcr.io/perfscale/perfscale:0.17.0 --version
docker run --rm --entrypoint k6 ghcr.io/perfscale/perfscale:0.17.0-k6 version
```

## Kubernetes

The platform agent (perfscaled) ships as a Helm chart in
[Perfscale/charts](https://github.com/Perfscale/charts): a DaemonSet that
runs one agent per node (`ghcr.io` image, non-root, read-only root
filesystem, secret via `existingSecret`). The agent self-registers with the
controlplane and picks up tasks.

```sh
helm install perfscaled oci://ghcr.io/perfscale/charts/perfscaled \
  --set-file secret...  # or --set existingSecret=perfscaled-env
```

Chart notes:

- `readOnlyRootFilesystem: true` — writable paths come from volumes. The
  library cache (RFC 005) lives on a dedicated `library-cache` volume
  (default `emptyDir`, switch to `hostPath` in `values.yaml` to survive pod
  reschedules) mounted at `/var/lib/perfscale/cache`, wired as
  `PERFSCALE_CACHE_DIR` — so libraries installed via
  `perfscaled library install` / `POST /api/v1/libraries/install` persist
  across container restarts.
- Fleet library policy is set on the agent's env: `PERFSCALE_LIBRARY_CAPABILITIES`
  (fleet ceiling, default: none) and `PERFSCALE_LIBRARY_ALLOW_DIGESTS`
  (optional sha256 allowlist) — the agent intersects them with every task's
  grants and rejects exceeding tasks with a validation error.

For one-shot load runs in a cluster (no agent), the engine images above run
fine as a [Job](https://kubernetes.io/docs/concepts/workloads/controllers/job/):
mount the scenario via a ConfigMap/PVC and use the image entrypoint as in the
examples here.

## Limits

- **Runner versions are pinned** — to run a different k6/JMeter/locust
  version, extend the image (above).
- **JMeter writes `jmeter.log` (and any `.jtl` results) into the working
  directory** — give it a writable mount (`-v "$PWD:/work" -w /work`, or
  `--user "$(id -u):$(id -g)"` to keep files owned by you).
- **`perfscale serve` needs a published port**: add `-p 7999:7999` and POST
  reports to the container's address, not `localhost`.
- **File actions are container-scoped**: `std/file-read@v1`/`std/file-write@v1`
  see the container filesystem — bind-mount what the scenario needs.
