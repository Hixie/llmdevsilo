# Tools

This document is the reference for every tool the model can call in a
harness session. The source of truth is the code. The tool definitions
live in
[`crates/silo-sandbox/src/tools.rs`](../crates/silo-sandbox/src/tools.rs)
(the six sandbox tools),
[`crates/silo-llm/src/common.rs`](../crates/silo-llm/src/common.rs)
(the Agent tool), and
[`crates/silo-frontend/src/tools.rs`](../crates/silo-frontend/src/tools.rs)
(the frontend tools). Execution lives in
[`crates/silo-sandbox/src/toolimpl.rs`](../crates/silo-sandbox/src/toolimpl.rs),
[`crates/silo-helper/src/ops.rs`](../crates/silo-helper/src/ops.rs),
[`crates/silo-harness/src/agent.rs`](../crates/silo-harness/src/agent.rs),
and the frontend implementations.

The intended readers are users who want to understand what the model can
and cannot do, and prompt authors who want to steer it.

Related documents, not duplicated here:

- [PROTOCOLS.md](PROTOCOLS.md) — the wire protocols: the helper protocol
  that carries the sandbox tools (section 3), the egress proxy rules
  (section 4), and the client event stream (section 2.5).
- [SUBAGENTS.md](SUBAGENTS.md) — the full subagent lifecycle behind the
  Agent tool, with a runnable example.
- [SECURITY.md](SECURITY.md) — the security model the tool boundaries
  implement.

## 1. Overview

Eleven tools exist. Each is owned by one component, and the harness
routes every call to its owner (`ToolRegistry` in
[`crates/silo-core/src/tool.rs`](../crates/silo-core/src/tool.rs)).

| Tool | Owner | Available to | Contributed by |
| --- | --- | --- | --- |
| `Read` | sandbox | top-level agent and subagents | the sandbox module, always |
| `Write` | sandbox | top-level agent and subagents | the sandbox module, always |
| `Edit` | sandbox | top-level agent and subagents | the sandbox module, always |
| `Bash` | sandbox | top-level agent and subagents | the sandbox module, always |
| `WebFetch` | sandbox | top-level agent and subagents | the sandbox module, always |
| `WebSearch` | sandbox | top-level agent and subagents | the sandbox module, always |
| `Agent` | harness | top-level agent and subagents | the LLM backend layer, always |
| `AwaitAgent` | harness | top-level agent and subagents | the LLM backend layer, always |
| `AskUserQuestion` | frontend | top-level agent only | the interactive frontend |
| `SendUserFile` | frontend | top-level agent only | the interactive frontend |
| `Exit` | frontend | top-level agent only | the headless frontend |

A session therefore never offers all eleven at once: an interactive
session has no `Exit`, and a headless session has no `AskUserQuestion` or
`SendUserFile`. (The mock frontend used by scripted tests contributes all
three frontend tools.) Subagents get the sandbox tools plus `Agent` and
`AwaitAgent`; the user-facing tools are withheld from them by design (see
[SUBAGENTS.md](SUBAGENTS.md)).

## 2. Common behavior

**Results.** Every tool call resolves to a result with two parts: a
`content` string and an `is_error` flag. The model sees both. Tool-level
failures — a missing file, a nonzero exit code, an edit mismatch, an HTTP
error status, malformed tool input — come back as error results, so the
model can read the message and adapt. Only session-level failures (a dead
helper, a closed frontend) abort the turn.

**Input validation.** For the six sandbox tools, a call that omits a
required string field gets the error result
`<Tool>: missing required string field "<field>"`, for example
`Read: missing required string field "path"`. An integer field that is
negative or not a number gets
`<Tool>: field "<field>" must be a non-negative integer`. The
harness-owned and frontend-owned tools word their validation errors
differently — each is given in that tool's own section (for example
`Agent requires a non-empty 'prompt' string`). A call to a name that is
not registered in the session gets `unknown tool: <name>`.

**Interrupts.** A user interrupt aborts the whole turn. The tool call
running at that moment is cancelled: a `Bash` execution returns its
partial output as an error result marked `(cancelled)`, and an open
`AskUserQuestion` returns the error result `[interrupted by the user]`.
Every tool call queued after it in the same model response gets the
synthetic error result `[interrupted by the user]` without running. The
conversation stays well-formed and the next prompt resumes it. The
client-visible event sequence is in [PROTOCOLS.md](PROTOCOLS.md)
section 2.5.

## 3. Sandbox tools

`Read`, `Write`, `Edit`, `Bash`, `WebFetch`, and `WebSearch` are defined
by the sandbox module and executed by **silo-helper**, the single
untrusted process the harness starts inside every sandbox. The harness
sends each call to the helper over the helper protocol
([PROTOCOLS.md](PROTOCOLS.md) section 3) and formats the reply into the
tool result.

Because the helper runs inside the sandbox, it has no privileges beyond
any other sandboxed process. That implies:

- File access is constrained by the sandbox policy: the workspace mount
  is read/write, the scratch space is read/write, allowlisted host paths
  are read-only, and everything else is invisible or read-protected. A
  denied access surfaces as an ordinary operating-system error in the
  tool result (for example
  `cannot open /etc/shadow: Permission denied (os error 13)`), not as a
  distinct "policy violation" message.
- Network access exists only through the egress proxy
  ([PROTOCOLS.md](PROTOCOLS.md) section 4). This holds both for the
  helper's own `WebFetch`/`WebSearch` client and for anything `Bash`
  spawns.
- The harness never executes model-supplied commands or paths outside
  the sandbox.

Relative paths in tool inputs resolve against the workspace mount;
absolute paths are used as given (and then succeed or fail under the
sandbox policy). Success messages echo the path as the model supplied
it.

The helper protocol has additional operations (directory listing,
cancellation, shutdown) that are not exposed as model tools; they are
documented in [PROTOCOLS.md](PROTOCOLS.md) section 3.3.

### 3.1 Read

Reads a file and returns its content.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `path` | string | yes | — | Relative paths resolve against the workspace mount. |
| `offset` | integer | no | 0 | Byte offset to start reading at. Minimum 0. |
| `limit` | integer | no | the 5 MiB cap | Maximum bytes to return. Minimum 1; values above the cap are reduced to it. |

**Behavior.** The helper opens the file, seeks to `offset` when one is
given, and reads up to the limit. The bytes are decoded as UTF-8 with
invalid sequences replaced (so a binary file reads as text with
replacement characters rather than failing). `offset` and `limit` are in
bytes, not lines.

**Output.** On success, the file content as plain text. When more bytes
remained past the returned range, the marker `[truncated]` is appended
on its own line. On failure, an error result with the helper's message.
The helper reports the path it actually opened, which for a relative
input is resolved against the workspace mount — for example a `Read` of
`src/missing.rs` reports `cannot open
/<workspace-mount>/src/missing.rs: No such file or directory (os error
2)`. The Edit and Write `cannot read/write <path>` errors report the
same resolved path; only the success messages echo the path as
supplied.

**Limits.** At most 5 mebibytes (MiB; 1 MiB is 1,048,576 bytes) per
call, regardless of `limit`. Longer files are paged with `offset` and
`limit`.

**Example.**

Input:

```json
{"path": "Cargo.toml"}
```

Result:

```
[package]
name = "demo"
version = "0.1.0"
```

### 3.2 Write

Creates or overwrites a file; can also append.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `path` | string | yes | — | Relative paths resolve against the workspace mount. |
| `content` | string | yes | — | The full text to write. |
| `append` | boolean | no | `false` | Append to the file instead of replacing it. |

**Behavior.** Parent directories are created as needed. Without
`append`, the file is replaced whole; with it, the content is added at
the end. Writes succeed only where the sandbox policy allows them (the
workspace and the scratch space).

The implementation also accepts a `content_b64` field (base64 bytes,
mutually exclusive with `content`); it is not part of the schema shown
to the model. The harness uses it internally to store client file
uploads byte-for-byte.

**Output.** On success, `Wrote <n> bytes to <path>` — or
`Appended <n> bytes to <path>` with `append` — where `<n>` counts bytes,
not characters, and `<path>` is echoed as supplied. On failure, an error
result with the helper's message (`cannot write <path>: …`,
`cannot create directory <dir>: …`).

**Limits.** No size cap of its own.

**Example.**

Input:

```json
{"path": "notes.txt", "content": "hello\n"}
```

Result:

```
Wrote 6 bytes to notes.txt
```

### 3.3 Edit

Replaces an exact string in a file with another.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `path` | string | yes | — | Relative paths resolve against the workspace mount. |
| `old_string` | string | yes | — | Must be non-empty and must match the file content exactly. |
| `new_string` | string | yes | — | The replacement text. |
| `replace_all` | boolean | no | `false` | Replace every occurrence instead of requiring a unique match. |

**Behavior.** The file must be valid UTF-8 text. Without `replace_all`,
the call succeeds only when `old_string` occurs exactly once; with it,
every occurrence is replaced. The file is rewritten in place.

**Output.** On success, `Replaced 1 occurrence in <path>` or
`Replaced <n> occurrences in <path>`. The error results are:

- `old string not found` — no match;
- `old string matches <n> times; set replace_all to change every
  occurrence` — a non-unique match without `replace_all`;
- `old string is empty`;
- `<path> is not valid UTF-8 text` — binary content;
- `cannot read <path>: …` or `cannot write <path>: …` — file access
  failures.

**Example.**

Input:

```json
{"path": "src/lib.rs", "old_string": "alpha", "new_string": "beta"}
```

Result:

```
Replaced 1 occurrence in src/lib.rs
```

### 3.4 Bash

Runs a shell command inside the sandbox.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `command` | string | yes | — | Run as `sh -c <command>`. |
| `timeout_ms` | integer | no | 120000 | Milliseconds. Values above 600000 (ten minutes) are clamped to 600000. |

**Behavior.** The helper spawns `sh -c <command>` in its own process
group, with the workspace mount as the working directory and with `HOME`
and `TMPDIR` pointing into the private scratch space. The command
inherits the sandbox environment: `HTTP_PROXY`, `HTTPS_PROXY`, and the
certificate-trust variables are preconfigured so common tools (curl,
git, cargo, Node.js, Python) reach the network through the egress proxy
without per-tool setup (the full environment table is in
[PROTOCOLS.md](PROTOCOLS.md) section 3.7). Standard input is closed.

On timeout the whole process group is killed. On a user interrupt the
harness cancels the execution the same way; the partial output captured
so far becomes the tool result.

**Output.** The result is assembled from these sections, joined by
newlines, in this order — sections that do not apply are omitted:

1. standard output (trailing newlines trimmed);
2. `--- stderr ---` followed by standard error (trailing newlines
   trimmed);
3. `[output truncated]` when either stream hit its cap;
4. exactly one status marker: `(cancelled)`, `(timed out)`, or
   `(exit code <n>)` for a nonzero exit. A clean exit adds no marker.

The result is an error when the command was cancelled, timed out, or
exited nonzero; otherwise it is a success. A command with no output and
exit code 0 produces an empty result.

**Limits.** Each stream (standard output and standard error) is capped
at 1 MiB; output past the cap is dropped and `[output truncated]` is
added. The default timeout is two minutes and the maximum is ten. After
the process exits, the helper waits up to five seconds for descendants
that still hold the output pipes.

**Example.**

Input:

```json
{"command": "echo out; echo err >&2; exit 3"}
```

Result (an error result, because the exit code is nonzero):

```
out
--- stderr ---
err
(exit code 3)
```

### 3.5 WebFetch

Fetches a URL and returns the response body as text.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `url` | string | yes | — | The URL to fetch. |
| `max_bytes` | integer | no | 1048576 | Cap on the response body, in bytes. Minimum 1. |

**Behavior.** The helper performs a GET request with no extra headers
and no body. The request takes the only network path out of the sandbox:
the helper's HTTP client is built with the egress proxy address (from
`HTTPS_PROXY`) and the session certificate-authority certificate (from
`SILO_PROXY_CA`), and certificate validation is never disabled.
Redirects are followed (up to the HTTP client's default of ten hops),
and every hop goes through the proxy and is policy-checked.

The proxy enforces the domain allowlist and the
private-address guard, and it intercepts TLS (Transport Layer Security):
it terminates the sandbox side of the connection with a certificate
minted from the per-session certificate authority, then opens its own
upstream connection that verifies the real server certificate against
the standard web-trust roots ([PROTOCOLS.md](PROTOCOLS.md) section 4).
Two consequences for this tool:

- Configured credentials are injected by the proxy on its side of the
  interception. The model fetches without secrets and the real service
  receives them; the secret never appears in any tool result.
- The proxy journals request metadata (host, path without the query
  string, status, byte counts) for every fetch; bodies and headers are
  never journaled.

**Output.** On success, `HTTP <status>` on the first line, then the body
decoded as UTF-8 with invalid sequences replaced. When the body exceeded
`max_bytes`, the marker `[truncated]` is appended on its own line. A
status of 400 or higher makes the result an error but keeps the same
shape, so the model sees the error page.

A **blocked domain** looks different depending on the URL scheme:

- `https://` URL: the proxy refuses the tunnel with status 403 before
  any TLS is spoken, the HTTP client reports a request failure, and the
  model sees an error result starting `fetch failed: …` (the client's
  connection-error text).
- `http://` URL: the proxy itself answers the request, and the model
  sees the error result `HTTP 403` with an empty body.

Once the proxy has allowed the domain, an upstream problem — a DNS
resolution failure, a refused connection, or a destination the
private-address guard rejects after resolution — comes back through the
intercepted connection as `HTTP 502` with an `upstream error: …` body,
the same for HTTPS and plain HTTP. The `fetch failed: …` form appears
only when the proxy refuses the request outright: a domain that is not
on the allowlist, or a literal blocked IP address in the URL.

**Limits.** The body is capped at `max_bytes` (default 1 MiB); the cap
is per call and the model may raise or lower it.

**Example.**

Input:

```json
{"url": "https://crates.io/api/v1/crates/serde", "max_bytes": 1048576}
```

Result:

```
HTTP 200
{"crate":{"id":"serde","name":"serde", … }}
```

### 3.6 WebSearch

Searches the web and returns result titles, URLs, and snippets.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `query` | string | yes | — | The search query. |

**Behavior.** The tool is a specialized fetch: it requests
`https://html.duckduckgo.com/html/?q=<percent-encoded query>` — the
HTML (no JavaScript) interface of the DuckDuckGo search engine — through
the same helper client and egress proxy as `WebFetch`, with the
User-Agent header `Mozilla/5.0 (compatible; llmdevsilo)`. The returned
page is parsed
([`crates/silo-sandbox/src/search.rs`](../crates/silo-sandbox/src/search.rs)):
result links are unwrapped from DuckDuckGo's redirect URLs, HTML tags
are stripped from titles and snippets, and entities are decoded.

For the tool to work, the host `html.duckduckgo.com` must be on the
domain allowlist. The allowlist matches exact host names, so the entry
`duckduckgo.com` does not cover it; use `html.duckduckgo.com` itself or
the wildcard `*.duckduckgo.com` (for example
`--allow-domain html.duckduckgo.com`, see "Sandbox flags" in
[CLI.md](CLI.md)).

**Output.** On success, up to ten results, numbered, separated by blank
lines, each formatted as the title, then the URL indented by three
spaces, then the snippet indented by three spaces (omitted when the
result has none):

```
1. <title>
   <url>
   <snippet>
```

A page with no result links yields the result `No results.`.

**Failure modes.**

- Domain not allowlisted (or any connection failure): the proxy refuses
  the tunnel and the model sees an error result starting
  `fetch failed: …`.
- The search engine answers with a status other than 200: the error
  result `search failed: HTTP <status>`.

**Limits.** At most ten results. The fetched results page is capped at
2 MiB; results are parsed from what arrived within the cap.

**Example.**

Input:

```json
{"query": "rust borrow checker"}
```

Result:

```
1. Rust Programming Language
   https://www.rust-lang.org/
   A language empowering everyone to build reliable and efficient software.

2. The Rust Book & Guide
   https://doc.rust-lang.org/book/
   Learn Rust with the "book".
```

## 4. Agent and AwaitAgent

These two harness-owned tools delegate work to subagents. `Agent`
launches one and returns at once; `AwaitAgent` collects a finished one.
[SUBAGENTS.md](SUBAGENTS.md) covers the lifecycle, events, journaling,
and prompt-writing advice in full.

### 4.1 Agent

Launches a subagent to handle a self-contained task. The call is
non-blocking: it returns immediately with the new subagent's id, and the
subagent runs in the background.

**Availability:** top-level agent and subagents (so subagents can spawn
their own subagents, within the depth limit).

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `prompt` | string | yes | — | Complete, self-contained task description. Must be non-empty. |
| `name` | string | no | — | Short display name. Used only by clients and journals; the subagent never sees it. |

**Behavior.** The harness assigns the subagent the next agent id, emits
`agent_spawned`, starts a fresh conversation with the same model (seeded
with `prompt` as its only user message) on a background task, and
returns at once. The subagent shares the parent's sandbox, workspace,
network policy, and usage meter, but not its conversation; it gets the
sandbox tools plus `Agent` and `AwaitAgent`, and none of the user-facing
tools. The subagent runs concurrently with the parent and any siblings.
The parent collects it later with `AwaitAgent`.

**Output.** On success, the message
`Started subagent '<name>' (<id>). It runs in the background; collect it
with AwaitAgent. Pass agent "<id>" to wait for this one.`, where `<id>`
is the new subagent's agent id. Error results (the launch is refused
without blocking; the session continues):

- `Agent requires a non-empty 'prompt' string` — missing or empty
  prompt;
- `subagent depth limit (3) reached` — the top-level agent is depth 0,
  and each spawn adds one;
- `subagent concurrency limit (8) reached` — eight subagents are already
  live across the session.

**Limits.** Depth 3 and concurrency 8, both returned as error results so
the session continues. A subagent counts against the concurrency pool
from launch until it is collected or cancelled.

**Example.**

Input:

```json
{"name": "test survey", "prompt": "Count the test files in the workspace and report the number."}
```

Result:

```
Started subagent 'test survey' (agent-1). It runs in the background; collect it with AwaitAgent. Pass agent "agent-1" to wait for this one.
```

### 4.2 AwaitAgent

Waits for a subagent launched with `Agent` to finish and returns its
report.

**Availability:** top-level agent and subagents.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `agent` | string | no | — | Id of a specific subagent to wait for, as returned by `Agent`. Omit to wait for the first of the calling agent's still-running subagents to finish. |

**Behavior.** With no `agent`, the call blocks until the first of the
calling agent's outstanding subagents finishes (any of them). With an
`agent` id, it waits for that specific subagent. Either way it collects
exactly one subagent, removes it from the calling agent's set, and frees
its concurrency slot. A subagent belongs only to the agent that spawned
it, so one agent cannot await another's children. The call selects
against the interrupt and shutdown signals, so an interrupt or shutdown
unblocks it rather than letting it hang. The `agent_completed` event for
the collected subagent is emitted by the subagent's own task when it
finishes, which can be before or after this call.

**Output.** On success, a heading line
`Subagent '<name>' (<id>) finished.` followed by the subagent's final
text. When the subagent's own outcome was an error (its model request
failed, it was interrupted, or the session shut down mid-run), the same
shape is returned as an error result carrying the subagent's error
output. Error results from `AwaitAgent` itself:

- `AwaitAgent: no subagents are running` — an await with no `agent` and
  no outstanding subagents;
- `AwaitAgent: "<id>" is not an outstanding subagent of this agent` — an
  `agent` id this agent did not spawn, or already collected.

**Limits.** Collects one subagent per call. An uncollected subagent is
cancelled when the calling agent's turn ends (see "Orphan cancellation"
in [SUBAGENTS.md](SUBAGENTS.md)).

**Example.**

Input:

```json
{}
```

Result:

```
Subagent 'test survey' (agent-1) finished.
There are 3 test files.
```

## 5. Frontend tools

These tools touch the user, so the frontend executes them and only the
top-level agent gets them. Which of them exist in a session depends on
the frontend: interactive sessions have `AskUserQuestion` and
`SendUserFile`; headless sessions have `Exit`. Calling one that is not
registered in the current session gets `unknown tool: <name>`.

### 5.1 AskUserQuestion

Asks the user a question and blocks until an answer arrives.

**Availability:** top-level agent only; contributed by the interactive
frontend.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `question` | string | yes | — | The question to show. |
| `options` | array | no | empty | Suggested answers; each is an object with a required `label` (string) and an optional `description` (string). |
| `multi_select` | boolean | no | `false` | Whether the user may pick more than one option. |
| `allow_free_text` | boolean | no | `false` | Whether the user may answer with free text instead of picking an option. |

**Behavior.** The frontend assigns the question an identifier and emits
a `question_asked` event, so the question appears on **every** connected
client. The tool call blocks. **The first answer wins**: the first
`answer_question` message for that identifier resolves the call, a
`question_answered` event closes the question on all clients, and any
later answer for the same identifier is silently ignored
([PROTOCOLS.md](PROTOCOLS.md) section 2.5). `options`, `multi_select`,
and `allow_free_text` shape the client's answer interface; the harness
does not enforce them, and the result is whatever single string the
answering client sent.

**Interrupt resolution.** A user interrupt resolves every open question:
each gets a `question_answered` event with no client identifier and the
literal answer `[interrupted]`, and the blocked tool call returns the
error result `[interrupted by the user]`. A session shutdown while a
question is open ends the call without a result.

**Output.** On success, the answer string as a success result. Malformed
input gets the error result `invalid AskUserQuestion input: …`.

**Limits.** No timeout: the call waits as long as the user takes,
subject to interrupt and shutdown.

**Example.**

Input:

```json
{
  "question": "Which approach should I take?",
  "options": [
    {"label": "A", "description": "Patch in place"},
    {"label": "B", "description": "Rewrite the module"}
  ],
  "allow_free_text": true
}
```

Result (the user picked the first option):

```
A
```

### 5.2 SendUserFile

Sends a file from the workspace to the user.

**Availability:** top-level agent only; contributed by the interactive
frontend.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `path` | string | yes | — | Workspace path of the file to send. Must name a file (a path like `..` is rejected). |
| `caption` | string | no | — | Accepted by the schema; the current interactive frontend does not forward it to clients. |

**Behavior.** The model never hands file content to this tool — only
the path. The harness reads the file itself through the sandbox `Read`
path and injects the content (base64-encoded) into the call before
forwarding it to the frontend, so what the user receives is what the
sandboxed file actually contains, not what the model claims it
contains. The frontend then emits a `file_shared` event with the file
name (the last path component), the content, and the origin `llm`, so
the file appears on every connected client.

**Output.** On success, `sent <file name> to the user`, for example
`sent report.html to the user`. Error results:

- `SendUserFile: cannot read <path>: <message>` — the sandbox read
  failed, with the underlying message appended;
- `SendUserFile requires a 'path' string` — missing path;
- `the path "<path>" has no file name` — a path with no final
  component.

**Limits.** The injected read uses the sandbox `Read` tool's path, so
the same constraints apply: at most 5 MiB of the file is read, and the
content is decoded as UTF-8 with invalid sequences replaced before
re-encoding. Text files arrive intact; binary files do not survive the
decoding. A file larger than the cap is sent truncated with the literal
`[truncated]` marker appended to the delivered content, since the
harness forwards the `Read` output verbatim.

**Example.**

Input:

```json
{"path": "report.html", "caption": "Coverage report"}
```

Result:

```
sent report.html to the user
```

### 5.3 Exit

Ends a headless session with a final report.

**Availability:** top-level agent only; contributed by the headless
frontend. Interactive sessions have no `Exit`; they end when the user
asks the harness to shut down.

| Field | Type | Required | Default | Constraints |
| --- | --- | --- | --- | --- |
| `message` | string | yes | — | Final report shown to the user. |

**Behavior.** The headless frontend runs a single command-line prompt to
completion with no user interaction; its first input to the model ends
with the instruction to call `Exit` with a final report when the task is
done, and every later input is a canned reminder to do so. The call
requests harness shutdown carrying the message; on shutdown the message
is printed, followed by one cost line per model backend.

**Output.** On success, the result `exiting` (the shutdown then ends the
session). With the headless frontend, a missing message gets the error
result `the Exit tool requires a 'message' string` and the session
continues so the model can retry. (The mock test frontend, which also
contributes Exit, instead ends the session with no message.)

**Example.**

Input:

```json
{"message": "Survey done: 3 test files."}
```

Result:

```
exiting
```

The harness then shuts down and prints `Survey done: 3 test files.`.

## 6. Routing table

Where each call goes, end to end. The owner names below are the exact
strings recorded on `tool_exec` journal entries. The protocols each
executor speaks are in [PROTOCOLS.md](PROTOCOLS.md): the helper protocol
is section 3, the client protocol (events, questions, file sharing) is
section 2, and the egress proxy that `WebFetch`, `WebSearch`, and
`Bash`-spawned processes traverse is section 4.

| Tool | Owner | Executor |
| --- | --- | --- |
| `Read` | `sandbox` | silo-helper, inside the sandbox |
| `Write` | `sandbox` | silo-helper, inside the sandbox |
| `Edit` | `sandbox` | silo-helper, inside the sandbox |
| `Bash` | `sandbox` | silo-helper, inside the sandbox (spawns `sh -c`) |
| `WebFetch` | `sandbox` | silo-helper, through the egress proxy |
| `WebSearch` | `sandbox` | silo-helper, through the egress proxy |
| `Agent` | `harness` | the harness agent loop (launches a background subagent; see [SUBAGENTS.md](SUBAGENTS.md)) |
| `AwaitAgent` | `harness` | the harness agent loop (collects a finished subagent; see [SUBAGENTS.md](SUBAGENTS.md)) |
| `AskUserQuestion` | `frontend` | the interactive frontend, answered by a connected client |
| `SendUserFile` | `frontend` | the interactive frontend, after the harness reads the file via the sandbox |
| `Exit` | `frontend` | the headless frontend (requests session shutdown) |
