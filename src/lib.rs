//! codex-flow: a Claude-Code-style dynamic-workflow engine where the workflow
//! is authored in JS (executed in an embedded deno_core V8 runtime) and each
//! `agent()` call is run by a real `codex exec` sub-process, surfaced live in a
//! ratatui TUI.

pub mod cli;
pub mod codex;
pub mod engine;
pub mod event;
pub mod journal;
pub mod registry;
pub mod render_opt;
pub mod tui;
pub mod worktree;
