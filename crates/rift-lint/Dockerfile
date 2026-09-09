# Build stage
# Using Rust nightly for if-let-chains feature (required by regress 0.10.5, a boa_engine dependency)
# Digest-pinned (issue #664); Dependabot reads the tag and bumps the digest.
FROM rustlang/rust:nightly@sha256:6300e8dd65f4c012d50c9422a44dac04227c3890ecb626bb73a9e1224c55acad AS builder

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
COPY crates/rift-store-redis/Cargo.toml ./crates/rift-store-redis/

# Create dummy source files to build dependencies.
# Every workspace member needs an entry here, even one this image does not run: cargo resolves the
# whole workspace, so a member with no manifest/source fails the build outright.
RUN mkdir -p crates/rift-lint/src crates/rift-http-proxy/src/bin crates/rift-http-proxy/benches crates/rift-tui/src crates/rift-types/src crates/rift-mock-core/src crates/rift-ffi/src crates/rift-store-redis/src && \
    echo "" > crates/rift-types/src/lib.rs && \
    echo "" > crates/rift-mock-core/src/lib.rs && \
    echo "" > crates/rift-ffi/src/lib.rs && \
    echo "" > crates/rift-store-redis/src/lib.rs && \
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
#
# The layer above compiled every workspace crate from a dummy source file. Those artifacts must be
# dropped or cargo links them instead of the real code: `COPY . .` restores the sources with the
# build context's mtimes, which are *older* than the dummy artifacts, so the fingerprint check calls
# them fresh. Use `cargo clean -p`, not `rm -rf` on target paths — cargo 1.100 moved artifacts from
# target/release/{deps,.fingerprint} to target/release/build/<pkg>/<hash>/, which silently turned
# the old globs into no-ops and let empty dummy rlibs reach the link (`no `extensions` in the root`).
# Third-party dependency artifacts are untouched, so the dependency cache layer still pays off.
RUN cargo clean --release \
      --package rift-types --package rift-mock-core --package rift-store-redis \
      --package rift-ffi --package rift-http-proxy --package rift-lint && \
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
