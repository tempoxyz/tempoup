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

download() {
    if command -v curl >/dev/null 2>&1; then
        download_client=curl
    elif command -v wget >/dev/null 2>&1; then
        download_client=wget
    else
        fail "curl or wget is required"
    fi

    download_max_retries=$(awk -v value="${TEMPOUP_MAX_RETRIES:-}" 'BEGIN {
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
        if (value ~ /^[+]?[0-9]+$/ && value + 0 <= 4294967295) {
            if (value + 0 > 10) value = 10
            printf "%.0f\n", value
        } else print 5
    }')
    download_attempt=0
    download_delay=1
    download_host=${1#https://}
    download_host=${download_host%%/*}
    download_host=${download_host%%:*}
    download_host=${download_host%.}

    while :; do
        download_http_status=
        if [ "$download_client" = curl ]; then
            if download_http_status=$(curl --proto '=https' --tlsv1.2 --silent --show-error --fail --location \
                --write-out '%{http_code}' "$1" --output "$2"); then
                return 0
            else
                download_status=$?
            fi
        else
            if download_error=$(LC_ALL=C wget --https-only --secure-protocol=TLSv1_2 \
                --server-response --tries=1 "$1" -O "$2" 2>&1); then
                return 0
            else
                download_status=$?
            fi
            download_http_status=$(printf '%s\n' "$download_error" |
                awk '$1 ~ /^HTTP\// { code=$2 } END { print code }')
            printf '%s\n' "$download_error" >&2
        fi

        download_retryable=false
        case "$download_client:$download_status" in
            curl:22|wget:8)
                case "$download_http_status" in
                    403|408|429|500|502|503|504) download_retryable=true ;;
                esac
                ;;
            curl:5|curl:6|curl:7|curl:16|curl:18|curl:28|curl:52|curl:55|curl:56|curl:92|wget:4)
                download_retryable=true
                ;;
        esac
        case "$download_host" in
            github.com|*.github.com|githubusercontent.com|*.githubusercontent.com) ;;
            *) download_retryable=false ;;
        esac
        if [ "$download_retryable" = false ] || [ "$download_attempt" -ge "$download_max_retries" ]; then
            return "$download_status"
        fi

        download_attempt=$((download_attempt + 1))
        say "download failed; retrying in ${download_delay}s (${download_attempt}/${download_max_retries})" >&2
        sleep "$download_delay"
        if [ "$download_delay" -lt 16 ]; then
            download_delay=$((download_delay * 2))
        fi
    done
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

add_shell_source() {
    config=$1
    source_file=$2
    if [ -f "$config" ] && grep -F "$source_file" "$config" >/dev/null 2>&1; then
        return
    fi
    mkdir -p "${config%/*}"
    {
        printf '\n# Added by tempoup installer\n'
        printf '. "%s"\n' "$source_file"
    } >> "$config"
    say "added Tempo to PATH in $config"
}

configure_shell() {
    shell_name=${SHELL:-}
    shell_name=${shell_name##*/}
    if [ -n "${ZDOTDIR:-}" ]; then
        add_shell_source "$ZDOTDIR/.zshenv" "$env_file"
    fi
    if [ -f "$HOME/.zshenv" ] || [ "$shell_name" = zsh ]; then
        add_shell_source "$HOME/.zshenv" "$env_file"
    fi
    if [ -f "$HOME/.bashrc" ] || [ "$shell_name" = bash ]; then
        add_shell_source "$HOME/.bashrc" "$env_file"
    fi
    if [ -f "$HOME/.bash_profile" ]; then
        add_shell_source "$HOME/.bash_profile" "$env_file"
    fi
    if [ -f "$HOME/.profile" ]; then
        add_shell_source "$HOME/.profile" "$env_file"
    fi

    fish_config=${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/tempo.fish
    if [ -d "${fish_config%/*}" ] || [ "$shell_name" = fish ]; then
        if [ ! -f "$fish_config" ] || ! grep -F "$env_file.fish" "$fish_config" >/dev/null 2>&1; then
            mkdir -p "${fish_config%/*}"
            {
                printf '# Added by tempoup installer\n'
                printf 'source "%s"\n' "$env_file.fish"
            } > "$fish_config"
            say "added Tempo to PATH in $fish_config"
        fi
    fi
}

case "${1:-}" in
    -f | --force)
        TEMPOUP_IGNORE_VERIFICATION=true
        shift
        ;;
esac

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

tempo_dir=${TEMPO_DIR:-$HOME/.tempo}
bin_dir=${TEMPO_BIN_DIR:-$tempo_dir/bin}
env_file=$tempo_dir/env

mkdir -p "$bin_dir"
tmp=$(mktemp -d "$bin_dir/.tempoup-init.XXXXXX")
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
mv "$binary" "$bin_dir/tempoup"

say "tempoup installed to $bin_dir/tempoup"
mkdir -p "$tempo_dir"
dollar='$'
{
    printf '# tempo shell setup\n'
    printf 'export PATH="%s:%sPATH"\n' "$bin_dir" "$dollar"
} > "$env_file"
{
    printf '# tempo shell setup\n'
    printf 'fish_add_path -g "%s"\n' "$bin_dir"
} > "$env_file.fish"
configure_shell

PATH=$bin_dir:${PATH:-}
export PATH
TEMPO_BIN_DIR=$bin_dir "$bin_dir/tempoup" "$@"

say "restart your shell or run: source $env_file"
