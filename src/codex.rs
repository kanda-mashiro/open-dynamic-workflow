//! The codex sub-agent runner: turns one `agent()` call from the JS workflow
//! into one real `codex exec --json` child process, streams its JSONL back to
//! the UI as `AgentUpdate`s, and returns the agent's final text.
//!
//! This module has NO knowledge of the JS runtime — the engine layer calls
//! `run_agent` and awaits the final string. Concurrency is bounded by a shared
//! `Semaphore` so a workflow can declare hundreds of agents while only N run.

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore};

use crate::event::{AgentId, AgentStatus, AgentUpdate, AppEvent, CodexEvent, StepId};
use crate::worktree::{WorktreeGuard, WorktreeSpec};

/// Everything one agent invocation needs. Built by the engine from a JS
/// `agent(prompt, opts)` call.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: AgentId,
    pub step: StepId,
    pub label: String,
    pub prompt: String,
    /// Working directory for `codex -C` (the agent's git worktree, if isolated).
    pub cwd: Option<String>,
    /// Model override (`-m`); None = codex config default.
    pub model: Option<String>,
    /// Sandbox policy (`-s`); None = codex config default.
    pub sandbox: Option<String>,
    /// Path to a JSON Schema file for `--output-schema` (structured output).
    pub output_schema: Option<String>,
    /// Per-agent wall-clock timeout in ms (from opts.timeoutMs); None = unbounded.
    pub timeout_ms: Option<u64>,
    /// Isolation request: the runner materializes a git worktree (and points
    /// cwd at it) only once this agent holds a concurrency permit.
    pub worktree: Option<WorktreeSpec>,
}

/// Shared handles every agent task needs. Cloned cheaply into each task.
#[derive(Clone)]
pub struct RunnerCtx {
    pub sem: Arc<Semaphore>,
    pub tx: mpsc::UnboundedSender<AppEvent>,
    /// Path to the codex binary (default "codex").
    pub codex_bin: Arc<str>,
    /// Cumulative output tokens across every agent this run (drives budget()).
    pub spent: Arc<AtomicU64>,
    /// Output-token ceiling (CODEX_FLOW_BUDGET); None = unbounded.
    pub budget_total: Option<u64>,
}

impl RunnerCtx {
    pub fn new(
        concurrency: usize,
        budget_total: Option<u64>,
        tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        // CODEX_FLOW_CODEX_BIN overrides the codex binary (used for offline TUI
        // tests with a fake codex that emits canned JSONL).
        let bin = std::env::var("CODEX_FLOW_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        Self {
            sem: Arc::new(Semaphore::new(concurrency.max(1))),
            tx,
            codex_bin: Arc::from(bin.as_str()),
            spent: Arc::new(AtomicU64::new(0)),
            budget_total,
        }
    }
}

fn send(tx: &mpsc::UnboundedSender<AppEvent>, id: AgentId, update: AgentUpdate) {
    // Errors only when the UI loop is gone (shutting down): safe to drop.
    let _ = tx.send(AppEvent::Agent { id, update });
}

/// Build the codex argv for one agent. `-a never` keeps it fully non-interactive.
fn build_args(spec: &AgentSpec) -> Vec<String> {
    let mut a = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
    ];
    if let Some(m) = &spec.model {
        a.push("-m".into());
        a.push(m.clone());
    }
    if let Some(s) = &spec.sandbox {
        a.push("-s".into());
        a.push(s.clone());
    }
    if let Some(c) = &spec.cwd {
        a.push("-C".into());
        a.push(c.clone());
    }
    if let Some(schema) = &spec.output_schema {
        a.push("--output-schema".into());
        a.push(schema.clone());
    }
    // Prompt is the trailing positional argument.
    a.push(spec.prompt.clone());
    a
}

/// Classification of one codex attempt's final text (pure, unit-tested).
#[derive(Debug, PartialEq, Eq)]
enum Usable {
    /// Usable as-is (success).
    Yes,
    /// Clean exit but no output — retry (the documented upstream-disconnect fix).
    Empty,
    /// A schema was requested but the text is not valid JSON — retry, and feed
    /// the parse error back into the next attempt's prompt.
    BadJson(String),
}

/// Decide whether an attempt's final text is usable or a retryable failure.
/// Empty/whitespace is always retryable; invalid JSON is retryable only when a
/// schema was requested (codex `--output-schema` enforces shape server-side, so
/// this is the client-side safety net the API-review P0 asked for — parse-only,
/// no full JSON-Schema validation).
fn classify_text(text: &str, has_schema: bool) -> Usable {
    if text.trim().is_empty() {
        return Usable::Empty;
    }
    if has_schema {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(text) {
            return Usable::BadJson(e.to_string());
        }
    }
    Usable::Yes
}

/// Append a steering note so the next attempt knows WHY its last output was
/// rejected. Model-facing text is English (global protocol).
fn augment_prompt(base: &str, reason: &str) -> String {
    format!(
        "{base}\n\nYour previous output was REJECTED: {reason}. \
Output ONLY a single valid JSON value matching the requested schema — \
no prose, no explanation, no markdown code fences."
    )
}

/// Resolve a per-agent timeout: explicit opts.timeoutMs wins over the global
/// CODEX_FLOW_TIMEOUT_MS; 0 or unset means no timeout. (pure, unit-tested)
fn resolve_timeout(spec_ms: Option<u64>, env_ms: Option<u64>) -> Option<Duration> {
    match spec_ms.or(env_ms) {
        Some(n) if n > 0 => Some(Duration::from_millis(n)),
        _ => None,
    }
}

fn env_timeout_ms() -> Option<u64> {
    std::env::var("CODEX_FLOW_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Output-token delta one update contributes to the budget counter (0 for any
/// non-token update). (pure, unit-tested)
pub(crate) fn budget_delta(update: &AgentUpdate) -> u64 {
    match update {
        AgentUpdate::Tokens { output, .. } => *output,
        _ => 0,
    }
}

/// Best-effort budget gate: true once a finite target is set and already met or
/// exceeded. No target (None) is never over. (pure, unit-tested)
pub(crate) fn over_budget(total: Option<u64>, spent: u64) -> bool {
    matches!(total, Some(t) if spent >= t)
}

/// Run one agent to completion. Acquires a concurrency permit first (so callers
/// can fire hundreds of these concurrently and only N proceed), spawns codex,
/// streams updates, and returns the final assistant text.
pub async fn run_agent(ctx: RunnerCtx, spec: AgentSpec) -> Result<String> {
    // Backpressure: hold a permit for the WHOLE retry sequence (an agent counts
    // as one concurrency slot regardless of how many attempts it takes).
    let _permit = ctx
        .sem
        .clone()
        .acquire_owned()
        .await
        .context("semaphore closed")?;

    let id = spec.id;
    // Post-queue budget re-check (codex M4 review #1): agents that passed the
    // engine's fast-path gate while under budget may have sat in the permit
    // queue while earlier agents exhausted it — without this, overshoot grows
    // with queue depth, not just with in-flight concurrency.
    let spent_now = ctx.spent.load(Ordering::Relaxed);
    if over_budget(ctx.budget_total, spent_now) {
        let msg = format!(
            "budget exceeded: {spent_now} output tokens spent >= total {}",
            ctx.budget_total.unwrap_or(0)
        );
        send(&ctx.tx, id, AgentUpdate::Note(format!("refused at spawn: {msg}")));
        send(&ctx.tx, id, AgentUpdate::Status(AgentStatus::Failed));
        anyhow::bail!("{msg}");
    }
    // M6: materialize the worktree only now, behind the permit — a big isolate
    // fan-out creates at most `concurrency` worktrees at a time, and an agent
    // refused above never creates one. The guard lives until this fn returns;
    // its Drop removes the worktree (repo bookkeeping included) on every path.
    let mut spec = spec;
    let _worktree = match &spec.worktree {
        Some(w) => match WorktreeGuard::add(w, id).await {
            Ok(g) => {
                // The isolated agent runs at the worktree root.
                spec.cwd = Some(g.path().to_string_lossy().into_owned());
                Some(g)
            }
            Err(e) => {
                send(&ctx.tx, id, AgentUpdate::Note(e.clone()));
                send(&ctx.tx, id, AgentUpdate::Status(AgentStatus::Failed));
                anyhow::bail!("{e}");
            }
        },
        None => None,
    };
    send(&ctx.tx, id, AgentUpdate::Status(AgentStatus::Running));

    // Retry on transient failure. The upstream relay (e.g. a proxy) drops the
    // stream on long agents, which surfaces as either a non-zero exit OR a
    // clean exit with empty output ("returned null"). Both are retried with
    // exponential backoff — this is the fix for the acceptance-report bug.
    let max_attempts: u32 = std::env::var("CODEX_FLOW_MAX_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(3);

    let timeout = resolve_timeout(spec.timeout_ms, env_timeout_ms());
    let has_schema = spec.output_schema.is_some();
    let mut last_err = String::new();
    // The prompt sent each attempt; on a schema-reject we feed the error back.
    let mut attempt_prompt = spec.prompt.clone();
    for attempt in 1..=max_attempts {
        let spec_attempt = AgentSpec {
            prompt: attempt_prompt.clone(),
            ..spec.clone()
        };
        match run_once(&ctx, &spec_attempt, timeout).await {
            Ok(text) => match classify_text(&text, has_schema) {
                Usable::Yes => {
                    send(&ctx.tx, id, AgentUpdate::Final(text.clone()));
                    send(&ctx.tx, id, AgentUpdate::Status(AgentStatus::Done));
                    return Ok(text);
                }
                Usable::Empty => {
                    last_err = "agent exited cleanly but produced no output (likely upstream stream disconnect)".into();
                }
                Usable::BadJson(e) => {
                    last_err = format!("output was not valid JSON for the requested schema: {e}");
                    attempt_prompt = augment_prompt(&spec.prompt, &last_err);
                }
            },
            Err(e) => {
                last_err = e.to_string();
            }
        }
        if attempt < max_attempts {
            // 0.8s, 1.6s, 3.2s, ... capped at 8s. Clamp the shift exponent so a
            // large CODEX_FLOW_MAX_ATTEMPTS can't overflow the shift (codex M2
            // review finding #2: `<< (attempt-1)` panics in debug once >= 64).
            let backoff_ms = (800u64 << (attempt - 1).min(13)).min(8000);
            send(
                &ctx.tx,
                id,
                AgentUpdate::Note(format!(
                    "attempt {attempt}/{max_attempts} failed: {last_err} — retrying in {backoff_ms}ms"
                )),
            );
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            send(&ctx.tx, id, AgentUpdate::Status(AgentStatus::Running));
        }
    }

    // Surface the final reason on the stream so the observability layer (M3) can
    // attribute the failure; the bail below is what reaches JS.
    send(
        &ctx.tx,
        id,
        AgentUpdate::Note(format!("failed after {max_attempts} attempts: {last_err}")),
    );
    send(&ctx.tx, id, AgentUpdate::Status(AgentStatus::Failed));
    anyhow::bail!("codex agent failed after {max_attempts} attempts: {last_err}");
}

/// One codex invocation, optionally bounded by a wall-clock timeout. On timeout
/// the inner future is dropped, which kills the child (kill_on_drop) and hands it
/// to tokio's orphan reaper — reaping is best-effort/async (not a synchronous
/// wait), but prevents lingering zombies (smoke-verified: a timed-out agent
/// leaves no live child). A timeout is reported as a retryable failure.
async fn run_once(ctx: &RunnerCtx, spec: &AgentSpec, timeout: Option<Duration>) -> Result<String> {
    match timeout {
        Some(dur) => match tokio::time::timeout(dur, run_once_inner(ctx, spec)).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!("codex timed out after {}ms", dur.as_millis()),
        },
        None => run_once_inner(ctx, spec).await,
    }
}

/// The actual codex invocation. Returns the final assistant text (may be empty
/// if the stream disconnected before a final message — the caller treats empty
/// as a retryable failure).
async fn run_once_inner(ctx: &RunnerCtx, spec: &AgentSpec) -> Result<String> {
    let id = spec.id;
    let args = build_args(spec);
    let mut child = Command::new(&*ctx.codex_bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", ctx.codex_bin))?;

    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    // Drain stderr in the background into Notes (codex streams progress here).
    let stderr_task = {
        let tx = ctx.tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    send(&tx, id, AgentUpdate::Note(line));
                }
            }
        })
    };

    // Parse stdout JSONL; the last assistant message is the result.
    let mut final_text = String::new();
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match CodexEvent::parse_line(&line) {
                None => {}
                Some(Err(e)) => {
                    send(&ctx.tx, id, AgentUpdate::Note(format!("unparsed line ({e}): {line}")))
                }
                Some(Ok(ev)) => {
                    for up in ev.into_updates() {
                        if let AgentUpdate::Message(t) = &up {
                            final_text = t.clone();
                        }
                        // Accumulate output tokens here (runner side) to dodge a
                        // cross-await OpState re-borrow in the engine op.
                        let d = budget_delta(&up);
                        if d > 0 {
                            ctx.spent.fetch_add(d, Ordering::Relaxed);
                        }
                        send(&ctx.tx, id, up);
                    }
                }
            },
            Ok(None) => break,
            Err(e) => {
                send(&ctx.tx, id, AgentUpdate::Note(format!("stdout read error: {e}")));
                break;
            }
        }
    }

    let status = child.wait().await.context("waiting on codex")?;
    stderr_task.abort();
    if status.success() {
        Ok(final_text)
    } else {
        anyhow::bail!("codex exited with status {status}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_no_schema_nonempty_is_usable() {
        assert_eq!(classify_text("hello", false), Usable::Yes);
        // Without a schema, non-JSON text is still a valid (string) result.
        assert_eq!(classify_text("not json at all", false), Usable::Yes);
    }

    #[test]
    fn classify_empty_or_whitespace_is_retryable() {
        assert_eq!(classify_text("   \n\t ", false), Usable::Empty);
        assert_eq!(classify_text("", true), Usable::Empty);
    }

    #[test]
    fn classify_schema_requires_valid_json() {
        assert_eq!(classify_text("{\"a\":1}", true), Usable::Yes);
        match classify_text("{ not json", true) {
            Usable::BadJson(_) => {}
            other => panic!("expected BadJson, got {other:?}"),
        }
    }

    #[test]
    fn augment_prompt_appends_rejection_reason() {
        let p = augment_prompt("do the task", "expected value at line 1");
        assert!(p.starts_with("do the task"));
        assert!(p.contains("REJECTED"));
        assert!(p.contains("expected value at line 1"));
        assert!(p.contains("JSON"));
    }

    #[test]
    fn resolve_timeout_spec_over_env_zero_disables() {
        assert_eq!(
            resolve_timeout(Some(500), Some(999)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            resolve_timeout(None, Some(999)),
            Some(Duration::from_millis(999))
        );
        assert_eq!(resolve_timeout(None, None), None);
        assert_eq!(
            resolve_timeout(Some(0), Some(999)),
            None,
            "explicit 0 disables even if env is set"
        );
    }

    #[test]
    fn build_args_order_schema_and_trailing_prompt() {
        let spec = AgentSpec {
            id: 1,
            step: 0,
            label: "x".into(),
            prompt: "hi".into(),
            cwd: None,
            model: Some("gpt".into()),
            sandbox: Some("read-only".into()),
            output_schema: Some("/tmp/s.json".into()),
            timeout_ms: None,
            worktree: None,
        };
        let a = build_args(&spec);
        assert_eq!(a[0], "exec");
        assert!(a.contains(&"--json".to_string()));
        assert!(a.contains(&"--output-schema".to_string()));
        assert_eq!(a.last().unwrap(), "hi", "prompt is the trailing positional");
        assert!(a.windows(2).any(|w| w[0] == "-m" && w[1] == "gpt"));
        assert!(a.windows(2).any(|w| w[0] == "-s" && w[1] == "read-only"));
    }

    #[test]
    fn budget_delta_counts_output_tokens_only() {
        assert_eq!(
            budget_delta(&AgentUpdate::Tokens { input: 100, output: 42 }),
            42
        );
        assert_eq!(budget_delta(&AgentUpdate::Message("hi".into())), 0);
        // Summing a synthetic stream accumulates only the output tokens.
        let stream = [
            AgentUpdate::Tokens { input: 1, output: 10 },
            AgentUpdate::Message("x".into()),
            AgentUpdate::Tokens { input: 1, output: 5 },
        ];
        let total: u64 = stream.iter().map(budget_delta).sum();
        assert_eq!(total, 15);
    }

    #[test]
    fn over_budget_only_when_finite_and_met() {
        assert!(!over_budget(None, 10_000), "no target is never over");
        assert!(!over_budget(Some(100), 99));
        assert!(over_budget(Some(100), 100), "met counts as over");
        assert!(over_budget(Some(100), 101));
    }
}
