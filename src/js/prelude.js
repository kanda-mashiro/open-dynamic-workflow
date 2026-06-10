// Embedded prelude: defines the workflow DSL globals that user workflow.js
// scripts call. Mirrors Claude Code's dynamic-workflow surface
// (agent/parallel/pipeline/phase/log + args/budget). Each call funnels to a
// Rust op (ext:core/ops) that drives the real codex sub-agent runner.
//
// Loaded as the extension's esm_entry_point, so these run once before the user
// module is evaluated, installing globals on globalThis.

import {
  op_agent,
  op_phase,
  op_log,
  op_get_args,
  op_set_result,
  op_set_error,
  op_meta,
  op_budget_total,
  op_budget_spent,
} from "ext:core/ops";

let __agentSeq = 0;

// Appended to every NON-schema agent prompt: codex-flow consumes an agent's final
// message as a return value for the program, not a chat reply. (Schema agents get
// their shape enforced by codex --output-schema instead.) English per protocol.
const RETURN_CONVENTION =
  "\n\nYour final message IS the return value consumed by a program. " +
  "Output raw data only, with no preamble, no prose, and no markdown code fences.";

// agent(prompt, opts?) -> Promise<string | object>
//   opts: { label?, step?, group?, model?, sandbox?, schema?, cwd?,
//           isolate?/isolation?, timeoutMs?, agentType? }
//   Returns the agent's final text; if `schema` is given, the parsed object.
//   group: one-level nesting tag -- agents of a child workflow share a group and
//   the TUI folds them under one header. Convention: a child workflow takes a
//   ctx ({ step, group }) as its FIRST param and spreads it into every agent
//   (explicit like step: phase()/any global slot misattributes under parallel).
//   Children must NOT call phase() -- it would create a new TOP-LEVEL step.
globalThis.agent = function agent(prompt, opts = {}) {
  if (typeof prompt !== "string" || prompt.length === 0) {
    return Promise.reject(new TypeError("agent(prompt): prompt must be a non-empty string"));
  }
  // Reject "" and non-strings up front: an empty group would render a nameless
  // header and a bare "/label" failure entry downstream (silent misattribution).
  const group = opts.group ?? null;
  if (group !== null && (typeof group !== "string" || group.length === 0)) {
    return Promise.reject(new TypeError("agent opts.group must be a non-empty string"));
  }
  const id = ++__agentSeq;
  const spec = {
    id,
    label: opts.label ?? `agent-${id}`,
    // phase() sets the "current" step; an explicit opts.step overrides it.
    step: opts.step ?? globalThis.__currentStep ?? 0,
    // Nesting group (cosmetic; excluded from the resume cache key).
    group,
    // Schema agents send the raw prompt (codex --output-schema enforces shape);
    // non-schema agents get the return-value convention appended.
    prompt: opts.schema ? prompt : prompt + RETURN_CONVENTION,
    model: opts.model ?? null,
    sandbox: opts.sandbox ?? null,
    // schema is a JS object; stringified to a temp JSON Schema file by Rust.
    schema: opts.schema ? JSON.stringify(opts.schema) : null,
    cwd: opts.cwd ?? null,
    // isolate:true or isolation:'worktree' -> Rust runs the agent in a fresh
    // git worktree of the workflow's repo (removed when the agent finishes).
    isolate: opts.isolate === true || opts.isolation === "worktree",
    // Per-agent wall-clock timeout (ms); null = unbounded.
    timeout_ms: opts.timeoutMs ?? null,
    // Registry agent type: <agents_dir>/<type>.md body prefixes the prompt.
    agent_type: opts.agentType ?? null,
  };
  // Validation + retry now live in the Rust runner: it only resolves with valid
  // JSON when a schema was set, so here we just deserialize the success result.
  return op_agent(spec).then((text) => (opts.schema ? JSON.parse(text) : text));
};

// parallel(thunks) -> Promise<any[]>
//   thunks: array of () => Promise (NOT bare promises). Barrier: waits for all.
//   A thunk that throws resolves to null (so one failure doesn't sink the batch).
globalThis.parallel = function parallel(thunks) {
  if (!Array.isArray(thunks)) {
    return Promise.reject(new TypeError("parallel(thunks): expects an array of functions"));
  }
  return Promise.all(
    thunks.map((t) => {
      let p;
      try {
        p = typeof t === "function" ? t() : t;
      } catch (e) {
        return null;
      }
      return Promise.resolve(p).catch(() => null);
    }),
  );
};

// pipeline(items, ...stages) -> Promise<any[]>
//   Each item flows through all stages independently, NO barrier between stages
//   (item A can be in stage 3 while item B is in stage 1). Stage callback gets
//   (prevResult, originalItem, index). A throwing stage drops that item to null.
globalThis.pipeline = function pipeline(items, ...stages) {
  if (!Array.isArray(items)) {
    return Promise.reject(new TypeError("pipeline(items, ...stages): items must be an array"));
  }
  return Promise.all(
    items.map(async (item, index) => {
      let acc = item;
      for (const stage of stages) {
        try {
          acc = await stage(acc, item, index);
        } catch (e) {
          return null;
        }
      }
      return acc;
    }),
  );
};

// phase(title) -- declares/sets the current step shown in the TUI's left pane.
// Subsequent agent() calls without an explicit step attach to this phase.
// NOTE: __currentStep is ONE global slot (mirrors Claude Code's phase()).
// Calling phase() from concurrently interleaved branches misattributes later
// agents -- inside parallel()/pipeline() stages, capture `const s = phase(...)`
// up front and pass `{ step: s }` explicitly.
globalThis.phase = function phase(title) {
  const step = op_phase(String(title));
  globalThis.__currentStep = step;
  return step;
};

// log(message) -- emit a narrator line to the UI.
globalThis.log = function log(message) {
  op_log(String(message));
};

// args -- the JSON value passed in from the host (CLI/TUI), or null.
globalThis.args = op_get_args();

// budget -- token target for parity with Claude Code. `total` comes from
// CODEX_FLOW_BUDGET (null = unbounded); spent() counts THIS process's live
// output tokens only -- journal replays are free, so a resumed run restarts
// from 0; remaining() is total - spent (Infinity when unbounded).
const __budgetTotal = op_budget_total();
globalThis.budget = {
  total: __budgetTotal,
  spent() {
    return op_budget_spent();
  },
  remaining() {
    return __budgetTotal == null ? Infinity : Math.max(0, __budgetTotal - op_budget_spent());
  },
};

// Internal: the bootstrap calls this with the workflow's normalized
// `export const meta` ({name, phases:[string]}) so the UI can pre-draw the phases.
globalThis.__meta = (m) => op_meta(m);

// Internal: bootstrap uses these to hand the workflow's return value (or error)
// back to Rust via OpState.
globalThis.__setResult = (v) => op_set_result(v === undefined ? null : v);
globalThis.__setError = (msg) => op_set_error(String(msg));
