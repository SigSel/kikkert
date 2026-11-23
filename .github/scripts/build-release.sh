#!/usr/bin/env bash
# Builds a statically linked kikkert for one musl target and packs it with the README and the
# licence into dist/kikkert-<target>.tar.gz, for the release workflow. The archive name leaves out
# the version so that releases/latest/download/kikkert-<target>.tar.gz always finds the newest.
#
# It runs on a machine of the target's own architecture, so no cross-compiler is involved: musl-gcc
# from musl-tools compiles the C code in the dependencies (SQLite, aws-lc, ring).
#
# usage: build-release.sh <target> <version>
#   e.g. build-release.sh x86_64-unknown-linux-musl 0.2.0
set -euo pipefail

target="$1"
version="$2"
name="kikkert-$target"
bin="${CARGO_TARGET_DIR:-target}/$target/release/kikkert"

# cc-rs reads CC_<target, dashes as underscores> for the C code it builds for that target.
export "CC_${target//-/_}=musl-gcc"

cargo build --release --locked --target "$target"

# The point of a musl build: the binary carries its libc and runs on any Linux of its architecture.
file "$bin"
if ! file "$bin" | grep -q -E 'static(-pie)? linked'; then
    echo "$bin is not statically linked" >&2
    exit 1
fi

reported="$("$bin" --version)"
if [[ "$reported" != "kikkert $version" ]]; then
    echo "the binary says '$reported', expected 'kikkert $version'" >&2
    exit 1
fi
echo "$reported"

rm -rf "dist/$name" "dist/$name.tar.gz"
mkdir -p "dist/$name"
cp "$bin" README.md LICENSE "dist/$name/"
tar --owner=0 --group=0 --numeric-owner -czf "dist/$name.tar.gz" -C dist "$name"
rm -r "dist/$name"
echo "packed dist/$name.tar.gz"
