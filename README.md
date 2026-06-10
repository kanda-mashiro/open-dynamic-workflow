# codex-flow (open-dynamic-workflow)

Orchestrate **many parallel `codex` CLI sub-agents** from one JavaScript file.
A single Rust binary embeds a deno_core (V8) runtime that executes your
`workflow.js`; every `agent()` call spawns a real `codex exec --json`
subprocess, streamed live into a ratatui TUI (or plain stdout for CI/logs).

```js
// review.workflow.js — fan out, verify, synthesize
export const meta = { name: "review", phases: [{ title: "find" }, { title: "verify" }] };
export default async function run(args) {
  const s1 = phase("find");
  const found = await parallel(
    ["bugs", "perf", "security"].map((d) => () =>
      agent(`Review ${args.dir} for ${d} issues. Return JSON.`,
        { step: s1, schema: FINDINGS, sandbox: "read-only" })),
  );
  const s2 = phase("verify");
  const verdicts = await parallel(
    found.filter(Boolean).flatMap((f) => f.findings).map((f, i) => () =>
      agent(`Adversarially verify: ${f.title}`, { step: s2, group: `f${i}`, schema: VERDICT })),
  );
  return { confirmed: verdicts.filter((v) => v && v.real).length };
}
```

## Install (agent-friendly, copy-paste)

Prerequisites: **Rust stable ≥ 1.91** (edition 2024) and the **codex CLI** on
PATH with a working login (`codex exec "say hi"` must succeed).

```bash
git clone https://github.com/kanda-mashiro/open-dynamic-workflow.git
cd open-dynamic-workflow
cargo build --release                       # first build compiles V8: ~5-15 min
ln -sf "$PWD/target/release/codex-flow" ~/.local/bin/codex-flow   # or any PATH dir
codex-flow examples/hello.workflow.js       # smoke: streams events, prints RESULT:
```

Optional — install the Claude Code / codex skill so agents know the DSL:

```bash
ln -sfn "$PWD/skill/codex-flow-workflow" ~/.claude/skills/codex-flow-workflow
ln -sfn "$PWD/skill/codex-flow-workflow" ~/.codex/skills/codex-flow-workflow
```

## Run

```bash
codex-flow <wf>.js '<args-json>'      # stdout stream (grep RESULT: / FAILURES)
codex-flow --tui <wf>.js              # live TUI: steps | agents | drill-in detail
codex-flow --concurrency 16 <wf>.js   # explicit fan-out cap
codex-flow --resume <run_id> <wf>.js  # crash recovery: replay journaled agents
codex-flow bench 100 100              # headless render benchmark
```

Every run prints `== run <id>` and journals agent results to `runs/<id>.jsonl`;
`--resume <id>` replays unchanged `agent()` calls instantly (deterministic
prompt+opts cache key) and re-runs only the rest.

## DSL in one breath

Globals (no imports): `agent(prompt, opts)` · `parallel(thunks)` (barrier,
failures→`null`) · `pipeline(items, ...stages)` (no inter-stage barrier) ·
`phase(title)` · `log(msg)` · `args` · `budget` · `export const meta`.

`agent()` opts: `label, step, group, model, sandbox, schema, cwd, timeoutMs,
agentType, isolate`. With `schema` the result arrives parse-validated (invalid
JSON is auto-retried with the rejection reason fed back). Nested workflows are
plain ES module imports sharing the run's engine/permits/journal/budget; pass
ctx `{step, group}` explicitly into child agents — the TUI folds each group
under a collapsible header with live rollup.

Full reference: [`skill/codex-flow-workflow/references/dsl-reference.md`](skill/codex-flow-workflow/references/dsl-reference.md) ·
patterns: [`references/patterns.md`](skill/codex-flow-workflow/references/patterns.md) ·
design spec & decision log: [`SPEC.md`](SPEC.md)

## Env vars

| var | default | meaning |
|---|---|---|
| `CODEX_FLOW_CONCURRENCY` | min(16, cores−2) | max concurrent codex processes |
| `CODEX_FLOW_MAX_ATTEMPTS` | 3 | retries per agent (backoff, schema-reject feedback) |
| `CODEX_FLOW_TIMEOUT_MS` | 0 (off) | per-agent wall clock; kills the subprocess |
| `CODEX_FLOW_BUDGET` | 0 (off) | output-token ceiling; over-budget `agent()` rejects |
| `CODEX_FLOW_RUNS_DIR` | `./runs` | journal location for `--resume` |
| `CODEX_FLOW_AGENTS_DIR` | `~/.codex-flow/agents` | `agentType` registry (`<type>.md`) |
| `CODEX_FLOW_CODEX_BIN` | `codex` | binary override (offline testing with a fake) |

## Development

```bash
cargo test --lib    # 54 unit tests (engine, journal, registry, worktree, TUI rows/scroll)
cargo build         # debug binary at target/debug/codex-flow
```

The project is spec-driven: `SPEC.md` is the single source of truth — every
requirement, decision, and cross-model review verdict is recorded there.
