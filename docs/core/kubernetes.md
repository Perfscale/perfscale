# Kubernetes

Two ways to run perfscale in a cluster: the **perfscaled agent** as a
DaemonSet (it joins your fleet and picks up platform tasks), and one-shot
**Jobs** for the CLI itself.

## The perfscaled agent (Helm chart)

The agent ships as a Helm chart in
[Perfscale/charts](https://github.com/Perfscale/charts):

```sh
helm install perfscaled oci://ghcr.io/perfscale/charts/perfscaled \
  --set existingSecret=perfscaled-env
```

The chart runs one agent per node as a DaemonSet, non-root, with a
read-only root filesystem. The agent's configuration (controlplane URL,
credentials) comes from a Secret — either `existingSecret` or the chart's
`secret` values; see the chart's `values.yaml` for the full surface.

### Library support on the agent (RFC 005 phase 4)

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

The cache lives on a dedicated `library-cache` volume mounted at
`/var/lib/perfscale/cache` (`PERFSCALE_CACHE_DIR`), so installs survive
container restarts. The default is `emptyDir` (per-pod); switch to
`hostPath` in `values.yaml` to also survive pod reschedules.

## One-shot runs as Jobs

The [engine images](docker.md) work as plain Jobs — mount the scenario
(ConfigMap for small tests, a PVC otherwise) and run:

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

## Links

- [Running perfscale in Docker](docker.md) — images, flavors, mounting
- [Libraries guide](libraries.md) — the libraries feature itself
- [Perfscale/charts](https://github.com/Perfscale/charts) — chart source
