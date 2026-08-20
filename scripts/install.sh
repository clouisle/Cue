#!/usr/bin/env sh
# Install the latest Cue release for the current platform.
set -eu

REPOSITORY="clouisle/Cue"
INSTALL_DIR="${CUE_INSTALL_DIR:-$HOME/.local/bin}"

require() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'error: %s is required\n' "$1" >&2
        exit 1
    }
}

case "$(uname -s):$(uname -m)" in
    Linux:x86_64 | Linux:amd64)
        target="x86_64-unknown-linux-gnu"
        ;;
    Darwin:arm64 | Darwin:aarch64)
        target="aarch64-apple-darwin"
        ;;
    *)
        printf 'error: Cue has no prebuilt release for %s on %s\n' "$(uname -m)" "$(uname -s)" >&2
        exit 1
        ;;
esac

require curl
require tar

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

asset="cue-${target}.tar.gz"
base_url="https://github.com/${REPOSITORY}/releases/latest/download"
archive="$tmpdir/$asset"
checksums="$tmpdir/SHA256SUMS"

printf 'Downloading Cue for %s...\n' "$target"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    "$base_url/$asset" --output "$archive"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    "$base_url/SHA256SUMS" --output "$checksums"

expected="$(awk -v asset="$asset" '$2 == asset { print $1 }' "$checksums")"
if [ -z "$expected" ]; then
    printf 'error: %s is missing from SHA256SUMS\n' "$asset" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$archive" | awk '{ print $1 }')"
else
    printf 'error: sha256sum or shasum is required to verify the download\n' >&2
    exit 1
fi

if [ "$actual" != "$expected" ]; then
    printf 'error: SHA-256 mismatch for %s\n' "$asset" >&2
    exit 1
fi

tar -xzf "$archive" -C "$tmpdir"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmpdir/cue" "$INSTALL_DIR/cue"

printf 'Installed Cue to %s/cue\n' "$INSTALL_DIR"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) printf 'Add %s to PATH to run cue from any directory.\n' "$INSTALL_DIR" >&2 ;;
esac
