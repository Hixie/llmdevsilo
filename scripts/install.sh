#!/bin/sh
# Installs (or uninstalls) the llmdevsilo command-line tools: the silo,
# silo-helper, and silo-tui binaries plus their man pages.
#
# The script works from any of these layouts:
#   - an unpacked release tarball (binaries in ../bin, man pages in ../man
#     relative to this script);
#   - a source checkout after `cargo build --release` (binaries in
#     ../target/release, man pages generated on the fly);
#   - an explicit directory of binaries given with --from (used by the
#     macOS disk image, whose binaries live inside Silo.app).
#
# Usage: install.sh [--system|--user] [--uninstall] [--from DIR]
#
#   --system     install under /usr/local (the default; requires root)
#   --user       install under $HOME/.local
#   --uninstall  remove exactly the files an install would create
#   --from DIR   take the binaries from DIR instead of auto-detecting
#
# Setting the PREFIX environment variable overrides the prefix entirely
# (and skips the root requirement, since the destination may be anywhere).

set -eu

die() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
}

mode=system
uninstall=0
from=''

while [ $# -gt 0 ]; do
    case $1 in
        --system) mode=system ;;
        --user) mode=user ;;
        --uninstall) uninstall=1 ;;
        --from)
            shift
            [ $# -gt 0 ] || die '--from needs a directory argument'
            from=$1
            ;;
        --from=*) from=${1#--from=} ;;
        -h|--help)
            usage
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

# --- Locate the binaries. -------------------------------------------------

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(dirname -- "$script_dir")

if [ -n "$from" ]; then
    bindir=$from
elif [ -x "$root/bin/silo" ]; then
    bindir=$root/bin
elif [ -x "$root/target/release/silo" ]; then
    bindir=$root/target/release
else
    die "no silo binaries found near $script_dir; build them with \
\"cargo build --release\" or pass --from DIR"
fi

for binary in silo silo-helper silo-tui; do
    [ -x "$bindir/$binary" ] || die "missing binary: $bindir/$binary"
done

# --- Choose the prefix. ---------------------------------------------------

if [ -n "${PREFIX:-}" ]; then
    prefix=$PREFIX
elif [ "$mode" = system ]; then
    prefix=/usr/local
    if [ "$(id -u)" -ne 0 ]; then
        die "a system install writes to $prefix and needs root; rerun \
with sudo, or use --user to install under \$HOME/.local"
    fi
else
    [ -n "${HOME:-}" ] || die 'HOME is not set; cannot pick a --user prefix'
    prefix=$HOME/.local
fi

bin_dest=$prefix/bin
man_dest=$prefix/share/man/man1

# --- Locate or generate the man pages. ------------------------------------

tmpman=''
cleanup() {
    if [ -n "$tmpman" ]; then
        rm -rf "$tmpman"
    fi
}
trap cleanup EXIT

if [ -f "$root/man/silo.1" ]; then
    mansrc=$root/man
else
    tmpman=$(mktemp -d "${TMPDIR:-/tmp}/silo-man.XXXXXX")
    "$bindir/silo" manpages "$tmpman" >/dev/null 2>&1 \
        || die "could not generate man pages with $bindir/silo manpages"
    mansrc=$tmpman
fi

# --- Uninstall. -------------------------------------------------------------

if [ "$uninstall" -eq 1 ]; then
    for binary in silo silo-helper silo-tui; do
        if [ -e "$bin_dest/$binary" ]; then
            rm -f "$bin_dest/$binary"
            printf 'removed %s\n' "$bin_dest/$binary"
        fi
    done
    for page in "$mansrc"/*.1; do
        [ -e "$page" ] || continue
        name=${page##*/}
        if [ -e "$man_dest/$name.gz" ]; then
            rm -f "$man_dest/$name.gz"
            printf 'removed %s\n' "$man_dest/$name.gz"
        fi
    done
    printf 'uninstalled from %s\n' "$prefix"
    exit 0
fi

# --- Install. ---------------------------------------------------------------

mkdir -p "$bin_dest" "$man_dest"

for binary in silo silo-helper silo-tui; do
    rm -f "$bin_dest/$binary"
    cp "$bindir/$binary" "$bin_dest/$binary"
    chmod 0755 "$bin_dest/$binary"
    printf 'installed %s\n' "$bin_dest/$binary"
done

for page in "$mansrc"/*.1; do
    [ -e "$page" ] || continue
    name=${page##*/}
    gzip -9 -c "$page" > "$man_dest/$name.gz"
    chmod 0644 "$man_dest/$name.gz"
    printf 'installed %s\n' "$man_dest/$name.gz"
done

# --- Post-install notes. ----------------------------------------------------

printf '\nInstalled silo, silo-helper, and silo-tui under %s.\n' "$prefix"
if [ "$mode" = user ] || [ -n "${PREFIX:-}" ]; then
    case ":${PATH-}:" in
        *:"$bin_dest":*) ;;
        *)
            printf '\nNote: %s is not on your PATH. Add it with:\n' "$bin_dest"
            printf '    export PATH="%s:$PATH"\n' "$bin_dest"
            ;;
    esac
    printf '\nIf "man silo" does not find the manual, add the man pages\n'
    printf 'to your MANPATH:\n'
    printf '    export MANPATH="%s:$MANPATH"\n' "$prefix/share/man"
    printf '(many systems pick this up automatically once %s is on PATH).\n' \
        "$bin_dest"
else
    printf 'Run "silo --help" or "man silo" to get started.\n'
fi
