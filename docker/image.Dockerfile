# Runnable perfscale image. Flavors (build targets) from one file:
#   --target base    → perfscale only (native step engine)
#   --target k6      → + k6
#   --target jmeter  → + JMeter (headless JRE)
#   --target locust  → + locust (Python)
#   --target full    → + k6 + JMeter + locust
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

# ── Shared flavor setup ─────────────────────────────────────────────────────
# Pinned runner versions — bump deliberately. Exposed as env so
# install-runners.sh can read them.
FROM base AS runner-base

ARG TARGETARCH
ARG K6_VERSION=v2.2.0
ARG JMETER_VERSION=5.6.3
ARG LOCUST_VERSION=2.46.4
ENV K6_VERSION=${K6_VERSION} \
    JMETER_VERSION=${JMETER_VERSION} \
    LOCUST_VERSION=${LOCUST_VERSION}

USER root
COPY docker/install-runners.sh /usr/local/bin/install-runners.sh

# ── Flavors ─────────────────────────────────────────────────────────────────
FROM runner-base AS k6
RUN sh /usr/local/bin/install-runners.sh "${TARGETARCH}" k6
USER perfscale

FROM runner-base AS jmeter
RUN sh /usr/local/bin/install-runners.sh "${TARGETARCH}" jmeter
USER perfscale

FROM runner-base AS locust
RUN sh /usr/local/bin/install-runners.sh "${TARGETARCH}" locust
USER perfscale

FROM runner-base AS full
RUN sh /usr/local/bin/install-runners.sh "${TARGETARCH}" k6 jmeter locust
USER perfscale
