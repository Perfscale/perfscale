# Kubernetes

Two ways to run perfscale in a cluster: one-shot or scheduled **Jobs** for
the CLI itself (the Helm chart below), and the **perfscaled agent** as a
DaemonSet (it joins your fleet and picks up platform tasks).

## The perfscale chart (Jobs and CronJobs)

The [Perfscale/charts](https://github.com/Perfscale/charts) repo ships the
CLI as a chart — a one-shot Job by default, a CronJob when `schedule` is
set:

```sh
helm repo add perfscale https://raw.githubusercontent.com/Perfscale/charts/main/
helm install smoke perfscale/perfscale                                    # one-shot Job
helm install nightly perfscale/perfscale --set schedule="0 3 * * *"       # CronJob
```

The scenario comes from an inline ConfigMap (`--set-file
scenario.test=my.test.yaml`) or an existing one
(`--set scenario.existingConfigMap=…`). The default scenario is a
self-contained SQLite smoke test, so `helm install` works out of the box.
Engine flavors (`image.flavor: k6|jmeter|locust|full`), resource limits and
extra CLI args are plain values — see the chart's `values.yaml`.

### Libraries on the chart (RFC 005)

Enable `cache.enabled` to get a `library-cache` PVC mounted at
`/var/lib/perfscale/cache` (`PERFSCALE_CACHE_DIR`): pre-populate it with
`perfscale install` (from a one-time Job on the same claim) and every run
reads libraries from the shared cache. With the cache disabled, libraries
ride the scenario volume like in Docker, or are burned into a standalone
binary.

## One-shot runs as plain Jobs

No Helm needed — the [engine images](docker.md) work as plain Jobs. Mount
the scenario (ConfigMap for small tests, a PVC otherwise) and run:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: perfscale-smoke
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: perfscale
          image: ghcr.io/perfscale/perfscale:latest
          args: ["run", "-f", "/work/test.yaml", "-c", "/work/config.yaml"]
          volumeMounts:
            - name: scenario
              mountPath: /work
      volumes:
        - name: scenario
          configMap:
            name: perfscale-smoke
```

Libraries work the same as in Docker: local components ride the mounted
volume (paths resolve relative to the declaring file); remote ones need a
shared install cache (`PERFSCALE_CACHE_DIR` on a PVC) populated by
`perfscale install`, or a [burned standalone binary](docker.md#wasm-libraries)
with libraries embedded.

## The perfscaled agent (DaemonSet)

The platform agent runs one pod per node as a DaemonSet and joins your
fleet. Its Helm chart is parked until the agent ships a container image —
track [Perfscale/charts#1](https://github.com/Perfscale/charts/issues/1);
the chart source from before the park is in the repo's git history
(`git show 9718911:perfscaled/values.yaml`).

Agents enforce a fail-closed fleet policy on `libraries:` declared by
tasks:

- `PERFSCALE_LIBRARY_CAPABILITIES` — the fleet ceiling (`fs`, `clock`,
  `net:<host>` globs). Default: **none** — libraries run, but with zero
  capabilities. A task granting more is rejected with a validation error,
  never silently downgraded.
- `PERFSCALE_LIBRARY_ALLOW_DIGESTS` — optional sha256 allowlist of
  artifacts permitted on the fleet at all.
- Remote libraries are never fetched at run time: they are installed by
  digest (`perfscaled library install <url> --sha256 …` or
  `POST /api/v1/libraries/install`) into a content-addressed cache, and the
  cached bytes are re-hashed when a task is accepted.

## Links

- [Running perfscale in Docker](docker.md) — images, flavors, mounting
- [Libraries guide](libraries.md) — the libraries feature itself
- [Perfscale/charts](https://github.com/Perfscale/charts) — chart source
