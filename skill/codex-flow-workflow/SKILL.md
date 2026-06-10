---
name: codex-flow-workflow
description: Author and run a codex-flow dynamic workflow. Use when the user wants to orchestrate MANY parallel codex (or other CLI coding-agent) sub-agents from a single task — fan-out builds/audits/migrations/research, multi-step pipelines, or "run N agents at once" — via a JavaScript workflow executed by the codex-flow binary. Generates a workflow.js using the agent/parallel/pipeline/phase/log DSL, then runs it (optionally with a live TUI, resumable via a run journal).
---

# codex-flow workflow authoring

codex-flow is a single binary that runs a **JavaScript workflow** in an embedded
V8 runtime; each `agent()` call in that JS spawns a real `codex exec` sub-agent.
The JS is the orchestration DSL (mirrors Claude Code dynamic workflows); codex
does the actual work. Your job in this skill: turn the user's task into a
`*.workflow.js` file and run it.

## When to use

Use this when the task benefits from **more than one codex agent**: parallel
fan-out (per file / module / item), multi-phase pipelines (scan -> fix ->
verify), loop-until-done, or adversarial cross-checking. For a single one-off
codex call, just run `codex exec` directly — don't author a workflow.

## The DSL (globals available inside workflow.js)

A workflow is an ES module with a **default async function** whose return value
is the final result. Optionally export a `meta` skeleton — the UI pre-draws all
phases before anything runs:

```js
export const meta = { name: "audit", phases: ["Scan", "Verify"] };
export default async function run(args) { /* ... */ return result }
```

| Global | Signature | Behavior |
|---|---|---|
| `agent(prompt, opts?)` | `=> Promise<string\|object>` | Spawn ONE `codex exec`. Returns final text; if `opts.schema` is set, returns the parsed object. The only thing that costs tokens. |
| `parallel(thunks)` | `([() => agent(...)]) => Promise<any[]>` | Run an array of **functions** concurrently. Barrier (waits for all). A throwing thunk becomes `null`. |
| `pipeline(items, ...stages)` | `=> Promise<any[]>` | Each item flows through all stages independently, **no barrier between stages**. Stage cb gets `(prev, item, index)`. |
| `phase(title)` | `(string) => number` | Declare a step (left pane). Subsequent `agent()` calls attach to it. Entering a new phase marks the previous one Done; run end finalizes the last one. |
| `log(msg)` | `(string) => void` | Narrator line. |
| `args` | value | The JSON passed on the command line (`{}`/null if none). Invalid JSON is a startup error (exit 2), not a silent null. |
| `budget` | `{ total, spent(), remaining() }` | Output-token budget. `total` from `CODEX_FLOW_BUDGET` (null = unlimited); once `spent() >= total`, new `agent()` calls reject (in-flight ones finish). |

**`agent()` opts:** `{ label, step, group, model, sandbox, schema, cwd, timeoutMs, agentType, isolate | isolation: "worktree" }`
- `sandbox`: `"read-only"` | `"workspace-write"` | `"danger-full-access"`
- `cwd`: working dir for that agent (`codex -C`); mutually exclusive with `isolate` (an isolated agent runs at its worktree root — combining them is an error). **Always set it for research/pure-generation agents**: codex reads the cwd repo's AGENTS.md as task context, so an agent inheriting a code-repo cwd can wander off following that repo's collaboration rules instead of doing the task (observed live: an explorer spent its run "confirming its role"). Point them at a neutral output dir.
- `schema`: a JSON Schema **object** -> codex `--output-schema`; the runner parse-validates the output and auto-retries with the rejection reason fed back; the resolved value is already `JSON.parse`d
- `timeoutMs`: per-agent wall clock; on expiry the codex process is killed and the attempt retried (global default `CODEX_FLOW_TIMEOUT_MS`; 0/unset = none)
- `agentType`: loads `<agents_dir>/<type>.md` (frontmatter ignored) and prepends the body to the prompt as system framing; missing type = error, not silent
- `isolate: true` (alias `isolation: "worktree"`): runs the agent in a fresh git worktree of the workflow's repo. **Ephemeral** — the worktree is removed when the agent returns, so have the agent output its diff/files, or copy them, before it finishes.
- `label`: shown in the TUI agent list
- `group`: one-level nesting tag — the TUI folds agents sharing a group under a
  collapsible header (rollup status + done/total + token sums); stdout prefixes
  `(group)` and FAILURES lists `group/label`. Cosmetic: excluded from the resume
  cache key. **Child-workflow convention**: a child workflow (a plain ES module
  using the DSL globals) takes a ctx `{ step, group }` as its FIRST param and
  spreads it into every `agent()` call — explicit, because any "current group"
  global slot misattributes under parallel (same rule as `step`). Children must
  NOT call `phase()` (it would create a new top-level step). One level only;
  deeper structure goes in labels.

Non-schema agents automatically get a return-value convention appended ("your
final message IS the return value — raw data only"), so don't add one yourself.

## Reliability semantics (what the runner guarantees)

- **Retries**: each agent gets up to `CODEX_FLOW_MAX_ATTEMPTS` (default 3)
  attempts with backoff; empty output, invalid-JSON-under-schema, non-zero
  exit, and timeouts are all retryable.
- **Failures don't sink the run**: a failed agent collapses to `null` in
  `parallel`/`pipeline`; the run prints a `FAILURES (n):` list with per-agent
  reasons at the end (stdout mode and TUI exit).
- **Journal + resume**: every run gets a run id (printed at start) and appends
  each success to `runs/<run_id>.jsonl`. `--resume <run_id>` replays unchanged
  `agent()` calls instantly — no spawn, no tokens, no budget spend. The cache
  key is the effective prompt + `{model, sandbox, schema, cwd, isolate}` plus
  an occurrence index (N identical calls stay N independent samples). Caveats:
  a timestamp or random value in a prompt changes the key every run and never
  hits — keep prompts deterministic, pass variability via `args`; resume from
  the SAME working directory you launched from; identical (prompt, opts) calls
  are interchangeable samples across a resume — when an agent's identity
  matters, put the item id in its prompt.

## Authoring rules (important)

1. **Default-export an async function** taking `args`. Return a JSON-serializable value.
2. **Fan out with `parallel(items.map(x => () => agent(...)))`** — pass thunks (`() =>`), not bare promises. Want hundreds? Map over hundreds; concurrency is capped by the runner, excess queue.
3. **Pass data between agents by putting it in the next prompt** (string-interpolate / `JSON.stringify`). There is no shared memory between agents.
4. **Each agent starts fresh** — include every file path, constraint, and prior result it needs directly in its prompt.
5. **Use `schema`** when a later line reads a field off the result; keep schemas small and `required`-tight.
6. **`phase()` is one global slot** (mirrors Claude Code): inside `parallel()`/`pipeline()` stages, capture `const s = phase("X")` up front and pass `{ step: s }` explicitly, or later agents get misattributed.
7. **Compose by ES module import** — workflows are real modules. `import { audit } from "./audit.lib.js"` and call it; everything shares one engine, concurrency cap, budget, and journal. This is the `workflow()`-nesting equivalent, for free.
8. Non-ASCII is fine in YOUR workflow.js (prompts can be Chinese). Only the engine's internal prelude must be ASCII — not your file.

## How to run

```bash
# build once: cargo build --release   (in the codex-flow repo)
codex-flow <workflow.js> '<args-json>'            # stream events to stdout
codex-flow --tui <workflow.js> '<args-json>'      # live two-pane TUI
codex-flow --concurrency 24 <workflow.js>         # override the agent cap
codex-flow --resume <run_id> <workflow.js>        # replay journaled successes
```

Env knobs (flag > env > default): `CODEX_FLOW_CONCURRENCY` (default
`min(16, cores-2)`), `CODEX_FLOW_TIMEOUT_MS`, `CODEX_FLOW_BUDGET` (output
tokens), `CODEX_FLOW_MAX_ATTEMPTS`, `CODEX_FLOW_RUNS_DIR` (journal dir,
default `./runs`), `CODEX_FLOW_AGENTS_DIR` (agentType registry, default
`~/.codex-flow/agents`), `CODEX_FLOW_CODEX_BIN` (fake codex for offline tests).

The run id line at startup is copy-pastable: if a long run dies (network, ^C),
rerun with `--resume <run_id>` and only the unfinished agents execute. Long
runs: launch inside tmux and redirect stdout to a file so it survives
disconnects.

## Minimal procedure for THIS skill

1. Restate the task: how many agents, what each does, what phases, what each returns.
2. Write `<name>.workflow.js` with `export const meta` + a default-export `run(args)` using the DSL above.
3. Run it (`--tui` if the user wants to watch; plain otherwise). Note the run id.
4. Read the final `RESULT: {...}` and the `FAILURES:` list; report. Independently verify if the task has a checkable outcome (curl an endpoint, run tests). If the run died midway, `--resume` it instead of restarting.

## Examples & deeper reference

- Full DSL, every opt, error/null semantics, gotchas -> `references/dsl-reference.md`
- Copy-paste patterns (parallel audit, build-verify, per-file pipeline,
  loop-until-count, adversarial verify) -> `references/patterns.md`
- Runnable examples in the codex-flow repo `examples/`:
  `hello.workflow.js`, `student-mgmt.workflow.js`, `mock.workflow.js`

Read `references/dsl-reference.md` before writing anything non-trivial.
