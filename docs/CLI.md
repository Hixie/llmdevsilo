# The `silo` command-line reference

`silo` is the harness binary: it runs sandboxed LLM coding sessions
against locked workspaces, manages those workspace locks, opens user
shells inside the same sandbox the model uses, converts session journals
into replayable test scripts, and lists running harnesses.

This document is the complete reference for every command and flag. It
assumes you have read [README.md](../README.md). Related documents, not
duplicated here:

- [PROTOCOLS.md](PROTOCOLS.md) — the wire protocols: the interactive
  WebSocket protocol and its authentication, the helper protocol, the
  proxy behavior, and what gets journaled.
- [SECURITY.md](SECURITY.md) — the security model and its boundaries.
- [SANDBOX-BACKENDS.md](SANDBOX-BACKENDS.md) — each sandbox backend's
  enforcement mechanism, invariants, and tradeoffs.
- [apps/silo_app/README.md](../apps/silo_app/README.md) — the Flutter
  client, including how it stores secrets and the macOS keychain and
  ad-hoc signing notes.

```
Usage: silo <COMMAND>

Commands:
  run          Run one harness session
  workspace    Manage workspace locks
  shell        Open an interactive shell under the same sandbox policy as the LLM
  replay-test  Convert a journal into a replayable test script
  harnesses    Inspect running harnesses
```

---

## Global behavior

### The state directory

Everything the harness persists outside workspaces lives in one state
directory: `~/.llmdevsilo` by default, overridable with the
`LLMDEVSILO_STATE_DIR` environment variable. Its layout:

| Path | Contents |
| --- | --- |
| `run/<harness-id>.json` | Run files: connection details for one live harness (address, certificate fingerprint, local token path, process id, workspace, sandbox policy). Written by the interactive frontend, removed at shutdown, pruned by `silo harnesses list` when stale. |
| `journals/<id>.jsonl` | Session journals (JSON Lines), one per harness session and one per `silo shell` session. |
| `harness/<harness-id>/` | Per-harness server material: the local auth token, registered client public keys, and the generated TLS certificate. |
| `client-keys/` | Private keys for local clients such as the terminal user interface. |
| `workspaces/` | The workspace registry (`registry.json` plus its lock file) and one snapshot directory per locked workspace (manifest, content blobs, and the contents container). |

The state directory is itself on the hardcoded risk list: the harness
refuses to add it to the sandbox read allowlist, so sandboxed code can
never read journals, tokens, or keys.

### Logging

Diagnostic logging goes to standard error, controlled by the `RUST_LOG`
environment variable using the standard tracing filter syntax (for
example `RUST_LOG=debug` or `RUST_LOG=silo_proxy=trace`). The default
filter is `warn`. Standard output is reserved for the command's actual
output (reports, connection details, final messages).

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. For `silo run` this means a session that ends through the normal shutdown path — a client shutdown request or the headless Exit tool. The final message, when there is one, is printed to standard output. |
| 1 | A runtime error. The message is printed to standard error, prefixed `silo:`. |
| 2 | A command-line usage error (unknown flag, missing argument, bad value). |
| 3 | `silo run` only: the session was ended by repeated LLM failures (for example an exhausted quota) — the first failure for the headless frontend, the eighth consecutive failed turn for the others. The failure message is printed to standard error, not standard output. |
| other | `silo shell` exits with the sandboxed shell's or command's own exit code (clamped to the 0–255 range). |

### The no-remote-secrets configuration model

Configuration never contains secret values. Every place a secret is
needed — LLM API keys, injected credentials — the configuration names an
environment variable, and the value is read from the harness's
environment at startup. Consequences:

- Configuration files and journals can be shared safely; they carry
  environment-variable names, never values.
- Secret values held in memory are wrapped in a type that prints and
  serializes as `[redacted]`, so they cannot leak through logs, journals,
  or debug output by construction.
- Secrets live only in the harness process, outside the sandbox. The
  sandboxed environment is started with a cleared environment and never
  sees them (credential injection happens at the proxy; see
  `--inject-credential` below).

---

## silo run

### Synopsis

```
silo run [OPTIONS]
```

### Description

Runs one harness session to completion: one workspace, one sandbox, one
LLM backend (plus its subagents), one frontend, with all network egress
through a filtering, credential-injecting proxy, and a journal recording
every module interaction.

The session ends when the frontend requests shutdown (a connected client
asks for it, or the headless agent calls the Exit tool), when a signal
arrives, or — for the headless frontend — on the first LLM failure. At
the end, the final message (if any) is printed to standard output and the
journal path is printed to standard error as `journal: <path>`. A session
ended by repeated LLM failures instead prints the failure message to
standard error and exits with code 3 (see the exit-code table above).

The defaults select a real session: the LLM backend is chosen from the
environment (see the LLM backend flags below) and the sandbox defaults to
`auto`, the platform's native backend. The mock components exist for
testing, require `--script`, and are only used when selected explicitly.

Before the session starts, `silo run` prints a warning to standard error
for each setting that cannot have its intended effect: an
`--inject-credential` host that no allowed domain covers (the credential
could never apply), a paid LLM backend with no `--quota-tokens` or
`--quota-usd` limit, and an `--allow-risky-path` entry that matches no
read-allowlist entry.

### Workspace flags

- `--workspace <WORKSPACE>` — the locked workspace directory. Required
  unless `--config` provides one. The workspace must already be locked
  (see `silo workspace lock`) unless `--create` is given. Attaching fails
  if another live harness is already attached; an attachment whose
  process is dead is pruned and replaced automatically.
- `--create` — lock the workspace first, exactly as
  `silo workspace lock` would (the directory may be new, may be empty,
  or may exist with contents, but must not already be locked). Lock
  warnings are printed to standard error. When combined with
  `--deterministic`, the lock uses the plain-directory container strategy
  on every platform, so deterministic runs do not depend on platform
  disk-image tooling.
- `--config <CONFIG>` — a TOML configuration file providing the same
  settings as the flags (see `silo_core::config::HarnessConfig` for the
  schema). The file supplies the base configuration and every
  command-line flag overrides the corresponding value. List-valued flags
  (`--allow-read`, `--allow-domain`, `--inject-credential`) replace the
  file's whole list when given at least once; they do not append.

### Frontend flags

- `--frontend <FRONTEND>` — `interactive`, `headless`, or `mock`.
  Default: `interactive`.

  - **interactive** starts a TLS WebSocket server speaking the protocol
    in [PROTOCOLS.md](PROTOCOLS.md) section 2. It contributes the
    `AskUserQuestion` and `SendUserFile` tools to the model. It writes a
    run file under `<state>/run/` so local clients (the terminal client,
    the Flutter app) can discover it, and once the server is up it prints
    the connection details to standard output:

    ```
    Interactive frontend: wss://127.0.0.1:55123
    Certificate fingerprint (SHA-256): ab12…
    Run file: /Users/you/.llmdevsilo/run/<id>.json
    ```

  - **headless** runs one prompt to completion with no user interaction.
    It contributes only the `Exit` tool. The first model input is the
    prompt plus an instruction to call Exit when done; every later input
    request gets a canned non-interactive reminder. At shutdown it prints
    the final message and one `cost[<backend>]: …` line per backend.

  - **mock** drives the harness from a test script (`--script`) and
    verifies observed events; used for deterministic end-to-end tests and
    journal replay.

- `--prompt <PROMPT>` — the initial prompt. Required by (and only used
  by) the headless frontend.
- `--listen <LISTEN>` — listen address for the interactive WebSocket
  server, as an IP address and port (for example `0.0.0.0:7777`).
  Default: `127.0.0.1:0`, that is, loopback only with an ephemeral port.
  Listening on a non-loopback address exposes the harness to the
  network; what still protects you is that every connection is TLS and
  every client must authenticate (local token, pairing code, or a
  registered key signing a challenge) before any other message is
  accepted. Exposure is still a larger attack surface — prefer loopback
  plus an SSH tunnel where practical.
- `--tls-cert <TLS_CERT>` / `--tls-key <TLS_KEY>` — a PEM certificate
  chain and its matching PEM private key for the interactive server.
  Must be set together. Default: a per-harness self-signed certificate
  is generated under `<state>/harness/<id>/` and reused across runs of
  that harness id. Native clients pin the certificate by its SHA-256
  fingerprint, so the self-signed default is fine for them. Browsers
  (the web build of the Flutter client) cannot pin fingerprints; for
  them, supply a certificate the browser already trusts (for development,
  mkcert generates one). The printed fingerprint is always that of the
  certificate actually in use, so fingerprint pinning keeps working with
  a supplied certificate too.
- `--pairing-code` — print a one-time pairing code at startup
  (interactive frontend only). The code is single use and valid for 120
  seconds; a remote client redeems it to register an Ed25519 public key
  and authenticates with challenge signatures from then on. Connected
  clients can also request fresh pairing codes at any time (terminal
  client: `/pair`), so this flag is only needed when no client is
  connected yet.

### LLM backend flags

- `--llm <LLM>` — `anthropic`, `openai`, `openai-ws`, `local`, or
  `mock`. Without this flag (and without a backend in the `--config`
  file), the backend is chosen from the environment: if
  `OPENAI_API_KEY` is set and non-empty, `openai` with default model
  `gpt-5`; otherwise, if `ANTHROPIC_API_KEY` is set and non-empty,
  `anthropic` with default model `claude-sonnet-4-6`. The auto-selection
  is announced with one line on standard error. If neither variable is
  set, the command fails with an error naming the three options (pass
  `--llm`, or set one of the two variables). An explicit `--llm` always
  wins; a backend set in the `--config` file beats the environment; and
  `--model` and `--api-key-env` override the auto-selected defaults.

  - `anthropic` — the Anthropic Messages REST API.
  - `openai` — the OpenAI Responses REST API.
  - `openai-ws` — the OpenAI Realtime WebSocket API (text only).
  - `local` — an OpenAI-compatible chat-completions server on
    localhost. With a `local_server_command` in the TOML configuration,
    the harness spawns that command (through `sh -c`), polls
    `GET <base>/v1/models` (falling back to `GET <base>/health`) every
    250 milliseconds for up to 60 seconds until the server is ready, and
    kills the server when the backend is dropped. Without it, the
    backend expects a server already running at `--base-url`. No
    Authorization header is sent; token usage is metered but priced at
    zero unless pricing is configured.
  - `mock` — scripted responses from `--script`, for tests.

- `--model <MODEL>` — the model identifier passed to the backend.
  Default: the environment auto-selection's model when it applies
  (`gpt-5` or `claude-sonnet-4-6`, as above); `claude-sonnet-4-6`
  otherwise.
- `--api-key-env <API_KEY_ENV>` — the name of the environment variable
  holding the LLM API key. Defaults: `ANTHROPIC_API_KEY` for the
  Anthropic backend, `OPENAI_API_KEY` for both OpenAI backends. The
  variable must be set in the harness's environment; only its name ever
  appears in configuration or journals. The key never enters the
  sandbox.
- `--base-url <BASE_URL>` — override the service base URL. For the
  local backend this is the local server URL; default
  `http://127.0.0.1:8080`.

### Quota flags

- `--quota-tokens <QUOTA_TOKENS>` — maximum total tokens (input plus
  output) for the session. Default: unlimited.
- `--quota-usd <QUOTA_USD>` — maximum dollar spend for the session,
  computed from configured pricing. Default: unlimited.

Running a paid backend (`anthropic`, `openai`, `openai-ws`) with neither
quota flag prints a startup warning to standard error suggesting one.

The quota is checked before each LLM request. Once exhausted, every
further request fails with a quota-exceeded error. What happens next
depends on the frontend:

- **headless**: the session ends on the first LLM failure (the loop
  would otherwise spin forever, because the headless frontend answers
  every input request immediately). The quota message is printed to
  standard error and the exit code is 3.
- **interactive** (and mock): the failure is broadcast to clients as an
  error event and the harness returns to awaiting input, so a human can
  decide what to do. After 8 consecutive failed turns the session ends:
  the last failure message is printed to standard error and the exit
  code is 3.

Connected clients see ongoing cost reports per backend; the headless
frontend prints them at exit.

### Sandbox flags

- `--sandbox <SANDBOX>` — `auto`, `mock`, `sandbox-exec`, `linux-vm`,
  `gvisor`, or `microvm`. Default: `auto` (unless the `--config` file
  sets a sandbox kind), which resolves per platform: `sandbox-exec` on
  macOS, `gvisor` on Linux. The mock sandbox requires `--script` and is
  only used when selected explicitly.

  Availability by platform (selecting a backend not compiled for the
  platform is an error):

  | Value | Platform | Status |
  | --- | --- | --- |
  | `sandbox-exec` | macOS | Implemented. Native processes under a kernel-enforced Seatbelt profile. Caveats: file *metadata* outside the allowlist is readable (so path traversal works), and services listening on the host loopback interface are reachable from inside the sandbox. |
  | `gvisor` | Linux | Implemented; pending validation on a real Linux host. Strong syscall isolation via runsc. |
  | `linux-vm` | macOS | Scaffolded, not yet runnable. Linux guest via Virtualization.framework. |
  | `microvm` | Linux | Scaffolded, not yet runnable. Firecracker-style hardware isolation. |
  | `mock` | any | Executes nothing; validates tool calls against `--script` and plays back recorded outputs. |

  The backends are not interchangeable; the per-backend enforcement
  mechanisms, invariants, and security tradeoffs are documented in
  [SANDBOX-BACKENDS.md](SANDBOX-BACKENDS.md).

- `--allow-read <ALLOW_READ>` — a host path the sandbox may read and
  execute (never write). Repeatable. Default: none, beyond the
  backend's fixed operating-system baseline (system libraries and
  binaries; see [SANDBOX-BACKENDS.md](SANDBOX-BACKENDS.md)). Each entry
  is checked against a hardcoded risk list before the session starts:
  the harness refuses any entry that equals, contains, or is contained
  in a known-sensitive path that exists on the system. The list covers
  SSH and GPG keys, cloud provider credentials (AWS, Azure, Google
  Cloud, Kubernetes, Docker), package-registry tokens (npm, PyPI,
  crates.io), `~/.netrc`, `~/.gitconfig` and `~/.git-credentials`,
  password stores, browser and mail profiles (Chrome, Chromium, Brave,
  Firefox, Safari, Thunderbird), macOS keychains and cookies, the
  Claude/OpenAI/Anthropic tool state directories, and the llmdevsilo
  state directory itself. The refusal names the entry, the exposed path,
  and the reason. This scan is best-effort defense in depth — it cannot
  know about every secret on your disk — so keep allowlist entries
  narrow (toolchains, not home directories).
- `--allow-risky-path <ALLOW_RISKY_PATH>` — accept one read-allowlist
  entry despite risk-scan hits. Repeatable. The value is matched against
  the `--allow-read` entries by location, not by spelling: both sides
  are canonicalized first, so a symlinked alias or a trailing slash
  still matches. It accepts the matching entry only, and other flagged
  entries are still refused; a value that matches no entry prints a
  startup warning that it had no effect. This is a deliberate override
  for the rare case where you have judged the exposure acceptable; the
  scan exists because a single broad entry can silently hand the model
  your credentials.
- `--allow-domain <ALLOW_DOMAIN>` — a domain the sandbox may reach
  through the egress proxy. Repeatable. Default: none (no network
  egress at all). An entry is either an exact host name
  (`crates.io`) or a wildcard (`*.github.com`), where the wildcard
  matches the base domain *and* every subdomain. Matching is
  case-insensitive and tolerates a single trailing dot. Independent of
  the allowlist, the proxy blocks localhost and private intranet
  addresses, so an allowlisted public name cannot be used to reach
  internal services (see [PROTOCOLS.md](PROTOCOLS.md) section 4).
- `--inject-credential <INJECT_CREDENTIAL>` — configure the proxy to
  attach a credential to requests for one host. Repeatable. Format:
  `host:header:ENV_VAR[:format]`, for example
  `api.github.com:Authorization:GH_TOKEN:Bearer {secret}`. The format
  part is optional (default `{secret}`) and may itself contain colons.
  Semantics:

  - The host must match exactly; wildcards are not supported here, and
    the host must *also* be allowed with `--allow-domain` (or a matching
    wildcard) for requests to reach it. A credential whose host no
    allowed domain covers prints a startup warning naming the missing
    `--allow-domain`.
  - The secret value is read once from the named environment variable
    when the proxy starts; a missing variable is a startup error.
  - The secret lives only in the harness process. It never enters the
    sandbox, never appears in journals or events (only the host name
    and a `credential_injected` boolean are recorded), and any
    client-supplied header of the same name is stripped before the
    injected value is set — sandboxed code cannot read the credential
    back or smuggle its own.
  - Use temporary, narrowly scoped development credentials (for
    example a fine-grained GitHub token limited to the one repository
    the agent works on). The design assumes any credential reachable
    through the proxy may be *used* by the agent for arbitrary requests
    to that host, even though it cannot be exfiltrated as a value.

### Testing flags

These exist for the replay-testing surface described in
[README.md](../README.md) and `silo replay-test` below.

- `--script <SCRIPT>` — a test script (JSON, the
  `silo_core::replay::TestScript` shape). Required when any mock
  component is selected (frontend, LLM, or sandbox); ignored otherwise.
- `--journal <JOURNAL>` — write the session journal to this path instead
  of `<state>/journals/<harness-id>.jsonl`. The path is validated
  against the read allowlist: a journal that the sandbox could read
  (inside an allowlisted directory, or with an allowlist entry inside
  the journal's directory) is refused.
- `--deterministic` — use a fake clock (sequence numbers only, no
  wall-clock timestamps), so journals are byte-stable across runs. Also
  disables operating-system signal handling, and makes `--create` use
  the plain-directory lock strategy on every platform.
- `--mock-proxy` — use the mock egress proxy with any sandbox backend.
  The mock sandbox always uses the mock proxy regardless of this flag.

### Platform notes

`auto` selects `sandbox-exec` on macOS and `gvisor` on Linux. Windows is
not supported. The non-default backends for each platform (`linux-vm`,
`microvm`) are scaffolds; selecting them fails at startup.

### Security considerations

The sandbox protects the host from the agent; nothing protects the
workspace contents from the agent. Do not point a harness at a workspace
containing secrets, and treat everything granted via `--allow-read`,
`--allow-domain`, and `--inject-credential` as readable, reachable, and
usable by the model respectively. See [SECURITY.md](SECURITY.md) for the
full model and its boundaries.

### Files read and written

Reads: the `--config` file, the `--script` file, `--tls-cert` and
`--tls-key`, and the environment variables named by `--api-key-env` and
`--inject-credential`. Writes: the journal, the run file (interactive),
per-harness TLS and token material under `<state>/harness/<id>/`, the
workspace registry (attach and detach records), and — with `--create` —
the workspace snapshot and container under `<state>/workspaces/`.

### Examples

An interactive session on macOS with the Anthropic backend, a spending
cap, and access to the Rust toolchain and the crates ecosystem:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
silo workspace lock ~/dev/myproject
silo run --workspace ~/dev/myproject \
    --llm anthropic --model claude-sonnet-4-6 \
    --sandbox auto \
    --allow-read /usr/bin --allow-read /bin --allow-read /opt/homebrew \
    --allow-domain crates.io --allow-domain '*.github.com' \
    --quota-usd 20
```

A one-shot headless task with a scoped GitHub token injected at the
proxy (the token never enters the sandbox):

```sh
export ANTHROPIC_API_KEY=sk-ant-...
export GH_TOKEN=github_pat_...
silo run --workspace ~/dev/myproject \
    --frontend headless --prompt "Fix the failing tests and push a branch" \
    --llm anthropic --sandbox auto \
    --allow-read /usr/bin \
    --allow-domain '*.github.com' \
    --inject-credential 'api.github.com:Authorization:GH_TOKEN:Bearer {secret}' \
    --quota-usd 5
```

A deterministic replay of a recorded session (no model calls, no code
execution, no network):

```sh
silo run --workspace /tmp/replay --create --deterministic \
    --frontend mock --llm mock --sandbox mock --mock-proxy \
    --script session.json
```

---

## silo workspace

### Synopsis

```
silo workspace lock <PATH>
silo workspace unlock <PATH>
silo workspace status <PATH>
```

### Description

A workspace is a directory you hand over to the harness. Locking
snapshots every file and moves the contents out of casual reach of the
host; unlocking terminates everything using the workspace, restores the
directory, and reports every change made while it was locked. The
registry of locked workspaces lives at
`<state>/workspaces/registry.json`, with per-workspace snapshot
directories beside it.

### silo workspace lock

Locks `<PATH>` as a workspace, creating the directory (empty) if it does
not exist. Locking an already-locked path fails. A workspace may not
contain, or live inside, the state directory.

What locking does, in order:

1. Reserves the path in the registry (so a concurrent lock of the same
   path fails).
2. Snapshots every file — path, mode, content hash, and content blobs
   for later diffing — into
   `<state>/workspaces/<id>/` (`manifest.json` and `blobs/`).
3. Moves the contents into a container:
   - **macOS** (default): a sparse bundle disk image
     (`workspace.sparsebundle`) managed with `hdiutil`, mounted only
     while a harness or shell is attached. If `hdiutil` fails, the lock
     falls back to the plain directory strategy and says so in a
     warning.
   - **Linux** (default when both `fuse2fs` and `mkfs.ext4` are on
     `PATH` at lock time): a raw ext4 disk image (`workspace.img`)
     formatted with `mkfs.ext4` and mounted user-space with `fuse2fs`
     (no root required), mounted only while a harness or shell is
     attached. The image size is fixed at lock time — twice the content
     size plus 256 MiB of headroom, at least 1 GiB — and does not grow.
     If the image setup fails, the lock falls back to the plain
     directory strategy and says so in a warning.
   - **Plain directory** (Linux without the ext4 tools, other
     platforms, and `--deterministic` locks): a plain directory under
     the state directory. On Linux a warning suggests installing
     `fuse2fs` and `mkfs.ext4` (the `fuse2fs` and `e2fsprogs` packages)
     to get the image strategy.
4. Leaves a marker file (`LOCKED_BY_LLMDEVSILO`) in the now-empty
   original directory explaining the lock and how to undo it, and makes
   the directory read-only.

Honest caveats, printed as warnings:

- Plain directory strategy: *"workspace contents moved under the harness
  state directory; they are protected by file permissions only"* — the
  host user (and anything running as them) can still modify the contents
  directly, defeating the unlock diff.
- Sparse bundle strategy: *"the workspace image is mounted host-visible
  while a harness is attached"* — while a session runs, the mounted
  volume is reachable from the host; do not edit it from outside.
- Ext4 image strategy: *"the workspace image is mounted host-visible
  while a harness is attached; the image size is fixed at lock time and
  does not grow"* — the same host-visibility caveat as the sparse
  bundle, plus the fixed-size limitation: a workspace that outgrows the
  image sees out-of-space errors until it is unlocked and re-locked.

The protection against host-side interference is therefore procedural,
not cryptographic: the lock's purpose is to guarantee a complete,
reviewable change report at unlock time, provided you do not modify the
contents from outside while locked.

### silo workspace unlock

Unlocks a workspace and prints the change report. The exact sequence:

1. Marks the unlock as in progress in the registry. From this point new
   attachments (harness or shell) are refused with *"an unlock of
   workspace … is in progress; re-run `silo workspace unlock` to finish
   it"* until the unlock completes.
2. Terminates the attached harness *and* every live shell attachment
   (signal, wait, then verify), with pid-reuse protection so an
   unrelated process that recycled the pid is never killed. Survivors
   abort the unlock.
3. Detaches the disk image (image-based strategies).
4. Restores the contents into the original directory, restores write
   permission, and removes the marker file.
5. Computes the change report by diffing the restored tree against the
   lock-time snapshot.
6. Removes the registry entry and deletes the snapshot directory.

The report prints the auto-exec warnings **first**, in a banner, before
the change list:

```
==============================
    AUTO-EXEC WARNINGS
==============================
Review these files before opening the workspace in any tool:
  !! .git/hooks/pre-commit — git hook: git runs these scripts on commit, checkout, merge, push, and other operations
```

Changed files that are auto-exec surfaces — git hooks, `.envrc`,
`.vscode` configuration, `package.json` scripts, `build.rs`, and the
like — can run code *outside* the sandbox the moment you open the
workspace in an editor, shell, or build tool. Review them before
touching the workspace with anything that might execute them; the safe
place to inspect or run unreviewed agent output is `silo shell` (see
below). After the banner comes `Changes since lock: <n>` with one line
per change (`added` / `modified` / `deleted`, the path, and a note for
metadata changes such as `mode 0644 -> 0755`), followed by a unified
diff for every text change.

**Failure and resumption.** Every step is idempotent and the snapshot,
container, and registry entry are kept until the report exists. A
mid-step failure produces an error naming the step:

```
silo: unlock step 'detaching the workspace image' failed: <detail>;
the workspace is unchanged for this step — re-run unlock to resume
```

Re-running `silo workspace unlock` resumes where it stopped and still
produces the full report. The unlocking barrier stays in place across
attempts, so nothing can re-attach to a half-unlocked workspace.

### silo workspace status

Prints the lock and attachment state:

```
workspace: /Users/you/dev/myproject
locked: yes
attached: harness ab12cd34ef56
shells: 2
warning: the workspace image is mounted host-visible while a harness is attached
```

- `locked` — whether the path has a registry entry.
- `attached` — the harness id currently attached, or `no`.
- `shells` — the number of live `silo shell` attachments sharing the
  mount.
- `warning` lines — the container strategy caveat plus anything recorded
  at lock time (for example the `hdiutil` fallback note).

Reading the status also prunes attachment records whose process is dead
or whose pid was recycled, so the counts reflect live processes.

### Files read and written

`<state>/workspaces/registry.json` (and its `.lock` sibling),
`<state>/workspaces/<id>/` (manifest, blobs, and `data/`,
`workspace.sparsebundle`, or `workspace.img` plus its `mnt/`
mountpoint), and the workspace directory itself (contents, marker file,
directory permissions).

### Examples

```sh
silo workspace lock ~/dev/myproject       # before the first run
silo workspace status ~/dev/myproject     # is anything attached?
silo workspace unlock ~/dev/myproject     # stop everything, get the report
```

Capture the unlock report for review in an editor:

```sh
silo workspace unlock ~/dev/myproject > unlock-report.txt
```

---

## silo shell

### Synopsis

```
silo shell [OPTIONS] --workspace <WORKSPACE> [-- <COMMAND>...]
```

### Description

Opens an interactive shell (your `$SHELL`), or runs one command, inside
a sandbox with the same confinement the model gets: read/write access to
the workspace and a scratch space, read-only access to the allowlist,
and network egress only through a filtering proxy. The exit code is the
shell's or command's own exit code.

This is the intended way to inspect or run what the agent produced. The
agent can write anything into the workspace — including code wired into
auto-exec surfaces like git hooks or `package.json` scripts — so running
it *outside* a sandbox executes unreviewed model output with your full
user privileges. Running it inside `silo shell` keeps anything malicious
confined by the same boundaries that confined the agent that wrote it.

The shell works alongside a running harness: the workspace may already
be attached to a live harness, and the shell then shares the workspace
mount, so the agent's edits are visible to you live (and your edits are
visible to the agent). The registry tracks the shell as a secondary
attachment; the mount is released only when the last attachment of any
kind goes away.

### Policy mirroring

When the workspace is attached to a live harness whose run file records
its sandbox policy, and you pass none of `--sandbox`, `--allow-read`,
or `--allow-domain`, the shell mirrors that harness's policy: the same
sandbox kind, the same configured read allowlist, and the same allowed
domains. It prints what it did:

```
Mirroring running harness ab12cd34ef56's sandbox policy
(macos-sandbox-exec, 3 readable path(s), 2 allowed domain(s));
credential injection is not mirrored.
```

Rules:

- Any explicit sandbox flag wins and disables mirroring entirely, with a
  printed note that the shell's access policy differs from the
  harness's.
- Mirrored read-allowlist entries inherit the running harness's risk
  acceptance: entries the risk scan would flag are accepted because the
  harness already accepted them, and the shell prints which ones
  (*"Read allowlist entries accepted by inheritance from running harness
  …"*). Entries you pass yourself are scanned as usual and need
  `--allow-risky-path` to override hits.
- Credential injection is **never** mirrored. The run file does not
  contain credential material, and the shell injects only what you pass
  with `--inject-credential` for this session. A shell beside a
  credentialed harness has, by default, no credentials at all.
- With no running harness (or a run file that does not record the
  policy), the shell uses the flags as given, with `--sandbox`
  defaulting to `auto`.

### Flags

- `--workspace <WORKSPACE>` — the locked workspace directory. Required.
- `--allow-read <ALLOW_READ>` — host path the sandbox may read
  (repeatable). Overrides mirroring. Same risk scan as `silo run`.
- `--allow-domain <ALLOW_DOMAIN>` — domain the sandbox may reach
  (repeatable). Overrides mirroring. Same wildcard semantics as
  `silo run`.
- `--sandbox <SANDBOX>` — sandbox backend (`auto`, `sandbox-exec`,
  `linux-vm`, `gvisor`, `microvm`). Defaults to the running harness's
  backend when mirroring, otherwise to `auto`. Overrides mirroring.
  `mock` is rejected with an error: the mock sandbox is script-driven
  and only usable via `silo run --script`.
- `--inject-credential <INJECT_CREDENTIAL>` — same format and semantics
  as `silo run`. Only credentials given here are injected.
- `--allow-risky-path <ALLOW_RISKY_PATH>` — accept a read-allowlist
  entry despite risk-scan hits (repeatable).
- `-- <COMMAND>...` — run this command instead of an interactive shell,
  for example `-- cargo test`. Everything after `--` is the command
  vector.

### Signal behavior

Ctrl-C (SIGINT) and SIGTERM terminate the sandboxed session's whole
process group cleanly, then the shell session unwinds through sandbox
shutdown and workspace detach. This is also how
`silo workspace unlock` stops live shells: it sends the signal and the
shell exits through the same orderly path.

### Files read and written

Writes a journal at `<state>/journals/shell-<id>.jsonl` (lifecycle notes
and network records from the shell's own proxy). Registers and removes a
secondary attachment in the workspace registry. The sandboxed session
reads and writes the workspace mount and its scratch space.

### Examples

Inspect a running agent's work live, with exactly the agent's own
access:

```sh
silo shell --workspace ~/dev/myproject
```

Run the test suite inside the sandbox as a one-off command:

```sh
silo shell --workspace ~/dev/myproject -- cargo test
```

A shell with its own explicit policy (mirroring disabled), plus a
scoped credential the agent does not have:

```sh
export GH_TOKEN=github_pat_...
silo shell --workspace ~/dev/myproject \
    --allow-read /usr/bin --allow-domain '*.github.com' \
    --inject-credential 'api.github.com:Authorization:GH_TOKEN:Bearer {secret}'
```

---

## silo replay-test

### Synopsis

```
silo replay-test [OPTIONS] --output <OUTPUT> <JOURNAL>
```

### Description

Converts a recorded session journal into a test script that replays the
session deterministically against mock components.

### Journals

Every harness session writes a journal — JSON Lines, by default at
`<state>/journals/<harness-id>.jsonl` (the path is printed to standard
error when the session ends). Each record is typed: session metadata,
events, full LLM requests and responses, every tool execution with its
output, frontend commands, network operation summaries, and lifecycle
notes. Journals carry no secrets by construction: configuration refers
to secrets by environment-variable name, secret values serialize as
`[redacted]`, and network records carry metadata only (host, port,
method, path, status, byte counts, whether a credential was injected) —
never bodies of credentialed calls and never credential values. A
journal is therefore safe to share, and safe to turn into a test.

### Generated script

The script (JSON, written to `--output`) has four lists, consumed
strictly in order by the corresponding mock component during replay:

- `llm` — one turn per recorded LLM response.
- `tools` — one entry per sandbox-owned tool execution, with the tool
  name and full input as expectations and the recorded output as the
  result.
- `frontend` — prompts, question answers, file uploads, interrupts
  (re-injected at the exact point the harness consumed them), and the
  expected shutdown.
- `network` — the recorded network operation summaries, for the mock
  proxy.

`--name <NAME>` sets the script's name; the default is the journal file
stem.

The command prints what it wrote and the exact replay command line,
with the workspace path recovered from the journal's metadata record:

```
Wrote session.json (llm turns: 12, tool execs: 31, frontend steps: 4)
Replay it with:
  silo run --workspace /tmp/ws --frontend mock --llm mock --sandbox mock \
      --mock-proxy --script session.json --deterministic
```

### Guarantees and limits

Replay is deterministic: mock components consume their script entries
by position, never by timers, so replays are race-free. Sessions that
were *recorded* with mock components (the harness's own test suite)
round-trip exactly — replaying the generated script reproduces the same
LLM, tool-execution, and event journal entries. Sessions recorded from a
real interactive frontend replay with mock-equivalent semantics: client
prompts become scripted prompts, client answers become scripted answers,
uploads and interrupts are re-injected in their recorded order, and
wall-clock timing collapses to sequence order. What is verified is the
recorded interaction order and content — not timing, not the behavior
of the real model, sandbox, or network.

### Files read and written

Reads the journal; writes the script to `--output`. Nothing else.

### Examples

```sh
silo replay-test ~/.llmdevsilo/journals/ab12cd34ef56.jsonl -o session.json
silo run --workspace /tmp/replay --create --deterministic \
    --frontend mock --llm mock --sandbox mock --mock-proxy --script session.json
```

Name the script for a regression suite:

```sh
silo replay-test journals/quota-bug.jsonl -o tests/scripts/quota-bug.json \
    --name quota_bug_regression
```

---

## silo harnesses list

### Synopsis

```
silo harnesses list
```

### Description

Lists live harnesses by reading the run files under `<state>/run/`. Run
files are written by the interactive frontend at startup and removed at
shutdown; headless and mock sessions do not write them, so they do not
appear here.

For each run file, the recorded process id is checked for liveness; dead
or unreadable run files are deleted as part of the listing (the output
notes how many were pruned). Output:

```
HARNESS             PID ADDRESS                WORKSPACE
ab12cd34ef56      48213 127.0.0.1:55123        /Users/you/dev/myproject
pruned 1 stale run file(s) from /Users/you/.llmdevsilo/run
```

- `HARNESS` — the harness id (also the run file name and the journal
  file stem).
- `PID` — the harness process id.
- `ADDRESS` — the interactive WebSocket listen address clients connect
  to.
- `WORKSPACE` — the workspace path the harness is attached to.

With no live harnesses it prints `no running harnesses`.

### Files read and written

Reads `<state>/run/*.json`; deletes stale ones.

---

## silo manpages

### Synopsis

```
silo manpages <OUTPUT_DIR>
```

### Description

Writes man pages generated from the live command definitions into
`<OUTPUT_DIR>` (created if needed): `silo.1` plus one
`silo-<subcommand>.1` per subcommand (`silo-run.1`, `silo-workspace.1`,
`silo-shell.1`, `silo-replay-test.1`, `silo-harnesses.1`). The command
exists for packaging and is hidden from `--help`.

---

## Troubleshooting

**A stale registry lock.** Registry mutations are serialized through a
lock file (`<state>/workspaces/registry.json.lock`) recording the
holder's process id and start time. Recovery is automatic: a waiter that
finds the holder dead (or the pid recycled by a different process, or
the file unreadable and older than a minute) removes the stale file and
retries. If a *live* process genuinely holds the lock for more than
about thirty seconds, commands fail with *"timed out waiting for the
workspace registry lock at <path>"*; check what that process is before
removing the file by hand.

**A wedged unlock.** Unlock failures name the failing step and end with
*"the workspace is unchanged for this step — re-run unlock to resume"*.
Re-running `silo workspace unlock <path>` is always the right move: the
steps are idempotent, the snapshot is kept until the report is produced,
and the in-progress barrier keeps new harnesses and shells from
attaching meanwhile. The most common cause on macOS is a busy disk-image
mount; the detach is retried with force automatically, and the error
tells you the mountpoint if even that fails.

**Finding a session's journal.** `silo run` prints `journal: <path>` to
standard error when the session ends. Otherwise look in
`<state>/journals/`; harness journals are named `<harness-id>.jsonl`
(the id `silo harnesses list` shows) and shell journals
`shell-<id>.jsonl`.

**macOS keychain prompts and ad-hoc signing (Flutter app).** The
desktop client's secret storage interacts with macOS code signing; if
you see repeated keychain prompts or keystore errors when running the
app from source, see "How the app stores its data (and macOS keychain
prompts)" in [apps/silo_app/README.md](../apps/silo_app/README.md).
