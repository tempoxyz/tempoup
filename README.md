# tempoup

Installer and updater for [Tempo](https://tempo.xyz).

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/tempoxyz/tempoup/main/tempoup-init.sh | sh
```

Then install the latest Tempo release:

```sh
tempoup
```

Use `tempoup --install v1.13.2` to install a specific release and
`tempoup --update` to update tempoup itself.

By default binaries are installed to `~/.tempo/bin`. `TEMPO_DIR` changes the
Tempo directory, while `TEMPO_BIN_DIR` sets the binary directory directly.

The bootstrap script is delivered over HTTPS and checks the downloaded binary
against its published attestation metadata. Once installed, Rust tempoup
cryptographically verifies Sigstore provenance, including the repository,
workflow, release tag, and artifact digest, before installing updates.

## Development

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo +nightly fmt --all --check
```
