//! Event model shared by the JS engine, the codex runner, and the TUI.
//!
//! Three layers feed one another:
//!   * `codex exec --json` emits raw JSONL (`CodexEvent`) on a child's stdout.
//!   * the runner parses those into normalized `AgentUpdate`s.
//!   * everything reaches the single render loop as an `AppEvent` over an mpsc.

use serde::Deserialize;

/// Stable identifier for one spawned agent (one `codex exec` process).
pub type AgentId = u64;
/// Index of a workflow step (phase) in declaration order.
pub type StepId = usize;

/// Everything the render loop consumes. The UI thread owns all state, so every
/// mutation arrives as one of these and is applied serially — no locking.
/// `Clone` is used by the bench harness to replay a deterministic event load.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A terminal key press (from crossterm's EventStream).
    Key(ratatui::crossterm::event::KeyEvent),
    /// Periodic redraw tick.
    Render,
    /// The JS workflow declared a step/phase (title shown in the left pane).
    StepDeclared { step: StepId, title: String },
    /// A step changed lifecycle state.
    StepStatus { step: StepId, status: StepStatus },
    /// The workflow's `export const meta`, surfaced once before it runs so the UI
    /// can pre-draw the phase skeleton (Pending) and show the workflow name.
    RunMeta { name: String, phases: Vec<String> },
    /// The JS workflow asked to spawn an agent; the UI should show it pending.
    AgentSpawned {
        id: AgentId,
        step: StepId,
        label: String,
        prompt: String,
        /// One-level nesting: the child-workflow group this agent belongs to
        /// (cosmetic — grouping/rollup in the UI, excluded from the journal key).
        group: Option<String>,
    },
    /// Streamed progress from a running agent.
    Agent { id: AgentId, update: AgentUpdate },
    /// The whole workflow finished (Ok = final JSON result, Err = message).
    WorkflowDone(Result<String, String>),
    /// Fatal engine/runtime error to surface before exit.
    EngineError(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Pending,
    Running,
    Done,
    Failed,
}

/// One normalized progress item for an agent. The TUI appends most of these to
/// the agent's scrollable detail log; a few also drive status/token columns.
#[derive(Debug, Clone)]
pub enum AgentUpdate {
    Status(AgentStatus),
    /// Model reasoning summary text.
    Reasoning(String),
    /// A shell command the agent ran (with optional captured output).
    Command { command: String, output: Option<String> },
    /// A file the agent created/edited.
    FileChange(String),
    /// An MCP tool call.
    ToolCall(String),
    /// A web search the agent performed.
    WebSearch(String),
    /// The agent's (possibly interim) assistant message text.
    Message(String),
    /// Cumulative token usage reported at turn end.
    Tokens { input: u64, output: u64 },
    /// The agent's final response payload (string; JSON if a schema was set).
    Final(String),
    /// A non-fatal note from the runner (parse warning, spawn detail, etc.).
    Note(String),
}

// ── Raw codex JSONL shapes (codex-cli 0.135 / SDK 0.136, June 2026) ──
//
// `codex exec --json` emits one JSON object per line. We deserialize loosely:
// unknown event/item types are tolerated (Other) so a codex upgrade that adds
// variants degrades to a Note instead of crashing the stream.

/// Top-level event line. Tag field is `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum CodexEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: Option<String> },
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: Option<Usage> },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: Option<serde_json::Value> },
    #[serde(rename = "item.started")]
    ItemStarted { item: Item },
    #[serde(rename = "item.updated")]
    ItemUpdated { item: Item },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: Item },
    #[serde(rename = "error")]
    Error { message: Option<String> },
    /// Forward-compatible catch-all for codex versions that add event types.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// The `item` object carried by item.* events. Tag field is `type`.
/// Codex renamed `assistant_message` → `agent_message` at v0.44; we accept both.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum Item {
    #[serde(rename = "agent_message", alias = "assistant_message")]
    AgentMessage {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "command_execution")]
    CommandExecution {
        #[serde(default)]
        command: String,
        #[serde(default)]
        aggregated_output: String,
    },
    #[serde(rename = "file_change")]
    FileChange {
        #[serde(default)]
        changes: serde_json::Value,
    },
    #[serde(rename = "mcp_tool_call")]
    McpToolCall {
        #[serde(default)]
        server: String,
        #[serde(default)]
        tool: String,
    },
    #[serde(rename = "web_search")]
    WebSearch {
        #[serde(default)]
        query: String,
    },
    #[serde(other)]
    Other,
}

impl CodexEvent {
    /// Parse one JSONL line. `None` for blank lines; `Err` for malformed JSON
    /// (the runner turns that into a `Note` rather than aborting the stream).
    pub fn parse_line(line: &str) -> Option<Result<CodexEvent, serde_json::Error>> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        Some(serde_json::from_str::<CodexEvent>(line))
    }

    /// Collapse a raw codex event into zero or more normalized agent updates.
    pub fn into_updates(self) -> Vec<AgentUpdate> {
        match self {
            CodexEvent::ThreadStarted { .. } | CodexEvent::TurnStarted {} | CodexEvent::Other => {
                vec![]
            }
            CodexEvent::TurnCompleted { usage } => usage
                .map(|u| {
                    vec![AgentUpdate::Tokens {
                        input: u.input_tokens,
                        output: u.output_tokens,
                    }]
                })
                .unwrap_or_default(),
            CodexEvent::TurnFailed { error } => vec![
                AgentUpdate::Note(format!(
                    "turn failed: {}",
                    error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unknown".into())
                )),
                AgentUpdate::Status(AgentStatus::Failed),
            ],
            CodexEvent::Error { message } => vec![AgentUpdate::Note(format!(
                "error: {}",
                message.unwrap_or_else(|| "unknown".into())
            ))],
            // We treat completed items as the canonical record; started/updated
            // for streamed-output item kinds are folded in too for liveness.
            CodexEvent::ItemStarted { item }
            | CodexEvent::ItemUpdated { item }
            | CodexEvent::ItemCompleted { item } => item.into_updates(),
        }
    }
}

impl Item {
    fn into_updates(self) -> Vec<AgentUpdate> {
        match self {
            Item::AgentMessage { text } if !text.is_empty() => vec![AgentUpdate::Message(text)],
            Item::Reasoning { text } if !text.is_empty() => vec![AgentUpdate::Reasoning(text)],
            Item::CommandExecution {
                command,
                aggregated_output,
            } => vec![AgentUpdate::Command {
                command,
                output: (!aggregated_output.is_empty()).then_some(aggregated_output),
            }],
            Item::FileChange { changes } => vec![AgentUpdate::FileChange(changes.to_string())],
            Item::McpToolCall { server, tool } => {
                vec![AgentUpdate::ToolCall(format!("{server}/{tool}"))]
            }
            Item::WebSearch { query } => vec![AgentUpdate::WebSearch(query)],
            _ => vec![],
        }
    }
}
