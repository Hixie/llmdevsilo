# Journals, replays, and test scripts

Every harness session writes a journal: a typed, append-only record of
everything the modules said to each other. A journal converts into a
test script, and a test script drives the mock components through an
exact, deterministic rerun of the session — no model calls, no code
execution, no network. The same script format can also be authored directly by
hand to describe a session that never happened, which is how this
repository's own end-to-end tests work.

This document covers the journal format, the script format, the replay
machinery, and a worked example you can run. Related documents, not
duplicated here:

- [README.md](../README.md) — the replay-testing overview and quick
  start.
- [CLI.md](CLI.md) — the `silo run` testing flags and the
  `silo replay-test` command reference.
- [PROTOCOLS.md](PROTOCOLS.md) — the event payload inventory (section
  2.5) and what the proxy journals (section 4.7).
- [ARCHITECTURE.md](ARCHITECTURE.md) — where the replay machinery lives
  in the crate layout, and the determinism invariants.

---

## 1. The journal

### Where journals live

Journals are JSON Lines files under the state directory: by default
`~/.llmdevsilo/journals/<harness-id>.jsonl` for harness sessions and
`journals/shell-<id>.jsonl` for `silo shell` sessions. `silo run`
prints `journal: <path>` to standard error when the session ends, and
`silo run --journal <path>` redirects the journal to a path of your
choosing (the path is refused if the sandbox could read it). See
"Finding a session's journal" in [CLI.md](CLI.md#troubleshooting).

The state directory is on the hardcoded risk list, so sandboxed code
can never read journals.

### The record envelope

Each line is one `JournalRecord`:

```json
{"seq": 7, "time": {"logical": 0}, "entry": "llm_request", ...}
```

- `seq` — position in the journal, starting at 0.
- `time` — `{"logical": <counter>, "wall_ms": <unix milliseconds>}`.
  Under `--deterministic` the fake clock is used: `wall_ms` is absent
  and `logical` stays at 0, so ordering is carried entirely by `seq`.
- `entry` — the record type tag; the rest of the line is that record's
  fields, flattened.

### Entry types

| `entry` | Meaning |
| --- | --- |
| `meta` | Session header: harness id, harness version, and a one-line configuration summary (`workspace=<path> llm=... sandbox=... frontend=...`). |
| `event` | One event from the session's event stream — prompts, assistant text, tool use and results, questions, cost reports, shutdown. The full payload inventory is in [PROTOCOLS.md](PROTOCOLS.md) section 2.5. |
| `llm_request` | A full completion request sent to the LLM backend: agent id, backend id, system prompt, message history, tool definitions, and the token limit. |
| `llm_response` | The backend's response: content blocks (text and tool-use), stop reason, and token usage. |
| `tool_exec` | One executed tool call: agent id, owner (`sandbox`, `frontend`, or `harness` — which component ran it), the call (id, name, input), and the output (content, `is_error`). Tool calls cancelled by an interrupt never executed and get no entry. |
| `frontend_command` | A command the harness consumed from the frontend — an interrupt or a shutdown request — journaled at the moment of consumption. |
| `network` | A summary of one proxied network exchange (the `NetworkRecord` fields listed in section 3 below). |
| `lifecycle` | A free-text harness note: startup, shutdown, sandbox lifecycle, LLM failures, interrupt accounting, upload storage. |

### No secrets, by construction

A journal is safe to share, and safe to turn into a test:

- Configuration names secrets by environment-variable name, never by
  value, and secret values held in memory serialize as `[redacted]`
  (see "The no-remote-secrets configuration model" in
  [CLI.md](CLI.md)).
- Network records carry metadata only — host, port, method, URL path
  without the query string, status, byte counts, and whether a
  credential was injected — never request or response bodies and never
  credential values ([PROTOCOLS.md](PROTOCOLS.md) section 4.7).

---

## 2. From journal to script: `silo replay-test`

```sh
silo replay-test <journal.jsonl> -o <script.json> [--name <name>]
```

Converts a journal into a script and prints the exact command line that
replays it, with the workspace path recovered from the journal's `meta`
record:

```
Wrote generated.json (llm turns: 2, tool execs: 1, frontend steps: 2)
Replay it with:
  silo run --workspace /tmp/replay-demo/ws --frontend mock --llm mock --sandbox mock --mock-proxy --script generated.json --deterministic
```

The conversion walks the journal in order and fills the script's four
lists:

| Journal record | Script entry |
| --- | --- |
| `llm_response` | One `llm` turn replaying the recorded response, with `expect_user_contains` unset. |
| `tool_exec` with owner `sandbox` | One `tools` entry expecting the recorded tool name and full input, returning the recorded output. |
| `event` of kind `user_prompt` | A `send_prompt` frontend step. |
| `event` of kind `question_answered` | An `answer_question` frontend step — except answers produced by an interrupt cancelling the question, which are covered by the `interrupt` step instead. |
| `event` of kind `file_shared` with a client origin | An `upload_file` frontend step. LLM-sent files produce no step; the replayed model resends them. |
| `event` of kind `shutdown` | An `expect_shutdown` frontend step carrying the recorded final message. |
| `frontend_command` of an interrupt | An `interrupt` frontend step, placed where the harness consumed the command, so the replay re-injects it at the same point. |
| `network` | One `network` entry (carried in the script; not checked during replay — see section 4). |

Tool executions owned by the frontend or the harness get no `tools`
entry because the replay reproduces them itself: `AskUserQuestion` is
answered by `answer_question` steps, `Exit` ends the session against
the `expect_shutdown` step, `SendUserFile` re-emits the file, and the
`Agent` tool re-spawns subagents whose conversations consume `llm`
turns like any other agent's.

---

## 3. The script format

A script is one JSON object (the `TestScript` type in
`crates/silo-core/src/replay.rs`):

```json
{
  "name": "informational label",
  "llm": [ ... ],
  "tools": [ ... ],
  "frontend": [ ... ],
  "network": [ ... ]
}
```

All four lists default to empty and each is consumed strictly in order
by the corresponding mock component. `name` is informational.

### `llm` — scripted model turns

Each entry answers one completion request:

```json
{
  "expect_user_contains": "optional substring",
  "response": {
    "content": [
      { "type": "text", "text": "..." },
      { "type": "tool_use", "id": "t1", "name": "Bash",
        "input": { "command": "ls" } }
    ],
    "stop_reason": "tool_use",
    "usage": { "input_tokens": 12, "output_tokens": 6 }
  }
}
```

- `expect_user_contains` (optional): the text of the most recent user
  message in the request must contain this substring, or the mock
  backend reports a mismatch. For matching purposes, tool-result
  content in the request is treated as text — so after a tool call,
  the next turn's expectation can match the tool's output (the worked
  example below matches `"src"`, the scripted output of its Bash
  call). Generated scripts leave this unset; set it in directly authored
  scripts to pin down what the "model" is responding to.
- `response.content`: a list of blocks, each `{"type": "text", ...}`
  or `{"type": "tool_use", "id": ..., "name": ..., "input": ...}`.
  Tool-use ids are echoed back in tool results and should be unique
  within the session.
- `response.stop_reason`: `"tool_use"` when the turn requests tools,
  `"end_turn"` when it is finished, `"max_tokens"` for a truncated
  turn.
- `response.usage`: token counts metered against any configured quota,
  exactly like a real backend's.

Tool names the harness routes (anything else gets an "unknown tool"
error result): `Read`, `Write`, `Edit`, `Bash`, `WebFetch`,
`WebSearch` (sandbox); `AskUserQuestion`, `SendUserFile`, `Exit`
(frontend); `Agent` (harness — spawns a subagent whose conversation
consumes further `llm` turns from this same list).

### `tools` — scripted sandbox executions

Each entry answers one sandbox-owned tool call:

```json
{
  "expect_name": "Bash",
  "expect_input": { "command": "ls" },
  "output": { "content": "src", "is_error": false }
}
```

- `expect_name`: must equal the called tool's name.
- `expect_input` (optional): subset matching. Every key in this object
  must appear in the actual input with an equal value; for nested
  objects the rule applies recursively, and for everything else
  (strings, numbers, arrays) equality is exact. Extra keys in the
  actual input are allowed. Omit it to accept any input.
- `output`: played back as the tool result; `is_error: true` produces
  an error result.

Only sandbox-owned tools consume entries from this list. Frontend
tools (`AskUserQuestion`, `Exit`, `SendUserFile`) and the `Agent` tool
do not.

### `frontend` — scripted user behavior

Each step is an object tagged with `"step"`. The six variants:

| Step | Fields | Consumed when |
| --- | --- | --- |
| `send_prompt` | `text` | The harness asks for user input; the step supplies the prompt. |
| `expect_event` | `kind`, optional `contains` | The harness asks for input or a question answer; the step blocks until an event of that kind has been observed. `kind` is an event payload tag from [PROTOCOLS.md](PROTOCOLS.md) section 2.5 (for example `cost_report`), and `contains`, when set, must appear as a substring of the event payload's JSON serialization. Events observed earlier in the session also match. |
| `answer_question` | optional `contains`, `answer` | The model calls `AskUserQuestion`; the step answers it. `contains`, when set, must appear in the JSON serialization of the question, or the replay reports a mismatch. |
| `upload_file` | `name`, `content_b64` | The harness asks for user input; the step emits a client-origin file-shared event (a simulated upload). The harness stores the upload through a sandbox `Write` of `_uploads/<name>` with the same `content_b64`, so the script's `tools` list must contain that `Write` execution; the step waits until it has been consumed before later steps proceed. |
| `interrupt` | — | Sends the user-interrupt command. Consumed while answering an `AskUserQuestion` (the question resolves as `[interrupted by the user]` and the turn unwinds) or while supplying user input (the interrupt arrives while the harness is idle and is inert against the next turn). |
| `expect_shutdown` | optional `message_contains` | The session ends; the final message must contain the substring when one is given. |

### `network` — recorded network summaries

Each entry is a `NetworkRecord`: `host`, `port`, optional `method`,
`path`, and `status`, `bytes_sent`, `bytes_received`, `allowed`,
`credential_injected`, and an optional `note`. `silo replay-test`
copies the recorded session's network activity here so the script
documents it, but the replay does not verify this list: the mock proxy
performs no network operations and consumes nothing. The other three
lists are the ones that drive and check a replay.

---

## 4. How a replay runs

All mock components share one script and consume their own list
through an independent cursor — one for `llm`, one for `tools`, one
for `frontend`. Sequencing is by script position and event sequence
numbers only; nothing waits on a timer, so replays cannot race. Under
`--deterministic` the harness also uses a fake clock (no wall-clock
timestamps) and disables operating-system signal handling.

- The **mock LLM backend** answers each completion request with the
  next `llm` turn, after checking the session quota and the turn's
  `expect_user_contains`. Usage is metered, so quota flags and cost
  reports behave as in a real session.
- The **mock sandbox** executes nothing. Each sandbox tool call is
  checked against the next `tools` entry (name, then input subset) and
  the recorded output is played back.
- The **mock frontend** supplies the next `frontend` step whenever the
  harness asks for input, answers a question, or shuts down, with the
  per-step behavior in the table above.
- The **mock proxy** (`--mock-proxy`) binds a loopback listener that
  drops every connection and journals nothing.

### When a replay diverges

A scripted run is self-checking. A divergence fails the run: `silo run`
exits with code 4 and prints
`silo: script failure: <detail>; remaining: <summary>` to standard
error, where the detail names the diverging list and cursor position
with the expected versus actual values, and the summary counts consumed
versus scripted entries per list (for example
`llm 1/2, tools 1/1, frontend 2/2`). The checks:

- **An LLM turn mismatch** (a failed `expect_user_contains`, or an
  exhausted `llm` list) ends the session immediately. It is a failure
  of the script, not of the backend, so it produces no `error` event,
  no return to awaiting input, and no LLM-failure counting.
- **A sandbox tool mismatch** (wrong name, input that fails the subset
  match, or an exhausted `tools` list) likewise ends the session
  immediately.
- **At session end** — a normal shutdown, the Exit tool, or the mock
  frontend running out of steps when asked for input — every script
  list must be fully consumed. Entries the session never reached fail
  the run with the remaining-entry summary.

Exit code 0 from a scripted run therefore means the session consumed
the script exactly: every turn, tool execution, and frontend step, in
order, with nothing left over.

Two frontend mismatches surface as ordinary runtime errors (exit code
1, message on standard error) rather than as script failures: a
pending `AskUserQuestion` the script cannot answer (a failed
`contains` filter, a non-answer step next, or an exhausted list), and
an `expect_shutdown` step whose `message_contains` does not match the
final message.

---

## 5. A worked example

One prompt, one Bash call, then the model exits. Save this as
`hello.json`:

```json
{
  "name": "hello_bash",
  "llm": [
    {
      "expect_user_contains": "list the workspace",
      "response": {
        "content": [
          { "type": "text", "text": "Listing the workspace." },
          { "type": "tool_use", "id": "t1", "name": "Bash",
            "input": { "command": "ls" } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 12, "output_tokens": 6 }
      }
    },
    {
      "expect_user_contains": "src",
      "response": {
        "content": [
          { "type": "text", "text": "The workspace contains src." },
          { "type": "tool_use", "id": "t2", "name": "Exit",
            "input": { "message": "done: listed the workspace" } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 20, "output_tokens": 8 }
      }
    }
  ],
  "tools": [
    {
      "expect_name": "Bash",
      "expect_input": { "command": "ls" },
      "output": { "content": "src", "is_error": false }
    }
  ],
  "frontend": [
    { "step": "send_prompt", "text": "list the workspace" },
    { "step": "expect_shutdown", "message_contains": "done" }
  ]
}
```

Reading it as a session: the scripted user sends "list the workspace";
the scripted model answers with text plus a `Bash` tool call; the
scripted sandbox checks the call and answers `src`; the model's second
turn sees `src` in its tool result (which is what the second
`expect_user_contains` matches) and calls `Exit`; the frontend checks
the final message.

Run it. The example is self-contained under `/tmp`, including its own
state directory, so it touches nothing of yours:

```sh
export LLMDEVSILO_STATE_DIR=/tmp/replay-demo/state
silo run --workspace /tmp/replay-demo/ws --create --deterministic \
    --frontend mock --llm mock --sandbox mock --mock-proxy \
    --script hello.json
```

Output of that exact command (run against this repository's build; the
harness id in the journal path is random per run):

```
warning: workspace contents moved under the harness state directory; they are protected by file permissions only
done: listed the workspace
journal: /tmp/replay-demo/state/journals/11495edb10f7.jsonl
```

The final message lands on standard output and the exit code is 0,
which also certifies that every script entry was consumed (section 4).
The session wrote a real journal, so the round trip works on it too:

```sh
silo replay-test /tmp/replay-demo/state/journals/11495edb10f7.jsonl -o generated.json
silo run --workspace /tmp/replay-demo/ws --frontend mock --llm mock \
    --sandbox mock --mock-proxy --script generated.json --deterministic
```

```
Wrote generated.json (llm turns: 2, tool execs: 1, frontend steps: 2)
Replay it with:
  silo run --workspace /tmp/replay-demo/ws --frontend mock --llm mock --sandbox mock --mock-proxy --script generated.json --deterministic
done: listed the workspace
journal: /tmp/replay-demo/state/journals/f958211b566d.jsonl
```

The generated script is the directly authored one minus the
`expect_user_contains` lines, with the recorded shutdown message
(`"done: listed the workspace"`) as the `expect_shutdown` expectation.
The second run needs no `--create` because the workspace is already
locked; replays can run against the same locked workspace any number
of times. When finished:

```sh
silo workspace unlock /tmp/replay-demo/ws
```

---

## 6. Guarantees and limits

- **Mock-recorded sessions round-trip exactly.** A session run against
  mock components, converted to a script and replayed, reproduces the
  same event stream and the same `llm_request`, `llm_response`, and
  `tool_exec` journal entries, compared with timestamps stripped. The
  test `crates/silo-harness/tests/replay_roundtrip.rs` asserts this,
  and `interrupt_session.rs` and `upload_session.rs` assert it for
  sessions containing interrupts and uploads.
- **Real sessions replay with mock-equivalent semantics.** A journal
  from an interactive session converts the same way: client prompts
  become `send_prompt` steps, client answers become `answer_question`
  steps, uploads and interrupts are re-injected in recorded order, and
  wall-clock timing collapses to sequence order. What the replay
  verifies is the recorded interaction order and content — not timing,
  and not the behavior of the real model, sandbox, or network.
- **`--deterministic` reruns are stable.** Replaying the same script
  twice produces journals that are byte-identical except for the
  per-run harness id, which appears in the `meta` record and the
  `harness_started` event. To remove that difference too, pin
  `harness_id` in a `--config` TOML file (see the `--config` flag in
  [CLI.md](CLI.md)); the journals are then byte-for-byte identical.
- **Sessions with concurrent subagents** consume `llm` turns from one
  shared list in whatever order the agents reach the backend. Replays
  of mock-recorded sessions are ordered by construction; a journal
  recorded from a real session whose subagents genuinely raced may
  need its turn order untangled before it replays cleanly.

---

## 7. Replays as regression tests in this repository

The end-to-end tests in `crates/silo-harness/tests/` are scripts of
exactly this shape, built in code rather than JSON: each test
constructs a `TestScript`, runs a full harness session against the
mock components (fake clock, temporary state directory, in-memory
journal), and asserts on the outcome, the event stream, and the
journal. The harness checks full script consumption itself at session
end (the outcome's `script_failure` field, section 4); many tests also
assert `script.finished()` directly. The round-trip tests
(`replay_roundtrip.rs`, `interrupt_session.rs`, `upload_session.rs`)
additionally regenerate a script from the recorded journal with
`script_from_journal` and verify the replay matches the recording.

They run with the rest of the suite:

```sh
cargo test -p silo-harness
```

To turn one of your own sessions into a regression test, convert its
journal and keep the script:

```sh
silo replay-test ~/.llmdevsilo/journals/<id>.jsonl -o quota-bug.json --name quota_bug
```

then replay it (in continuous integration, or locally) with the mock
command line from section 5 and check the exit code: 0 means the
replay consumed the script exactly, and 4 means it diverged, with the
detail and the remaining-entry summary on standard error. Editing the
generated script is normal: trim turns that are irrelevant to the regression, loosen
`expect_input` to the keys that matter, or add `expect_user_contains`
to pin the conversation down at the points you care about.
