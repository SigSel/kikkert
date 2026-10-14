#!/usr/bin/env bash
# Installs the command line tools the pipeline needs into ~/.local/bin: just, and cargo-deny for
# `just deny`. They come as release archives from the projects' GitHub releases and are checked
# against the checksums written here, so a changed archive stops the job.
#
# The versions are the ones the recipes are developed with. To move to a new release, change the
# version and take the checksum from the release's own checksum file.
set -euo pipefail

JUST_VERSION=1.49.0
JUST_SHA256=05eb2f068b641b06e5b318796c2e27d4dcca608e65b34329a08c1b9f582611bd
CARGO_DENY_VERSION=0.19.6
CARGO_DENY_SHA256=0021d321c781f0159a150ca308859ad93ccce64a887b22ad2e129f096a8a2c07

target=x86_64-unknown-linux-musl
bin="$HOME/.local/bin"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$bin" "$tmp/just" "$tmp/cargo-deny"

# fetch <url> <sha256> <file>
fetch() {
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location --retry 3 --output "$3" "$1"
    echo "$2  $3" | sha256sum --check --quiet
}

fetch "https://github.com/casey/just/releases/download/$JUST_VERSION/just-$JUST_VERSION-$target.tar.gz" \
    "$JUST_SHA256" "$tmp/just.tar.gz"
tar -xzf "$tmp/just.tar.gz" -C "$tmp/just"
install -m 0755 "$tmp/just/just" "$bin/just"

fetch "https://github.com/EmbarkStudios/cargo-deny/releases/download/$CARGO_DENY_VERSION/cargo-deny-$CARGO_DENY_VERSION-$target.tar.gz" \
    "$CARGO_DENY_SHA256" "$tmp/cargo-deny.tar.gz"
tar -xzf "$tmp/cargo-deny.tar.gz" -C "$tmp/cargo-deny" --strip-components=1
install -m 0755 "$tmp/cargo-deny/cargo-deny" "$bin/cargo-deny"

# Later steps of a GitHub Actions job find the tools on the PATH.
if [[ -n "${GITHUB_PATH:-}" ]]; then
    echo "$bin" >> "$GITHUB_PATH"
fi
echo "installed just $JUST_VERSION and cargo-deny $CARGO_DENY_VERSION into $bin"
