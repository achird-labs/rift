#!/bin/bash
#
# Make the intercept demo's CA with rift itself: `rift intercept-ca generate` writes a persistent
# CA pair offline, before rift or the SUT starts — the step a containerized SUT needs, since it
# reads its trust store once at startup.
#
# Usage: ./generate-intercept-ca.sh            (uses the zainalpour/rift-proxy:latest image)
#        RIFT_IMAGE=<image> ./generate-intercept-ca.sh
#
set -euo pipefail

IMAGE="${RIFT_IMAGE:-zainalpour/rift-proxy:latest}"
OUT_DIR="$(cd "$(dirname "$0")" && pwd)/intercept-ca"
mkdir -p "$OUT_DIR"

# Run as the invoking user so the files are ours to adjust below.
docker run --rm --user "$(id -u):$(id -g)" -v "$OUT_DIR:/out" "$IMAGE" \
  intercept-ca generate --out-dir /out --cn "Rift Intercept Demo CA" --validity-days 30 --force

# Demo-only: the rift container runs as uid 1000, which must read the key it is handed. A real
# deployment keeps the key 0600 and hands it over as a secret (RIFT_INTERCEPT_CA_KEY_PEM).
chmod 644 "$OUT_DIR/ca-key.pem"
echo "Intercept CA ready in $OUT_DIR"
