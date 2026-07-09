# Build stage
# Using Rust nightly for if-let-chains feature (required by regress 0.10.5, a boa_engine dependency)
FROM rustlang/rust:nightly AS builder

WORKDIR /build

RUN apt-get update && \
    apt-get install -y pkg-config git ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Build args for metadata
ARG VERSION=dev
ARG VCS_REF=unknown
ARG BUILD_DATE=unknown

# Features to build (all features enabled by default for full functionality)
# Available features: javascript, redis-backend
ARG FEATURES=javascript,redis-backend

# Ensure broad ARM64 compatibility for M1/M2/M3 Macs
# This prevents "Illegal instruction" errors on Apple Silicon and other ARM64 CPUs
# by avoiding CPU-specific optimizations that may not be available on all ARM64 processors
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=generic"

# Copy only Cargo files first to cache dependencies
COPY Cargo.toml ./
COPY Cargo.lock* ./
COPY crates/rift-http-proxy/Cargo.toml ./crates/rift-http-proxy/
COPY crates/rift-lint/Cargo.toml ./crates/rift-lint/
COPY crates/rift-tui/Cargo.toml ./crates/rift-tui/
COPY crates/rift-types/Cargo.toml ./crates/rift-types/
COPY crates/rift-core/Cargo.toml ./crates/rift-core/
COPY crates/rift-ffi/Cargo.toml ./crates/rift-ffi/

# Create dummy source files to build dependencies
RUN mkdir -p crates/rift-http-proxy/src/bin crates/rift-http-proxy/benches \
             crates/rift-core/benches \
             crates/rift-lint/src crates/rift-tui/src crates/rift-types/src crates/rift-core/src crates/rift-ffi/src && \
    echo "" > crates/rift-types/src/lib.rs && \
    echo "" > crates/rift-core/src/lib.rs && \
    echo "" > crates/rift-ffi/src/lib.rs && \
    echo "fn main() {}" > crates/rift-http-proxy/src/main.rs && \
    echo "fn main() {}" > crates/rift-http-proxy/src/bin/verify.rs && \
    echo "fn main() {}" > crates/rift-http-proxy/benches/matcher_bench.rs && \
    echo "fn main() {}" > crates/rift-core/benches/imposter_matcher_bench.rs && \
    echo "pub fn lint() {}" > crates/rift-lint/src/lib.rs && \
    echo "fn main() {}" > crates/rift-lint/src/main.rs && \
    echo "fn main() {}" > crates/rift-tui/src/main.rs && \
    cargo build --release --locked --package rift-http-proxy --features ${FEATURES} && \
    rm -rf crates/*/src crates/*/benches

# Copy entire workspace (needed for internal dependencies)
COPY . .

# Build application (only this layer rebuilds when source changes)
# Touch all src files to force rebuild since we replaced dummy files
# Also remove any cached target artifacts that might have old compilation
RUN find crates/rift-http-proxy/src -name '*.rs' -exec touch {} \; && \
    rm -rf target/release/.fingerprint/rift-http-proxy* target/release/deps/*rift* && \
    cargo build --release --locked --package rift-http-proxy --features ${FEATURES}

# Runtime stage
# Using debian:trixie-slim (Debian 13/testing) for GLIBC_2.39+ compatibility with nightly Rust build
FROM debian:trixie-slim

# Install CA certificates and curl
RUN apt-get update && \
    apt-get install -y ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /build/target/release/rift-http-proxy /usr/local/bin/rift

# OCI labels and metadata
ARG VERSION=dev
ARG VCS_REF=unknown
ARG BUILD_DATE=unknown
ARG FEATURES=javascript,redis-backend
LABEL \
  org.opencontainers.image.title="Rift HTTP Proxy" \
  org.opencontainers.image.description="Mountebank-compatible HTTP chaos engineering proxy with Rhai/JavaScript scripting and Redis state backend." \
  org.opencontainers.image.url="https://github.com/EtaCassiopeia/rift" \
  org.opencontainers.image.source="https://github.com/EtaCassiopeia/rift" \
  org.opencontainers.image.version="$VERSION" \
  org.opencontainers.image.revision="$VCS_REF" \
  org.opencontainers.image.created="$BUILD_DATE" \
  org.opencontainers.image.licenses="Apache-2.0" \
  org.opencontainers.image.vendor="Rift" \
  org.opencontainers.image.authors="Rift Contributors" \
  org.opencontainers.image.features="$FEATURES"

# Create non-root user
RUN useradd -m -u 1000 rift

# Create default data and config directories
RUN mkdir -p /data /config && chown -R rift:rift /data /config

USER rift

# Set working directory for data persistence
WORKDIR /data

# Expose ports:
# - 2525: Admin API (Mountebank-compatible, configurable via MB_PORT)
# - 9090: Metrics server (Prometheus format, configurable via RIFT_METRICS_PORT)
# - 4545-4550: Common imposter ports (Mountebank convention)
# - 8080-8090: Additional imposter/proxy port range
# Note: Docker publish (-p) works regardless of EXPOSE; this is documentation
EXPOSE 2525 9090 4545-4550 8080-8090

# Health check for admin API
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS http://localhost:${MB_PORT:-2525}/ >/dev/null || exit 1

# ============================================================
# Environment Variables
# ============================================================
# Mountebank-compatible options (MB_* prefix)
# ------------------------------------------------------------
ENV MB_PORT=2525
# Admin API port (--port)

ENV MB_HOST=0.0.0.0
# Hostname to bind admin API (--host)

ENV MB_LOGLEVEL=info
# Log level: debug, info, warn, error (--loglevel)

# MB_CONFIGFILE=/path/to/imposters.json
# Load imposters from config file on startup (--configfile)

# MB_DATADIR=/path/to/data
# Directory for persistent imposter storage (--datadir)

# MB_ALLOW_INJECTION=1
# Enable JavaScript injection in responses (--allow-injection)

# MB_LOCAL_ONLY=1
# Only accept requests from localhost (--local-only)

# ============================================================
# Rift-specific options (RIFT_* prefix)
# ------------------------------------------------------------
ENV RIFT_METRICS_PORT=9090
# Prometheus metrics server port (--metrics-port)

# ============================================================
# Scripting Features (enabled by default)
# ============================================================
# This image includes both scripting engines:
# - Rhai: Built-in, always available
# - JavaScript (Boa): Enabled via --features javascript
#
# Flow state storage:
# - In-memory: Built-in, always available
# - Redis: Enabled via --features redis-backend
#
# Rift Extensions (_rift namespace):
# - Flow state for stateful testing scenarios
# - Probabilistic fault injection (latency, error, TCP faults)
# - Multi-engine scripting (Rhai, JavaScript)

# Default command: Start Mountebank-compatible server
#
# Examples:
#   # Basic usage
#   docker run -p 2525:2525 -p 9090:9090 rift-proxy
#
#   # With injection enabled (for JavaScript inject responses)
#   docker run -p 2525:2525 -e MB_ALLOW_INJECTION=1 rift-proxy
#
#   # With imposter config file
#   docker run -p 2525:2525 -v ./imposters.json:/config/imposters.json \
#     -e MB_CONFIGFILE=/config/imposters.json rift-proxy
#
#   # With persistent data directory
#   docker run -p 2525:2525 -v ./mb-data:/data \
#     -e MB_DATADIR=/data rift-proxy
#
#   # With Redis backend for distributed flow state
#   docker run -p 2525:2525 --network my-network \
#     -e REDIS_URL=redis://redis:6379 rift-proxy
#
# CLI Reference (all flags work as environment variables with MB_ or RIFT_ prefix):
#   --port <PORT>           Admin API port [env: MB_PORT] [default: 2525]
#   --host <HOST>           Bind hostname [env: MB_HOST] [default: 0.0.0.0]
#   --configfile <FILE>     Load imposters from file [env: MB_CONFIGFILE]
#   --datadir <DIR>         Persistent storage directory [env: MB_DATADIR]
#   --allow-injection       Enable JS injection [env: MB_ALLOW_INJECTION]
#   --local-only            Localhost only [env: MB_LOCAL_ONLY]
#   --loglevel <LEVEL>      Log level [env: MB_LOGLEVEL] [default: info]
#   --metrics-port <PORT>   Metrics port [env: RIFT_METRICS_PORT] [default: 9090]
#   --mock                  Run in mock mode
#   --debug                 Enable debug mode
#   --nologfile             Disable log file (stdout only)
#   --log <FILE>            Log file path
#   --pidfile <FILE>        PID file path
#   --origin <ORIGIN>       CORS allowed origin
#   --ip-whitelist <IPS>    Allowed IP addresses (comma-separated)

ENTRYPOINT ["/usr/local/bin/rift"]
CMD []
