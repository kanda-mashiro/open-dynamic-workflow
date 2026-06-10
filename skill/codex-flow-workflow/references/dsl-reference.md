# codex-flow workflow DSL — full reference

A workflow is an ES module. The host loads it, calls its **default export** with
`args`, and the return value becomes the run's result. All DSL names are
globals — do NOT import them.

```js
export default async function run(args) {
  // ... use agent / parallel / pipeline / phase / log / args ...
  return { /* JSON-serializable result */ };
}
```

`export function run(...)` and `export default <promise>` also work, but
`export default async function run(args)` is canonical.

## agent(prompt, opts?) -> Promise<string | object>

Spawns one `codex exec --json` sub-process; streams its events to the UI and
resolves with the agent's final assistant text.

- `prompt` (string, required): full task. The agent starts with a **fresh
  context** — embed every file path, error, constraint, and upstream result it
  needs. Any language (Chinese fine).
- `opts`:
  - `label` (string): TUI agent name. Default `agent-<n>`.
  - `step` (number): phase to attach to. Defaults to current `phase()`.
  - `group` (non-empty string): one-level nesting tag — the TUI folds agents
    sharing a group under a collapsible header (rollup + done/total + token
    sums); stdout prefixes `(group)`; FAILURES lists `group/label`. Cosmetic:
    excluded from the resume cache key. `""` is rejected with a TypeError.
  - `model` (string): codex `-m` override. Omit for codex's config default.
  - `sandbox` (string): codex `-s` — `read-only` | `workspace-write` | `danger-full-access`.
  - `cwd` (string): codex `-C` working dir. Use distinct dirs per agent to avoid
    write collisions. **Always set it for research/pure-generation agents**:
    codex reads the cwd repo's AGENTS.md as task context and can wander off
    following that repo's rules; point such agents at a neutral output dir.
  - `schema` (object): JSON Schema. Written to a temp file, passed as codex
    `--output-schema`; the runner parse-validates the output, RETRIES invalid
    JSON with the rejection reason fed back into the prompt, and resolves with
    the already-`JSON.parse`d value. OpenAI-strict: every object must list ALL
    its property keys in `required`.
  - `timeoutMs` (number): per-agent wall clock; on expiry the codex process is
    killed (kill_on_drop) and the attempt retried. Global default
    `CODEX_FLOW_TIMEOUT_MS`; 0/unset = none.
  - `agentType` (string): loads `<agents_dir>/<type>.md` (frontmatter stripped)
    and prepends the body as system framing. Missing type = error, not silent.
    Registry: `~/.codex-flow/agents/` or `CODEX_FLOW_AGENTS_DIR`.
  - `isolate: true` (alias `isolation: "worktree"`): runs the agent in a fresh
    git worktree of the workflow's repo, removed when the agent returns
    (ephemeral — have the agent output its diff before finishing). Mutually
    exclusive with `cwd`.

Return: final text (string), or parsed object when `schema` is set. Transient
failures (non-zero exit, empty output, schema-invalid JSON) are retried with
exponential backoff (`CODEX_FLOW_MAX_ATTEMPTS`, default 3); after the last
attempt agent() **rejects** — try/catch, or run via `parallel`/`pipeline`
(which turn a throw into `null`). Non-schema prompts automatically get a
return-value convention appended ("your final message IS the return value");
don't add your own.

## parallel(thunks) -> Promise<any[]>

```js
const out = await parallel(items.map(it => () => agent(`do ${it}`)));
```

- Array of **thunks** (`() => Promise`), NOT bare promises.
- **Barrier**: resolves when all settle; order preserved.
- A throwing thunk/agent -> `null` in the array. `.filter(Boolean)` before use.
- Fan as wide as you want; runner caps real concurrency and queues the rest.

## pipeline(items, ...stages) -> Promise<any[]>

```js
const results = await pipeline(
  files,
  f  => agent(`review ${f}`, { schema: REVIEW }),     // stage 1
  rv => agent(`fix per: ${JSON.stringify(rv)}`),      // stage 2
);
```

- Each item flows through ALL stages independently. **No barrier between
  stages** (max throughput).
- Stage cb gets `(prevResult, originalItem, index)`.
- A throwing stage drops that item to `null` (later stages skipped).
- Default to `pipeline` for multi-stage per-item work; use `parallel` only when a
  stage needs ALL prior results at once (dedup/merge/early-exit).

## phase(title) -> number ; log(message) ; args ; budget ; meta

- `phase(title)`: declare a step (TUI left pane), returns its index. Later
  agents attach to it unless they pass `opts.step`. Entering a new phase marks
  the previous one Done; the last phase is finalized by the run's outcome.
- `log(msg)`: narrator line.
- `args`: JSON from the command line (`codex-flow wf.js '{"x":1}'` -> `args.x===1`).
- `budget`: `{ total, spent(), remaining() }` — output-token budget. `total`
  from `CODEX_FLOW_BUDGET` (null = unlimited); once `spent() >= total`, new
  `agent()` calls reject (in-flight agents finish; journal replays are free).
- `export const meta = { name, description, phases: [{title}...] }`: optional;
  pre-draws the phase skeleton in the TUI and shows the name in the status bar.
  Use the SAME titles as your `phase()` calls (matched by index).

## Nested workflows (child = plain ES module)

A child workflow is just an exported async function using the same DSL globals
— `import { buildModule } from "./lib/build_module.js"` and call it. Children
share the run's engine, concurrency permits, journal and budget. Convention:
the child takes a ctx `{ step, group }` as its FIRST param and spreads it into
every `agent()` call (`{ ...ctx, label: ... }`) — explicit, because any
"current group" global slot misattributes under parallel execution. Children
must NOT call `phase()` (it would create a new top-level step). One level of
grouping; deeper structure goes in labels.

## Passing data between agents

No shared memory. The only channel into an agent is its prompt. Thread results
forward by interpolating:

```js
const plan = await agent("Decompose into 3 tasks", { schema: PLAN });
const done = await parallel(
  plan.tasks.map(t => () => agent(`Implement: ${JSON.stringify(t)}`, { cwd: dir }))
);
```

## Gotchas

- **Thunks, not promises** in `parallel`: `() => agent(...)`.
- **Errors**: bare `await agent()` throws; inside `parallel`/`pipeline` -> `null`.
- **Determinism**: keep orchestration pure JS; side effects belong inside agents.
- **Verify independently** when the outcome is checkable — don't trust an agent's
  self-reported "done"; curl/run-tests yourself.

## Run

```bash
codex-flow <name>.workflow.js '<args-json>'        # stdout stream
codex-flow --tui <name>.workflow.js '<args-json>'  # live TUI
codex-flow --concurrency 16 <wf>.js                # explicit fan-out cap
codex-flow --resume <run_id> <wf>.js               # replay journaled agents
```

Every run prints `== run <id>` (stderr) and journals successful agent results
to `runs/<id>.jsonl`; `--resume <id>` replays unchanged `agent()` calls
instantly (same prompt+opts, same cwd) and re-runs only the rest.

Env vars: `CODEX_FLOW_CONCURRENCY` (default min(16, cores-2)),
`CODEX_FLOW_MAX_ATTEMPTS` (3), `CODEX_FLOW_TIMEOUT_MS` (0=off),
`CODEX_FLOW_BUDGET` (output tokens, 0/unset=off), `CODEX_FLOW_RUNS_DIR`
(./runs), `CODEX_FLOW_AGENTS_DIR` (~/.codex-flow/agents),
`CODEX_FLOW_CODEX_BIN` (codex binary override, for testing/mocking).
