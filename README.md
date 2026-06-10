# llmdevsilo

A harness for running LLM coding agents in a *safe* environment: the agent
gets real tools (a shell, the project source, compilers and test runners,
the Internet, subagents, the ability to install and run programs) while the
host machine, the user's data, and the user's credentials stay out of
reach. The security comes from static sandboxing — not from permission
prompts, which fatigue users into rubber-stamping.

The full requirements document is [docs/DESIGN.md](docs/DESIGN.md); the
implementation map is [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the
security model is [docs/SECURITY.md](docs/SECURITY.md).

> **Read this before using llmdevsilo.**
> The sandbox does not protect the *contents of the workspace* or anything
> you explicitly grant access to. The design assumes that **all code being
> developed is or will be open source**, that any credentials reachable by
> the agent (for example tokens the proxy injects for GitHub access) are
> **temporary, scoped development credentials**, and that **production data
> and environments are never exposed** to the agent. Do not point a harness
> at a workspace containing secrets.

## What's in the box

| Component | What it is |
| --- | --- |
| `silo` | The harness binary: runs an LLM agent (and its subagents) against a locked workspace inside a sandbox, with all network egress through a filtering, credential-injecting proxy. |
| `silo-tui` | An interactive, colorful terminal client that connects to a running harness. |
| `apps/silo_app` | A Flutter app (desktop, mobile, web) that connects to one or more harnesses, locally or remotely. |

One harness = one workspace + one sandbox + one LLM backend (plus
subagents) + one frontend (interactive WebSocket server, headless, or mock
for tests).

## Quick start

Build everything (Rust 1.85+):

```sh
cargo build --release
```

Lock a directory as a workspace and start an interactive harness with the
Anthropic backend and a macOS sandbox:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
target/release/silo workspace lock ~/dev/myproject
target/release/silo run \
    --workspace ~/dev/myproject \
    --llm anthropic --model claude-sonnet-4-6 \
    --sandbox auto \
    --allow-read /usr/bin --allow-read /bin --allow-read /opt/homebrew \
    --allow-domain docs.rs --allow-domain crates.io --allow-domain '*.github.com' \
    --quota-usd 20
```

Then connect a client:

```sh
target/release/silo-tui            # picks up the local harness automatically
```

Run a one-shot background task instead (no UI; the agent calls the Exit
tool when done):

```sh
target/release/silo run --workspace ~/dev/myproject \
    --frontend headless --prompt "Fix the failing tests" \
    --llm anthropic --sandbox auto --allow-read /usr/bin --quota-usd 5
```

When you want your files back, unlock. The harness is terminated, and you
get a review of *everything* that changed while the workspace was locked —
with changes to auto-exec surfaces (git hooks, `.envrc`, `.vscode`
configuration, `package.json` scripts, `build.rs`, …) flagged first:

```sh
target/release/silo workspace unlock ~/dev/myproject
```

Work inside the same sandbox the agent uses (same filesystem and network
restrictions — this is the safe way to run the code the agent wrote,
because anything malicious it planted is still confined):

```sh
target/release/silo shell --workspace ~/dev/myproject --allow-read /usr/bin
```

Pair a phone or another machine: in any connected client request a pairing
code (TUI: `/pair`), or start the harness with `--pairing-code`. Enter the
address, code, and certificate fingerprint in the remote client; it
generates a key pair and authenticates with signatures from then on.

Browsers (the web build of the Flutter client) cannot pin the harness's
self-signed certificate the way the other clients do. Either open
`https://host:port` in the browser once and accept the certificate warning
— the harness answers with a small confirmation page, and the web client
can connect from then on — or start the harness with `--tls-cert` and
`--tls-key` pointing at a PEM certificate and key the browser already
trusts (for development, [mkcert](https://github.com/FiloSottile/mkcert)
generates such certificates).

## Choosing a sandbox

| Backend | Platform | Status | Use case |
| --- | --- | --- | --- |
| `sandbox-exec` | macOS | Implemented, integration-tested | Native macOS development (the only practical option for building macOS programs). Seatbelt profile: read-only allowlist, read/write workspace+scratch, network only to the egress proxy. |
| `gvisor` | Linux | Implemented; runtime validation on a Linux host pending | Strong syscall isolation via runsc; egress only through a relay to the harness proxy. |
| `linux-vm` | macOS | Designed, scaffolded — not yet runnable | Linux development from a Mac via Virtualization.framework; see docs/sandbox-backends.md. |
| `microvm` | Linux | Designed, scaffolded — not yet runnable | Firecracker-style hardware isolation; see docs/sandbox-backends.md. |
| `mock` | any | Implemented | Tests: nothing executes, tool calls are validated and answered from a script. |

These are not interchangeable: they have different security tradeoffs,
documented in [docs/sandbox-backends.md](docs/sandbox-backends.md).

## LLM backends

`--llm anthropic` (Messages REST), `--llm openai` (Responses REST),
`--llm openai-ws` (Realtime WebSocket, text only), `--llm local` (a
locally hosted OpenAI-compatible server, optionally spawned and managed by
the harness — for example llama.cpp's `llama-server`), `--llm mock`
(scripted, for tests).

Cloud backends meter usage in tokens and dollars, enforce session quotas
(`--quota-tokens`, `--quota-usd`), and report ongoing cost to every
connected client.

## Replay testing

Every session writes a journal (under `~/.llmdevsilo/journals/`) recording
all module interactions: prompts, full LLM requests and responses, every
tool execution, and network traffic summaries — but never secrets. A
journal converts into a deterministic regression test:

```sh
silo replay-test ~/.llmdevsilo/journals/<id>.jsonl -o session.json
silo run --workspace /tmp/replay --create --deterministic \
    --frontend mock --llm mock --sandbox mock --mock-proxy --script session.json
```

The replay uses mock components throughout: no model calls, no code
execution, no network.

## State

Everything the harness persists outside workspaces lives in
`~/.llmdevsilo` (override with `LLMDEVSILO_STATE_DIR`): journals, frontend
authentication material, workspace snapshots and containers. The sandbox
can never read this directory — it is on the hardcoded risk list, along
with `~/.ssh`, browser profiles, cloud credentials, and other known
sensitive paths that the harness refuses to add to the read allowlist.

## Development

```sh
cargo test --workspace          # unit + integration tests (mock components)
cargo clippy --workspace --all-targets
cd apps/silo_app && flutter test
```

The crate layout and contribution conventions are described in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
