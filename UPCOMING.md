# Upcoming release

<!--
Release notes for the next release, written as features land.

- Append short, user-facing entries below this comment as you merge changes
  (what changed and why a user cares — not commit messages).
- On a `v*` tag, the release workflow publishes everything below the comment
  as the release body (with the auto-generated changelog appended), then
  resets this file back to the template.
- If this file has no entries at tag time, the release falls back to
  auto-generated notes and the workflow prints a warning.
-->

## Docker images on ghcr.io

Every release now also publishes runnable Docker images to
`ghcr.io/perfscale/perfscale` (multi-arch `linux/amd64` + `linux/arm64`),
in two flavors: the default slim image (native step engine, ~50 MB) and
`*-full` (slim + k6, for `--k6` scripts). No install needed — mount your
scenarios and run:

```sh
docker run --rm -v "$PWD:/work" -w /work ghcr.io/perfscale/perfscale:latest \
  run -f test.yaml -c config.yaml
```

Both flavors are smoke-tested in the release pipeline before they go live.
Details: [docs/core/docker.md](docs/core/docker.md).
