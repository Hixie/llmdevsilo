# Installing llmdevsilo

llmdevsilo ships three command-line binaries — `silo` (the harness),
`silo-helper` (the sandbox helper), and `silo-tui` (the terminal
client) — plus man pages, and on macOS the Silo desktop app. There is
no binary download service yet: every path below starts from a source
checkout on the machine (or platform) you are installing for.

| Path | What you get | Where |
| --- | --- | --- |
| [System install](#system-install) | The three binaries in `/usr/local/bin`, man pages in `/usr/local/share/man/man1` | macOS, Linux |
| [User install](#user-install) | The same, under `~/.local` — no root needed | macOS, Linux |
| [macOS disk image](#macos-disk-image) | `Silo.app` with the binaries embedded, plus an optional command-line tools installer | macOS |
| [Tarball and Debian package](#tarball-and-debian-package) | A relocatable archive, or a `.deb` installing to `/usr/bin` | any / Debian-based Linux |

All paths need a Rust toolchain (1.85 or newer) to build the binaries;
the disk image additionally needs Flutter.

## System install

Builds the release binaries and installs them for all users. Writing to
`/usr/local` requires root, so run the installer with `sudo`:

```sh
cargo build --release
sudo sh scripts/install.sh           # --system is the default
```

This installs `silo`, `silo-helper`, and `silo-tui` into
`/usr/local/bin`, and the man pages (gzipped, generated from the
binaries themselves so they always match) into
`/usr/local/share/man/man1`. Both directories are on the default search
paths, so `silo --help` and `man silo` work immediately.

To remove exactly what was installed:

```sh
sudo sh scripts/install.sh --uninstall
```

## User install

The same contents under `~/.local`, with no root required:

```sh
cargo build --release
sh scripts/install.sh --user
```

The installer prints what it did and, when needed, how to put
`~/.local/bin` on your `PATH` and `~/.local/share/man` on your
`MANPATH`. Uninstall with `sh scripts/install.sh --user --uninstall`.

Setting the `PREFIX` environment variable overrides the prefix for
either mode (and skips the root check, since the destination may be
anywhere you can write).

## macOS disk image

The self-contained desktop distribution. Build it with:

```sh
dist/macos/make-dmg.sh
```

This builds the release binaries, builds the Flutter app in release
mode (allow several minutes), and produces `dist/out/Silo-<version>.dmg`
containing:

- **Silo.app** — the desktop client, with `silo`, `silo-helper`, and
  `silo-tui` embedded at `Silo.app/Contents/Helpers/`. The app finds
  its embedded `silo` on its own, so dragging the app to
  `/Applications` is a complete install.
- **Install Command Line Tools.command** — double-click to install the
  embedded binaries and man pages to `/usr/local` (it asks for your
  administrator password). This is optional; it only matters if you
  want `silo` in a terminal.
- **man/** — the man pages, readable straight off the image.
- **README.txt**.

**Signing note:** the app and binaries in the image are ad-hoc signed
unless the builder sets the `SILO_SIGN_IDENTITY` environment variable
to a codesigning identity (for example a "Developer ID Application"
certificate), in which case everything is signed with that identity and
the hardened runtime. An ad-hoc signed image runs fine on the machine
that built it, but Gatekeeper warns when it is downloaded to another
machine.

## Tarball and Debian package

### Tarball

```sh
dist/make-tarball.sh
```

Builds the release binaries and writes
`dist/out/llmdevsilo-<version>-<os>-<arch>.tar.gz`, whose root contains
`bin/` (the three binaries), `man/` (the man pages), `scripts/install.sh`,
`LICENSE`, and `README.md`. The version comes from the workspace
`Cargo.toml`; the operating system and architecture come from the build
machine, and the binaries only run on that platform.

To install from a tarball, unpack it into a directory and run the
bundled installer (the same script, with the same flags, as the system
and user installs above):

```sh
mkdir llmdevsilo && tar -xzf llmdevsilo-*.tar.gz -C llmdevsilo
cd llmdevsilo
sudo sh scripts/install.sh        # or: sh scripts/install.sh --user
```

### Debian package

```sh
dist/linux/make-deb.sh
```

Builds the release binaries and assembles
`dist/out/llmdevsilo_<version>_<arch>.deb` with `dpkg-deb`, installing
the binaries to `/usr/bin` and the man pages to `/usr/share/man/man1`:

```sh
sudo dpkg -i dist/out/llmdevsilo_<version>_<arch>.deb
```

The script needs `dpkg-deb`, so it runs on Debian-based Linux systems;
on macOS (or any machine without `dpkg-deb`) it exits with a message
saying so and pointing at the tarball and disk-image scripts instead.

## Checksums

Each build script prints the SHA-256 checksum of the artifact it wrote.
When passing an artifact to another machine, record that checksum and
verify it on the receiving end before installing:

```sh
shasum -a 256 -c <<< "<checksum>  llmdevsilo-<version>-<os>-<arch>.tar.gz"
```

(on Linux, `sha256sum -c` with the same input).
