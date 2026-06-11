#!/usr/bin/env bash
# Assembles a Debian package at dist/out/llmdevsilo_<version>_<arch>.deb:
# the silo, silo-helper, and silo-tui binaries in /usr/bin and the
# generated man pages in /usr/share/man/man1.
#
# This script runs on a Debian-based Linux system (anywhere dpkg-deb is
# installed). It builds the binaries on the spot, so the package
# architecture is the build machine's.

set -euo pipefail

if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "make-deb.sh: dpkg-deb was not found on this machine." >&2
    echo "Debian packages can only be assembled where dpkg-deb is installed" >&2
    echo "(a Debian-based Linux system). On macOS, use dist/make-tarball.sh" >&2
    echo "or dist/macos/make-dmg.sh instead." >&2
    exit 1
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    echo "make-deb.sh: could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

case $(uname -m) in
    x86_64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *)
        echo "make-deb.sh: unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

echo "building release binaries..."
cargo build --release --manifest-path "$repo/Cargo.toml"

stage=$(mktemp -d "${TMPDIR:-/tmp}/silo-deb.XXXXXX")
trap 'rm -rf "$stage"' EXIT
pkg=$stage/llmdevsilo

mkdir -p "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/share/man/man1"
for binary in silo silo-helper silo-tui; do
    cp "$repo/target/release/$binary" "$pkg/usr/bin/"
    chmod 0755 "$pkg/usr/bin/$binary"
done

"$repo/target/release/silo" manpages "$stage/man"
for page in "$stage/man"/*.1; do
    gzip -9 -n -c "$page" > "$pkg/usr/share/man/man1/$(basename "$page").gz"
done
chmod 0644 "$pkg/usr/share/man/man1"/*.gz

cat > "$pkg/DEBIAN/control" <<EOF
Package: llmdevsilo
Version: $version
Architecture: $arch
Maintainer: Ian Hickson <ian@hixie.ch>
Depends: libc6
Section: devel
Priority: optional
Homepage: https://github.com/ianh/llmdevsilo
Description: sandboxed LLM coding harness
 Runs LLM coding agents against locked workspaces inside a static
 sandbox, with all network egress through a filtering proxy. Ships the
 silo harness binary, the silo-helper sandbox helper, and the silo-tui
 terminal client.
EOF

mkdir -p "$repo/dist/out"
deb=$repo/dist/out/llmdevsilo_${version}_${arch}.deb
dpkg-deb --build --root-owner-group "$pkg" "$deb"

echo "wrote dist/out/$(basename "$deb")"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$repo/dist/out" && sha256sum "$(basename "$deb")")
fi
