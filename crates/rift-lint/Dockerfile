# Build stage
# Using Rust nightly for if-let-chains feature (required by regress 0.10.5, a boa_engine dependency)
# Digest-pinned (issue #664); Dependabot reads the tag and bumps the digest.
FROM rustlang/rust:nightly@sha256:cb1f9cc4facad7f84e6cf1a07fe0b8c6d54cac10abd4036f82b751d0c7e76a33 AS builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config git ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Build args for metadata
ARG VERSION=dev
ARG VCS_REF=unknown
ARG BUILD_DATE=unknown

# Copy only Cargo files first to cache dependencies
COPY Cargo.toml ./
COPY Cargo.lock* ./
COPY crates/rift-lint/Cargo.toml ./crates/rift-lint/
# rift-lint depends on rift-http-proxy for types
COPY crates/rift-http-proxy/Cargo.toml ./crates/rift-http-proxy/
COPY crates/rift-tui/Cargo.toml ./crates/rift-tui/
COPY crates/rift-types/Cargo.toml ./crates/rift-types/
COPY crates/rift-mock-core/Cargo.toml ./crates/rift-mock-core/
COPY crates/rift-ffi/Cargo.toml ./crates/rift-ffi/

# Create dummy source files to build dependencies
RUN mkdir -p crates/rift-lint/src crates/rift-http-proxy/src/bin crates/rift-http-proxy/benches crates/rift-tui/src crates/rift-types/src crates/rift-mock-core/src crates/rift-ffi/src && \
    echo "" > crates/rift-types/src/lib.rs && \
    echo "" > crates/rift-mock-core/src/lib.rs && \
    echo "" > crates/rift-ffi/src/lib.rs && \
    echo "pub fn lint() {}" > crates/rift-lint/src/lib.rs && \
    echo "fn main() {}" > crates/rift-lint/src/main.rs && \
    echo "fn main() {}" > crates/rift-http-proxy/src/main.rs && \
    echo "fn main() {}" > crates/rift-http-proxy/src/bin/verify.rs && \
    echo "fn main() {}" > crates/rift-tui/src/main.rs && \
    cargo build --release --locked --package rift-lint && \
    rm -rf crates/*/src crates/*/benches

# Copy entire workspace (needed for internal dependencies)
COPY . .

# Build application (only this layer rebuilds when source changes)
RUN find crates/rift-lint/src -name '*.rs' -exec touch {} \; && \
    rm -rf target/release/.fingerprint/rift-lint* target/release/deps/*rift* && \
    cargo build --release --locked --package rift-lint

# Runtime stage
# Using debian:trixie-slim (Debian 13/testing) for GLIBC_2.39+ compatibility with nightly Rust build
# Digest-pinned (issue #664); Dependabot reads the tag and bumps the digest.
FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd

# Install CA certificates (needed for potential HTTPS validation in future)
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /build/target/release/rift-lint /usr/local/bin/rift-lint

# OCI labels and metadata
ARG VERSION=dev
ARG VCS_REF=unknown
ARG BUILD_DATE=unknown
LABEL \
  org.opencontainers.image.title="Rift Lint" \
  org.opencontainers.image.description="Imposter configuration validator for Rift/Mountebank. Validates JSON syntax, schema, and script correctness." \
  org.opencontainers.image.url="https://github.com/achird-labs/rift" \
  org.opencontainers.image.source="https://github.com/achird-labs/rift" \
  org.opencontainers.image.version="$VERSION" \
  org.opencontainers.image.revision="$VCS_REF" \
  org.opencontainers.image.created="$BUILD_DATE" \
  org.opencontainers.image.licenses="Apache-2.0" \
  org.opencontainers.image.vendor="Rift"

# Create non-root user
RUN useradd -m -u 1000 rift

# Create default working directory and set permissions
RUN mkdir -p /imposters && chown -R rift:rift /imposters

USER rift

# Working directory for mounting imposter files
WORKDIR /imposters

# ============================================================
# Usage Examples
# ============================================================
# Validate a single file:
#   docker run --rm -v $(pwd):/imposters rift-lint imposter.json
#
# Validate a directory:
#   docker run --rm -v $(pwd):/imposters rift-lint .
#
# Validate with verbose output:
#   docker run --rm -v $(pwd):/imposters rift-lint --verbose .
#
# In GitHub Actions:
#   - uses: docker://zainalpour/rift-lint:latest
#     with:
#       args: ./imposters/
#
# In GitLab CI:
#   validate-imposters:
#     image: zainalpour/rift-lint:latest
#     script:
#       - rift-lint ./imposters/

ENTRYPOINT ["/usr/local/bin/rift-lint"]
CMD ["--help"]
