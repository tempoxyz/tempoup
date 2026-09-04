#!/bin/sh

set -eu

TEMPOUP_REPO="tempoxyz/tempoup"
TEMPOUP_IGNORE_VERIFICATION="${TEMPOUP_IGNORE_VERIFICATION:-false}"

say() {
    printf 'tempoup-init: %s\n' "$1"
}

fail() {
    say "$1" >&2
    exit 1
}

usage() {
    cat <<EOF
tempoup-init 0.1.0

Install the tempoup binary.

Usage: tempoup-init.sh [OPTIONS]

Options:
  -f, --force     Skip checksum verification (insecure)
  -h, --help      Print help
  -V, --version   Print version

Environment variables:
  TEMPOUP_VERSION              Install a specific tempoup version
  TEMPOUP_IGNORE_VERIFICATION  Skip verification if set to true
  TEMPO_BIN_DIR                Install directly into this directory
  TEMPO_DIR                    Tempo directory (uses its bin subdirectory)
EOF
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 --retry 5 --retry-all-errors --silent --show-error --fail --location "$1" --output "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only --secure-protocol=TLSv1_2 --tries=6 --quiet "$1" -O "$2"
    else
        fail "curl or wget is required"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        fail "sha256sum or shasum is required"
    fi
}

architecture() {
    os=$(uname -s)
    cpu=$(uname -m)

    case "$os" in
        Linux) platform=linux ;;
        Darwin) platform=darwin ;;
        *) fail "unsupported operating system: $os" ;;
    esac

    case "$cpu" in
        x86_64 | amd64)
            if [ "$platform" = darwin ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=arm64
            else
                arch=amd64
            fi
            ;;
        aarch64 | arm64) arch=arm64 ;;
        *) fail "unsupported architecture: $cpu" ;;
    esac

    if [ "$platform" = darwin ] && [ "$arch" != arm64 ]; then
        fail "unsupported platform: darwin/$arch"
    fi
    printf '%s_%s\n' "$platform" "$arch"
}

for arg in "$@"; do
    case "$arg" in
        -f | --force) TEMPOUP_IGNORE_VERIFICATION=true ;;
        -h | --help) usage; exit 0 ;;
        -V | --version) echo "tempoup-init 0.1.0"; exit 0 ;;
        *) fail "unknown option: $arg" ;;
    esac
done

target=$(architecture)
asset="tempoup_$target"
if [ -n "${TEMPOUP_VERSION:-}" ]; then
    version=${TEMPOUP_VERSION#v}
    base_url="https://github.com/$TEMPOUP_REPO/releases/download/v$version"
    say "installing tempoup v$version"
else
    base_url="https://github.com/$TEMPOUP_REPO/releases/latest/download"
    say "installing latest tempoup"
fi

if [ -n "${TEMPO_BIN_DIR:-}" ]; then
    bin_dir=$TEMPO_BIN_DIR
elif [ -n "${TEMPO_DIR:-}" ]; then
    bin_dir=$TEMPO_DIR/bin
else
    bin_dir=$HOME/.tempo/bin
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
binary=$tmp/tempoup
expected=

if [ "$TEMPOUP_IGNORE_VERIFICATION" = true ]; then
    say "skipping verification"
else
    pointer=$tmp/attestation.txt
    bundle=$tmp/attestation.json
    download "$base_url/$asset.attestation.txt" "$pointer"
    attestation_url=$(head -n 1 "$pointer" | tr -d '\r')
    [ -n "$attestation_url" ] || fail "release attestation pointer is empty"
    download "$attestation_url/download" "$bundle"
    payload=$(awk '/"payload":/ {gsub(/[",]/, "", $2); print $2; exit}' "$bundle")
    decoded=$(printf '%s' "$payload" | base64 -d 2>/dev/null || printf '%s' "$payload" | base64 -D 2>/dev/null || true)
    compact=$(printf '%s' "$decoded" | tr -d '[:space:]')
    case "$compact" in
        *\"predicateType\":\"https://slsa.dev/provenance/v1\"*) ;;
        *) fail "release metadata is not SLSA provenance" ;;
    esac
    case "$compact" in
        *\"name\":\"$asset\"*) ;;
        *) fail "release metadata does not describe $asset" ;;
    esac
    expected=$(printf '%s' "$decoded" | grep -oE '"sha256"[[:space:]]*:[[:space:]]*"[a-fA-F0-9]{64}"' | head -n 1 | grep -oE '[a-fA-F0-9]{64}' || true)
    [ -n "$expected" ] || fail "could not read the attested SHA-256 digest"
fi

download "$base_url/$asset" "$binary"
if [ -n "$expected" ]; then
    actual=$(sha256 "$binary")
    [ "$actual" = "$expected" ] || fail "checksum verification failed (expected $expected, got $actual)"
    say "checksum verified ✓"
fi

chmod 755 "$binary"
"$binary" --version >/dev/null || fail "downloaded tempoup binary could not run"
mkdir -p "$bin_dir"
staged=$bin_dir/.tempoup-new
cp "$binary" "$staged"
chmod 755 "$staged"
mv "$staged" "$bin_dir/tempoup"

say "tempoup installed to $bin_dir/tempoup"
case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) say "add $bin_dir to PATH" ;;
esac
