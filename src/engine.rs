//! The JS workflow engine: an embedded deno_core (V8) runtime that executes a
//! user-authored ESM `workflow.js`, exposing the `agent/parallel/pipeline/
//! phase/log` DSL as ops. Each `agent()` call is bridged to the real codex
//! sub-agent runner; every UI-relevant event is pushed over an mpsc.
//!
//! API pinned to deno_core 0.403 (June 2026): op2(async) with JsErrorBox,
//! OpState cloned before any .await, custom ModuleLoader for the `workflow:`
//! virtual scheme, mod_evaluate → run_event_loop → await, result via OpState.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use deno_core::error::ModuleLoaderError;
use deno_core::{
    extension, op2, resolve_import, JsRuntime, ModuleLoadOptions, ModuleLoadReferrer,
    ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType,
    OpState, PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use deno_error::JsErrorBox;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::codex::{over_budget, run_agent, AgentSpec, RunnerCtx};
use crate::event::{AgentStatus, AgentUpdate, AppEvent, StepStatus};
use crate::journal::{journal_key, journal_path, Journal, KeyInput};
use crate::registry::{agent_system_prefix, agents_dir};
use crate::worktree::WorktreeSpec;

// ── State stored in OpState, accessible from ops ──

/// Host config + channels the ops need. Cloned out of OpState before awaits.
struct EngineState {
    ctx: RunnerCtx,
    tx: mpsc::UnboundedSender<AppEvent>,
    /// JSON args passed from the host, exposed to JS as `globalThis.args`.
    args: serde_json::Value,
    /// Monotonic step counter assigned by phase().
    step_seq: AtomicU64,
    /// Where the user workflow file lives (for resolving its relative imports).
    workflow_dir: PathBuf,
    /// Temp dir holding generated JSON Schema files (kept alive for the run).
    tempdir: Arc<tempfile::TempDir>,
    /// Resume cache + per-key occurrence counters for this run's agent() calls.
    journal: Journal,
    /// Where new successes are appended (`runs/<run_id>.jsonl`).
    journal_path: PathBuf,
}

/// Final workflow result, stashed by op_set_result / op_set_error.
struct ResultSlot(Result<serde_json::Value, String>);

// ── DSL op argument shapes (deserialized from the JS spec object) ──

#[derive(Deserialize)]
struct JsAgentSpec {
    id: u64,
    label: String,
    step: usize,
    prompt: String,
    model: Option<String>,
    sandbox: Option<String>,
    /// JSON Schema as a string (already JSON.stringify'd in JS), or null.
    schema: Option<String>,
    cwd: Option<String>,
    isolate: bool,
    /// Per-agent timeout in ms (opts.timeoutMs); absent/null = unbounded.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Registry agent type (opts.agentType): its .md body prefixes the prompt.
    #[serde(default)]
    agent_type: Option<String>,
    /// One-level nesting group (opts.group); cosmetic, NOT part of the journal key.
    #[serde(default)]
    group: Option<String>,
}

// (Worktree isolation lives in crate::worktree; the runner materializes it
// behind the concurrency permit — see codex.rs / codex M6 review #1.)

/// Normalized `export const meta` the bootstrap forwards to op_meta.
#[derive(Deserialize)]
struct JsMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    phases: Vec<String>,
}

// ── Step lifecycle (pure, unit-tested) ──

/// When phase() enters `new_step`, the step just left (if any) becomes Done.
/// phase() indices are dense from 0, so the predecessor is simply `step - 1`.
fn left_step(new_step: usize) -> Option<usize> {
    new_step.checked_sub(1)
}

/// The current (last) step's terminal status when the run ends.
fn final_step_status(success: bool) -> StepStatus {
    if success {
        StepStatus::Done
    } else {
        StepStatus::Failed
    }
}

// ── Ops ──

/// agent(spec) -> Promise<string>. Spawns one codex sub-agent, returns final text.
/// Async ops use plain `#[op2]` + `async fn` in deno_ops 0.279 (the `async`
/// flag form is reserved for async(lazy|fake|deferred) variants).
#[op2]
#[string]
async fn op_agent(
    state: Rc<RefCell<OpState>>,
    #[serde] spec: JsAgentSpec,
) -> Result<String, JsErrorBox> {
    // M6: isolate and cwd are mutually exclusive — the isolated agent runs at
    // its worktree root (a fresh copy); silently ignoring either would mislead.
    if spec.isolate && spec.cwd.is_some() {
        return Err(JsErrorBox::generic(
            "agent opts: `isolate`/`isolation` and `cwd` are mutually exclusive — an isolated agent runs at its worktree root",
        ));
    }

    // M6: an agentType's registry body becomes the prompt's system framing.
    // Resolved BEFORE the journal key so the EFFECTIVE prompt is the cache
    // identity — editing the registry .md correctly re-runs on resume.
    let prompt = match &spec.agent_type {
        Some(t) => {
            let prefix = agent_system_prefix(t, &agents_dir()).map_err(JsErrorBox::generic)?;
            format!("{prefix}\n\n{}", spec.prompt)
        }
        None => spec.prompt.clone(),
    };

    // Resume replay first (it's free — no spawn, no permit, no budget spend).
    // The key excludes id/label/step; the occurrence index keeps N identical
    // (prompt, opts) calls N independent samples (SPEC M5).
    let key = journal_key(&KeyInput {
        prompt: &prompt,
        model: spec.model.as_deref(),
        sandbox: spec.sandbox.as_deref(),
        schema: spec.schema.as_deref(),
        cwd: spec.cwd.as_deref(),
        isolate: spec.isolate,
    });
    let (occ, cached) = {
        let mut s = state.borrow_mut();
        let es = s.borrow_mut::<EngineState>();
        let occ = es.journal.occurrence(&key);
        (occ, es.journal.get(&key, occ).map(str::to_string))
    };
    if let Some(hit) = cached {
        let tx = state.borrow().borrow::<EngineState>().tx.clone();
        let _ = tx.send(AppEvent::AgentSpawned {
            id: spec.id,
            step: spec.step,
            label: spec.label.clone(),
            prompt: prompt.clone(),
            group: spec.group.clone(),
        });
        for up in [
            AgentUpdate::Note("resumed from journal".to_string()),
            AgentUpdate::Final(hit.clone()),
            AgentUpdate::Status(AgentStatus::Done),
        ] {
            let _ = tx.send(AppEvent::Agent { id: spec.id, update: up });
        }
        return Ok(hit);
    }

    // Best-effort budget gate: refuse to START a new agent once the output-token
    // target is met. In-flight agents may still overshoot (this check races their
    // accumulation) — that is the documented best-effort contract.
    {
        let s = state.borrow();
        let es = s.borrow::<EngineState>();
        let spent = es.ctx.spent.load(Ordering::Relaxed);
        if over_budget(es.ctx.budget_total, spent) {
            return Err(JsErrorBox::generic(format!(
                "budget exceeded: {spent} output tokens spent >= total {}",
                es.ctx.budget_total.unwrap_or(0)
            )));
        }
    }
    // Clone everything out of OpState BEFORE awaiting (OpState is !Send and the
    // borrow must not be held across .await — 0.403 op2 panics otherwise).
    let (ctx, tx, schema_path, worktree) = {
        let s = state.borrow();
        let es = s.borrow::<EngineState>();

        // Materialize a JSON Schema file for codex --output-schema, if provided.
        let schema_path = if let Some(schema) = &spec.schema {
            let p = es
                .tempdir
                .path()
                .join(format!("schema-{}.json", spec.id));
            std::fs::write(&p, schema)
                .map_err(|e| JsErrorBox::generic(format!("write schema: {e}")))?;
            Some(p.to_string_lossy().into_owned())
        } else {
            None
        };

        // M6 (R6.1): isolate -> tell the runner WHAT to fork; it materializes
        // the worktree only once the agent holds a concurrency permit.
        let worktree = spec.isolate.then(|| WorktreeSpec {
            repo: es.workflow_dir.clone(),
            base: es.tempdir.clone(),
        });

        (es.ctx.clone(), es.tx.clone(), schema_path, worktree)
    };

    // Announce the agent so the TUI can show it as pending immediately.
    let _ = tx.send(AppEvent::AgentSpawned {
        id: spec.id,
        step: spec.step,
        label: spec.label.clone(),
        prompt: prompt.clone(),
        group: spec.group.clone(),
    });

    let agent_spec = AgentSpec {
        id: spec.id,
        step: spec.step,
        label: spec.label,
        prompt,
        cwd: spec.cwd,
        model: spec.model,
        sandbox: spec.sandbox,
        output_schema: schema_path,
        timeout_ms: spec.timeout_ms,
        worktree,
    };

    let text = run_agent(ctx, agent_spec)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;

    // Journal the success so a later --resume replays it. An append failure
    // (e.g. read-only CWD) degrades to a non-resumable run — noted, not fatal.
    {
        let mut s = state.borrow_mut();
        let es = s.borrow_mut::<EngineState>();
        let path = es.journal_path.clone();
        if let Err(e) = es.journal.append(&path, key, occ, text.clone()) {
            // On the agent's own id (not narrator 0): the TUI drops id-0 notes,
            // and this degradation must be visible there too (codex M5 #5).
            let _ = es.tx.send(AppEvent::Agent {
                id: spec.id,
                update: AgentUpdate::Note(format!("journal append failed: {e}")),
            });
        }
    }
    Ok(text)
}

/// phase(title) -> step index. Declares a step and marks it running.
#[op2(fast)]
#[bigint]
fn op_phase(state: &mut OpState, #[string] title: String) -> u64 {
    let es = state.borrow::<EngineState>();
    let step = es.step_seq.fetch_add(1, Ordering::SeqCst) as usize;
    // Entering a new phase completes the one just left (live progress in the TUI).
    if let Some(prev) = left_step(step) {
        let _ = es.tx.send(AppEvent::StepStatus {
            step: prev,
            status: StepStatus::Done,
        });
    }
    let _ = es.tx.send(AppEvent::StepDeclared { step, title });
    let _ = es.tx.send(AppEvent::StepStatus {
        step,
        status: StepStatus::Running,
    });
    step as u64
}

/// log(message) — narrator line.
#[op2(fast)]
fn op_log(state: &mut OpState, #[string] message: String) {
    let es = state.borrow::<EngineState>();
    // Reuse the agent channel with a synthetic id 0 = "narrator".
    let _ = es.tx.send(AppEvent::Agent {
        id: 0,
        update: AgentUpdate::Note(message),
    });
}

/// args() -> the host JSON value (or null).
#[op2]
#[serde]
fn op_get_args(state: &mut OpState) -> serde_json::Value {
    state.borrow::<EngineState>().args.clone()
}

/// __setResult(value) — stash the workflow's return value.
#[op2]
fn op_set_result(state: &mut OpState, #[serde] value: serde_json::Value) {
    state.put(ResultSlot(Ok(value)));
}

/// __setError(message) — stash a workflow error.
#[op2(fast)]
fn op_set_error(state: &mut OpState, #[string] message: String) {
    state.put(ResultSlot(Err(message)));
}

/// meta({name, phases}) — declare the workflow's metadata. Emits one RunMeta
/// event so the UI pre-draws the phases as Pending steps; does NOT advance
/// step_seq, so later phase() calls (which start at 0) align index-for-index.
#[op2]
fn op_meta(state: &mut OpState, #[serde] meta: JsMeta) {
    let es = state.borrow::<EngineState>();
    let _ = es.tx.send(AppEvent::RunMeta {
        name: meta.name,
        phases: meta.phases,
    });
}

/// budget.total — the run's token target as a JS number, or null if unset.
#[op2]
#[serde]
fn op_budget_total(state: &mut OpState) -> serde_json::Value {
    match state.borrow::<EngineState>().ctx.budget_total {
        Some(t) => serde_json::json!(t),
        None => serde_json::Value::Null,
    }
}

/// budget.spent() — cumulative output tokens across all agents, read live.
/// Returned as f64 (exact to 2^53) so JS budget math stays plain numbers.
#[op2(fast)]
fn op_budget_spent(state: &mut OpState) -> f64 {
    state.borrow::<EngineState>().ctx.spent.load(Ordering::Relaxed) as f64
}

extension!(
    codexflow,
    ops = [
        op_agent,
        op_phase,
        op_log,
        op_get_args,
        op_set_result,
        op_set_error,
        op_meta,
        op_budget_total,
        op_budget_spent,
    ],
    esm_entry_point = "ext:codexflow/prelude.js",
    esm = [dir "src/js", "prelude.js"],
    // EngineState must be present in OpState BEFORE the esm_entry_point
    // (prelude.js) runs — it calls op_get_args() at module top level during
    // JsRuntime::new. The state closure runs at extension init, in time.
    options = { engine_state: EngineState },
    state = |state, options| {
        state.put(options.engine_state);
    },
);

// ── Module loader: serves the `workflow:` virtual scheme ──

struct WorkflowLoader {
    bootstrap: String,
    workflow_path: PathBuf,
}

impl ModuleLoader for WorkflowLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        // The user file imported by the bootstrap under a stable virtual name.
        if specifier == "workflow:main" {
            return ModuleSpecifier::from_file_path(&self.workflow_path)
                .map_err(|_| JsErrorBox::generic("bad workflow path"));
        }
        if specifier == "workflow:bootstrap" {
            return ModuleSpecifier::parse("workflow:bootstrap")
                .map_err(|e| JsErrorBox::generic(e.to_string()));
        }
        // Relative imports from the user file resolve against its directory.
        if referrer.starts_with("workflow:") {
            let base = ModuleSpecifier::from_file_path(&self.workflow_path)
                .map_err(|_| JsErrorBox::generic("bad workflow path"))?;
            return resolve_import(specifier, base.as_str()).map_err(JsErrorBox::from_err);
        }
        resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(self.load_inner(module_specifier))
    }
}

impl WorkflowLoader {
    fn load_inner(&self, spec: &ModuleSpecifier) -> Result<ModuleSource, ModuleLoaderError> {
        if spec.as_str() == "workflow:bootstrap" {
            return Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(self.bootstrap.clone().into()),
                spec,
                None,
            ));
        }
        let path = spec
            .to_file_path()
            .map_err(|_| JsErrorBox::generic(format!("cannot load {spec}")))?;
        let code = std::fs::read_to_string(&path).map_err(JsErrorBox::from_err)?;
        Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(code.into()),
            spec,
            None,
        ))
    }
}

/// The bootstrap main module: imports the user workflow's default export
/// (expected to be an async `run` function OR a top-level promise), awaits it,
/// and hands the result back to Rust. Tolerates both `export default fn` and
/// `export function run()`.
fn bootstrap_source() -> String {
    r#"
import * as user from "workflow:main";
try {
  // Pre-declare the phase skeleton from `export const meta` (if any) before the
  // workflow runs. Defensive normalization (codex M4 review #3): a name-only
  // meta still surfaces its name, and a non-string title degrades to "" instead
  // of failing deserialization at startup.
  const m = user.meta;
  if (m && typeof m === "object") {
    const phases = Array.isArray(m.phases)
      ? m.phases.map((p) =>
          typeof p === "string" ? p : p && typeof p.title === "string" ? p.title : "")
      : [];
    globalThis.__meta({ name: typeof m.name === "string" ? m.name : "", phases });
  }
  const entry = user.default ?? user.run ?? user.workflow;
  let result;
  if (typeof entry === "function") {
    result = await entry(globalThis.args);
  } else if (entry && typeof entry.then === "function") {
    result = await entry;
  } else if (entry !== undefined) {
    result = entry;
  } else {
    throw new Error("workflow.js must `export default` an async function (or `export function run()`)");
  }
  globalThis.__setResult(result);
} catch (e) {
  globalThis.__setError(e && e.stack ? e.stack : String(e));
}
"#
    .to_string()
}

/// Run a user workflow file to completion on the CURRENT thread (deno_core is
/// !Send). Drives the V8 event loop; each agent() op spawns a codex process.
/// Returns the workflow's final result value.
pub async fn run_workflow(
    workflow_path: PathBuf,
    args: serde_json::Value,
    concurrency: usize,
    run_id: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<serde_json::Value, String> {
    // ModuleSpecifier::from_file_path requires an absolute path.
    let workflow_path = std::fs::canonicalize(&workflow_path)
        .map_err(|e| format!("workflow file {}: {e}", workflow_path.display()))?;
    let tempdir = Arc::new(
        tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?,
    );
    // 0 disables, matching the CODEX_FLOW_TIMEOUT_MS convention. Lives in
    // RunnerCtx (single source): the engine's fast-path gate AND the runner's
    // post-queue re-check both read it.
    let budget_total = std::env::var("CODEX_FLOW_BUDGET")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0);
    let ctx = RunnerCtx::new(concurrency, budget_total, tx.clone());
    let workflow_dir = workflow_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // Resume: load this run id's journal (a fresh id loads nothing). An
    // unreadable file degrades LOUDLY to a fresh non-resumable journal — a
    // --resume that silently re-ran everything would defeat its purpose.
    let journal_path = journal_path(&run_id);
    let journal = match Journal::load(&journal_path) {
        Ok(j) => j,
        Err(e) => {
            let _ = tx.send(AppEvent::Agent {
                id: 0,
                update: AgentUpdate::Note(format!(
                    "journal {} unreadable ({e}); starting fresh",
                    journal_path.display()
                )),
            });
            Journal::new()
        }
    };
    let engine_state = EngineState {
        ctx,
        tx: tx.clone(),
        args,
        step_seq: AtomicU64::new(0),
        workflow_dir,
        tempdir,
        journal,
        journal_path,
    };

    let loader = Rc::new(WorkflowLoader {
        bootstrap: bootstrap_source(),
        workflow_path,
    });

    let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(loader),
        extensions: vec![codexflow::init(engine_state)],
        is_main: true,
        ..Default::default()
    });

    let spec = ModuleSpecifier::parse("workflow:bootstrap").unwrap();
    let mod_id = runtime
        .load_main_es_module(&spec)
        .await
        .map_err(|e| format!("load workflow: {e}"))?;

    let eval = runtime.mod_evaluate(mod_id);
    runtime
        .run_event_loop(PollEventLoopOptions::default())
        .await
        .map_err(|e| format!("event loop: {e}"))?;
    eval.await.map_err(|e| format!("evaluate: {e}"))?;

    // Read the stashed result.
    let op_state = runtime.op_state();
    let slot = op_state.borrow_mut().try_take::<ResultSlot>();
    let result = match slot {
        Some(ResultSlot(Ok(v))) => Ok(v),
        Some(ResultSlot(Err(msg))) => Err(msg),
        None => Err("workflow produced no result".to_string()),
    };

    // Finalize the current step: phase() completed the earlier ones as it
    // advanced; the last one reaches its terminal state by the run's outcome.
    {
        let st = op_state.borrow();
        let es = st.borrow::<EngineState>();
        let entered = es.step_seq.load(Ordering::SeqCst) as usize;
        if let Some(last) = entered.checked_sub(1) {
            let _ = es.tx.send(AppEvent::StepStatus {
                step: last,
                status: final_step_status(result.is_ok()),
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{final_step_status, left_step, JsAgentSpec};
    use crate::event::StepStatus;

    #[test]
    fn js_agent_spec_group_roundtrip() {
        // serde silently DROPS unknown fields: if JsAgentSpec ever loses `group`,
        // prelude keeps sending it and every unit test stays green while grouping
        // vanishes at runtime. This pins the deserialization.
        let spec: JsAgentSpec = serde_json::from_value(serde_json::json!({
            "id": 1, "label": "x", "step": 0, "prompt": "p",
            "model": null, "sandbox": null, "schema": null, "cwd": null,
            "isolate": false, "timeout_ms": null, "agent_type": null,
            "group": "models.py"
        }))
        .unwrap();
        assert_eq!(spec.group.as_deref(), Some("models.py"));
        // Prelude sends null when opts.group is unset; absent must work too.
        let spec2: JsAgentSpec = serde_json::from_value(serde_json::json!({
            "id": 2, "label": "y", "step": 0, "prompt": "p",
            "model": null, "sandbox": null, "schema": null, "cwd": null,
            "isolate": false, "group": null
        }))
        .unwrap();
        assert_eq!(spec2.group, None);
    }

    #[test]
    fn step_terminal_sequence() {
        // The first phase leaves no predecessor; each later phase completes the
        // one before it (phase() indices are dense from 0, so prev == step - 1).
        assert_eq!(left_step(0), None);
        assert_eq!(left_step(1), Some(0));
        assert_eq!(left_step(2), Some(1));
        // At run end the current step is finalized by the workflow's outcome.
        assert_eq!(final_step_status(true), StepStatus::Done);
        assert_eq!(final_step_status(false), StepStatus::Failed);
    }

    #[test]
    fn prelude_is_seven_bit_ascii() {
        // deno_core requires the esm_entry_point extension source to be 7-bit
        // ASCII; a stray em-dash panics at runtime (not compile time). This guard
        // turns that into a fast unit-test failure.
        let src = include_str!("js/prelude.js");
        assert!(
            src.is_ascii(),
            "prelude.js must be 7-bit ASCII (deno_core extension constraint)"
        );
    }
}
