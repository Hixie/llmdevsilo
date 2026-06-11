# Cost metering and quotas

This document explains how the harness counts tokens, turns them into
dollar figures, shows those figures, enforces spending limits, and how to
change the prices it uses. It assumes you have read
[README.md](../README.md). Related documents, not duplicated here:

- [CLI.md](CLI.md) — every `silo run` flag, including the quota flags and
  the exit-code table.
- [PROTOCOLS.md](PROTOCOLS.md) — the wire shape of the `cost_report`
  event and the `request_cost`/`cost` message pair.
- [ARCHITECTURE.md](ARCHITECTURE.md) — where cost events sit in the agent
  loop, for contributors.

## How usage is counted

Every large language model (LLM) backend owns one usage meter for the
session (`silo_core::cost::UsageMeter`). After each completed model request, the
backend records the token usage the provider reported for that request:
one input-token count and one output-token count. The meter holds the
running totals. Subagents share their parent's backend, so one meter
covers the whole session, subagent traffic included.

The counts are the provider's own billing numbers, taken from each
response:

| Backend | Where the counts come from |
| --- | --- |
| `anthropic` | The `usage.input_tokens` and `usage.output_tokens` fields of each Messages API response. |
| `openai` | The `usage.input_tokens` and `usage.output_tokens` fields of each Responses API response. A missing field counts as zero. |
| `openai-ws` | The `usage.input_tokens` and `usage.output_tokens` fields of the final `response.done` event of each call. A missing field counts as zero. |
| `local` | The `usage.prompt_tokens` and `usage.completion_tokens` fields of each chat-completions response. A missing field counts as zero, so a server that reports no usage meters as zero tokens. |
| `mock` | The `usage` field of each scripted turn. |

Two consequences of this model are worth knowing:

- The harness resends the whole conversation on every request, so the
  input-token count of each request covers the full history up to that
  point. Later turns in a long session cost more than early ones. This
  matches how the providers bill.
- The dollar figure is an approximation of the provider's invoice, not a
  statement of it. The meter prices all input tokens at one flat rate and
  all output tokens at another. Provider-side variations — prompt
  caching, batch discounts, long-context surcharges, tiered rates — are
  not modeled.

## The dollars formula

A usage snapshot converts the running token totals to dollars using two
per-million rates:

```
usd = input_tokens  × usd_per_million_input_tokens  / 1,000,000
    + output_tokens × usd_per_million_output_tokens / 1,000,000
```

In words: the input-token total times the input rate per million tokens,
plus the output-token total times the output rate per million tokens.
Nothing is rounded in the meter; rounding happens only in displays.

## Where the rates come from

Each backend resolves its pricing once, at session start, with this
precedence:

1. **Configured pricing** — the `pricing` field of the LLM section in a
   `--config` file (see below). Always wins when present.
2. **The built-in price table** — consulted by the `anthropic`, `openai`,
   and `openai-ws` backends when no pricing is configured. The `local`
   and `mock` backends never consult the table.
3. **Zero** — when neither applies, both rates are zero. Tokens are still
   counted; dollars stay at `$0.0000`. This happens silently: an
   unrecognized model on a paid backend meters dollars as zero with no
   warning.

### The built-in price table

The table lives in `default_pricing_for_model` in
[`crates/silo-llm/src/common.rs`](../crates/silo-llm/src/common.rs). It
is matched against the configured model name by **substring**: the rows
are scanned top to bottom, and the first row whose key appears anywhere
in the model name wins. Matching is case-sensitive. Order matters — the
`-mini` rows sit above their parents so that, for example,
`gpt-4o-mini-2024` matches the `gpt-4o-mini` row rather than the
`gpt-4o` row.

| Substring | Dollars per million input tokens | Dollars per million output tokens |
| --- | --- | --- |
| `claude-opus` | 15.00 | 75.00 |
| `claude-sonnet` | 3.00 | 15.00 |
| `claude-haiku` | 0.80 | 4.00 |
| `gpt-4o-mini` | 0.15 | 0.60 |
| `gpt-4o` | 2.50 | 10.00 |
| `gpt-4.1-mini` | 0.40 | 1.60 |
| `gpt-4.1` | 2.00 | 8.00 |
| `gpt-5` | 1.25 | 10.00 |
| `o3` | 2.00 | 8.00 |

Treat these numbers as approximate and perishable. They were entered by
hand from public price lists and providers change prices without notice.
If a correct dollar figure matters to you — in particular if you rely on
`--quota-usd` as a spending cap — check the provider's current price
list and configure the rates yourself rather than trusting the table.

### Setting your own rates

There is no command-line flag for pricing. Set it in a `--config` TOML
file; the file supplies the base configuration and the other flags still
work as usual (see the `--config` flag in [CLI.md](CLI.md)). The two
rates are dollars per million tokens:

```toml
[llm]
backend = "anthropic"
model = "claude-sonnet-4-6"

[llm.pricing]
usd_per_million_input_tokens = 3.0
usd_per_million_output_tokens = 15.0
```

Configured pricing applies to whichever backend the session runs,
including `local` and `mock` — that is how a local model can be given a
notional cost, and how replays reproduce dollar figures (see below).

### Updating the built-in defaults

The defaults are source code, so updating them means editing
`default_pricing_for_model` in
[`crates/silo-llm/src/common.rs`](../crates/silo-llm/src/common.rs) and
rebuilding. The unit test `pricing_table_matches_by_substring` in the
same file pins the table — it asserts the `claude-sonnet` input rate and
that unknown models get no match — so adjust it together with the table.
For a one-off correction, prefer the `--config` pricing override; it
needs no rebuild.

## Quotas

Two optional per-session limits, settable as flags (`--quota-tokens`,
`--quota-usd`; see [CLI.md](CLI.md)) or in the configuration file:

```toml
[llm.quota]
max_total_tokens = 500000
max_usd = 5.0
```

- `max_total_tokens` is compared against the sum of the input-token and
  output-token totals.
- `max_usd` is compared against the dollar figure computed by the
  formula above — so a dollar quota on a backend with zero pricing
  (unrecognized model, unconfigured `local` backend) never triggers.

The check runs at the start of every model request, before anything is
sent. A total that has reached or passed its limit makes the request fail
with `llm quota exceeded: token quota reached: <used> of <max> tokens
used` or `llm quota exceeded: dollar quota reached: $<used> of $<max>
used`. Because usage is recorded only after each completed request, the
quota does not truncate a request in flight: a session can overshoot the
limit by up to one request's usage, and the quota then blocks the next
request. Size the limit with that margin in mind.

What happens after exhaustion depends on the frontend (this is the
general repeated-LLM-failure path; a quota-exceeded error is simply a
failure that never goes away):

- **headless** — the session ends on the first failure. The message goes
  to standard error and `silo run` exits with code 3.
- **interactive** (and mock) — each failure is broadcast to clients as an
  `error` event and the harness returns to awaiting input, so you can
  decide what to do (for example, shut down from a client). After 8
  consecutive failed turns the session ends: the last failure message
  goes to standard error and the exit code is 3.

A failure-ended session prints `silo: session ended by LLM failure:
<message>` to standard error; the exit-code table in [CLI.md](CLI.md)
has the full mapping. Running a paid backend (`anthropic`, `openai`,
`openai-ws`) with no quota at all prints a startup warning:
`warning: no session quota is set for a paid LLM backend; consider
--quota-tokens or --quota-usd`.

## Where cost figures appear

After every model completion the harness emits a `cost_report` event
carrying the backend id, the usage snapshot (input tokens, output
tokens, dollars), and the quota configuration. All connected clients
receive it as part of the event stream. Each surface shows the latest
report per backend:

- **Headless sessions** print one line per backend at exit, after the
  final message:

  ```
  cost[anthropic:claude-sonnet-4-6]: 52341 input tokens, 8120 output tokens, $0.2789
  ```

- **The terminal client** shows a running total in the status bar
  (dollars and tokens summed across backends, for example
  `$0.8100 | 150.0k tok`). The `/cost` command asks the harness for the
  latest figures and opens a popup with one entry per backend — dollars,
  input and output tokens, and the configured quota limits when any are
  set. Any key closes it.

- **The Flutter app** shows a cost chip in the chat screen with the
  session total (dollars and tokens). Tapping it refreshes the figures
  and opens a "Session cost" dialog with one row per backend: token
  counts, the quota limits when set, and dollars to four decimal places.

- **On the wire**, any client can send `request_cost` at any time and
  gets a `cost` reply with the latest entry per backend. The message
  shapes are in [PROTOCOLS.md](PROTOCOLS.md) sections 2.3 through 2.5.

## Journals and replays

Cost figures are recorded twice in the session journal:

- every `cost_report` event, like all events, is journaled;
- every `llm_response` journal record embeds the provider-reported
  `usage` for that single request.

`silo replay-test` copies each recorded response — including its usage —
into the generated script, and the mock backend records that scripted
usage into its meter during replay. A replayed session therefore
reproduces the recorded token counts and emits matching `cost_report`
events. The dollar figure in a replay is computed from the *replaying*
session's pricing: the mock backend prices as zero unless the replay is
run with a `--config` file that sets `[llm.pricing]`.

## A runnable demonstration

This walkthrough uses the mock components, so it needs no API key, costs
nothing, and executes nothing. It shows configured pricing, the headless
cost line, a dollar quota ending a session with exit code 3, and usage
surviving into a replay. It keeps everything under `/tmp`, including the
state directory.

Create a configuration with pricing and a dollar quota, and a script
whose single scripted response reports 120,000 input and 30,000 output
tokens (at these rates: 0.36 plus 0.45, so $0.81):

```sh
mkdir -p /tmp/silo-cost-demo
export LLMDEVSILO_STATE_DIR=/tmp/silo-cost-demo/state

cat > /tmp/silo-cost-demo/config.toml <<'EOF'
workspace = "/tmp/silo-cost-demo/ws"

[llm]
backend = "mock"

[llm.pricing]
usd_per_million_input_tokens = 3.0
usd_per_million_output_tokens = 15.0

[llm.quota]
max_usd = 0.50
EOF

cat > /tmp/silo-cost-demo/script.json <<'EOF'
{
  "name": "cost_demo",
  "llm": [
    {
      "response": {
        "content": [{"type": "text", "text": "Working on it."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 120000, "output_tokens": 30000}
      }
    }
  ]
}
EOF
```

Run it headless. The first request succeeds and overshoots the $0.50
quota; the second request is blocked, which ends the headless session:

```sh
silo run --config /tmp/silo-cost-demo/config.toml \
    --create --deterministic --frontend headless --prompt "Do a long task." \
    --sandbox mock --mock-proxy --script /tmp/silo-cost-demo/script.json
echo "exit: $?"
```

Output (the cost line on standard output; the warning, failure, and
journal lines on standard error):

```
warning: workspace contents moved under the harness state directory; they are protected by file permissions only
cost[mock]: 120000 input tokens, 30000 output tokens, $0.8100
silo: session ended by LLM failure: llm quota exceeded: dollar quota reached: $0.8100 of $0.50 used
journal: /tmp/silo-cost-demo/state/journals/<id>.jsonl
exit: 3
```

The journal contains the matching records — a `cost_report` event with
the priced snapshot, and the raw usage on the `llm_response` record:

```sh
grep -o '"kind":"cost_report".\{0,120\}' /tmp/silo-cost-demo/state/journals/*.jsonl
```

```
"kind":"cost_report","backend":"mock","usage":{"input_tokens":120000,"output_tokens":30000,"usd":0.81},"quota":{"max_usd":0.5}}}
```

Convert the journal to a replay script and confirm the recorded usage
was carried over:

```sh
silo replay-test /tmp/silo-cost-demo/state/journals/*.jsonl \
    -o /tmp/silo-cost-demo/replay.json
grep -A3 '"usage"' /tmp/silo-cost-demo/replay.json
```

Replaying that script re-records the same 150,000 tokens; run the replay
with the same `--config` file and the replayed `cost_report` events show
the same $0.81.

When you are done, unlock the demo workspace before deleting the
directory (the lock leaves it read-only; see `silo workspace unlock` in
[CLI.md](CLI.md)):

```sh
silo workspace unlock /tmp/silo-cost-demo/ws
rm -rf /tmp/silo-cost-demo
```
