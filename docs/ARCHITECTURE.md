# Architecture

This document describes how the design in [DESIGN.md](DESIGN.md) maps onto the
codebase. Read it together with the rustdoc comments in
`crates/silo-core/src/` — the types and traits in `silo-core` are the
contracts between all other crates.

## Crate layout

| Crate | Role |
| --- | --- |
| `silo-core` | Shared types and traits. No I/O beyond the journal writer. Every other crate depends only on this crate (plus external libraries), never on its sibling implementation crates. |
| `silo-llm` | LLM backends: Anthropic Messages REST, OpenAI Responses REST, OpenAI WebSocket (Realtime, text only), managed local model server, scripted mock. |
| `silo-proxy` | The egress proxy: domain allowlist, intranet/localhost/link-local blocking (post-DNS, IPv4/IPv6/IPv4-mapped-IPv6), TLS interception with a per-session ephemeral CA, credential injection, traffic journaling, DNS filter module, mock proxy. |
| `silo-sandbox` | Sandbox backends (mock everywhere; sandbox-exec and Linux-VM on macOS; gVisor and microVM on Linux), the sandbox-side tool implementations (routing tool calls to the helper), scratch-space management, access reports. |
| `silo-helper` | The untrusted helper process that runs *inside* every sandbox. Library plus thin binary. Connects back to the harness and executes Exec/Read/Write/Edit/ListDir/Fetch requests. |
| `silo-workspace` | Workspace lifecycle: lock (snapshot + containerize), attach (mount for a harness), unlock (force-terminate harness, diff against snapshot, flag auto-exec surfaces). |
| `silo-frontend` | Frontend implementations: interactive (TLS WebSocket server with auth), headless (Exit tool), mock (scripted). |
| `silo-harness` | The orchestrator: agent loop, subagent spawning, tool routing, journaling, cost events, shutdown. |
| `silo` | The CLI binary: `silo run`, `silo workspace lock/unlock/status`, `silo shell`, `silo replay-test`, `silo harnesses`. |
| `apps/silo-tui` | Interactive colorful terminal client app (connects to the interactive frontend over TLS WebSocket). A Cargo workspace member that lives under `apps/` with the other client applications. |
| `apps/silo_app` | Flutter client (desktop, mobile, web). Multiple simultaneous harness connections; can spawn local harnesses on desktop. |

Dependency rule: implementation crates (`silo-llm`, `silo-proxy`,
`silo-sandbox`, `silo-workspace`, `silo-frontend`) depend on `silo-core`
only. Only `silo-harness` and the binaries compose them.

## Runtime composition

`silo run` builds one harness session:

1. Load `HarnessConfig` (TOML file and/or flags). Validate the read
   allowlist with `silo_core::risk::scan_allowlist` and refuse risky
   entries.
2. Create the clock (`RealClock`, or `FakeClock` when `--deterministic`),
   the `JournalWriter` (under the state directory), and the `EventBus`.
3. `silo_workspace::WorkspaceManager::attach` the locked workspace → the
   workspace mount path for the sandbox.
4. `silo_proxy::create_proxy` (or `create_mock_proxy`) → start → a
   `ProxyHandle` (HTTP proxy address + session CA public PEM).
5. `silo_sandbox::create_sandbox` with the config, proxy handle, and
   journal → start. The sandbox creates its scratch space, writes the CA
   public PEM into the sandbox as `proxy-ca.pem`, launches `silo-helper`
   inside the sandbox, and accepts the helper's connection.
6. `silo_frontend::create_frontend` → start with a `FrontendContext`.
7. Build the `ToolRegistry`:
   - sandbox tools (`Read`, `Write`, `Edit`, `Bash`, `WebFetch`,
     `WebSearch`) → owner `Sandbox`;
   - frontend tools (interactive: `AskUserQuestion`, `SendUserFile`;
     headless: `Exit`; mock: all three) → owner `Frontend`;
   - `silo_llm::common::agent_tool_def()` (`Agent`) → owner `Harness`.
8. Run the top-level agent loop until the frontend requests shutdown, the
   Exit tool is called, or a signal arrives. Then shut everything down in
   reverse order and detach the workspace (which stays locked).

### The agent loop (silo-harness)

For each agent (top-level `agent-0`, subagents `agent-N`):

- Ask the frontend for user input when the top-level conversation needs a
  user turn (`Frontend::next_user_input`); the harness emits
  `EventPayload::AwaitingInput` right before. Subagents never get user
  turns; their conversation is seeded from the Agent tool's prompt and runs
  until the model stops calling tools.
- Call `LlmBackend::complete` with the conversation and the tool list
  filtered by `AgentKind`. Journal `LlmRequest`/`LlmResponse`. Emit
  `AssistantText` for text blocks, `ToolUse` for tool-use blocks, then a
  `CostReport` event with `LlmBackend::usage()`.
- Route each tool call by `ToolRegistry::owner_of`:
  - `Sandbox` → `Sandbox::run_tool`;
  - `Frontend` → `Frontend::run_tool` (for `SendUserFile`, the harness
    first reads the file via the sandbox `Read` path and injects
    `content_b64` into the call input before forwarding);
  - `Harness` → the `Agent` tool: spawn a subagent task (same sandbox,
    shared backend, `AgentKind::Subagent`), emit
    `AgentSpawned`/`AgentCompleted`, return the subagent's final text as
    the tool result. Tool calls within one response run sequentially in
    order; subagents themselves run concurrently with their parent's next
    completion only if the model issues other tool calls alongside —
    keep it simple: execute tool calls in order, awaiting each.
  - Subagent depth is capped at 3 and concurrent subagents at 8; exceeding
    either returns an error tool result.
- Journal every `ToolExec` with the owning component name ("sandbox",
  "frontend", "harness").
- Append the tool results as the next user message (tool_result blocks)
  and loop. `StopReason::EndTurn` on the top level emits `TurnComplete`
  and goes back to `next_user_input`.
- The `Exit` tool (headless/mock): the frontend resolves it by sending
  `FrontendCommand::Shutdown { message }`; the harness emits
  `EventPayload::Shutdown`, returns the message in `HarnessOutcome`.

Uploads: when an interactive client uploads a file, the frontend emits
`FileShared { origin: Client }`. The harness listens for these and writes
the bytes into `_uploads/<name>` in the workspace via the sandbox helper,
so the model can read them.

## State directory

`~/.llmdevsilo` (override: `LLMDEVSILO_STATE_DIR`). See
`silo_core::paths`. Layout:

- `run/<harness_id>.json` — `silo_core::protocol::RunInfo` for each live
  interactive harness (removed on exit). Local clients discover harnesses
  here.
- `harness/<harness_id>/local-token` — 64 hex chars, mode 0600. The
  filesystem-shared key for local client auth.
- `harness/<harness_id>/tls-cert.pem`, `tls-key.pem` — the interactive
  server's self-signed certificate (per harness, persisted so fingerprints
  stay stable across reconnects within a session).
- `harness/<harness_id>/authorized-keys.json` — paired client public keys
  (`key_id` → Ed25519 public key, client name).
- `journals/<harness_id>.jsonl` — the journal (one per session by
  default).
- `workspaces/` — workspace registry, snapshots, and containers.
- `client-keys/` — private keys for the TUI client.

The state directory is on the hardcoded risk list: it can never be added
to the sandbox read allowlist, so journals, tokens, and keys are not
reachable from inside the sandbox.

## Interactive frontend protocol

See `silo_core::protocol` for the wire types and auth flow. Conventions:

- The server is a TLS WebSocket server (`rcgen` self-signed cert). Clients
  pin the certificate by SHA-256 fingerprint: local clients read it from
  the run file; remote clients learn it at pairing time (trust-on-first-use)
  and store it.
- Auth: `local_token` (from the run file's token path), or pairing code →
  register Ed25519 public key → on later connections request `challenge`
  and answer with `signature`. Pairing codes are 8 characters, single-use,
  expire after 120 seconds.
- After `AuthOk`, clients send `RequestEvents { from_seq }` to catch up,
  then receive live `Event` messages. All clients see the same stream:
  prompts, assistant output, tool activity, questions/answers, files,
  cost reports.
- `AskUserQuestion`: server emits `question_asked`; every client may
  render it; the first `AnswerQuestion` wins, the server emits
  `question_answered` (which removes the question UI everywhere) and
  unblocks the tool call. Later answers for the same id are ignored.
- A `Prompt` from any client is immediately emitted as a `user_prompt`
  event (so all clients display it) and queued; the harness consumes
  queued prompts in order whenever the top-level agent awaits input.

## Helper protocol

See `silo_core::helper`. The sandbox backend listens on a Unix socket in
the scratch space (or TCP loopback for VM backends), passes the connect
string to the helper (argv or `SILO_HELPER_CONNECT`), and exchanges
JSON-line `HelperRequest`/`HelperResponse` frames. Request ids let
responses arrive out of order; the sandbox side correlates by id.

Environment inside the sandbox (set by the sandbox backend when spawning
the helper, and for `Bash` executions):

- `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` → `http://127.0.0.1:<proxy port>`
  (or the VM-routed proxy address).
- `SILO_PROXY_CA`, `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO`,
  `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, `CARGO_HTTP_CAINFO` →
  `<scratch>/proxy-ca.pem` (the session CA public certificate).
- `HOME` → `<scratch>/home` (created by the sandbox backend).
- `TMPDIR` → `<scratch>/tmp`.

The helper's `Fetch` op uses the proxy and the session CA explicitly
(reqwest with `Proxy::all` + `add_root_certificate`), so `WebFetch` and
`WebSearch` are subject to exactly the same egress policy as everything
else in the sandbox.

`WebSearch` is implemented in `silo-sandbox` as a `Fetch` of
`https://html.duckduckgo.com/html/?q=...` (the search domain must be on
the allowlist) plus result parsing harness-side.

## Determinism and replay

- Events and journal records carry `Timestamp { logical, wall_ms }`.
  Under `--deterministic` the harness uses `FakeClock` and `wall_ms` is
  absent, so journals are byte-stable.
- Mock components never use timers; they consume their slice of the
  `SharedScript` strictly in order. Anything that would race in a real
  session is serialized by script position and event sequence numbers.
- `silo replay-test <journal> -o <script.json>` converts a journal into a
  `TestScript` (`silo_core::replay::script_from_journal`). Running
  `silo run --frontend mock --llm mock --sandbox mock --script script.json
  --deterministic` replays the session with no real LLM, no code
  execution, and no file writes.

## Security invariants (enforced in code, asserted in tests)

1. No secrets in journals or events: secrets only exist as `SecretString`
   and env-var names; `NetworkRecord` carries metadata only.
2. The proxy never persists the session CA private key, and only the
   public certificate is readable from the sandbox.
3. The proxy refuses connections to loopback, RFC 1918, link-local,
   unique-local, and IPv4-mapped equivalents — checked against every
   resolved address, not just the name.
4. Credential injection only adds headers for exactly-matching allowlisted
   hosts, over connections the proxy itself opened.
5. The sandbox read allowlist is risk-scanned (`silo_core::risk`) before
   the harness starts.
6. Logs/journals live outside the workspace and outside the allowlist.

## Conventions for contributors

- Rust edition 2021. `cargo fmt` formatting. No new dependencies beyond
  the root `[workspace.dependencies]` table without a strong reason; add
  crate-local `{ workspace = true }` references as needed.
- All errors flow through the enums in `silo_core::error`; no panics on
  I/O or network paths. `expect` only where invariants make failure
  impossible (e.g. poisoned mutexes).
- Every module gets unit tests; cross-component behavior gets integration
  tests in `silo-harness/tests` using the mock components.
- Linux-only code is gated `#[cfg(target_os = "linux")]` and must pass
  `cargo check --target aarch64-unknown-linux-gnu` (std installed in CI
  and locally).
- Comments: plain, neutral, present tense; explain behavior, not history
  or justification.
