# silo_app

Flutter client for [llmdevsilo](../../docs/DESIGN.md) harnesses. Runs on
macOS, iOS, Android, and the web, and can hold connections to several
harnesses at the same time. Every connected client sees the same event
stream: prompts, assistant output, tool activity, questions and answers,
shared files, and cost reports.

## Running against a local harness

1. Start a harness with the interactive frontend on the same machine:

   ```sh
   silo run --workspace ~/dev/myproject
   ```

   The harness writes a run file to `~/.llmdevsilo/run/<harness_id>.json`
   containing its address, its TLS certificate fingerprint, and the path to
   a local auth token.

2. Run the app:

   ```sh
   cd apps/silo_app
   flutter run -d macos
   ```

3. Tap **Add harness → Connect to a local harness**. The app lists the run
   files it finds, reads the token and certificate fingerprint from the one
   you pick, and connects. No pairing is needed for local harnesses.

On macOS you can also use **Add harness → Start a local harness**: pick a
workspace directory and the app runs `silo run --workspace <dir>` for you,
then attaches to the run file once it appears. The dialog remembers every
field across launches — including when it is cancelled or the app is
closed — and prefills the last-used values the next time it opens. See the
next section for how the app locates the `silo` binary.

## How the app finds the `silo` binary

The **Start a local harness** dialog has a **silo binary** field, prefilled
with the first existing file among, in order:

1. the path you last entered in that field (persisted across launches);
2. the path in the `SILO_BIN` environment variable;
3. `silo` in each directory on `PATH`;
4. `~/.cargo/bin/silo`, `/opt/homebrew/bin/silo`, and `/usr/local/bin/silo`.

You can edit the field before starting; the dialog shows the exact command
it will run. Note that a GUI app launched from the Finder or the Dock does
not inherit your shell's `PATH`, so the conventional locations in step 4
matter more than they would in a terminal. If nothing is found, build silo
with `cargo build --release` in the llmdevsilo repository and enter the
path to `target/release/silo` in the field, or install silo somewhere the
app probes.

## Pairing from a phone (or any remote client)

1. On a client that is already connected (for example the desktop app or
   the TUI), choose **Pair another device**. The app opens a sheet with
   everything the other device needs:

   - the harness's WebSocket URL (`wss://host:port`);
   - the pinned certificate fingerprint (hex SHA-256);
   - a one-time pairing code (8 characters, valid for 120 seconds), shown
     large with a live countdown.

   Every field has a copy button, and **Copy connection details** copies
   the whole block at once. When the harness address is loopback
   (`127.0.0.1` or `localhost`) or unspecified (`0.0.0.0`), the sheet also
   lists candidate LAN URLs built from this machine's network interfaces,
   since the address the app used is not dialable from another device. A
   loopback address additionally gets a warning: other devices cannot
   reach the harness at all unless `silo run` was started with
   `--listen 0.0.0.0:<port>` (or a LAN address), and the sheet shows that
   exact flag with the real port.
2. On the phone, tap **Add harness → Pair with a harness** and enter the
   WebSocket URL, pairing code, and certificate fingerprint from the
   sheet.
3. The app generates a fresh Ed25519 key pair, registers the public key
   with the harness, and stores the private key in the platform keystore
   (Keychain on iOS/macOS, the Android keystore on Android). Later
   connections authenticate by signing a server-issued challenge; the
   pairing code is never needed again.

On macOS the app stores secrets in the legacy login keychain
(`useDataProtectionKeyChain: false` in `lib/src/connection/secret_store.dart`).
The modern data-protection keychain needs the `keychain-access-groups`
entitlement, which only builds under real development signing; the legacy
keychain works with Flutter's default ad-hoc signing, so `flutter run -d
macos` needs no Apple developer account. Projects that adopt development
signing can add the entitlement and flip the option back. If the keystore
ever rejects operations anyway, the app keeps settings in memory for the
session and logs the failure instead of crashing.

## TLS and certificate pinning

The harness uses a self-signed TLS certificate. On desktop and mobile the
app pins it: the connection is accepted only if the certificate's SHA-256
fingerprint matches the stored one (from the run file for local harnesses,
or entered at pairing time for remote ones).

**Web limitation:** browsers do not let page code inspect TLS certificates,
so certificate pinning is not possible on the web build. The browser's
normal certificate validation applies instead, which means a self-signed
harness certificate is rejected until you accept it once: open
`https://<host>:<port>/` in a tab and click through the certificate
warning (the harness serves a confirmation page), then connect. The
pairing sheet shows this address as a hint, and when a connection fails on
the web build, the error banner repeats it as copyable text with a Retry
button. For a setup without the warning, put the harness behind a
certificate the browser trusts (for example a reverse proxy with a
certificate from a real certificate authority).

## Security assumptions

Per the project design, all code in a harness workspace is assumed to be or
become open source: there are no secrets in the workspace, credentials
exposed to the model are temporary development credentials, and production
data is never attached to a harness. The app stores its own secrets (local
tokens, pairing private keys, pinned certificate fingerprints) in the
platform keystore via `flutter_secure_storage`, and never sends them
anywhere except the harness they belong to.

On macOS the app runs without the App Sandbox so it can read run files and
spawn local harnesses; see `macos/Runner/README.md`.

## Development

```sh
flutter analyze
flutter test
```

Code layout:

- `lib/src/protocol/` — Dart mirrors of the `silo-core` wire types
  (`ClientMessage`, `ServerMessage`, `Event`, `AccessReport`, `RunInfo`),
  with JSON shapes matching the Rust serde output exactly.
- `lib/src/connection/` — the WebSocket connection (handshake, the three
  auth methods, backlog catch-up, reconnect with resume), the ordered event
  store, the persisted harness registry, secret storage, and local-harness
  discovery/spawning.
- `lib/src/ui/` — home screen (harness list, add flows), chat screen
  (transcript, question card, input row, access sheet, cost chip), and the
  pairing sheet (`pairing_sheet.dart`, with its pure helpers in
  `pairing_info.dart`).
