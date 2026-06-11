#!/usr/bin/env bash
# Builds the self-contained macOS disk image dist/out/Silo-<version>.dmg.
# The image contains:
#   - Silo.app, the Flutter client, with the silo, silo-helper, and
#     silo-tui command-line binaries embedded at Silo.app/Contents/Helpers/
#     and the installer script at Silo.app/Contents/Resources/install.sh;
#   - "Install Command Line Tools.command", which installs those embedded
#     binaries and their man pages to /usr/local via the installer script;
#   - man/, the generated man pages, readable straight off the image;
#   - README.txt.
#
# Signing: with SILO_SIGN_IDENTITY set to a codesigning identity (for
# example "Developer ID Application: ..."), the embedded binaries and the
# app bundle are signed with that identity and the hardened runtime.
# Without it everything is ad-hoc signed, which runs locally but makes
# Gatekeeper warn on other machines.

set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    echo "make-dmg.sh: could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

echo "building release binaries..."
cargo build --release --manifest-path "$repo/Cargo.toml"

echo "building the Flutter app (release)..."
(cd "$repo/apps/silo_app" && flutter build macos --release)

app_src=$repo/apps/silo_app/build/macos/Build/Products/Release/Silo.app
if [ ! -d "$app_src" ]; then
    echo "make-dmg.sh: $app_src not found after the Flutter build" >&2
    exit 1
fi

stage=$(mktemp -d "${TMPDIR:-/tmp}/silo-dmg.XXXXXX")
trap 'rm -rf "$stage"' EXIT
dmgroot=$stage/dmg
mkdir -p "$dmgroot"

# --- Assemble Silo.app with the embedded command-line tools. ---------------

app=$dmgroot/Silo.app
ditto "$app_src" "$app"
mkdir -p "$app/Contents/Helpers"
for binary in silo silo-helper silo-tui; do
    cp "$repo/target/release/$binary" "$app/Contents/Helpers/"
done
cp "$repo/scripts/install.sh" "$app/Contents/Resources/install.sh"
chmod 0755 "$app/Contents/Resources/install.sh"

# --- Man pages and README, readable straight off the image. ----------------

"$app/Contents/Helpers/silo" manpages "$dmgroot/man"

cat > "$dmgroot/README.txt" <<EOF
Silo $version
=============

Silo.app is the desktop client for llmdevsilo, the sandboxed LLM coding
harness. Drag it to /Applications (or run it from anywhere).

The app is self-contained: it bundles the silo, silo-helper, and
silo-tui command-line binaries at Silo.app/Contents/Helpers/ and finds
them there on its own, so the app works without any further setup.

To also use the command-line tools from a terminal, double-click
"Install Command Line Tools.command". It copies the binaries to
/usr/local/bin and the manual pages to /usr/local/share/man/man1
(asking for your administrator password). The same pages are in the
man/ folder on this image.

Documentation: https://github.com/ianh/llmdevsilo
EOF

# --- The command-line tools installer. --------------------------------------

cat > "$dmgroot/Install Command Line Tools.command" <<'EOF'
#!/bin/sh
# Installs the silo command-line tools and man pages to /usr/local from
# the binaries embedded in Silo.app.
set -eu
dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
app=$dir/Silo.app
if [ ! -d "$app" ]; then
    app=/Applications/Silo.app
fi
if [ ! -d "$app" ]; then
    echo "Silo.app was not found next to this script or in /Applications." >&2
    exit 1
fi
echo "Installing the silo command-line tools from $app to /usr/local."
echo "You may be asked for your administrator password."
echo
sudo sh "$app/Contents/Resources/install.sh" --system --from "$app/Contents/Helpers"
echo
echo "Done. Press Return to close this window."
read -r _
EOF
chmod 0755 "$dmgroot/Install Command Line Tools.command"

# --- Sign the modified bundle. ----------------------------------------------

# Adding files under Contents/ invalidates the seal the Flutter build
# created, so the bundle is re-signed: with SILO_SIGN_IDENTITY when set,
# ad-hoc otherwise. The build's entitlements are extracted and re-applied.
entitlements=$stage/entitlements.plist
sign_args=()
if codesign -d --entitlements - --xml "$app" > "$entitlements" 2>/dev/null \
    && [ -s "$entitlements" ]; then
    sign_args+=(--entitlements "$entitlements")
fi

if [ -n "${SILO_SIGN_IDENTITY:-}" ]; then
    identity=$SILO_SIGN_IDENTITY
    runtime_args=(--options runtime)
else
    identity=-
    runtime_args=()
fi

for binary in silo silo-helper silo-tui; do
    codesign --force --sign "$identity" "${runtime_args[@]+"${runtime_args[@]}"}" \
        "$app/Contents/Helpers/$binary"
done
codesign --force --sign "$identity" \
    "${runtime_args[@]+"${runtime_args[@]}"}" \
    "${sign_args[@]+"${sign_args[@]}"}" \
    "$app"
codesign --verify "$app"

# --- Produce the disk image. -------------------------------------------------

mkdir -p "$repo/dist/out"
dmg=$repo/dist/out/Silo-$version.dmg
rm -f "$dmg"
hdiutil create -volname "Silo $version" -srcfolder "$dmgroot" \
    -format UDZO -ov "$dmg"

echo "wrote dist/out/Silo-$version.dmg"
if command -v shasum >/dev/null 2>&1; then
    (cd "$repo/dist/out" && shasum -a 256 "Silo-$version.dmg")
fi
