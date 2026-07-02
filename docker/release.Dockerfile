# Cross-compilation Dockerfile for perfscale.
# Used by GitHub Actions with `docker buildx --platform linux/amd64,linux/arm64`.
#
# Output (via --output type=local,dest=./dist):
#   dist/perfscale-linux-amd64
#   dist/perfscale-linux-arm64

FROM rust:1.88-alpine AS builder

# TARGETARCH is injected by buildx: amd64 | arm64 — used to key the cache
# mounts below so amd64/arm64 builds don't clobber each other's cache.
ARG TARGETARCH

RUN apk add --no-cache musl-dev

WORKDIR /workspace

COPY . .

# Cache mounts persist cargo's registry/git checkouts and the incremental
# target/ dir across runs, but their contents are NOT part of the final
# image layer — copy the binary out to a normal path before the mount
# unmounts.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry-${TARGETARCH} \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git-${TARGETARCH} \
    --mount=type=cache,target=/workspace/target,id=cargo-target-${TARGETARCH} \
    cargo build --release -p perfscale-cli && \
    cp target/release/perfscale /perfscale-out

# ── Export ──────────────────────────────────────────────────────────────────
FROM scratch
ARG TARGETARCH
COPY --from=builder /perfscale-out /perfscale-linux-${TARGETARCH}
