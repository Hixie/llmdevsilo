# Protocols

This document is the reference for every protocol spoken between processes
in the llmdevsilo system. The source of truth is the code. The primary
definitions live in
[`crates/silo-core/src/protocol.rs`](../crates/silo-core/src/protocol.rs)
(client protocol), [`crates/silo-core/src/event.rs`](../crates/silo-core/src/event.rs)
(event stream), and [`crates/silo-core/src/helper.rs`](../crates/silo-core/src/helper.rs)
(helper protocol). Where a unit test pins an exact wire shape, the JSON
example here copies that shape.

The intended reader is someone implementing a new client or a new helper
from scratch, without reading the Rust.

## 1. Overview

A running session involves these processes:

- **The harness** (the `silo` binary): one process that runs the agent
  loop, the interactive frontend (a WebSocket server), the sandbox module
  (the harness side of the helper protocol), and the egress proxy. The
  proxy and the WebSocket server are loopback TCP listeners inside this
  one process.
- **Clients**: the terminal user interface (`silo-tui`) and any other
  client applications. They connect to the harness over a TLS
  (Transport Layer Security) WebSocket.
- **silo-helper**: the single untrusted process the harness starts inside
  every sandbox. It executes the sandbox tools (Read, Write, Edit, Bash,
  WebFetch, WebSearch) on the harness's behalf.
- **Sandboxed tools**: the shell commands and child processes the helper
  spawns for `Bash` executions. They run under the same sandbox policy as
  the helper.
- **The egress proxy**: the only network path out of the sandbox. The
  helper and every spawned process reach it through the standard
  `HTTP_PROXY`/`HTTPS_PROXY` convention.

Three live interfaces connect these processes, plus one filesystem
interface for discovery and key storage:

```
clients (TUI, apps, web)
   │
   │  (2) TLS WebSocket, JSON text frames
   ▼
┌─────────────────────────────────────────────────────┐
│ harness process                                     │
│   interactive frontend ─ event bus ─ agent loop     │
│   sandbox module                    egress proxy    │
└───────┬─────────────────────────────────▲───────────┘
        │ (3) JSON Lines over a           │ (4) HTTP proxy protocol
        │     Unix socket                 │     (CONNECT / absolute-form)
        ▼                                 │
┌─────────────────────────────────────────┼───────────┐
│ sandbox                                 │           │
│   silo-helper ──────────────────────────┤           │
│   spawned tool processes (Bash, …) ─────┘           │
└─────────────────────────────────────────────────────┘

(5) filesystem: run files, local token, authorized keys
    under the harness state directory
```

Section numbers in the diagram match the sections below.

## 2. Interactive frontend ↔ clients (TLS WebSocket)

Defined in [`crates/silo-core/src/protocol.rs`](../crates/silo-core/src/protocol.rs);
served by [`crates/silo-frontend/src/interactive/`](../crates/silo-frontend/src/interactive/mod.rs).

### 2.1 Transport

The interactive frontend listens on a TCP address (configured
`listen_addr`, defaulting to `127.0.0.1` with an ephemeral port) and
speaks TLS. There are two sources of TLS material
([`tls.rs`](../crates/silo-frontend/src/interactive/tls.rs)):

- **Per-harness self-signed certificate** (the default): generated on
  first start with subject alternative names `localhost` and
  `llmdevsilo`, persisted under the harness state directory
  (`tls-cert.pem`, `tls-key.pem` with mode 0600), so the fingerprint is
  stable across restarts of the same harness. Clients do not validate the
  certificate against a certificate-authority chain; they pin the SHA-256
  fingerprint of the leaf certificate in DER form, written as 64
  lowercase hexadecimal characters. The fingerprint is published in the
  run file (section 5.2) and printed at startup.
- **User-supplied certificate**: a PEM certificate chain (leaf first) and
  matching PEM private key, configured with `tls_cert_path` and
  `tls_key_path` (both or neither). The pinned fingerprint is then the
  SHA-256 of the supplied leaf certificate.

After the TLS handshake, the server reads the HTTP request head (at most
8192 bytes, within a 30-second timeout). If the head carries an `Upgrade`
header whose comma-separated token list contains `websocket`
(case-insensitive), the bytes are replayed into a standard WebSocket
handshake. Any other request receives a single **HTTPS landing page**
response and the connection closes
([`http.rs`](../crates/silo-frontend/src/interactive/http.rs)):

```
HTTP/1.1 200 OK
Content-Type: text/html; charset=utf-8
Content-Length: <n>
Connection: close

<!DOCTYPE html> … Silo harness <id> … WebSocket URL: wss://<host> …
```

The page exists so that a browser user can visit `https://<addr>` once,
click through the self-signed-certificate warning, and thereby let a web
client open the WebSocket. The `wss://` URL in the body uses the
request's `Host` header (HTML-escaped), falling back to the listen
address.

All protocol messages are JSON objects carried in WebSocket **text**
frames, one message per frame. Binary frames are ignored by the server.
WebSocket-level ping and pong frames are handled by the WebSocket layer
and carry no protocol meaning; the protocol has its own `ping` message.

### 2.2 Connection lifecycle and authentication

Connection establishment is implemented in
[`connection.rs`](../crates/silo-frontend/src/interactive/connection.rs);
the authentication state lives in
[`auth.rs`](../crates/silo-frontend/src/interactive/auth.rs).

1. The **server speaks first**, sending `hello`:

   ```json
   {"type": "hello", "harness_id": "a1b2c3d4e5f6", "protocol_version": 1}
   ```

2. The client must authenticate before anything else. Every message of
   the authentication phase must arrive within 30 seconds. During this
   phase:
   - a message that parses but is not `authenticate` → `auth_error`
     `"authentication required"`, connection closed;
   - a message that does not parse → `auth_error` `"malformed message"`,
     connection closed;
   - any authentication failure → `auth_error` with a reason, connection
     closed.

3. On success the server sends `auth_ok` and the connection enters the
   authenticated state.

There are three authentication methods, all sent as
`{"type": "authenticate", "method": …, …}` (the method fields are
flattened into the same object).

**Local token** — for clients on the same machine. The client reads the
token from the file named in the run file and sends it verbatim. The
server compares SHA-256 digests in constant time.

```json
{"type": "authenticate", "method": "local_token", "token": "9f2c…64 hex…"}
```

Failure: `"invalid token"`.

**Pairing code + key registration** — for a new remote client. The
harness mints a one-time code (printed at startup when
`issue_pairing_code` is configured, or returned to an existing client by
`request_pairing_code`). Codes are 8 characters drawn from
`ABCDEFGHJKLMNPQRSTUVWXYZ23456789` (no lookalikes I, O, 0, 1), expire 120
seconds after issuance, and are removed on first redemption attempt
whether or not they have expired. The client generates an Ed25519 key
pair and sends the public key (32 bytes, standard base64) together with a
display name:

```json
{
  "type": "authenticate",
  "method": "pair",
  "code": "QF7GTPXW",
  "public_key_b64": "K7gNU3sdo+OL0wNhqoVWhr3g6s1xYv72ol/pe/Unols=",
  "client_name": "Ian's phone"
}
```

Failures: `"invalid or expired pairing code"`, `"invalid public key"`,
`"could not persist the public key"`. On success the server stores the
key under a freshly assigned `key_id` (a 12-hexadecimal-character
identifier) and returns it in `auth_ok`. The client keeps its private key
and the `key_id` for future logins.

**Challenge + signature** — for a returning paired client. Two
round trips inside the authentication phase:

```json
{"type": "authenticate", "method": "challenge", "key_id": "9f8e7d6c5b4a"}
```

The server replies with 32 random bytes, base64:

```json
{"type": "auth_challenge", "challenge_b64": "5fK…"}
```

The client signs the raw challenge bytes with Ed25519 and sends:

```json
{
  "type": "authenticate",
  "method": "signature",
  "key_id": "9f8e7d6c5b4a",
  "signature_b64": "Qmx…"
}
```

Failures: `"unknown key"`, `"no pending challenge"` (signature sent with
no outstanding challenge), `"challenge does not match this key"`
(the `key_id` differs from the one in the challenge request),
`"invalid signature"`. Only one challenge is pending at a time; a new
`challenge` request replaces it.

**AuthOk** ends the phase:

```json
{
  "type": "auth_ok",
  "client_id": "4be0643f-1d98-4f3a-97cd-ca98a65347dd",
  "key_id": "9f8e7d6c5b4a",
  "next_seq": 42
}
```

- `client_id`: a fresh UUID identifying this connection. It appears as
  the `client_id` in events caused by this client.
- `key_id`: present for the pair and challenge/signature methods (for
  pairing it is the newly assigned id); omitted entirely for local-token
  logins.
- `next_seq`: the sequence number the **next** event will carry, i.e. the
  number of events emitted so far. After `auth_ok` the server pushes
  every event with `seq >= next_seq` automatically. To see history, the
  client sends `request_events` with a `from_seq` below `next_seq`
  (typically 0, or its own last-seen sequence number plus one when
  reconnecting).

After authentication, the connection is symmetric-duplex: the client may
send any client message at any time; the server pushes events and replies
to requests. Replies are not correlated to requests by id — each request
type has exactly one response type, and events arrive interleaved.

On shutdown the server drains queued messages, sends `shutting_down` to
every connected client, and closes the sockets.

### 2.3 Client messages

All client messages are tagged with `"type"` (snake_case variant names).
Variants with no fields serialize as a bare tag object. The exact shape
`{"type": "interrupt"}` is pinned by the unit test
`interrupt_wire_format_is_a_bare_type_tag` in
[`protocol.rs`](../crates/silo-core/src/protocol.rs).

| `type` | Fields | Sender timing | Semantics |
|---|---|---|---|
| `authenticate` | `method` + method fields (section 2.2) | First message(s) only | Authenticates the connection. Sent after authentication, the server replies with `error` `"already authenticated"` and keeps the connection open. |
| `prompt` | `text` (string) | Any time after auth | Submits a user prompt. The frontend immediately emits a `user_prompt` event to all clients and queues the text; see section 2.5 for queueing. |
| `answer_question` | `question_id` (string), `answer` (string) | While a question is open | Answers an `AskUserQuestion`. First answer wins; see section 2.5. |
| `upload_file` | `name` (string), `content_b64` (base64 string) | Any time after auth | Shares a file. The frontend emits `file_shared` with a client origin; the harness then writes the bytes into `_uploads/<sanitized name>` in the workspace via the sandbox Write tool. |
| `request_events` | `from_seq` (integer) | Any time after auth | Requests the backlog of all events with `seq >= from_seq`. Answered with one `events` message. |
| `request_access_report` | — | Any time after auth | Answered with `access_report`. |
| `request_cost` | — | Any time after auth | Answered with `cost`: the latest `cost_report` per backend, ordered by backend name. |
| `request_pairing_code` | — | Any time after auth | Mints a fresh one-time pairing code. Answered with `pairing_code`. |
| `interrupt` | — | Any time after auth | Asks the harness to abort the in-progress turn. See section 2.5 for the resulting events. |
| `shutdown` | — | Any time after auth | Asks the harness to shut down the whole session. |
| `ping` | `nonce` (integer) | Any time after auth | Answered with `pong` carrying the same nonce. |

Examples:

```json
{"type": "prompt", "text": "Fix the failing test"}
```

```json
{"type": "answer_question", "question_id": "3c9d2f4a8b1e", "answer": "Option A"}
```

```json
{"type": "upload_file", "name": "spec.pdf", "content_b64": "JVBERi0xLjcK…"}
```

```json
{"type": "request_events", "from_seq": 0}
```

```json
{"type": "interrupt"}
```

```json
{"type": "shutdown"}
```

```json
{"type": "ping", "nonce": 7}
```

A text frame that does not parse as a client message gets
`{"type": "error", "message": "unrecognized message"}`; the connection
stays open.

### 2.4 Server messages

All server messages are tagged with `"type"` (snake_case).

| `type` | Fields | When sent |
|---|---|---|
| `hello` | `harness_id` (string), `protocol_version` (integer) | First message on every connection. |
| `auth_challenge` | `challenge_b64` (base64 of 32 bytes) | Reply to `authenticate`/`challenge`. |
| `auth_ok` | `client_id` (string), `key_id` (string, optional — omitted when absent), `next_seq` (integer) | Successful authentication. |
| `auth_error` | `message` (string) | Failed authentication; the server closes the connection afterwards. |
| `event` | `event` (Event object, section 2.5) | Pushed for every event with `seq >= next_seq`, in order. |
| `events` | `events` (array of Event objects) | Reply to `request_events`. |
| `access_report` | `report` (AccessReport object) | Reply to `request_access_report`. |
| `cost` | `entries` (array of `{backend, usage, quota}`) | Reply to `request_cost`. |
| `pairing_code` | `code` (string), `expires_in_secs` (integer, 120) | Reply to `request_pairing_code`. |
| `pong` | `nonce` (integer) | Reply to `ping`. |
| `error` | `message` (string) | Non-fatal protocol error (unrecognized message, authenticate-after-auth). |
| `shutting_down` | `message` (string, optional — omitted when absent) | Last message before the server closes the socket. |

The AccessReport shape (defined in
[`crates/silo-core/src/sandbox.rs`](../crates/silo-core/src/sandbox.rs)):

```json
{
  "type": "access_report",
  "report": {
    "sandbox_kind": "macos-sandbox-exec",
    "workspace_mount": "/Users/me/project",
    "scratch_dir": "/tmp/silo-scratch-1a2b3c4d5e6f",
    "readable_paths": ["/usr/bin", "/opt/homebrew"],
    "allowed_domains": ["crates.io", "*.github.com"],
    "credential_domains": ["api.anthropic.com"],
    "notes": []
  }
}
```

The cost reply (usage and quota types are in
[`crates/silo-core/src/cost.rs`](../crates/silo-core/src/cost.rs); the
quota fields are omitted when unset):

```json
{
  "type": "cost",
  "entries": [
    {
      "backend": "anthropic:claude-sonnet-4-6",
      "usage": {"input_tokens": 52341, "output_tokens": 8120, "usd": 0.2789},
      "quota": {"max_usd": 5.0}
    }
  ]
}
```

### 2.5 The event stream

Defined in [`crates/silo-core/src/event.rs`](../crates/silo-core/src/event.rs).

Every user-visible occurrence in a session is one **Event**. Events carry
zero-based sequence numbers that increment by one, with no gaps; all
connected clients observe the same stream. An event is an envelope with
the payload's fields flattened in next to a `kind` tag:

```json
{
  "seq": 7,
  "time": {"logical": 31, "wall_ms": 1765432100123},
  "kind": "assistant_text",
  "agent": "agent-0",
  "text": "Done."
}
```

`time.logical` is a monotonic counter; `time.wall_ms` is wall-clock
milliseconds since the Unix epoch and is omitted in deterministic test
journals (see [`crates/silo-core/src/clock.rs`](../crates/silo-core/src/clock.rs)).

**Catch-up**: a client that connects (or reconnects, or detects a gap)
sends `request_events` with `from_seq` and receives every event with
`seq >= from_seq` in one `events` message (the harness retains the full
stream in memory for the life of the session). Live events with
`seq >= next_seq` (from `auth_ok`) are always pushed; a client therefore
sees no gaps if it requests `[from_seq, next_seq)` once after
authenticating.

**Agent identifiers**: the top-level agent is the string `"agent-0"`;
subagents get successive numbers (`"agent-1"`, …).

#### Event payload inventory

All payloads are tagged with `"kind"` (snake_case). Fields marked
*optional* are omitted from the JSON entirely when absent.

| `kind` | Fields | Meaning |
|---|---|---|
| `harness_started` | `harness_id`, `workspace`, `sandbox`, `llm` (all strings) | First event of a session. `sandbox` is the backend kind; `llm` is the backend id. |
| `user_prompt` | `client_id` (string, optional), `client_name` (string, optional), `text` (string) | A user prompt was accepted, from whichever client sent it first. |
| `assistant_text` | `agent` (string), `text` (string) | Model output text. |
| `tool_use` | `agent` (string), `call` (`{id, name, input}`) | The model invoked a tool. `input` is arbitrary JSON. |
| `tool_result` | `agent`, `tool_use_id`, `tool_name` (strings), `output` (`{content, is_error}`) | The result of a tool call. |
| `agent_spawned` | `parent`, `agent` (strings), `name` (string, optional), `prompt` (string) | A subagent started. |
| `agent_completed` | `agent` (string), `result` (string), `is_error` (boolean) | A subagent finished. |
| `question_asked` | `id` (string), `agent` (string), `question` (UserQuestion, below) | An `AskUserQuestion` is open; shown on all clients. |
| `question_answered` | `id` (string), `client_id` (string, optional), `answer` (string) | The question is resolved; all clients drop the question UI. |
| `file_shared` | `name`, `content_b64` (strings), plus a flattened origin: `"origin": "client"` with `client_id`, or `"origin": "llm"` with `agent` | A file moved between user and model. |
| `cost_report` | `backend` (string), `usage` (`{input_tokens, output_tokens, usd}`), `quota` (`{max_total_tokens?, max_usd?}`) | Updated usage for one model backend. |
| `turn_complete` | `agent` (string), `stop_reason` | The top-level turn ended normally. `stop_reason` is `"end_turn"`, `"tool_use"`, `"max_tokens"`, or `{"other": "<provider value>"}`. |
| `interrupted` | `agent` (string) | The user aborted the turn; emitted in place of `turn_complete`. |
| `awaiting_input` | — | The harness is idle; the next client input starts the next turn. |
| `access_report_updated` | `report` (AccessReport) | The access report changed (also emitted once at startup). |
| `error` | `context` (string), `message` (string) | A non-fatal error, e.g. a failed model request. |
| `shutdown` | `message` (string, optional) | Final event of a session. |

**Optional-field shapes pinned by tests.** The `user_prompt` payload
with and without the client fields, copied from
`user_prompt_wire_format_with_and_without_client_name`:

```json
{
  "kind": "user_prompt",
  "client_id": "c1",
  "client_name": "Ian's phone",
  "text": "hello"
}
```

```json
{"kind": "user_prompt", "text": "hello"}
```

`client_name` is the display name registered at pairing time. It is
absent for local-token clients (they register no name), for keys paired
with an empty name, and for prompts injected by non-interactive
frontends (which also omit `client_id`).

The `agent_spawned` payload with and without `name`, copied from
`agent_spawned_wire_format_with_and_without_name` — `name` is the
display name from the Agent tool's `"name"` input and is absent when the
model gave none:

```json
{
  "kind": "agent_spawned",
  "parent": "agent-0",
  "agent": "agent-1",
  "name": "refactor tests",
  "prompt": "fix them"
}
```

```json
{"kind": "agent_spawned", "parent": "agent-0", "agent": "agent-1", "prompt": "fix them"}
```

The `interrupted` payload, copied from
`interrupted_payload_wire_format`:

```json
{"kind": "interrupted", "agent": "agent-0"}
```

A `question_asked` example (the UserQuestion sub-object always carries
all four fields when serialized by the harness):

```json
{
  "kind": "question_asked",
  "id": "3c9d2f4a8b1e",
  "agent": "agent-0",
  "question": {
    "question": "Which approach should I take?",
    "options": [
      {"label": "A", "description": "Patch in place"},
      {"label": "B", "description": ""}
    ],
    "multi_select": false,
    "allow_free_text": true
  }
}
```

A `file_shared` example with each origin:

```json
{"kind": "file_shared", "name": "spec.pdf", "content_b64": "JVBERi0…", "origin": "client", "client_id": "4be0643f-…"}
```

```json
{"kind": "file_shared", "name": "report.html", "content_b64": "PGh0bWw+…", "origin": "llm", "agent": "agent-0"}
```

#### Question semantics: first answer wins

A `question_asked` event opens the question on every client. The first
`answer_question` for that `question_id` resolves it: the server emits
`question_answered` carrying the winning client's `client_id` and the
answer, and the blocked tool call returns the answer to the model. Any
later `answer_question` for the same id is silently ignored — no event,
no error.

When the user interrupts while questions are open, every pending question
is resolved as interrupted: a `question_answered` event is emitted for
each, with **no** `client_id` and the literal answer `"[interrupted]"`
(the constant `INTERRUPTED_ANSWER`). The model sees the tool result
`"[interrupted by the user]"` as an error.

#### Prompt queueing

A `prompt` from any client is broadcast immediately as a `user_prompt`
event — even mid-turn — and appended to an unbounded queue. The harness
top-level loop emits `awaiting_input`, then takes exactly one queued
prompt and runs one turn with it; prompts sent while the harness is busy
therefore start subsequent turns, one each, in arrival order. "First
prompt wins the turn" only in the sense that the queue is ordered; no
prompt is dropped.

#### Busy/idle derivation

The protocol has no explicit busy flag. Clients derive it from the event
stream; the rule used by the terminal client
([`apps/silo-tui/src/app.rs`](../apps/silo-tui/src/app.rs),
`busy_after`) is:

- **busy** after: `user_prompt`, `assistant_text`, `tool_use`,
  `tool_result`, `agent_spawned`, `agent_completed`, `question_asked`,
  `question_answered`;
- **idle** after: `awaiting_input`, `interrupted`, `shutdown`;
- all other kinds leave the state unchanged.

`awaiting_input` is also the marker that the next prompt starts a new
turn (as opposed to joining the queue mid-turn).

#### Interrupt sequence

After a client sends `{"type": "interrupt"}` during a turn, the harness
unwinds at its next checkpoint. Observable effects, in order: in-flight
sandbox executions are cancelled (their `tool_result` events carry
partial output), pending questions resolve as described above with
`question_answered` events, then `interrupted` is emitted instead of
`turn_complete`, followed by `awaiting_input`. An interrupt sent while
the harness is idle is consumed without effect on the next turn.

### 2.6 Protocol versioning

`hello.protocol_version` is the constant `PROTOCOL_VERSION` in
[`crates/silo-core/src/lib.rs`](../crates/silo-core/src/lib.rs);
its current value is **1**.

Within a version, changes are additive: new fields are optional, with
serde defaults on deserialization and omission when absent
(`#[serde(default, skip_serializing_if)]`). Both sides ignore unknown
object fields. A client that receives `hello` with a `protocol_version`
it does not know should expect message variants it cannot parse; see
section 6 for behavior around unknown variants.

## 3. Harness ↔ silo-helper (JSON Lines)

Defined in [`crates/silo-core/src/helper.rs`](../crates/silo-core/src/helper.rs).
The helper runtime is [`crates/silo-helper/src/`](../crates/silo-helper/src/lib.rs);
the harness side is
[`crates/silo-sandbox/src/session.rs`](../crates/silo-sandbox/src/session.rs).

### 3.1 Transport and framing

The helper is started inside the sandbox and **connects back** to the
harness. The connect string is `unix:<path>` (a Unix domain socket) or
`tcp:<host>:<port>` (TCP loopback, for backends where a Unix socket
cannot cross the boundary). It is passed as the helper's first
command-line argument, falling back to the `SILO_HELPER_CONNECT`
environment variable
([`crates/silo-helper/src/main.rs`](../crates/silo-helper/src/main.rs)).
Both implemented backends use Unix sockets in the scratch space:
the macOS sandbox-exec backend passes `unix:<scratch>/helper.sock` as an
argument; the Linux gVisor backend runs
`/scratch/bin/silo-helper unix:/scratch/helper.sock` as the container's
init process.

Framing is **JSON Lines**: one JSON object per line, terminated by `\n`,
UTF-8. Blank lines are skipped. On the helper side, a line that is not
valid JSON is skipped silently; a JSON line that is not a valid request
gets a per-request error response when it carries a numeric `id`, and is
otherwise dropped. Bad input never terminates the helper's serve loop.

Every request carries a caller-chosen `id` (an unsigned 64-bit integer;
the harness uses a sequential counter starting at 0). The response echoes
the `id`. **Requests are handled concurrently**: the helper spawns one
task per request and a single writer serializes responses, so responses
may arrive in any order and must be correlated by `id`. The harness keeps
any number of requests in flight at once.

A response is `{"id": …, "result": …}` where `result` is either
`{"Ok": <payload>}` or `{"Err": "<message>"}` (the standard serde
encoding of a Rust `Result`; note the capitalized keys). Payloads are
tagged with `"payload"` (snake_case); requests flatten the operation,
tagged with `"op"` (snake_case), next to the `id`.

### 3.2 The Hello exchange

The first request on a fresh connection is `hello`, sent by the
**harness**; the helper answers with its version and process id. The
harness rejects the connection if the first reply is not a `hello`
payload.

```json
{"id": 0, "op": "hello"}
```

```json
{"id": 0, "result": {"Ok": {"payload": "hello", "version": "0.1.0", "pid": 4242}}}
```

### 3.3 Request inventory (HelperOp)

Fields marked *optional* are omitted when absent. `env` and `headers`
are arrays of two-element `[name, value]` string arrays (the JSON
encoding of a list of string pairs).

| `op` | Fields | Response payload |
|---|---|---|
| `hello` | — | `hello` |
| `exec` | `command` (string, run as `sh -c`), `cwd` (string, optional), `env` (array of pairs, may be empty), `timeout_ms` (integer) | `exec` |
| `read_file` | `path` (string), `offset` (integer bytes, optional), `limit` (integer bytes, optional) | `file` |
| `write_file` | `path` (string), `content_b64` (base64), `append` (boolean) | `written` |
| `edit_file` | `path`, `old`, `new` (strings), `replace_all` (boolean) | `edited` |
| `list_dir` | `path` (string) | `dir` |
| `cancel` | `cancel_id` (integer: the `id` of the request to cancel) | `ack`, or a per-request error |
| `fetch` | `url`, `method` (strings), `headers` (array of pairs), `body_b64` (base64, optional), `max_bytes` (integer) | `fetched` |
| `shutdown` | — | `ack`, then the helper exits |

Examples:

```json
{"id": 7, "op": "exec", "command": "echo hello", "env": [], "timeout_ms": 1000}
```

```json
{"id": 8, "op": "exec", "command": "cargo test", "cwd": "/workspace", "env": [["CI", "1"]], "timeout_ms": 120000}
```

```json
{"id": 1, "op": "read_file", "path": "/workspace/Cargo.toml"}
```

```json
{"id": 2, "op": "write_file", "path": "/workspace/notes.txt", "content_b64": "aGVsbG8=", "append": false}
```

```json
{"id": 3, "op": "edit_file", "path": "/workspace/a.rs", "old": "alpha", "new": "beta", "replace_all": false}
```

```json
{"id": 4, "op": "list_dir", "path": "/workspace"}
```

```json
{"id": 5, "op": "fetch", "url": "https://crates.io/api/v1/crates/serde", "method": "GET", "headers": [["accept", "application/json"]], "max_bytes": 1048576}
```

```json
{"id": 6, "op": "shutdown"}
```

The `cancel` request, with the exact shape pinned by the unit test
`cancel_op_wire_format` — the field is named `cancel_id` on the wire
because the operation is flattened into the request, whose own `id`
occupies the `id` key:

```json
{"id": 9, "op": "cancel", "cancel_id": 7}
```

### 3.4 Response inventory (HelperPayload)

| `payload` | Fields | Notes |
|---|---|---|
| `hello` | `version` (string), `pid` (integer) | Helper crate version and process id. |
| `exec` | `exit_code` (integer), `stdout` (string), `stderr` (string), `timed_out` (boolean), `truncated` (boolean), `cancelled` (boolean) | `exit_code` is −1 on timeout, cancellation, or signal death. Each stream is capped at 1 MiB (1,048,576 bytes); `truncated` is true when either stream was cut. Output is decoded as UTF-8 with replacement. `cancelled` defaults to false when absent. |
| `file` | `content_b64` (base64), `truncated` (boolean) | At most 5 MiB per request (lower when `limit` is smaller). `truncated` means more bytes remained past the returned range. |
| `written` | `bytes` (integer) | Bytes written. Parent directories are created as needed. |
| `edited` | `replacements` (integer) | 1 without `replace_all`; the match count with it. A non-unique match without `replace_all`, a missing match, an empty `old`, or non-UTF-8 content are per-request errors. |
| `dir` | `entries` (array of `{name, is_dir, size}`) | Sorted by name. |
| `fetched` | `status` (integer), `headers` (array of pairs), `body_b64` (base64), `truncated` (boolean) | Body capped at `max_bytes`. |
| `ack` | — | Reply to `cancel` and `shutdown`. |

Example responses:

```json
{"id": 7, "result": {"Ok": {"payload": "exec", "exit_code": 0, "stdout": "hello\n", "stderr": "", "timed_out": false, "truncated": false, "cancelled": false}}}
```

```json
{"id": 1, "result": {"Ok": {"payload": "file", "content_b64": "W3BhY2thZ2Vd…", "truncated": false}}}
```

```json
{"id": 4, "result": {"Ok": {"payload": "dir", "entries": [{"name": "Cargo.toml", "is_dir": false, "size": 312}, {"name": "src", "is_dir": true, "size": 96}]}}}
```

```json
{"id": 9, "result": {"Ok": {"payload": "ack"}}}
```

```json
{"id": 1, "result": {"Err": "cannot open /etc/shadow: Permission denied (os error 13)"}}
```

### 3.5 Cancellation

Only `exec` requests are cancellable; every other operation completes
quickly on its own. `cancel` names the target request by its `id` (wire
key `cancel_id`). On success the helper kills the target's process group
(the spawned shell is its own process-group leader, so descendants die
with it) and replies `ack` to the `cancel`; the **original** `exec`
request then responds normally with `exit_code` −1, `cancelled` true,
`timed_out` false, and whatever partial output was captured.

When the target id is unknown or the execution already finished, `cancel`
gets a per-request error
(`"no cancellable request with id <n>"`). This is a benign race — the
target's response may already be on the wire — and the harness ignores
such failures. On interrupt, the harness sends one `cancel` for every
in-flight `exec`
([`session.rs`](../crates/silo-sandbox/src/session.rs),
`cancel_inflight`).

The `cancelled` flag on `exec` responses defaults to false when absent,
pinned by the test `exec_payload_without_cancelled_defaults_to_false`:
an `exec` payload without the field parses as not cancelled.

### 3.6 Shutdown

`shutdown` is answered with `ack` — the helper writes the response, then
exits its serve loop and the process terminates. The helper also exits
when the harness closes the stream. On the harness side, a closed
connection fails all outstanding requests with a connection-closed error
and rejects new ones.

### 3.7 Environment contract

The helper, and every process it spawns for `exec`, runs with the
environment assembled in
[`crates/silo-sandbox/src/scratch.rs`](../crates/silo-sandbox/src/scratch.rs)
(`sandbox_env`):

| Variable | Value |
|---|---|
| `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` | `http://<proxy address>` — the egress proxy. On macOS sandbox-exec this is the proxy's host loopback address; inside gVisor it is `http://127.0.0.1:3128`, the helper's relay (below). |
| `SILO_PROXY_CA` | Path to the session certificate-authority public certificate, `<scratch>/proxy-ca.pem`. Read by the helper's own `fetch` client. |
| `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, `CARGO_HTTP_CAINFO` | The same `proxy-ca.pem` path, so common TLS clients (OpenSSL-based tools, curl, git, Node.js, Python requests, cargo) trust the proxy without per-tool setup. |
| `HOME` | `<scratch>/home` — a private, writable home directory. |
| `TMPDIR` | `<scratch>/tmp` — a private, writable temporary directory. |
| `PATH` | A fixed system path set by the backend (e.g. `/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`). |
| `SILO_HELPER_CONNECT` | Fallback for the connect string when it is not passed as the first argument. |
| `SILO_PROXY_RELAY` | Set only by backends whose sole path out of the sandbox is a Unix socket (gVisor). Format `<unix socket path>:<port>`, e.g. `/scratch/proxy.sock:3128`. |

The helper's own `fetch` operation builds its HTTP client from
`HTTPS_PROXY` (falling back to `HTTP_PROXY`) and `SILO_PROXY_CA`, read
once per process; certificate validation is never disabled
([`crates/silo-helper/src/fetch.rs`](../crates/silo-helper/src/fetch.rs)).

**The relay** ([`crates/silo-helper/src/relay.rs`](../crates/silo-helper/src/relay.rs)):
when `SILO_PROXY_RELAY` is set and non-empty, the helper — before serving
any requests — binds TCP `127.0.0.1:<port>` inside the sandbox and pipes
each accepted connection, byte for byte in both directions, to a fresh
connection on the named Unix socket. The harness forwards that Unix
socket to the egress proxy's TCP address. Sandboxed programs thus reach
the proxy at `127.0.0.1:<port>` even though the sandbox has no network
interface. The relay stops accepting when the helper exits; established
connections keep flowing until either side closes.

Per-`exec` environment entries supplied in the request are applied on top
of the helper's inherited environment (they add or override; they never
remove). The harness's Bash tool always sets `cwd` to the workspace mount
and passes `HOME` and `TMPDIR` (pointing into the scratch space) in the
per-request `env`
([`crates/silo-sandbox/src/toolimpl.rs`](../crates/silo-sandbox/src/toolimpl.rs)).

## 4. Sandbox processes ↔ egress proxy (HTTP)

Implemented in [`crates/silo-proxy/src/`](../crates/silo-proxy/src/proxy.rs).
The proxy is the only egress path from the sandbox: backends provide no
other route (gVisor runs with networking disabled except in-sandbox
loopback; sandbox-exec confines outbound connections to the proxy).

The proxy listens on a loopback TCP port and understands the two standard
HTTP-proxy request shapes.

### 4.1 CONNECT tunnels with TLS interception

For `CONNECT host:port` (port defaults to 443; bracketed IPv6 literals
are accepted):

1. The host policy is applied (section 4.3). Rejection → `403`.
2. The proxy replies `HTTP/1.1 200 Connection Established` with
   `Content-Length: 0`.
3. The proxy then performs a **TLS server handshake** on the tunnel,
   presenting a leaf certificate for the requested server name, minted on
   demand from the per-session certificate authority
   ([`ca.rs`](../crates/silo-proxy/src/ca.rs)). The CA key pair is
   generated when the proxy starts and its private key never leaves
   memory; the public certificate is what the sandbox sees at
   `<scratch>/proxy-ca.pem`. Leaves are cached per host.
4. The decrypted stream is served as HTTP/1.1. Each inner request is
   forwarded upstream over a **new TLS connection that verifies the real
   server certificate** against the standard web-trust roots, presenting
   the target host as the server name. The session CA plays no role
   upstream.

A client inside the sandbox that does not trust the session CA fails the
handshake in step 3; the failure is journaled
(`"tls handshake failed"`). A tunnel that does not start a TLS handshake
fails the same way — there is no passthrough mode, so non-TLS protocols
cannot be tunneled.

### 4.2 Absolute-form plain HTTP

A non-CONNECT request whose target is absolute-form
(`GET http://host/path HTTP/1.1`) is policy-checked the same way and
forwarded upstream as plain HTTP (default port 80). A non-CONNECT request
whose target does not start with `http://` gets
`HTTP/1.1 400 Bad Request`. Origin-form requests (plain `GET /path`) are
therefore rejected; the proxy is not an origin server.

### 4.3 What is blocked, and the responses

The connection-level host policy, applied to the CONNECT target and the
absolute-form authority:

1. **Domain allowlist**
   ([`allowlist.rs`](../crates/silo-proxy/src/allowlist.rs)): entries are
   exact host names or wildcards `*.example.com` (matching the base
   domain and every subdomain; `xfoo.com` does not match `*.foo.com`).
   Matching is case-insensitive and tolerates one trailing dot. An empty
   allowlist allows nothing. Hosts with credentials configured must still
   be allowlisted. Failure note: `"domain not allowlisted"`.
2. **IP-literal guard**: a host that is an IP-address literal is checked
   against the IP guard (section 4.5) before any connection. Failure
   note: `"blocked address"`.

A rejected CONNECT or absolute-form request receives:

```
HTTP/1.1 403 Forbidden
Content-Length: 0
```

Inside an established tunnel (or on a forwarded plain request):

- An inner request carrying an `Upgrade` header, or a `Connection`
  header containing `upgrade`, is refused with status `403` and body
  `upgrade blocked`. WebSocket and other protocol upgrades therefore
  never cross the proxy.
- A name that resolves to **any** blocked address fails the whole
  request: status `502` with body `upstream error: blocked address: …`
  ([`upstream.rs`](../crates/silo-proxy/src/upstream.rs) checks every
  resolved address before connecting to the first permitted one).
- Resolution failures, connection failures, and upstream TLS failures
  are also `502` with `upstream error: …` bodies.

### 4.4 Credential injection

[`credentials.rs`](../crates/silo-proxy/src/credentials.rs). Each
configured injection names a host, a header, an environment variable
holding the secret, and a format string. Semantics:

- The secret is read from its environment variable once, when the proxy
  starts; a missing variable is a startup error. The value is held in
  memory only — it never enters the sandbox, events, or journals.
- Matching is **exact host** only (case-insensitive, trailing-dot
  tolerant): `api.example.com` does not cover `sub.api.example.com`.
- On a matching inner request, any client-supplied header of the
  configured name is **removed**, then the header is set to the format
  string with `{secret}` replaced by the secret value (e.g.
  `Bearer {secret}`). Exactly one header value results.
- The journal records only the boolean `credential_injected`; the access
  report lists only the host names (`credential_domains`).

Because injection happens on the harness side of the TLS interception,
the sandboxed process sends requests without credentials and the real
service receives them with credentials; the secret is never observable
inside the sandbox.

### 4.5 The IP guard

[`ipguard.rs`](../crates/silo-proxy/src/ipguard.rs). Applied to
IP-literal hosts at policy time and to every resolved address before an
upstream connection (and to DNS answers, section 4.6). Blocked:

- IPv4: loopback `127.0.0.0/8`; unspecified `0.0.0.0`; broadcast
  `255.255.255.255`; private `10.0.0.0/8`, `172.16.0.0/12`,
  `192.168.0.0/16`; link-local `169.254.0.0/16`; carrier-grade NAT
  `100.64.0.0/10`; multicast `224.0.0.0/4`.
- IPv6: loopback `::1`; unspecified `::`; link-local `fe80::/10`;
  unique-local `fc00::/7`; multicast `ff00::/8`.
- IPv6 addresses embedding an IPv4 address are unwrapped and checked
  against the IPv4 rules: IPv4-mapped `::ffff:a.b.c.d`, the NAT64
  well-known prefix `64:ff9b::/96`, and IPv4-compatible `::a.b.c.d`
  (excluding `::` and `::1`).

Loopback can be permitted by a builder option used only by integration
tests; all other ranges stay blocked regardless.

### 4.6 The DNS filter (Linux backends)

[`dns.rs`](../crates/silo-proxy/src/dns.rs). When enabled, a UDP DNS
server on loopback answers only `A` and `AAAA` queries for allowlisted
names, resolving through the host resolver and returning only addresses
the IP guard permits (TTL 30 seconds). A non-allowlisted name gets
`NXDOMAIN`; an allowlisted name queried for any other record type gets an
empty `NOERROR`. Queries are journaled as network records with method
`"DNS"` and port 53.

### 4.7 What gets journaled

Every inner request, blocked attempt, and DNS decision appends one
`NetworkRecord` to the journal
([`crates/silo-core/src/journal.rs`](../crates/silo-core/src/journal.rs)):
`host`, `port`, `method`, `path`, `status`, `bytes_sent`,
`bytes_received`, `allowed`, `credential_injected`, and a short `note`
for rejections. **Metadata only**: the `path` is the URL path without
the query string, and request/response bodies, headers, and credential
values are never journaled.

## 5. Filesystem interfaces

### 5.1 The state directory

[`crates/silo-core/src/paths.rs`](../crates/silo-core/src/paths.rs).
Everything the harness persists outside workspaces lives under one state
directory: `~/.llmdevsilo` by default, overridable with the
`LLMDEVSILO_STATE_DIR` environment variable. Layout:

| Path | Contents |
|---|---|
| `run/` | Per-harness run files, `<harness_id>.json` (section 5.2). |
| `journals/` | Session journals (JSON Lines of `JournalRecord`). |
| `client-keys/` | Client-side private keys for the terminal client and other local clients. |
| `harness/<harness_id>/` | Harness-side per-session data: `local-token`, `authorized-keys.json`, `tls-cert.pem`, `tls-key.pem`. |
| `workspaces/` | The workspace registry managed by `silo workspace`. |

**The sandbox can never read the state directory.** It is never part of
the sandbox read allowlist, and the risk checks in
[`crates/silo-core/src/risk.rs`](../crates/silo-core/src/risk.rs) block
configurations that would add it.

### 5.2 Run files

When the interactive frontend starts, it writes
`<state>/run/<harness_id>.json` — pretty-printed JSON of the `RunInfo`
struct in [`protocol.rs`](../crates/silo-core/src/protocol.rs) — so local
clients can discover running harnesses. The file is deleted on frontend
shutdown.

```json
{
  "harness_id": "a1b2c3d4e5f6",
  "addr": "127.0.0.1:55123",
  "cert_fingerprint_sha256": "9f2c41e87b…64 lowercase hex characters…",
  "local_token_path": "/Users/me/.llmdevsilo/harness/a1b2c3d4e5f6/local-token",
  "pid": 4242,
  "workspace": "/Users/me/project",
  "sandbox_kind": "macos-sandbox-exec",
  "read_allowlist": ["/usr/bin"],
  "allowed_domains": ["crates.io"]
}
```

A local client connects to `wss://<addr>`, pins
`cert_fingerprint_sha256`, reads the token from `local_token_path`, and
authenticates with the `local_token` method. Remote clients receive the
address, fingerprint, and a pairing code out of band instead.

The last three fields describe the harness's sandbox access policy:
the sandbox backend name (`sandbox_kind`), the read allowlist as
configured for the harness (`read_allowlist`), and the domains the
egress proxy allows (`allowed_domains`). `read_allowlist` holds the
configured entries, exactly as the harness was started with; the
expanded per-platform list of readable paths (operating-system
baseline directories and the like) appears only in the access report.
`silo shell` reads these fields to mirror the policy when it opens a
shell into the same workspace, and treats the mirrored allowlist
entries as already accepted by the running harness's risk scan
(printing the entries accepted by inheritance). They never contain
credential material — credential injection settings are not written to
run files. The fields are additive and optional: run files written
before they existed parse with empty defaults, and older readers
ignore them.

### 5.3 The local token

`harness/<harness_id>/local-token`: 32 random bytes encoded as 64
hexadecimal characters, created on first start with file mode 0600
(owner read/write only) and reused thereafter. Possession of the file
contents proves same-user local access; the server stores and compares
only the SHA-256 digest, in constant time.

### 5.4 Pairing and authorized keys

`harness/<harness_id>/authorized-keys.json`, written with mode 0600: a
JSON object mapping `key_id` to a record of the registered public key and
the display name given at pairing:

```json
{
  "9f8e7d6c5b4a": {
    "public_key_b64": "K7gNU3sdo+OL0wNhqoVWhr3g6s1xYv72ol/pe/Unols=",
    "client_name": "Ian's phone"
  }
}
```

Keys are added by successful `pair` authentications and consulted by the
challenge/signature method. The `client_name` recorded here is what
appears as `client_name` on that client's `user_prompt` events. Pairing
codes themselves are held only in memory and never persisted.

### 5.5 TLS material

`harness/<harness_id>/tls-cert.pem` and `tls-key.pem` (key mode 0600)
hold the generated self-signed certificate described in section 2.1.
They are created once per harness id and reloaded on restart, keeping the
pinned fingerprint stable.

### 5.6 The scratch space (inside the sandbox)

[`crates/silo-sandbox/src/scratch.rs`](../crates/silo-sandbox/src/scratch.rs).
Not part of the state directory: each sandbox gets one private writable
directory `silo-scratch-<12 hex>` (mode 0700) under the configured
scratch root or the platform temporary directory, containing `home/`,
`tmp/`, and `proxy-ca.pem` (the session CA public certificate from
section 4.1). The directory is removed at sandbox shutdown. Backends also
place their helper socket (`helper.sock`), and on gVisor the proxy relay
socket (`proxy.sock`) and the helper binary (`bin/silo-helper`), inside
it.

## 6. Compatibility

**Version signal.** The only version number on any wire is
`hello.protocol_version` (currently 1) on the client interface, plus the
helper's crate version string in its `hello` payload, which is
informational.

**Additive-optional fields.** Both JSON protocols rely on serde's
default behavior: unknown object fields are ignored on read, and
optional fields are omitted on write and defaulted when missing on read.
Fields in this category today — an older peer that never sends or reads
them interoperates cleanly:

- Client interface: `auth_ok.key_id`; `shutting_down.message`;
  `user_prompt.client_id` and `user_prompt.client_name`;
  `agent_spawned.name`; `question_answered.client_id`;
  `shutdown.message` (the event); the quota fields `max_total_tokens`
  and `max_usd`; `time.wall_ms`.
- Helper interface: `exec.cwd`, `read_file.offset`, `read_file.limit`,
  `fetch.body_b64` on requests; the `cancelled` flag on `exec`
  responses (absent means false, pinned by
  `exec_payload_without_cancelled_defaults_to_false`).

**Unknown variants.** New enum variants (a new `type`, `kind`, `op`, or
`payload` tag) are not ignorable — they fail deserialization on a peer
that predates them. Behavior today:

- The interactive server answers an unparseable client message with
  `{"type": "error", "message": "unrecognized message"}` and keeps the
  connection open.
- A client receiving an unknown server message or event kind fails its
  own parse; how it recovers is client-defined (the protocol gives it no
  way to report the failure).
- The helper skips a line that is not JSON, answers
  `{"id": …, "result": {"Err": "malformed request: …"}}` when a JSON
  line with a numeric `id` is not a valid request, and drops invalid
  lines without an `id`. The harness treats an unexpected payload
  variant as a session error.

Introducing a new message variant on either interface therefore requires
either bumping `protocol_version` or accepting that older peers will
surface it as an unrecognized-message error.
