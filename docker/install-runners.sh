#!/bin/sh
# install-runners.sh <targetarch> [k6] [jmeter] [locust]
#
# Installs optional external runners into a perfscale image flavor. Keeps the
# install logic in one place so every flavor (k6 / jmeter / locust / full)
# pulls identical, pinned versions. Runs as root on Alpine.
set -eu

ARCH="${1:?usage: install-runners.sh <targetarch> [k6] [jmeter] [locust]}"
shift

: "${K6_VERSION:?}" "${JMETER_VERSION:?}" "${LOCUST_VERSION:?}"

[ $# -gt 0 ] && apk add --no-cache --virtual .fetch curl

for runner in "$@"; do
  case "$runner" in
    k6)
      # GitHub tarball, not the dl.k6.io apt repo — that one has no arm64.
      curl -fsSL "https://github.com/grafana/k6/releases/download/${K6_VERSION}/k6-${K6_VERSION}-linux-${ARCH}.tar.gz" \
        | tar xz -C /tmp
      cp "/tmp/k6-${K6_VERSION}-linux-${ARCH}/k6" /usr/local/bin/k6
      rm -rf "/tmp/k6-${K6_VERSION}-linux-${ARCH}"
      ;;
    jmeter)
      # archive.apache.org, not dlcdn — dlcdn drops old versions over time.
      apk add --no-cache openjdk17-jre-headless
      curl -fsSL --retry 5 --retry-all-errors -o /tmp/jmeter.tgz \
        "https://archive.apache.org/dist/jmeter/binaries/apache-jmeter-${JMETER_VERSION}.tgz"
      tar -xzf /tmp/jmeter.tgz -C /opt
      ln -s "/opt/apache-jmeter-${JMETER_VERSION}/bin/jmeter" /usr/local/bin/jmeter
      rm /tmp/jmeter.tgz
      ;;
    locust)
      apk add --no-cache python3 py3-pip
      pip install --no-cache-dir --break-system-packages "locust==${LOCUST_VERSION}"
      ;;
    *)
      echo "unknown runner: $runner" >&2
      exit 1
      ;;
  esac
done

[ $# -gt 0 ] && apk del .fetch
