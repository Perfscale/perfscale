# Runnable perfscale image. Two flavors from one file:
#   --target base  → slim: perfscale only (native engine)
#   --target full  → slim + k6 (for `uses: k6` scenarios)
#
# The perfscale binary is NOT compiled here — release.yml builds the static
# musl binaries in its matrix and this Dockerfile just copies the right one:
#   dist/perfscale-linux-amd64
#   dist/perfscale-linux-arm64
#
# Build (from repo root):
#   docker buildx build -f docker/image.Dockerfile --target base \
#     --platform linux/amd64,linux/arm64 -t ghcr.io/perfscale/perfscale:latest .

FROM alpine:3.21 AS base

ARG TARGETARCH

LABEL org.opencontainers.image.source="https://github.com/Perfscale/perfscale" \
      org.opencontainers.image.description="perfscale — YAML-driven load testing tool" \
      org.opencontainers.image.licenses="MIT"

# ca-certificates: HTTPS targets need a CA bundle even though the binary is static.
RUN apk add --no-cache ca-certificates && adduser -D -u 10001 perfscale

COPY --chmod=0755 dist/perfscale-linux-${TARGETARCH} /usr/local/bin/perfscale

USER perfscale
ENTRYPOINT ["perfscale"]
CMD ["--help"]

# ── full: base + k6 ─────────────────────────────────────────────────────────
# k6 is fetched from GitHub releases (not dl.k6.io apt — that repo has no
# arm64 packages). Pinned on purpose; bump deliberately.
FROM base AS full

ARG TARGETARCH
ARG K6_VERSION=v2.2.0

USER root
RUN apk add --no-cache --virtual .fetch curl && \
    curl -fsSL "https://github.com/grafana/k6/releases/download/${K6_VERSION}/k6-${K6_VERSION}-linux-${TARGETARCH}.tar.gz" \
      | tar xz -C /tmp && \
    cp "/tmp/k6-${K6_VERSION}-linux-${TARGETARCH}/k6" /usr/local/bin/k6 && \
    rm -rf "/tmp/k6-${K6_VERSION}-linux-${TARGETARCH}" && \
    apk del .fetch

USER perfscale
