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
then attaches to the run file once it appears. This requires `silo` to be
on your `PATH`.

## Pairing from a phone (or any remote client)

1. On a client that is already connected (for example the desktop app or
   the TUI), choose **Pair another device**. The harness issues a one-time
   pairing code (8 characters, valid for 120 seconds).
2. On the phone, tap **Add harness → Pair with a harness** and enter the
   harness's WebSocket URL (`wss://host:port`), the pairing code, and the
   certificate fingerprint shown alongside the code.
3. The app generates a fresh Ed25519 key pair, registers the public key
   with the harness, and stores the private key in the platform keystore
   (Keychain on iOS/macOS, the Android keystore on Android). Later
   connections authenticate by signing a server-issued challenge; the
   pairing code is never needed again.

## TLS and certificate pinning

The harness uses a self-signed TLS certificate. On desktop and mobile the
app pins it: the connection is accepted only if the certificate's SHA-256
fingerprint matches the stored one (from the run file for local harnesses,
or entered at pairing time for remote ones).

**Web limitation:** browsers do not let page code inspect TLS certificates,
so certificate pinning is not possible on the web build. The browser's
normal certificate validation applies instead, which means a self-signed
harness certificate is rejected. To use the web client, put the harness
behind a certificate the browser trusts (for example a reverse proxy with a
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
  (transcript, question card, input row, access sheet, cost chip).
