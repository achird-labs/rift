#!/bin/bash
# Rift Installation Script
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/EtaCassiopeia/rift/main/scripts/install.sh | bash
#
# Options:
#   RIFT_VERSION=v0.1.0  - Install specific version (default: latest)
#   RIFT_INSTALL_DIR=/usr/local/bin - Installation directory (default: /usr/local/bin or ~/.local/bin)
#   RIFT_NO_MODIFY_PATH=1 - Don't modify PATH

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
GITHUB_REPO="EtaCassiopeia/rift"
BINARY_NAME="rift"

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
    exit 1
}

# Detect OS and architecture
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)  os="unknown-linux" ;;
        Darwin*) os="apple-darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc" ;;
        *) log_error "Unsupported operating system: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) log_error "Unsupported architecture: $(uname -m)" ;;
    esac

    # For Linux, prefer musl (static) builds for maximum compatibility
    if [ "$os" = "unknown-linux" ]; then
        # Check if running on musl-based system (Alpine, etc.)
        if ldd --version 2>&1 | grep -q musl; then
            os="unknown-linux-musl"
        else
            # Default to glibc for better feature support (JS)
            os="unknown-linux-gnu"
        fi
    fi

    echo "${arch}-${os}"
}

# Get the latest release version
get_latest_version() {
    curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | \
        grep '"tag_name":' | \
        sed -E 's/.*"([^"]+)".*/\1/'
}

# Determine installation directory
get_install_dir() {
    if [ -n "$RIFT_INSTALL_DIR" ]; then
        echo "$RIFT_INSTALL_DIR"
    elif [ -w "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    else
        mkdir -p "$HOME/.local/bin"
        echo "$HOME/.local/bin"
    fi
}

# Download and install
install_rift() {
    local version="${RIFT_VERSION:-$(get_latest_version)}"
    local platform=$(detect_platform)
    local install_dir=$(get_install_dir)
    local archive_ext="tar.gz"
    local tmp_dir=$(mktemp -d)

    if [[ "$platform" == *"windows"* ]]; then
        archive_ext="zip"
    fi

    local archive_name="${BINARY_NAME}-${version}-${platform}.${archive_ext}"
    local download_url="https://github.com/${GITHUB_REPO}/releases/download/${version}/${archive_name}"
    local checksum_url="${download_url}.sha256"

    log_info "Installing Rift ${version} for ${platform}"
    log_info "Download URL: ${download_url}"

    # Download archive
    log_info "Downloading ${archive_name}..."
    if ! curl -fSL "$download_url" -o "${tmp_dir}/${archive_name}"; then
        log_error "Failed to download ${archive_name}. Check if the version and platform are correct."
    fi

    # Download and verify checksum
    log_info "Verifying checksum..."
    if curl -fsSL "$checksum_url" -o "${tmp_dir}/${archive_name}.sha256" 2>/dev/null; then
        cd "$tmp_dir"
        if command -v sha256sum &> /dev/null; then
            sha256sum -c "${archive_name}.sha256" || log_warning "Checksum verification failed"
        elif command -v shasum &> /dev/null; then
            shasum -a 256 -c "${archive_name}.sha256" || log_warning "Checksum verification failed"
        fi
        cd - > /dev/null
    else
        log_warning "Checksum file not found, skipping verification"
    fi

    # Extract archive
    log_info "Extracting..."
    cd "$tmp_dir"
    if [ "$archive_ext" = "tar.gz" ]; then
        tar -xzf "$archive_name"
    else
        unzip -q "$archive_name"
    fi

    # Find and install binary
    local binary_path=$(find . -name "$BINARY_NAME" -type f -executable 2>/dev/null | head -1)
    if [ -z "$binary_path" ]; then
        binary_path=$(find . -name "$BINARY_NAME" -type f 2>/dev/null | head -1)
    fi

    if [ -z "$binary_path" ]; then
        log_error "Could not find ${BINARY_NAME} binary in archive"
    fi

    log_info "Installing to ${install_dir}..."

    # Check if we need sudo
    if [ -w "$install_dir" ]; then
        cp "$binary_path" "${install_dir}/${BINARY_NAME}"
        chmod +x "${install_dir}/${BINARY_NAME}"
        # Create mb symlink for Mountebank compatibility
        ln -sf "${install_dir}/${BINARY_NAME}" "${install_dir}/mb"
    else
        log_info "Requesting sudo access to install to ${install_dir}..."
        sudo cp "$binary_path" "${install_dir}/${BINARY_NAME}"
        sudo chmod +x "${install_dir}/${BINARY_NAME}"
        sudo ln -sf "${install_dir}/${BINARY_NAME}" "${install_dir}/mb"
    fi

    # Cleanup
    cd - > /dev/null
    rm -rf "$tmp_dir"

    # Verify installation
    if command -v "${install_dir}/${BINARY_NAME}" &> /dev/null; then
        log_success "Rift ${version} installed successfully!"
        echo ""
        "${install_dir}/${BINARY_NAME}" --version || true
    else
        log_success "Rift ${version} installed to ${install_dir}/${BINARY_NAME}"
    fi

    # Check if install_dir is in PATH
    if [[ ":$PATH:" != *":${install_dir}:"* ]]; then
        log_warning "${install_dir} is not in your PATH"

        if [ -z "$RIFT_NO_MODIFY_PATH" ]; then
            echo ""
            echo "Add the following to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
            echo ""
            echo "  export PATH=\"${install_dir}:\$PATH\""
            echo ""
        fi
    fi

    echo ""
    log_info "Quick start:"
    echo "  # Start server"
    echo "  rift --port 2525"
    echo ""
    echo "  # Or use the mb alias (Mountebank-compatible)"
    echo "  mb --port 2525"
    echo ""
    echo "  # Start with imposters config file"
    echo "  rift --configfile imposters.json"
    echo ""
    log_info "Documentation: https://github.com/${GITHUB_REPO}#readme"
}

# Uninstall
uninstall_rift() {
    local install_dir=$(get_install_dir)

    log_info "Uninstalling Rift from ${install_dir}..."

    if [ -w "$install_dir" ]; then
        rm -f "${install_dir}/${BINARY_NAME}" "${install_dir}/mb"
    else
        sudo rm -f "${install_dir}/${BINARY_NAME}" "${install_dir}/mb"
    fi

    log_success "Rift uninstalled"
}

# Main
main() {
    case "${1:-install}" in
        install)
            install_rift
            ;;
        uninstall|remove)
            uninstall_rift
            ;;
        *)
            echo "Rift Installation Script"
            echo ""
            echo "Usage: $0 [install|uninstall]"
            echo ""
            echo "Environment variables:"
            echo "  RIFT_VERSION        Version to install (default: latest)"
            echo "  RIFT_INSTALL_DIR    Installation directory"
            echo "  RIFT_NO_MODIFY_PATH Skip PATH modification hints"
            ;;
    esac
}

main "$@"
