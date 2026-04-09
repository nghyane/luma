#!/bin/sh
# Install or update Luma — lightweight coding agent
# Usage: curl -fsSL https://raw.githubusercontent.com/nghyane/luma/master/install.sh | sh
# Compatible with POSIX sh on Linux and macOS.
set -e

REPO="nghyane/luma"
BIN_DIR="${LUMA_INSTALL_DIR:-$HOME/.local/bin}"

# --- helpers ----------------------------------------------------------------

has() { command -v "$1" >/dev/null 2>&1; }

info()  { printf '  \033[1;34minfo\033[0m  %s\n' "$*"; }
warn()  { printf '  \033[1;33mwarn\033[0m  %s\n' "$*" >&2; }
error() { printf '  \033[1;31merror\033[0m %s\n' "$*" >&2; }

# Portable HTTP GET to stdout.
fetch() {
  if has curl; then
    curl -fsSL "$1"
  elif has wget; then
    wget -qO- "$1"
  else
    error "curl or wget is required"
    exit 1
  fi
}

# Portable HTTP download to file.
download() {
  url="$1"; dest="$2"
  if has curl; then
    curl --fail --location --progress-bar --output "$dest" "$url"
  elif has wget; then
    wget --quiet --show-progress --output-document="$dest" "$url"
  else
    error "curl or wget is required"
    exit 1
  fi
}

# --- platform detection -----------------------------------------------------

detect_os() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  case "$os" in
    darwin)           printf "apple-darwin" ;;
    linux)            printf "unknown-linux-musl" ;;
    mingw*|msys*|cygwin*) printf "pc-windows-msvc" ;;
    *) error "Unsupported OS: $os"; exit 1 ;;
  esac
}

detect_arch() {
  arch="$(uname -m | tr '[:upper:]' '[:lower:]')"
  case "$arch" in
    x86_64|amd64)   arch="x86_64" ;;
    arm64|aarch64)   arch="aarch64" ;;
    *) error "Unsupported arch: $arch"; exit 1 ;;
  esac
  # Guard against 32-bit userland on 64-bit kernel
  if [ "$arch" = "x86_64" ] && [ "$(getconf LONG_BIT 2>/dev/null)" = "32" ]; then
    error "32-bit x86 is not supported"; exit 1
  fi
  printf '%s' "$arch"
}

# --- resolve version --------------------------------------------------------

resolve_version() {
  if [ -n "${LUMA_VERSION:-}" ]; then
    printf '%s' "$LUMA_VERSION"
    return
  fi
  tag=$(fetch "https://api.github.com/repos/$REPO/releases?per_page=1" \
    | tr ',' '\n' | grep '"tag_name"' | head -1 | cut -d'"' -f4)
  if [ -z "$tag" ]; then
    error "Failed to detect latest version"
    exit 1
  fi
  printf '%s' "$tag"
}

# --- install ----------------------------------------------------------------

OS="$(detect_os)"
ARCH="$(detect_arch)"
TARGET="${ARCH}-${OS}"
TAG="$(resolve_version)"

case "$OS" in
  pc-windows-msvc) EXT=".exe"; ARCHIVE="zip" ;;
  *)               EXT="";     ARCHIVE="tar.gz" ;;
esac

URL="https://github.com/$REPO/releases/download/$TAG/luma-${TARGET}.${ARCHIVE}"

info "Installing luma $TAG ($TARGET)"
info "  from: $URL"
info "  to:   $BIN_DIR/luma${EXT}"

# Download
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

download "$URL" "$TMP/luma.${ARCHIVE}"

# Extract
if [ "$ARCHIVE" = "zip" ]; then
  if has unzip; then
    unzip -qo "$TMP/luma.zip" -d "$TMP"
  elif has python3; then
    python3 -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" \
      "$TMP/luma.zip" "$TMP"
  else
    error "unzip or python3 is required to extract zip archives"
    exit 1
  fi
else
  tar xzf "$TMP/luma.tar.gz" -C "$TMP"
fi

# Install binary
mkdir -p "$BIN_DIR"
mv "$TMP/luma${EXT}" "$BIN_DIR/luma${EXT}"
chmod +x "$BIN_DIR/luma${EXT}" 2>/dev/null || true

info "Installed luma $TAG"

# --- PATH setup -------------------------------------------------------------

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    PROFILE=""
    IS_FISH=false
    case "${SHELL:-}" in
      */zsh)  PROFILE="$HOME/.zshrc" ;;
      */bash) PROFILE="$HOME/.bashrc" ;;
      */fish) PROFILE="$HOME/.config/fish/config.fish"; IS_FISH=true ;;
    esac
    # Fallback: try common rc files
    if [ -z "$PROFILE" ]; then
      for f in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
        [ -f "$f" ] && PROFILE="$f" && break
      done
    fi

    if [ -n "$PROFILE" ] && ! grep -qF "$BIN_DIR" "$PROFILE" 2>/dev/null; then
      mkdir -p "$(dirname "$PROFILE")"
      if $IS_FISH; then
        echo "fish_add_path $BIN_DIR" >> "$PROFILE"
      else
        echo "export PATH=\"$BIN_DIR:\$PATH\"" >> "$PROFILE"
      fi
      info "Added to $PROFILE"
    fi

    printf '\n'
    info "Restart your shell or run:"
    if $IS_FISH; then
      info "  fish_add_path $BIN_DIR"
    else
      info "  export PATH=\"$BIN_DIR:\$PATH\""
    fi
    ;;
esac
