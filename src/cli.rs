//! CLI-side pure helpers: concurrency resolution, args-JSON parsing, and
//! run-failure aggregation. Kept in the lib (not main.rs) so they are unit-tested
//! by `cargo test --lib` and reused by both the stdout and TUI front-ends.

use std::collections::HashMap;

use crate::event::{AgentStatus, AgentUpdate, AppEvent};

/// Resolve the agent concurrency cap. Precedence: explicit env
/// `CODEX_FLOW_CONCURRENCY` > `--concurrency N` flag > default `min(16, cores-2)`
/// (clamped to >=1). Explicit values are honored VERBATIM (not re-clamped to 16);
/// only the default is bounded by 16 — opting in to high fan-out is allowed.
pub fn resolve_concurrency(env: Option<String>, flag: Option<usize>, cores: usize) -> usize {
    if let Some(n) = env
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
    {
        return n;
    }
    if let Some(n) = flag.filter(|&n| n >= 1) {
        return n;
    }
    cores.saturating_sub(2).clamp(1, 16)
}

/// Parse the optional second positional arg as JSON. Missing => Null (a workflow
/// may legitimately take no args); present-but-invalid => Err, surfaced to the
/// user instead of the old silent `null` that hid typos.
pub fn parse_args_json(s: Option<&str>) -> Result<serde_json::Value, String> {
    match s {
        None => Ok(serde_json::Value::Null),
        Some(s) => serde_json::from_str(s).map_err(|e| format!("invalid args JSON: {e}")),
    }
}

/// Aggregates per-agent failures from the event stream so a run can print one
/// consolidated FAILURES summary (parity with Claude Code's failures list).
#[derive(Default)]
pub struct FailureTracker {
    labels: HashMap<u64, String>,
    last_note: HashMap<u64, String>,
    failed: Vec<u64>,
}

impl FailureTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event: record labels (from spawn), the most recent note (the
    /// best-effort error reason), and which agents reached Failed.
    pub fn observe(&mut self, ev: &AppEvent) {
        match ev {
            AppEvent::AgentSpawned {
                id, label, group, ..
            } => {
                // Display identity folds the nesting group in up front (R7.5), so
                // failures()/format_failures stay group-agnostic.
                let display = match group {
                    Some(g) => format!("{g}/{label}"),
                    None => label.clone(),
                };
                self.labels.insert(*id, display);
            }
            AppEvent::Agent {
                id,
                update: AgentUpdate::Note(n),
            } => {
                self.last_note.insert(*id, n.clone());
            }
            AppEvent::Agent {
                id,
                update: AgentUpdate::Status(AgentStatus::Failed),
            } => {
                if !self.failed.contains(id) {
                    self.failed.push(*id);
                }
            }
            // A retry can recover: codex may stream a transient turn.failed
            // (-> Status(Failed)) yet the runner retries and ultimately succeeds
            // (-> Status(Done)). Drop recovered agents so a succeeded agent is
            // never listed (codex M3 review finding #1).
            AppEvent::Agent {
                id,
                update: AgentUpdate::Status(AgentStatus::Done),
            } => {
                self.failed.retain(|x| x != id);
            }
            _ => {}
        }
    }

    /// (label, best-effort error reason) per failed agent, in failure order.
    pub fn failures(&self) -> Vec<(String, Option<String>)> {
        self.failed
            .iter()
            .map(|id| {
                let label = self
                    .labels
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("agent-{id}"));
                (label, self.last_note.get(id).cloned())
            })
            .collect()
    }
}

/// Render a FAILURES block, or None if there were no failures. Pure (testable).
pub fn format_failures(items: &[(String, Option<String>)]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = format!("FAILURES ({}):", items.len());
    for (label, err) in items {
        match err {
            Some(e) => out.push_str(&format!("\n  - {label}: {e}")),
            None => out.push_str(&format!("\n  - {label}")),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_env_over_flag_over_default() {
        assert_eq!(
            resolve_concurrency(Some("32".into()), Some(4), 8),
            32,
            "explicit env honored verbatim (not clamped to 16)"
        );
        assert_eq!(resolve_concurrency(None, Some(4), 8), 4, "flag over default");
        assert_eq!(resolve_concurrency(None, None, 8), 6, "default = cores-2");
        assert_eq!(resolve_concurrency(None, None, 64), 16, "default clamped to 16");
        assert_eq!(resolve_concurrency(None, None, 2), 1, "default clamped to >=1");
        assert_eq!(
            resolve_concurrency(Some("0".into()), Some(4), 8),
            4,
            "env 0 is invalid -> fall through to flag"
        );
        assert_eq!(
            resolve_concurrency(Some("x".into()), None, 8),
            6,
            "env garbage -> default"
        );
    }

    #[test]
    fn args_json_missing_valid_invalid() {
        assert_eq!(parse_args_json(None).unwrap(), serde_json::Value::Null);
        assert_eq!(
            parse_args_json(Some("{\"a\":1}")).unwrap(),
            serde_json::json!({"a":1})
        );
        assert!(parse_args_json(Some("{bad")).is_err());
    }

    #[test]
    fn format_failures_empty_is_none_else_block() {
        assert!(format_failures(&[]).is_none());
        let s = format_failures(&[
            ("a".into(), Some("boom".into())),
            ("b".into(), None),
        ])
        .unwrap();
        assert!(s.contains("FAILURES (2)"));
        assert!(s.contains("a: boom"));
        assert!(s.contains("- b"));
    }

    #[test]
    fn tracker_failure_carries_group_prefix() {
        // R7.5: a failed agent that belongs to a nested-workflow group is listed
        // as group/label so the summary attributes it to its child workflow.
        let mut t = FailureTracker::new();
        t.observe(&AppEvent::AgentSpawned {
            id: 7,
            step: 1,
            label: "build".into(),
            prompt: "p".into(),
            group: Some("models.py".into()),
        });
        t.observe(&AppEvent::Agent {
            id: 7,
            update: AgentUpdate::Status(AgentStatus::Failed),
        });
        assert_eq!(t.failures()[0].0, "models.py/build");
    }

    #[test]
    fn tracker_records_label_note_and_failed_only() {
        let mut t = FailureTracker::new();
        t.observe(&AppEvent::AgentSpawned {
            id: 1,
            step: 0,
            label: "alpha".into(),
            prompt: "p".into(),
            group: None,
        });
        t.observe(&AppEvent::Agent {
            id: 1,
            update: AgentUpdate::Note("boom reason".into()),
        });
        t.observe(&AppEvent::Agent {
            id: 1,
            update: AgentUpdate::Status(AgentStatus::Failed),
        });
        // A succeeding agent must NOT appear in failures.
        t.observe(&AppEvent::AgentSpawned {
            id: 2,
            step: 0,
            label: "beta".into(),
            prompt: "p".into(),
            group: None,
        });
        t.observe(&AppEvent::Agent {
            id: 2,
            update: AgentUpdate::Status(AgentStatus::Done),
        });
        let f = t.failures();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, "alpha");
        assert_eq!(f[0].1.as_deref(), Some("boom reason"));
    }

    #[test]
    fn tracker_recovered_agent_not_listed() {
        // turn.failed passthrough marks Failed, but a retry ultimately succeeds.
        let mut t = FailureTracker::new();
        t.observe(&AppEvent::AgentSpawned {
            id: 1,
            step: 0,
            label: "x".into(),
            prompt: "p".into(),
            group: None,
        });
        t.observe(&AppEvent::Agent {
            id: 1,
            update: AgentUpdate::Status(AgentStatus::Failed),
        });
        t.observe(&AppEvent::Agent {
            id: 1,
            update: AgentUpdate::Status(AgentStatus::Done),
        });
        assert!(t.failures().is_empty(), "recovered agent must not be listed");
    }
}
