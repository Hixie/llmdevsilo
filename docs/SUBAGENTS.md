# Subagents

The model running in a harness session can delegate work to subagents
through a tool called `Agent`. This document describes what a subagent
is, which tools it gets, how its lifecycle appears to connected clients,
the limits, how interrupts and costs work, and how to write prompts that
delegate well.

It assumes you have read [README.md](../README.md). Related documents,
not duplicated here:

- [CLI.md](CLI.md) — the `silo run` flags used by the examples below,
  and the journal and replay-test commands.
- [PROTOCOLS.md](PROTOCOLS.md) — section 2.5 has the full event payload
  inventory, including the `agent_spawned` and `agent_completed` wire
  formats.
- [ARCHITECTURE.md](ARCHITECTURE.md) — the agent loop internals, for
  contributors.
- [DESIGN.md](DESIGN.md) — the design rationale.

## What a subagent is

A subagent is one more conversation with the same model, run by the same
harness, inside the same session. When the model calls the `Agent` tool,
the harness starts a fresh conversation seeded with the tool's `prompt`
as its first user message, runs it until the model stops calling tools,
and returns the text of the subagent's last message as the `Agent`
tool's result. The parent conversation is paused while the subagent
runs and resumes when it finishes.

Everything operational is shared with the parent:

- the **same sandbox** — the same workspace mount (read/write), the same
  scratch space, the same read-only host allowlist;
- the **same network policy** — the same egress proxy, domain allowlist,
  and injected credentials;
- the **same LLM backend** — the same model, API key, and usage meter
  (see "Cost and quotas" below);
- the **same system prompt**.

This is by design ([DESIGN.md](DESIGN.md)): all agents in a harness
share one sandbox, and it is the model's responsibility to ensure their
work does not conflict — for example, two subagents told to modify the
same file in conflicting ways will do exactly that, with no mediation
from the harness.

What a subagent is *not*:

- It is **not isolated from the parent's files**. There is no
  per-subagent checkout, branch, or copy-on-write view. A subagent's
  edits are immediately visible to the parent, to other subagents, and
  to any `silo shell` sharing the workspace.
- It does **not see the parent's conversation**. The `prompt` string is
  its entire briefing; nothing else carries over, and the parent gets
  back only the final text.
- It has **no user interaction**. Subagents run autonomously from their
  prompt to their final report (the tools that touch the user are
  withheld; see the next section).
- It cannot end the session (no `Exit` tool).

## The Agent tool

The tool definition lives in `crates/silo-llm/src/common.rs` and is
registered for every session; the harness itself executes it
(`crates/silo-harness/src/agent.rs`). Its input schema:

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `prompt` | string | yes | Complete, self-contained task description for the subagent. It becomes the subagent's first (and only) user message. An empty or missing prompt returns the error tool result `Agent requires a non-empty 'prompt' string`. |
| `name` | string | no | Short display name for the subagent. Used only for display: it travels on the `agent_spawned` event and clients label the subagent's output with it. It is not shown to the subagent. |

The `Agent` tool is available to subagents too, so a subagent can spawn
its own subagents, up to the depth limit below.

## Which tools a subagent gets

Every tool is registered as available to top-level agents, to subagents,
or to both (`ToolAvailability` in `crates/silo-core/src/tool.rs`), and
each request to the model carries only the tools available to that
agent's kind.

| Tool | Top-level agent | Subagent |
| --- | --- | --- |
| `Read`, `Write`, `Edit`, `Bash`, `WebFetch`, `WebSearch` | yes | yes |
| `Agent` | yes | yes |
| `AskUserQuestion` (interactive frontend) | yes | no |
| `SendUserFile` (interactive frontend) | yes | no |
| `Exit` (headless frontend) | yes | no |

The withheld tools are exactly the user-facing ones: a subagent cannot
ask the user questions, cannot push files to the connected clients, and
cannot end the session. This matches the tool's contract — the prompt
must be self-contained because there is no way for the subagent to ask
for clarification. Anything the user needs to see or decide has to go
through the top-level agent.

## Lifecycle and events

Agents are identified by strings: the top-level agent is `agent-0`, and
each spawned subagent gets the next number (`agent-1`, `agent-2`, …) in
spawn order across the whole session. Every event names the agent it
belongs to, and all connected clients see the same stream (the event
inventory is in [PROTOCOLS.md](PROTOCOLS.md) section 2.5).

One delegation produces this sequence:

1. `tool_use` — the parent's `Agent` call, with the prompt and optional
   name in its input.
2. `agent_spawned` — carries `parent`, the new `agent` id, the optional
   `name`, and the full `prompt`.
3. The subagent's own activity: `assistant_text`, `tool_use`, and
   `tool_result` events tagged with the subagent's id, plus a
   `cost_report` after each of its model responses. Nested spawns repeat
   this sequence one level down.
4. `agent_completed` — carries the subagent's id, its final text as
   `result`, and `is_error` (false for a normal completion; true when
   the subagent's model request failed, the user interrupted, or the
   session shut down mid-run).
5. `tool_result` — the parent's `Agent` call resolves with the same
   final text.

`turn_complete` is only ever emitted for the top-level agent; a
subagent's end is `agent_completed`. Clients treat `agent_spawned` and
`agent_completed` as activity for the busy indicator (see "Busy/idle
derivation" in [PROTOCOLS.md](PROTOCOLS.md)).

## Limits

Two caps, both enforced when the `Agent` tool runs
(`crates/silo-harness/src/agent.rs`). Hitting either one returns an
error tool result to the calling model — the session continues and the
model sees the message and can adapt:

- **Depth: 3.** The top-level agent is depth 0 and each spawn adds one,
  so chains can reach `agent-0` → subagent → subagent → subagent. An
  `Agent` call by a depth-3 subagent returns
  `subagent depth limit (3) reached`.
- **Concurrency: 8.** At most 8 subagents can be live at once; a spawn
  past that returns `subagent concurrency limit (8) reached`. In
  practice this cap is a backstop: tool calls within one model response
  execute sequentially and the parent waits for each subagent inline, so
  live subagents form a single nested chain and the depth cap binds
  first.

## Failures, interrupts, and shutdown

**Model failure inside a subagent.** If a subagent's request to the
model fails (network error, exhausted quota, …), the subagent ends:
`agent_completed` is emitted with `is_error: true` and the failure
message as `result`, and the parent's `Agent` call returns that message
as an error tool result. The parent decides what to do next — though if
the cause was an exhausted session quota, the parent's own next request
fails the same way and the failure handling described under "Quota
flags" in [CLI.md](CLI.md) takes over.

**User interrupts.** An interrupt (for example Escape in the terminal
client) aborts the whole turn, however deep the agent tree is at that
moment. Every agent in the tree checks the same interrupt signal: the
innermost running tool call is cancelled (a sandbox execution returns
its partial output as the tool result), each agent's remaining queued
tool calls get the synthetic error result `[interrupted by the user]`,
and the tree unwinds. Each interrupted subagent gets an
`agent_completed` event with `is_error: true` and the result
`interrupted by the user`; the top level then emits `interrupted` in
place of `turn_complete` and the harness returns to awaiting input. The
conversation is left well-formed, so the next prompt resumes normally.

**Shutdown.** A shutdown request (client shutdown, signal) mid-subagent
ends the subagent with `agent_completed` (`is_error: true`, result
`session shutdown requested`) and the session unwinds.

## Cost and quotas

There is one usage meter per LLM backend for the whole session. Subagent
requests go through the same backend as the parent's, so their tokens
and dollars accumulate on the same meter, the session quota
(`--quota-tokens`, `--quota-usd`) is checked before every request
regardless of which agent makes it, and the `cost_report` events carry
session totals per backend. There is no per-agent cost breakdown.
Delegating to subagents multiplies model calls — every subagent runs its
own conversation loop — so quotas sized for single-agent sessions may
exhaust sooner than expected.

## Journals and replays

Subagent activity is journaled like any other activity (see "Journals"
in [CLI.md](CLI.md)):

- `llm_request` and `llm_response` records carry the subagent's agent
  id, so a journal shows each agent's full conversation, including
  which tools were offered to it.
- `tool_exec` records carry the agent id and the owning component. The
  `Agent` call itself is journaled with owner `harness`, after the
  subagent finishes, with the subagent's final text as its output.
- The `agent_spawned` and `agent_completed` events are journaled with
  the rest of the event stream.

`silo replay-test` converts such a journal into a deterministic test
script: the subagent's model turns land in the script's `llm` list in
recorded order (the mock backend consumes one shared list, so the
interleaving is reproduced exactly), and the `Agent` executions are
*not* listed under `tools` — during replay the harness spawns the
subagent again and replays its recorded turns. The round trip was
verified with the example below: record, convert, replay, same outcome.

## How clients render subagents

**Terminal client (`silo-tui`).** Subagent output is indented two spaces
under the top-level conversation and labeled `[subagent {name}]` using
the spawn name, or `[subagent {N}]` from the agent id when no name was
given. Spawn and completion show as notes:

```
  [subagent test survey] spawned: Count the test files in the workspace and report the number.
  [subagent test survey] completed: There are 3 test files.
```

A failed subagent shows `failed:` instead of `completed:`. Raw agent ids
appear only in the client's debug mode.

**Flutter app (`apps/silo_app`).** Subagent tiles are indented under the
top-level conversation and labeled with the subagent's display name —
the spawn name when one was given, otherwise `subagent N` from the agent
id (the top-level agent is labeled `main agent` where a label is
needed). Spawning and completion render as marker tiles: a split icon
with "*name* spawned" (or "*name* spawned by *parent*" for nested
spawns) and the prompt as detail, then a check or error icon with
"*name* completed" or "*name* failed" and the result as detail.

## Writing prompts that delegate well

The harness gives subagents no supervision, so the quality of a
delegation is set entirely by the prompt. When you ask the top-level
agent to parallelize or delegate work, it helps to know (and to tell it)
what makes a good `Agent` call:

- **Self-contained prompts.** The subagent sees the prompt and nothing
  else — no prior conversation, no chance to ask questions. The prompt
  must carry the task, the relevant paths, the constraints, and what the
  final report should contain.
- **Non-overlapping work.** All agents share one workspace, and the
  harness does not mediate conflicts ([DESIGN.md](DESIGN.md)): the model
  is responsible for ensuring two subagents do not edit the same files
  in conflicting ways. Partition work by file or directory, or run
  dependent steps in sequence rather than delegating them side by side.
- **Reports over side effects.** The only thing the parent receives is
  the subagent's final text. Asking the subagent to end with a precise
  report ("list the files you changed and the test command you ran")
  makes the result usable; a subagent that ends with a bare "done"
  forces the parent to re-inspect the workspace.
- **Names help you, not the model.** The `name` input is purely for
  display in clients and journals. Encouraging the model to set it makes
  long sessions much easier to follow.

## A runnable example

Subagents need no extra flags — any `silo run` session has the `Agent`
tool. The fastest way to *see* the machinery, without spending tokens,
is a scripted session against the mock components (the same mechanism as
[CLI.md](CLI.md)'s replay testing). The following script has the
top-level agent delegate a small survey to a named subagent, which runs
one `Bash` command and reports back. Save it as `subagent-demo.json`:

```json
{
  "name": "subagent_demo",
  "llm": [
    {
      "expect_user_contains": "count the tests",
      "response": {
        "content": [
          {"type": "text", "text": "Delegating the survey to a subagent."},
          {"type": "tool_use", "id": "t1", "name": "Agent",
           "input": {"name": "test survey",
                     "prompt": "Count the test files in the workspace and report the number."}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 0, "output_tokens": 0}
      }
    },
    {
      "expect_user_contains": "Count the test files",
      "response": {
        "content": [
          {"type": "tool_use", "id": "t2", "name": "Bash",
           "input": {"command": "ls tests | wc -l"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 0, "output_tokens": 0}
      }
    },
    {
      "expect_user_contains": "3",
      "response": {
        "content": [{"type": "text", "text": "There are 3 test files."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 0, "output_tokens": 0}
      }
    },
    {
      "expect_user_contains": "There are 3 test files.",
      "response": {
        "content": [
          {"type": "tool_use", "id": "t3", "name": "Exit",
           "input": {"message": "Survey done: 3 test files."}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 0, "output_tokens": 0}
      }
    }
  ],
  "tools": [
    {
      "expect_name": "Bash",
      "expect_input": {"command": "ls tests | wc -l"},
      "output": {"content": "3", "is_error": false}
    }
  ],
  "frontend": [
    {"step": "send_prompt", "text": "count the tests"},
    {"step": "expect_event", "kind": "agent_spawned", "contains": "test survey"},
    {"step": "expect_event", "kind": "agent_completed"},
    {"step": "expect_shutdown", "message_contains": "Survey done"}
  ],
  "network": []
}
```

Run it (no model calls, no code execution, no network):

```sh
silo run --workspace /tmp/subagent-demo --create --deterministic \
    --frontend mock --llm mock --sandbox mock --mock-proxy \
    --journal /tmp/subagent-demo.jsonl \
    --script subagent-demo.json
```

It prints `Survey done: 3 test files.` and exits 0. The journal then
shows the lifecycle described above:

```sh
grep -o '"kind":"agent_[a-z]*"[^}]*' /tmp/subagent-demo.jsonl
```

```
"kind":"agent_spawned","parent":"agent-0","agent":"agent-1","name":"test survey","prompt":"Count the test files in the workspace and report the number."
"kind":"agent_completed","agent":"agent-1","result":"There are 3 test files.","is_error":false
```

and the `Agent` execution journaled with owner `harness`:

```
"entry":"tool_exec","agent":"agent-0","owner":"harness","call":{"id":"t1","name":"Agent",…},"output":{"content":"There are 3 test files.","is_error":false}
```

The journal also shows the tool filtering directly: the `llm_request`
records for `agent-0` offer `AskUserQuestion`, `SendUserFile`, `Exit`,
and `Agent` alongside the sandbox tools, while the records for `agent-1`
offer only the sandbox tools and `Agent`. Note that the mock frontend
contributes all three frontend tools; a real interactive session has no
`Exit`, and a headless one has no `AskUserQuestion` or `SendUserFile`
(see "Frontend flags" in [CLI.md](CLI.md)).
