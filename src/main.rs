//! codex-flow entrypoint.
//!
//!   codex-flow [--tui] <workflow.js> [args-json]
//!
//! Default: stream events to stdout (good for logs/CI).
//! --tui  : launch the two-pane ratatui UI (steps | agents | scrollable detail).
//!
//! deno_core's JsRuntime is !Send, so the engine runs on its own OS thread with
//! a current-thread tokio runtime; it streams AppEvents over an mpsc to whoever
//! consumes them (stdout printer, or the TUI on the main thread).

use std::path::PathBuf;
use std::thread;

use codex_flow::engine::run_workflow;
use codex_flow::event::{AgentUpdate, AppEvent};
use tokio::sync::mpsc;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Hidden bench subcommand: `codex-flow bench [agents] [events_per_agent]`.
    // Measures render cost (old per-event draw vs new coalesced draw) on a
    // deterministic load via ratatui's headless TestBackend.
    if args.first().map(|s| s.as_str()) == Some("bench") {
        let agents = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(16);
        let per = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
        codex_flow::tui::bench::run(agents, per);
        return Ok(());
    }

    let tui_mode = args.iter().any(|a| a == "--tui");
    args.retain(|a| a != "--tui");

    // `--concurrency N`: take the value, then drop both tokens so the workflow /
    // args positionals don't shift.
    let mut concurrency_flag: Option<usize> = None;
    if let Some(i) = args.iter().position(|a| a == "--concurrency") {
        // Consume the next token as the value ONLY if it parses as a number;
        // otherwise drop just the flag and keep positionals intact (so
        // `--concurrency myflow.js` doesn't eat the workflow path — codex #3).
        match args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
            Some(n) => {
                concurrency_flag = Some(n);
                args.remove(i + 1);
                args.remove(i);
            }
            None => {
                args.remove(i);
            }
        }
    }
    // `--resume <run_id>`: reuse a previous run's journal so unchanged agent()
    // calls replay instantly. The guard keeps a missing value from eating the
    // workflow positional (same trap as --concurrency).
    let mut resume: Option<String> = None;
    if let Some(i) = args.iter().position(|a| a == "--resume") {
        match args.get(i + 1) {
            Some(v) if !v.starts_with('-') && !v.ends_with(".js") => {
                resume = Some(v.clone());
                args.remove(i + 1);
                args.remove(i);
            }
            _ => {
                eprintln!("--resume requires a run id: codex-flow --resume <run_id> <workflow.js>");
                std::process::exit(2);
            }
        }
    }

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let concurrency = codex_flow::cli::resolve_concurrency(
        std::env::var("CODEX_FLOW_CONCURRENCY").ok(),
        concurrency_flag,
        cores,
    );

    let workflow = args
        .first()
        .cloned()
        .unwrap_or_else(|| "examples/hello.workflow.js".to_string());
    let args_json = match codex_flow::cli::parse_args_json(args.get(1).map(|s| s.as_str())) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let run_id = resume.clone().unwrap_or_else(codex_flow::journal::new_run_id);
    // To stderr: RESULT-grepping consumers only watch stdout.
    eprintln!(
        "== run {run_id}{}  (resume: codex-flow --resume {run_id} {workflow})",
        if resume.is_some() { " [resumed]" } else { "" }
    );

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();

    // Engine runs on its own thread (JsRuntime is !Send).
    let engine = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio current-thread");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let result =
                run_workflow(PathBuf::from(&workflow), args_json, concurrency, run_id, tx.clone())
                    .await;
            // Signal completion to the consumer, then drop tx to close the channel.
            let _ = tx.send(match &result {
                Ok(v) => AppEvent::WorkflowDone(Ok(v.to_string())),
                Err(e) => AppEvent::WorkflowDone(Err(e.clone())),
            });
            result
        })
    });

    if tui_mode {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let final_result = rt.block_on(codex_flow::tui::run_tui(rx))?;
        let _ = engine.join();
        match final_result {
            Some(Ok(v)) => println!("RESULT: {v}"),
            Some(Err(e)) => println!("ERROR: {e}"),
            None => {}
        }
    } else {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            let mut rx = rx;
            let mut failures = codex_flow::cli::FailureTracker::new();
            while let Some(ev) = rx.recv().await {
                failures.observe(&ev);
                match ev {
                    AppEvent::StepDeclared { step, title } => eprintln!("== step[{step}] {title}"),
                    AppEvent::StepStatus { step, status } => {
                        eprintln!("== step[{step}] -> {status:?}")
                    }
                    AppEvent::AgentSpawned { id, label, group, .. } => match group {
                        Some(g) => eprintln!("-- agent[{id}] ({g}) spawned: {label}"),
                        None => eprintln!("-- agent[{id}] spawned: {label}"),
                    },
                    AppEvent::Agent { id, update } => match update {
                        AgentUpdate::Final(t) => eprintln!("-- agent[{id}] FINAL: {t}"),
                        AgentUpdate::Note(n) => eprintln!("-- agent[{id}] note: {n}"),
                        other => eprintln!("-- agent[{id}] {other:?}"),
                    },
                    AppEvent::WorkflowDone(Ok(v)) => println!("RESULT: {v}"),
                    AppEvent::WorkflowDone(Err(e)) => println!("ERROR: {e}"),
                    AppEvent::EngineError(e) => eprintln!("!! engine error: {e}"),
                    AppEvent::RunMeta { name, phases } => {
                        eprintln!("== workflow: {name} [phases: {}]", phases.join(", "))
                    }
                    AppEvent::Key(_) | AppEvent::Render => {}
                }
            }
            if let Some(s) = codex_flow::cli::format_failures(&failures.failures()) {
                eprintln!("{s}");
            }
        });
        let _ = engine.join();
    }

    Ok(())
}
