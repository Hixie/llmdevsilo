#!/usr/bin/env bash
# Builds the release binaries and assembles a distributable tarball at
# dist/out/llmdevsilo-<version>-<os>-<arch>.tar.gz. The tarball root
# contains bin/ (silo, silo-helper, silo-tui), man/ (generated man
# pages), scripts/install.sh, LICENSE, and README.md. Unpack it into a
# directory and run scripts/install.sh from there.

set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    echo "make-tarball.sh: could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
name=llmdevsilo-$version-$os-$arch

echo "building release binaries..."
cargo build --release --manifest-path "$repo/Cargo.toml"

stage=$(mktemp -d "${TMPDIR:-/tmp}/silo-tarball.XXXXXX")
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/bin" "$stage/scripts"
for binary in silo silo-helper silo-tui; do
    cp "$repo/target/release/$binary" "$stage/bin/"
done
"$repo/target/release/silo" manpages "$stage/man"
cp "$repo/scripts/install.sh" "$stage/scripts/install.sh"
chmod 0755 "$stage/scripts/install.sh"
cp "$repo/LICENSE" "$repo/README.md" "$stage/"

mkdir -p "$repo/dist/out"
tar -czf "$repo/dist/out/$name.tar.gz" -C "$stage" \
    bin man scripts LICENSE README.md

echo "wrote dist/out/$name.tar.gz"
if command -v shasum >/dev/null 2>&1; then
    (cd "$repo/dist/out" && shasum -a 256 "$name.tar.gz")
elif command -v sha256sum >/dev/null 2>&1; then
    (cd "$repo/dist/out" && sha256sum "$name.tar.gz")
fi
