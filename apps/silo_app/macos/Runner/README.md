# macOS Runner entitlements

`DebugProfile.entitlements` and `Release.entitlements` set
`com.apple.security.app-sandbox` to `false`. The app reads harness run files
under `~/.llmdevsilo/run/` and spawns `silo run` to start local harnesses;
the macOS App Sandbox blocks both, so the app runs unsandboxed on macOS.
`com.apple.security.network.client` stays enabled for the WebSocket
connection to harnesses.

This is the app's own sandbox setting; it is unrelated to the workspace
sandbox the harness puts around the model.
