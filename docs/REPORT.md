# codex-flow 项目报告

## TL;DR

- codex-flow 是一个 package 名为 `codex-flow` 的 single Rust binary，版本 `0.1.0`、edition `2024`，用 embedded `deno_core` 执行 JavaScript workflow，并把每个 `agent()` 调度到真实 `codex exec --json` 子进程。（`Cargo.toml`; `src/main.rs`; `src/engine.rs`; `src/codex.rs`）
- 项目的核心设计不变量是“编排在代码、不在模型”：JavaScript workflow 决定流程，非确定性只留在 `agent()` 边界内。（`SPEC.md`; `src/js/prelude.js`）
- CLI 形态是 `codex-flow [--tui] <workflow.js> [args-json]`，默认输出 stdout/stderr 事件，带 `--tui` 时进入 ratatui 前端。（`src/main.rs`）
- runtime 由独立 OS thread 承载 `JsRuntime`，主线程只消费 `AppEvent`；engine 和 TUI 之间通过 `mpsc::unbounded_channel` 解耦。（`src/main.rs`; `src/tui.rs`）
- DSL 暴露 `agent`、`parallel`、`pipeline`、`phase`、`log`、`args`、`budget`；这些不是 npm API，而是 `src/js/prelude.js` 安装到 `globalThis` 的 host globals。（`src/js/prelude.js`; `src/engine.rs`）
- agent runner 固定调用 `codex exec --json --skip-git-repo-check`，并按 opts 追加 `-m`、`-s`、`-C`、`--output-schema`。（`src/codex.rs`）
- 并发上限优先级是 `CODEX_FLOW_CONCURRENCY` env > `--concurrency N` flag > 默认 `min(16, cores-2)`，默认值至少 clamp 到 `1`。（`src/cli.rs`; `SPEC.md`）
- retry 默认最多 `3` 次，backoff 从 `800ms` 指数增长并 capped at `8000ms`；`CODEX_FLOW_MAX_ATTEMPTS` 可覆盖。（`src/codex.rs`）
- resume 由 journal 实现：成功 agent 结果写入 `runs/<run_id>.jsonl` 或 `CODEX_FLOW_RUNS_DIR`，key 是固定字段序的 `64-bit FNV-1a`，并用 occurrence index 区分相同调用。（`src/journal.rs`; `src/engine.rs`; `SPEC.md`）
- budget 只统计 output tokens，`CODEX_FLOW_BUDGET` 未设或为 `0` 表示无限；journal replay 免费，不消耗本进程 live budget。（`src/codex.rs`; `src/engine.rs`; `src/js/prelude.js`; `SPEC.md`）
- TUI 的性能优化从逐事件 redraw 改成 `16ms` frame coalescing + key-immediate draw，并用 `PaneCache`、`WrapCache`、512 event drain、256 key drain 控制高频输出和滚动风暴。（`src/render_opt.rs`; `src/tui.rs`; `SPEC.md`）
- release bench 显示 `codex-flow bench 16 200` 从 `3218` 次 draw 降到 `202` 次，渲染时间 `423.84ms -> 25.92ms`；`104 x 50` 负载从 `5306` 次 draw 降到 `52` 次，`890.03ms -> 8.58ms`。（`codex-flow bench`; `src/tui.rs`）
- 400-key scroll freeze 从旧记录 `35.9s` 降到回归测试约 `521.21075ms`，也就是从十秒级冻结降到半秒级。（`SPEC.md`; `src/tui.rs`; `cargo test`）
- 开发方法论是 spec-driven + TDD + read-only plan workflow + per-module review：`SPEC.md` 是唯一事实源，测试先把 bug 固化，再让实现变绿。（`SPEC.md`; `RENDER_OPT_SPEC.md`; `src/`）

## 目录

- [第 1 章：概览与架构](#第-1-章概览与架构)
- [第 2 章：Workflow DSL](#第-2-章workflow-dsl)
- [第 3 章：可靠性机制](#第-3-章可靠性机制)
- [第 4 章：Performance & TUI：从逐事件重绘到可交互的 60fps](#第-4-章performance--tui从逐事件重绘到可交互的-60fps)
- [第 5 章：开发方法论复盘](#第-5-章开发方法论复盘)

## 第 1 章：概览与架构

本章保留架构总览和模块边界。DSL 作者语义的完整版本见[第 2 章](#第-2-章workflow-dsl)，retry、journal、budget、worktree 和 registry 的可靠性细节见[第 3 章](#第-3-章可靠性机制)，TUI 性能路径见[第 4 章](#第-4-章performance--tui从逐事件重绘到可交互的-60fps)。

codex-flow 是一个以 package 名 `codex-flow` 产出的 single Rust binary，版本 `0.1.0`、edition `2024`，依赖中直接包含 `deno_core = "0.403"`、`tokio = "1.52.3"`、`ratatui = "0.29.0"`、`crossterm = "0.28"` 和 `serde_json = "1.0.150"`。（`Cargo.toml`; `src/main.rs`）

从入口形态看，它作为一个命令运行：`codex-flow [--tui] <workflow.js> [args-json]`，默认把事件打印到 stdout/stderr，带 `--tui` 时进入 ratatui UI。（`src/main.rs`）

从 crate 边界看，`src/lib.rs` 把 `cli`、`codex`、`engine`、`event`、`journal`、`registry`、`render_opt`、`tui`、`worktree` 作为公开模块导出，因此主程序只是这些模块的装配点。（`src/lib.rs`）

项目目标不是让模型自己决定编排，而是让 JS 脚本决定下一步执行什么，非确定性只留在 `agent()` 边界内；这是 SPEC 的第一条设计不变量。（`SPEC.md`）

核心执行路径是：用户提供 ESM `workflow.js`，Rust 在 embedded `deno_core` / V8 `JsRuntime` 中加载它，并通过 ops 暴露 `agent`、`parallel`、`pipeline`、`phase`、`log`、`args`、`budget` 这些 DSL 能力。（`src/engine.rs`; `src/js/prelude.js`）

每一次 JS `agent(prompt, opts)` 都会被 `op_agent` 反序列化成 `JsAgentSpec`，再构造成 `AgentSpec`，最终调用 runner 的 `run_agent`。（`src/engine.rs`; `src/codex.rs`）

runner 不模拟 agent，而是通过 `Command::new(codex_bin)` 启动真实子进程，argv 固定以 `exec --json --skip-git-repo-check` 开头，并按需追加 `-m`、`-s`、`-C`、`--output-schema`，最后把 prompt 作为尾部 positional 参数。（`src/codex.rs`）

因此，本项目的 agent 语义是“JS workflow 调度真实 `codex exec --json` subprocess”，不是内置 LLM 客户端，也不是纯 Rust mock runner。（`src/codex.rs`; `src/engine.rs`）

主线程和 engine 的关系由 `JsRuntime` 的线程约束决定：`main.rs` 明确说明 `deno_core::JsRuntime` 是 `!Send`，所以 engine 必须运行在自己的 OS thread 上。（`src/main.rs`）

`main.rs` 通过 `thread::spawn` 启动 engine，并在那个线程里创建 `tokio::runtime::Builder::new_current_thread()` 和 `tokio::task::LocalSet`，再调用 `run_workflow(...)`。（`src/main.rs`）

这意味着 V8 event loop、deno ops 和 `run_workflow` 都留在 engine 所在的 current-thread runtime 内，避免把 `JsRuntime` 或 `OpState` 跨线程移动。（`src/main.rs`; `src/engine.rs`）

engine 与消费者之间只共享事件流：`main.rs` 创建一个 `mpsc::unbounded_channel::<AppEvent>()`，把 `tx` 交给 `run_workflow`，把 `rx` 交给 TUI 或 stdout printer。（`src/main.rs`）

TUI 自己也说明 engine 在独立 OS thread 上运行，并通过 mpsc 把 `AppEvent` 推给 UI；UI loop 拥有全部状态，所以不需要锁。（`src/tui.rs`）

事件模型分三层，第一层是 raw `CodexEvent`，它对应 `codex exec --json` 在子进程 stdout 上逐行输出的 JSONL。（`src/event.rs`; `src/codex.rs`）

`CodexEvent` 以 `type` 字段做 serde tag，当前解析 `thread.started`、`turn.started`、`turn.completed`、`turn.failed`、`item.started`、`item.updated`、`item.completed`、`error`，并用 `Other` 容忍未来版本新增事件。（`src/event.rs`）

`event.rs` 注释把 raw 格式锚定到 `codex-cli 0.135 / SDK 0.136, June 2026`，并说明未知 event/item 类型会降级成可忽略或 Note，而不是让 stream 崩溃。（`src/event.rs`）

`CodexEvent::parse_line` 对空行返回 `None`，对 malformed JSON 返回 `Err`，runner 再把解析失败变成 `AgentUpdate::Note`。（`src/event.rs`; `src/codex.rs`）

第二层是 normalized `AgentUpdate`，它把 raw Codex JSONL 折叠成 UI 和 failure tracker 能理解的 agent 进度项。（`src/event.rs`）

`AgentUpdate` 包含 `Status`、`Reasoning`、`Command`、`FileChange`、`ToolCall`、`WebSearch`、`Message`、`Tokens`、`Final`、`Note` 等变体；其中 `Tokens { input, output }` 来自 `turn.completed.usage`。（`src/event.rs`）

`Item::AgentMessage` 同时接受 `agent_message` 和旧名 `assistant_message`，说明事件层显式处理了 Codex 命名迁移。（`src/event.rs`）

runner 在读取 stdout JSONL 时把每个 `CodexEvent` 转成 `AgentUpdate`，如果遇到 `AgentUpdate::Message` 就更新 `final_text`，如果遇到 token update 就把 output token 累加到 `RunnerCtx.spent`。（`src/codex.rs`）

第三层是 application-level `AppEvent`，这是 TUI render loop 和 stdout printer 唯一消费的事件枚举。（`src/event.rs`; `src/main.rs`; `src/tui.rs`）

`AppEvent` 覆盖 workflow 结构事件 `StepDeclared`、`StepStatus`、`RunMeta`，agent 生命周期事件 `AgentSpawned` 和 `Agent { id, update }`，终态事件 `WorkflowDone`、`EngineError`，以及 UI 内部的 `Key`、`Render`。（`src/event.rs`）

`engine.rs` 的 `op_phase` 会发送 `StepDeclared` 与 `StepStatus::Running`，进入新 phase 时还会把前一个 step 标成 `Done`。（`src/engine.rs`）

`engine.rs` 的 `op_meta` 会发送 `RunMeta { name, phases }`，让 UI 在 workflow 真正运行前预画 phase skeleton。（`src/engine.rs`; `src/tui.rs`）

`main.rs` 在 engine 返回后发送 `AppEvent::WorkflowDone(Ok(v.to_string()))` 或 `WorkflowDone(Err(e.clone()))`，然后 drop sender 关闭 channel。（`src/main.rs`）

JS runtime 的加载结构由 `WorkflowLoader` 和 bootstrap 组成：`workflow:bootstrap` 是虚拟模块，`workflow:main` 指向用户 workflow 文件。（`src/engine.rs`）

`WorkflowLoader::resolve` 对 `workflow:main`、`workflow:bootstrap` 和用户文件相对 import 分别处理；相对 import 会以 workflow 文件路径作为 base。（`src/engine.rs`）

bootstrap 会先读取 `export const meta`，把 `name` 和 `phases` 规范化后调用 `globalThis.__meta`，再寻找 `default`、`run` 或 `workflow` 作为入口。（`src/engine.rs`）

入口可以是函数、top-level promise 或非 `undefined` 值；如果没有入口，bootstrap 抛出 `workflow.js must export default...` 这类错误，并通过 `__setError` 写回 Rust `OpState`。（`src/engine.rs`）

`run_workflow` 的 deno 执行顺序是 `load_main_es_module`、`mod_evaluate`、`run_event_loop`、await eval，最后从 `ResultSlot` 取结果。（`src/engine.rs`）

DSL 层由 `src/js/prelude.js` 安装到 `globalThis`：`agent` 负责构造 spec，`parallel` 接收 thunk array 并把单个失败折叠成 `null`，`pipeline` 让每个 item 独立穿过多个 stage。完整 DSL 规则见[第 2 章](#第-2-章workflow-dsl)。（`src/js/prelude.js`）

非 schema agent 的 prompt 会追加固定返回值约定：final message 是程序消费的 return value，不要 preamble、prose 或 markdown fences；schema agent 则交给 `codex --output-schema` 并在成功后 `JSON.parse`。（`src/js/prelude.js`; `src/codex.rs`）

`phase(title)` 通过 `op_phase` 创建 step，并把 step 写入全局 `__currentStep`；prelude 注释说明并发分支里应显式传 `{ step }`，因为这个全局槽位会在并行 interleave 下误归属。（`src/js/prelude.js`）

`budget.total` 来自 Rust op，`budget.spent()` 调 `op_budget_spent()`，而 SPEC 约定 `CODEX_FLOW_BUDGET` 未设或 `0` 表示无限，并且 resume replay 是免费的。（`src/js/prelude.js`; `src/engine.rs`; `SPEC.md`）

并发控制在 runner 层，而不是 JS 层：`RunnerCtx` 内部持有 `Arc<Semaphore>`，`run_agent` 先获取 permit，再真正启动 codex 子进程。（`src/codex.rs`）

并发上限解析在 `cli.rs`：优先级是 `CODEX_FLOW_CONCURRENCY` env > `--concurrency N` flag > 默认 `min(16, cores-2)`，默认值还会 clamp 到至少 `1`。（`src/cli.rs`; `SPEC.md`）

`main.rs` 解析 `--concurrency` 时只在下一个 token 能 parse 为 `usize` 时消费值，否则只删除 flag，避免把 workflow path 误吃掉。（`src/main.rs`）

runner 的失败重试默认最多 `3` 次，`CODEX_FLOW_MAX_ATTEMPTS` 可覆盖；退避从 `800ms` 开始按位移增长，最高 capped at `8000ms`。完整可靠性机制见[第 3 章](#第-3-章可靠性机制)。（`src/codex.rs`）

per-agent timeout 来自 `opts.timeoutMs` 或 `CODEX_FLOW_TIMEOUT_MS`，`0` 或未设置表示无 timeout；子进程使用 `kill_on_drop(true)`，timeout 被视为可重试失败。（`src/codex.rs`; `SPEC.md`）

resume 机制由 journal 层实现：每个 run 有 `run_id`，成功 agent 结果 append 到 `runs/<run_id>.jsonl` 或 `CODEX_FLOW_RUNS_DIR` 指定目录。（`src/journal.rs`; `src/main.rs`）

cache key 使用 `64-bit FNV-1a`，初始值 `0xcbf2_9ce4_8422_2325`，乘数 `0x0000_0100_0000_01b3`，并按固定字段编码 prompt、model、sandbox、schema、cwd、isolate。（`src/journal.rs`）

key 显式排除 `id`、`label`、`step` 和 `timeout_ms`，并通过 occurrence index 让相同 `(prompt, opts)` 的第 N 次调用成为独立样本。（`src/journal.rs`; `SPEC.md`）

`op_agent` 在真正 spawn 前先查 journal，命中时发送 `AgentSpawned`、`Note("resumed from journal")`、`Final`、`Status(Done)`，然后直接返回 cached result。（`src/engine.rs`）

append 失败不会使 workflow 失败，而是向当前 agent 发送 `AgentUpdate::Note("journal append failed: ...")`，因此可观测但降级为不可 resume。（`src/engine.rs`）

TUI 不是独立数据源，它只是 `AppEvent` 的状态机和渲染器：`App::apply` 串行应用事件，维护 steps、agents、by_id、step_agents、collapsed groups、detail buffer 和 done 状态。（`src/tui.rs`）

TUI 左侧是 workflow steps，右侧是当前 step 的 agents，Detail 视图展示 prompt、streamed events 和 final output；按键包括 Up/Down、Right/Enter、Left/Esc、q。（`src/tui.rs`）

渲染优化按 SPEC M1 落在 `render_opt.rs` 与 `tui.rs`：frame interval 是 `16ms`，data event 只 mark dirty，key event 立即 redraw。（`src/render_opt.rs`; `src/tui.rs`; `SPEC.md`）

TUI 对 engine channel 做批量 drain：数据事件每轮最多 `512` 个，按键 burst 每轮最多 `256` 个，以避免每个 event 或每个 trackpad arrow 都触发完整 redraw。（`src/tui.rs`）

Detail buffer 每个 agent 最多保留 `2000` 行，超出时丢弃头部；Detail wrap cache 以 `(detail_ver, width)` 为 key，并只物化可见窗口。（`src/tui.rs`; `src/render_opt.rs`）

`SPEC.md` 把实现路线分成 M1 到 M7：M1 render parity，M2 runner reliability，M3 concurrency and observability，M4 DSL alignment，M5 journal/resume，M6 agentType registry and worktree isolation，M7 nested workflow TUI grouping。（`SPEC.md`）

其中已经体现在代码里的关键点包括：RunMeta 预声明 phase，`left_step(new_step)=new_step-1` 的 step 终态推进，budget output-token 累加，journal key 固定字段序，`agentType` markdown body 前缀，`isolate` worktree，以及 group 只作为 UI 字段且排除在 journal key 外。（`SPEC.md`; `src/engine.rs`; `src/journal.rs`; `src/registry.rs`; `src/worktree.rs`; `src/tui.rs`）

SPEC 的全局验收要求是 `cargo build` 干净、`cargo test` 全绿、每模块经 codex review 无 P0 遗留，并且 `outputs/cc-vs-codexflow-api-review.md` 的 P0/P1 差距清零。（`SPEC.md`）

### 模块图

`src/lib.rs` 是库边界文件，只声明 crate 文档并公开 `cli`、`codex`、`engine`、`event`、`journal`、`registry`、`render_opt`、`tui`、`worktree` 九个模块。（`src/lib.rs`）

`src/main.rs` 是二进制入口，负责解析 `--tui`、`--concurrency`、`--resume`、workflow path 和 args JSON，创建 `mpsc::unbounded_channel::<AppEvent>()`，启动 engine thread，并把同一条 `rx` 接到 TUI 或 stdout printer。（`src/main.rs`）

`src/engine.rs` 是 JS workflow engine，负责 deno_core extension ops、`WorkflowLoader`、bootstrap、`run_workflow`、phase lifecycle、journal lookup/append、schema temp file、budget gate、agentType prefix 与 isolate worktree spec 的构造。（`src/engine.rs`）

`src/event.rs` 是三层事件模型的唯一声明处：raw `CodexEvent` 和 `Item` 解析 Codex JSONL，`AgentUpdate` 表达 agent 进度，`AppEvent` 表达 UI/stdout 消费的 workflow 事件。（`src/event.rs`）

`src/codex.rs` 是真实子 agent runner，把 `AgentSpec` 转换成 `codex exec --json` 子进程，读取 stdout JSONL，drain stderr 为 Note，执行 retry、timeout、schema JSON parse safety、token budget accumulation 和 semaphore 并发限制。（`src/codex.rs`）

`src/cli.rs` 放 CLI 侧纯函数，包括 concurrency 解析、第二 positional JSON 解析，以及 `FailureTracker` / `format_failures`，这些逻辑被 stdout 与 TUI 退出摘要共享。（`src/cli.rs`; `src/main.rs`; `src/tui.rs`）

`src/journal.rs` 实现 resume journal，包括 `KeyInput`、`64-bit FNV-1a` key、`JournalEntry` JSONL、occurrence counter、append/load、torn tail healing、`journal_path` 和 `new_run_id`。（`src/journal.rs`）

`src/registry.rs` 实现 `agentType` 注册表，目录优先取 `CODEX_FLOW_AGENTS_DIR`，否则为 `~/.codex-flow/agents`；它手写 frontmatter stripping，并把 `<type>.md` 正文作为 prompt system prefix。（`src/registry.rs`）

`src/worktree.rs` 实现 isolated agent 的临时 git worktree，`WorktreeGuard::add` 执行 `git worktree add --detach`，Drop 时执行 `git worktree remove --force`，并用 `Arc<TempDir>` 保持基目录生命周期。（`src/worktree.rs`）

`src/render_opt.rs` 提供 TUI 渲染优化的小单元：`RenderScheduler` 的 `16ms` frame 和 key-immediate redraw，`PaneCache` 的 area/dirty 缓存，`wrap_width` 与 `WrapCache` 的 CJK display-width wrapping。（`src/render_opt.rs`）

`src/tui.rs` 是 ratatui 前端，拥有 `App` 状态机、grouped agents 可见行模型、steps/agents/detail 绘制、scroll clamp、RenderScheduler 接入、channel/key burst drain、failure summary 和 headless bench harness。（`src/tui.rs`）

`src/js/prelude.js` 是 embedded JS DSL prelude，安装 `agent`、`parallel`、`pipeline`、`phase`、`log`、`args`、`budget` 和内部 `__meta`/`__setResult`/`__setError`，并把所有 DSL 调用 funnel 到 Rust ops。（`src/js/prelude.js`; `src/engine.rs`）

## 第 2 章：Workflow DSL

本章聚焦 DSL 的作者语义和 host 交互边界。第 1 章已经给出架构位置；重试、timeout、journal、budget gate 和 worktree 创建顺序的执行细节统一放在[第 3 章](#第-3-章可靠性机制)，避免在本章重复解释 runner 内部。

codex-flow 的 workflow 是一个 JavaScript ES module，host 通过 `workflow:bootstrap` 导入用户模块并执行入口；规范推荐 `export default async function run(args)`，同时 `src/engine.rs` 的 bootstrap 也兼容 `export function run()`、`export const workflow` 和 default promise。

DSL 名称不是从 npm 包导入的 API，而是 `src/js/prelude.js` 在 V8 runtime 初始化时挂到 `globalThis` 的全局函数和值；`skill/codex-flow-workflow/SKILL.md` 明确说 workflow.js 内可用的 globals 是 `agent/parallel/pipeline/phase/log` 加 `args/budget`。

`export const meta` 是可选的 UI skeleton：`skill/codex-flow-workflow/SKILL.md` 示例为 `export const meta = { name: "audit", phases: ["Scan", "Verify"] }`，`src/engine.rs` 会在 workflow 正式运行前读取 `user.meta`，把 `name` 归一成 string，把 `phases` 归一成 string 数组。

`src/js/prelude.js` 通过内部 `__meta(m)` 调用 `op_meta`；`src/engine.rs` 的 `op_meta` 发出 `RunMeta{name, phases}`，用于让 UI 预绘 phase，但它不推进 `step_seq`，所以后续 `phase()` 仍从 step `0` 开始并与 `meta.phases` 的数组索引对齐。

`args` 是 host 传入 workflow 的 JSON 值；`src/js/prelude.js` 在模块顶层执行 `globalThis.args = op_get_args()`，`src/engine.rs` 的 bootstrap 调入口函数时也传入同一个 `globalThis.args`。

命令行 args 的解析规则写在 `skill/codex-flow-workflow/SKILL.md`：未传时是 `{}` 或 `null` 的语义，非法 JSON 是启动错误并以 exit `2` 结束，而不是静默降级为 `null`。

`budget` 是只读预算视图，形状为 `{ total, spent(), remaining() }`；`src/js/prelude.js` 从 `op_budget_total()` 取 `total`，`spent()` 调 `op_budget_spent()`，`remaining()` 在无上限时返回 `Infinity`，有上限时返回 `max(0, total - spent)`。

预算上限来自 `CODEX_FLOW_BUDGET`；`src/engine.rs` 只接受能 parse 成 `u64` 且大于 `0` 的值，未设或 `0` 表示无限，`src/codex.rs` 只累计 `AgentUpdate::Tokens` 里的 output tokens。

预算门禁是 best-effort：`src/engine.rs` 在 `op_agent` 启动前检查 `spent >= total` 并拒绝新 agent，`src/codex.rs` 在拿到 concurrency permit 后再复查一次；已经在跑的 agent 可以自然结束，所以实际消耗可能超过目标。

`phase(title)` 声明并设置当前 step；`src/js/prelude.js` 把 title 转成 string 后调用 `op_phase`，再把返回的 step 写入唯一的 `globalThis.__currentStep`。

`src/engine.rs` 的 `op_phase` 使用单调 `step_seq` 分配 step index，index 从 `0` 开始且稠密；进入新 phase 时，`left_step(step)` 把前一个 step 标为 `Done`。

run 结束时，`src/engine.rs` 只把最后进入过的 step 终态化：成功为 `Done`，失败为 `Failed`；如果 `meta.phases` 预声明了更多 phase 而 workflow 没有进入，它们保持 Pending，这一点在 `SPEC.md` 的 M4 实况里有说明。

`log(message)` 是旁白通道；`src/js/prelude.js` 把 message 转 string 后调用 `op_log`，`src/engine.rs` 用 synthetic agent id `0` 发送 `AgentUpdate::Note`。

`parallel(thunks)` 负责并发 barrier；`src/js/prelude.js` 要求参数是 array，否则返回 TypeError，并用 `Promise.all` 等待全部分支。

作者规则要求传 thunks，即 `() => agent(...)`，`skill/codex-flow-workflow/SKILL.md` 和 `references/dsl-reference.md` 都强调不是 bare promises；这是因为 thunk 能让 codex-flow 控制启动、排队和失败折叠。

实现上，`parallel` 会捕获 thunk 同步 throw，也会把 rejected promise catch 成 `null`，所以单个 agent 失败不会炸掉整个 batch；返回数组保留输入顺序。

`pipeline(items, ...stages)` 是逐 item 的流水线；`src/js/prelude.js` 对每个 item 启一个 async task，并在该 item 内依次 `await` 每个 stage。

pipeline 的关键语义是 item 之间没有 stage barrier：A 可以在 stage 3，B 仍在 stage 1；stage callback 的参数是 `(prevResult, originalItem, index)`，任一 stage throw 时该 item 结果变成 `null`。

`agent(prompt, opts?)` 是唯一会启动真实 `codex exec` 子进程的 DSL 边界；`skill/codex-flow-workflow/SKILL.md` 也明确说这是唯一消耗 tokens 的 global。

`src/js/prelude.js` 先校验 `prompt` 必须是非空 string，否则 reject 一个 TypeError；这避免空任务进入 runner。

每次 `agent()` 都分配递增 id，默认 `label` 是 `agent-<id>`；`src/js/prelude.js` 从 `++__agentSeq` 得到 id，所以 label 默认值在一个 workflow 进程内单调递增。

`label` 只用于显示和失败摘要；`src/engine.rs` 的 journal key 明确排除 `id/label/step`，所以改 label 不会影响 resume 命中。

`step` 决定 agent 归属哪个 phase；`src/js/prelude.js` 的规则是 `opts.step ?? globalThis.__currentStep ?? 0`，因此显式 `step` 优先，其次当前 phase，最后归到 step `0`。

`group` 是一级 nesting tag；`src/js/prelude.js` 允许 `null`，但非 `null` 时必须是非空 string，否则拒绝，避免 TUI 出现无名组头和 `group/label` 的空前缀。

`SPEC.md` 的 M7 把 `group` 定义为纯外观字段；`src/engine.rs` 把 group 透传到 `AgentSpawned`，但 journal key 仍只含 prompt、model、sandbox、schema、cwd、isolate，不含 group。

`model` 是 per-agent model override；`src/js/prelude.js` 原样放入 spec，`src/codex.rs` 在构造 argv 时把它翻成 `codex exec -m <model>`。

`sandbox` 是 per-agent sandbox policy；`skill/codex-flow-workflow/SKILL.md` 给出的取值是 `read-only`、`workspace-write`、`danger-full-access`，`src/codex.rs` 用 `-s <sandbox>` 传给 codex。

`cwd` 是 agent 的工作目录；`src/codex.rs` 把它转成 `codex exec -C <cwd>`，`skill/codex-flow-workflow/SKILL.md` 说明它对 research 或纯生成 agent 很重要，因为 codex 会读取 cwd repo 的上下文。

`schema` 是 JSON Schema object；`src/js/prelude.js` 对它执行 `JSON.stringify`，`src/engine.rs` 写到临时文件 `schema-<id>.json`，`src/codex.rs` 再追加 `--output-schema <path>` 到 codex argv。

有 `schema` 时，`agent()` 的 prompt 不追加返回值约定；`src/js/prelude.js` 注释说明 shape 由 `codex --output-schema` 强制，成功返回后再执行 `JSON.parse(text)`，所以 JS 侧 resolve 的是 object。

无 `schema` 时，`src/js/prelude.js` 自动把返回值约定追加到 prompt：最终消息是程序消费的 return value，只输出 raw data，不要 preamble、prose 或 markdown fences；作者不应该再手写一份重复约定。

runner 侧的 schema 兜底是 parse-only；`src/codex.rs` 的 `classify_text(text, has_schema)` 在 has_schema 时只检查是否为合法 JSON，`SPEC.md` M2 明确说形状交给 `codex --output-schema` 服务端。

因此严格 output-schema 的作者规则要在 schema 本身写对：每个 `properties` 中定义的 key 都必须出现在同层 `required` 里，并配合 `additionalProperties: false` 收紧对象；`examples/deep-research.workflow.js` 的每个 object schema 都按这个模式列出 required keys。

如果 schema agent 输出不是合法 JSON，`src/codex.rs` 把它归类为 `BadJson`，把拒绝原因追加进下一次 attempt 的 prompt，要求输出单个匹配 schema 的 JSON value。

重试次数默认是 `3`；`src/codex.rs` 从 `CODEX_FLOW_MAX_ATTEMPTS` 读取大于等于 `1` 的值，否则用 `3`，退避从 `800ms` 开始翻倍并封顶 `8000ms`。

`timeoutMs` 是 per-agent wall-clock timeout；`src/js/prelude.js` 传成 `timeout_ms`，`src/codex.rs` 的优先级是显式 `opts.timeoutMs` 高于 `CODEX_FLOW_TIMEOUT_MS`，`0` 或未设置表示无超时。

超时会杀掉当前 codex 子进程并作为 retryable failure 进入同一重试圈；`src/codex.rs` 的错误文本包含 `codex timed out after <ms>ms`。

`agentType` 是 registry agent type；`src/js/prelude.js` 传成 `agent_type`，`src/engine.rs` 在计算 journal key 前加载 registry body 并前缀到 prompt，所以修改 agent type 文件会导致 resume 重新运行。

registry 的路径规则来自 `skill/codex-flow-workflow/SKILL.md` 和 `src/registry.rs`：默认是 `<agents_dir>/<type>.md`，环境变量 `CODEX_FLOW_AGENTS_DIR` 可覆盖目录，frontmatter 被忽略，正文作为 system framing。

`isolate` 有两个写法：`isolate: true` 或 `isolation: "worktree"`；`src/js/prelude.js` 把它们归一成 boolean `isolate`。

隔离模式会让 runner 在 workflow repo 上创建临时 git worktree，并把 agent 的 cwd 指向该 worktree root；`src/worktree.rs` 和 `SPEC.md` M6 说明 worktree 是 ephemeral，agent 结束后通过 guard 清理。

`cwd` 与 `isolate` 互斥；`src/engine.rs` 在 `op_agent` 开头显式报错，因为 isolated agent 必须运行在它自己的 worktree root。

嵌套 workflow 不需要额外 `workflow()` API；`skill/codex-flow-workflow/SKILL.md` 规定组合方式就是 ES module import，例如把子 workflow 写成普通导出函数再由父 workflow 调用。

这种子 workflow 与父 workflow 共享同一个 V8 engine、concurrency cap、budget 和 journal；这是 plain ES module 的自然结果，也是 `skill/codex-flow-workflow/SKILL.md` 所说的 workflow nesting equivalent。

子 workflow 的约定是把 `ctx` 作为第一个参数，形状为 `{ step, group }`，并把它 spread 到每个 `agent()` opts 中；`src/js/prelude.js` 的注释和 `SPEC.md` M7 都把这个约定写成显式归属。

显式传 `ctx` 的原因是不能有“当前 group”这样的全局槽位；`phase()` 已经证明唯一全局 `__currentStep` 在 `parallel()` 或 `pipeline()` 并发交错下会误归属，所以 group 也不能做 ambient global。

同理，子 workflow 内部不要调用 `phase()`；`src/js/prelude.js` 注释说 child 调 `phase()` 会创建新的 top-level step，`SPEC.md` M7 把“子 workflow 内调 phase 污染顶层 step”定为不做 runtime 拦截、只靠文档约束。

正确 nesting 写法是父层先 `const s = phase("X")`，再调用 `await child({ step: s, group: "child-a" }, data)`；子层只调用 `agent(prompt, { ...ctx, label: "..." })`。

并发分支里也应显式捕获 step：先在串行位置 `const s = phase("Review")`，再在 `parallel()` 或 `pipeline()` 的每个 agent opts 里传 `{ step: s }`；`skill/codex-flow-workflow/SKILL.md` 把这是避免 misattribution 的作者规则。

agent 的 resume identity 由 `src/engine.rs` 构造：effective prompt 加 `{model, sandbox, schema, cwd, isolate}`，再加 occurrence index；这解释了为什么 label、step、group 都是显示属性而不是执行身份。

`agentType` 的正文会先拼到 prompt 前再计算 key；`src/engine.rs` 注释说明这是为了让 registry `.md` 的编辑能正确触发重新运行。

schema 的缓存身份是 `JSON.stringify(schema)` 的字符串；`SPEC.md` 说明属性顺序变化会 miss，但这是无害重跑。

`parallel` 和 `pipeline` 的失败折叠只发生在组合器内部；裸 `await agent()` 失败会 reject，作者需要自己 `try/catch`，这一点在 `references/dsl-reference.md` 有明确说明。

从工程边界看，prelude 只负责把 DSL 调用规范化成 op spec；`src/engine.rs` 负责 meta、phase、journal、budget gate 和 worktree request；`src/codex.rs` 负责真正的 `codex exec --json`、argv、timeout、重试和 output token 统计。

这套 DSL 的核心约束可以概括为三条：编排逻辑在 JavaScript 里保持确定性，非确定性只放在 `agent()` 边界；agent 输出直接作为数据消费；并发场景下所有归属关系都显式传入 opts，而不是依赖可被交错污染的全局状态。

## 第 3 章：可靠性机制

本章讨论的可靠性边界集中在 runner、engine、journal、worktree 和 agent registry 五个模块，SPEC 把 M2-M6 排列为“可靠性优先，journal 居中，P2 殿后”的实现顺序。（`SPEC.md`）

### Runner retry 与 schema 闭环

`agent()` 在 JS 层把一次用户请求归一为 `JsAgentSpec`，再由 `op_agent` 转成 Rust `AgentSpec` 并交给 `run_agent`。（`src/js/prelude.js`; `src/engine.rs`; `src/codex.rs`）

非 schema agent 会在 prompt 末尾追加 return convention，schema agent 保留原始 prompt 并依赖 `--output-schema` 与 runner 校验。（`src/js/prelude.js`; `SPEC.md`）

`build_args` 固定调用 `codex exec --json --skip-git-repo-check`，并按 opts 追加 `-m`、`-s`、`-C` 和 `--output-schema`。（`src/codex.rs`）

`classify_text` 把空白或空字符串输出归类为 `Usable::Empty`，这是一个 retryable failure。（`src/codex.rs`）

`classify_text` 只在 `has_schema=true` 时把非 JSON 文本归类为 `Usable::BadJson`，没有 schema 的非 JSON 文本仍是合法 string result。（`src/codex.rs`; `SPEC.md`）

schema 校验在 runner 中是 parse-only，形状约束交给 `codex --output-schema` 服务端，SPEC 明确不在本地做完整 JSON Schema validation。（`src/codex.rs`; `SPEC.md`）

当 schema 输出不是合法 JSON 时，runner 把错误写成 `output was not valid JSON for the requested schema: {e}`，并进入下一次 attempt。（`src/codex.rs`）

下一次 attempt 的 prompt 由 `augment_prompt` 生成，包含 `Your previous output was REJECTED: {reason}` 和“只输出单个合法 JSON 值”的英文 steering。（`src/codex.rs`; `SPEC.md`）

runner 默认最多尝试 `3` 次，`CODEX_FLOW_MAX_ATTEMPTS` 可覆盖，但只有解析后 `n >= 1` 的值会生效。（`src/codex.rs`）

每个 agent 在整个 retry sequence 期间只占一个 semaphore permit，因为 `run_agent` 先 acquire permit，再进入 attempt loop。（`src/codex.rs`）

失败后若还有下一次 attempt，backoff 从 `800ms` 开始，按 `800u64 << (attempt - 1)` 指数增长，并 capped at `8000ms`。（`src/codex.rs`）

backoff 的 shift exponent 被 clamp 到 `13`，避免很大的 `CODEX_FLOW_MAX_ATTEMPTS` 在 debug 下触发 shift overflow。（`src/codex.rs`）

每次 retry 前 runner 会发送 `AgentUpdate::Note`，文本包含 `attempt {attempt}/{max_attempts}`、最后错误和 backoff 毫秒数。（`src/codex.rs`）

最后一次失败后 runner 发送 `failed after {max_attempts} attempts: {last_err}` note，再把 agent 标记为 `AgentStatus::Failed`。（`src/codex.rs`）

### Timeout 与 child process lifecycle

per-agent timeout 来自 `opts.timeoutMs`，JS prelude 将其传成 `timeout_ms`，缺省为 `null`。（`src/js/prelude.js`; `src/engine.rs`）

timeout 的优先级是 `opts.timeoutMs` 高于 `CODEX_FLOW_TIMEOUT_MS`，两者都缺省或值为 `0` 时禁用 timeout。（`src/codex.rs`; `SPEC.md`）

`run_once` 用 `tokio::time::timeout` 包住 `run_once_inner`，因此 timeout 覆盖 stdout JSONL read loop 和 child wait。（`src/codex.rs`; `SPEC.md`）

timeout 命中时错误文本为 `codex timed out after {ms}ms`，该错误进入 `last_err` 并按普通 retry 处理。（`src/codex.rs`）

`run_once_inner` 使用 `tokio::process::Command` 启动子进程，并对 child 设置 `.kill_on_drop(true)`。（`src/codex.rs`）

timeout 发生时 inner future 被 drop，`kill_on_drop(true)` 负责杀掉 child，代码注释说明 tokio orphan reaper 会异步 best-effort reap。（`src/codex.rs`）

stderr 被单独 drain 成 `AgentUpdate::Note`，stdout 被按 JSONL 解析成 `CodexEvent` 并转换为 `AgentUpdate`。（`src/codex.rs`; `src/event.rs`）

stdout 解析失败不会终止 agent，而是发送 `unparsed line ({e}): {line}` note，最终结果仍取最后一条 assistant message。（`src/codex.rs`; `src/event.rs`）

### Journal key、occurrence 与 torn-tail repair

journal 的职责是把成功 agent result append 到 `runs/<run_id>.jsonl`，并在 `--resume` 时加载为 cache。（`src/journal.rs`; `src/engine.rs`; `SPEC.md`）

`journal_path` 默认使用相对当前目录的 `runs`，也可以由 `CODEX_FLOW_RUNS_DIR` 覆盖。（`src/journal.rs`; `SPEC.md`）

`new_run_id` 使用 `start-time seconds` 的 hex 加 `pid` 的 hex，格式是 `{secs:x}-{pid:x}`，没有引入 uuid 或 RNG dependency。（`src/journal.rs`; `SPEC.md`）

cache identity 使用 `KeyInput` 的 `prompt`、`model`、`sandbox`、`schema`、`cwd` 和 `isolate`。（`src/journal.rs`; `src/engine.rs`）

cache identity 明确排除 `id`、`label`、`step` 和 `timeout_ms`，因为这些字段是 cosmetic 或 operational。（`src/journal.rs`; `SPEC.md`）

`group` 也不参与 journal key，因为 engine 构造 `KeyInput` 时没有读取 `spec.group`。（`src/engine.rs`; `SPEC.md`）

`journal_key` 使用 `64-bit FNV-1a`，offset basis 是 `0xcbf2_9ce4_8422_2325`，prime 是 `0x0000_0100_0000_01b3`。（`src/journal.rs`）

key 输出为 `16` 位十六进制字符串，代码用 `format!("{:016x}", h.finish())` 固定宽度。（`src/journal.rs`）

编码顺序固定为 `prompt`、`model`、`sandbox`、`schema`、`cwd`、`isolate`，并用 NUL 加字段名分隔字段边界。（`src/journal.rs`）

optional 字段带 `+` 或 `-` presence tag，因此 `None` 与 `Some("")` 不会坍缩为同一个 key。（`src/journal.rs`; `SPEC.md`）

每个 journal entry 还带 0-based `occ`，也就是同一 key 在本进程调用序列里的 occurrence index。（`src/journal.rs`）

`Journal::occurrence` 对每个 key 单独计数，所以 N 个相同 `(prompt, opts)` 调用会保留 N 个独立样本，而不是坍缩成一个 cache hit。（`src/journal.rs`; `SPEC.md`）

`Journal::append` 对已经存在的 `(key, occ)` 是 no-op，resume 命中不会重复 append 同一条记录。（`src/journal.rs`）

每条 journal entry 序列化为一行 JSON，字段包含 `key`、`occ` 和 `result`。（`src/journal.rs`）

append 前如果文件非空且最后一个字节不是 `\n`，代码先补一个 newline，再写入新 entry，这就是 torn-tail repair。（`src/journal.rs`; `SPEC.md`）

`Journal::load` 对 missing file 返回空 journal，对其他 read error 返回错误，对 malformed JSONL line 直接 skip。（`src/journal.rs`）

engine 读取 journal 失败时会发送 id `0` 的 note：`journal {path} unreadable ({e}); starting fresh`，并降级为新 journal。（`src/engine.rs`; `SPEC.md`）

### Resume replay 与零成本语义

`main` 解析 `--resume <run_id>`，并拒绝缺失 run id、以 `-` 开头的值和以 `.js` 结尾的值。（`src/main.rs`; `SPEC.md`）

resume 模式复用用户给定的 run id，非 resume 模式调用 `new_run_id` 生成新 run id。（`src/main.rs`; `src/journal.rs`）

`op_agent` 先解析 `agentType` 并得到 effective prompt，再用 effective prompt 计算 journal key。（`src/engine.rs`; `src/registry.rs`）

resume lookup 在 budget gate、schema temp file 写入、`AgentSpawned` 的正常 pending announce 和 `run_agent` 之前执行。（`src/engine.rs`）

resume hit 会发送 `AgentSpawned`，然后发送 `Note("resumed from journal")`、`Final(hit)` 和 `Status(Done)`。（`src/engine.rs`）

resume hit 直接 `return Ok(hit)`，因此不 acquire semaphore permit、不 spawn codex、不消耗 output-token budget。（`src/engine.rs`; `SPEC.md`）

JS `budget.spent()` 只读取本进程 live output token 累计值，prelude 注释明确 journal replay 是 free，并且 resumed run 从 `0` 开始计 spent。（`src/js/prelude.js`; `src/codex.rs`; `SPEC.md`）

新的成功结果会在 `run_agent` 返回后 append 到同一个 journal path，append 失败只给当前 agent id 发送 note，不让 workflow 失败。（`src/engine.rs`; `SPEC.md`）

### CODEX_FLOW_BUDGET 与 output-token ceiling

`CODEX_FLOW_BUDGET` 在 `run_workflow` 启动时解析为 `u64`，只有大于 `0` 的值会成为有限 budget。（`src/engine.rs`）

未设置、解析失败或值为 `0` 的 `CODEX_FLOW_BUDGET` 都表现为 unbounded budget。（`src/engine.rs`; `SPEC.md`）

budget 只累计 output tokens，`budget_delta` 对 `AgentUpdate::Tokens { input, output }` 返回 `output`，对其他 update 返回 `0`。（`src/codex.rs`）

output token 累计存放在 `RunnerCtx.spent: Arc<AtomicU64>`，`run_once_inner` 在 streaming updates 时 `fetch_add`。（`src/codex.rs`; `SPEC.md`）

`over_budget` 的判断是 `spent >= total`，所以刚好达到 ceiling 时也会拒绝新的 agent start。（`src/codex.rs`）

第一道 gate 在 `op_agent` 中，resume miss 后读取 `spent`，若 over budget 就直接让 `agent()` 抛错。（`src/engine.rs`）

第二道 gate 在 `run_agent` 中，runner acquire semaphore permit 后再次读取 `spent`，以覆盖排队期间其他 agent 已耗尽 budget 的情况。（`src/codex.rs`; `SPEC.md`）

post-permit gate 失败时 runner 会向该 agent 发送 `refused at spawn: {msg}` note 和 `AgentStatus::Failed`。（`src/codex.rs`）

budget contract 是 best-effort，代码和 SPEC 都说明 in-flight agents 可能在并发执行中继续 overshoot。（`src/engine.rs`; `src/codex.rs`; `SPEC.md`）

`budget.total` 经 `op_budget_total` 暴露给 JS，有限值返回 JSON number，无限时返回 `null`。（`src/engine.rs`; `src/js/prelude.js`）

`budget.spent()` 经 `op_budget_spent` 暴露为 `f64`，代码注释说明在 `2^53` 以内保持精确。（`src/engine.rs`; `SPEC.md`）

### FAILURES aggregation

stdout 和 TUI 都复用 `cli::format_failures`，因此失败摘要文本格式集中在一个 helper 中。（`src/cli.rs`; `src/main.rs`; `src/tui.rs`）

`FailureTracker` 从 `AgentSpawned` 记录 label，从 `AgentUpdate::Note` 记录最后一个 best-effort error reason，从 `AgentStatus::Failed` 记录失败 agent id。（`src/cli.rs`）

若后续收到 `AgentStatus::Done`，`FailureTracker` 会移除该 id，因此 retry 后恢复成功的 agent 不会出现在 `FAILURES` 中。（`src/cli.rs`）

带 `group` 的 agent 在 failures 中显示为 `group/label`，未分组 agent 显示原 label。（`src/cli.rs`; `src/tui.rs`）

`format_failures` 在有失败时输出 `FAILURES ({n}):`，每条失败是 `- {label}: {err}` 或 `- {label}`。（`src/cli.rs`）

stdout front-end 在事件 channel 结束后把 `format_failures` 的结果打印到 stderr，`RESULT:` 或 `ERROR:` 仍打印到 stdout。（`src/main.rs`）

TUI front-end 退出 alternate screen 后才打印 failures，且只有 `app.done.is_some()` 时打印，避免用户早退时输出误导性的 partial list。（`src/tui.rs`）

SPEC 规定非法 args JSON 退出码为 `2`，而有 agent failure 仍保持退出码 `0`，以免破坏 `RESULT:` grep 工作流。（`SPEC.md`; `src/main.rs`）

### Git worktree isolation

JS prelude 把 `opts.isolate === true` 或 `opts.isolation === "worktree"` 归一为布尔 `isolate`。（`src/js/prelude.js`; `SPEC.md`）

engine 明确禁止 `isolate` 与 `cwd` 同时出现，因为 isolated agent 的 cwd 必须是新 worktree root。（`src/engine.rs`; `SPEC.md`）

engine 只构造 `WorktreeSpec { repo: workflow_dir, base: tempdir }`，真正创建 worktree 的动作推迟到 runner。（`src/engine.rs`; `src/worktree.rs`）

runner 在 acquire semaphore permit 并通过 post-queue budget re-check 后才调用 `WorktreeGuard::add`。（`src/codex.rs`; `src/worktree.rs`; `SPEC.md`）

这个顺序保证 `300` 个 isolate fan-out 最多同时创建 `concurrency` 个 worktree，被 budget 拒绝的 agent 不会创建 worktree。（`src/worktree.rs`; `SPEC.md`）

`WorktreeGuard::add` 的路径是 run tempdir 下的 `wt-{id}`，命令是 `git -C {repo} worktree add --detach {path}`。（`src/worktree.rs`）

创建失败时错误文本会包含 `isolate: git worktree add failed (is {repo} inside a git repo?)`，非 git repo 被当作 error 而不是 panic。（`src/worktree.rs`）

创建成功后 runner 把 `spec.cwd` 改成 worktree path，codex 子进程通过 `-C` 在隔离 worktree 内运行。（`src/codex.rs`）

`WorktreeGuard` 持有 `Arc<TempDir>`，避免 run tempdir 在 guard 活着时提前删除。（`src/worktree.rs`; `SPEC.md`）

`Drop` 使用 `git -C {repo} worktree remove --force {path}` 做 RAII cleanup，删除目录并清理源 repo 的 worktree bookkeeping。（`src/worktree.rs`）

### agentType registry

`opts.agentType` 由 JS prelude 传给 Rust 的 `agent_type`，engine 在 journal key 计算前解析它。（`src/js/prelude.js`; `src/engine.rs`）

registry 目录优先使用 `CODEX_FLOW_AGENTS_DIR`，否则使用 `~/.codex-flow/agents`。（`src/registry.rs`; `SPEC.md`）

`agent_system_prefix` 拒绝空 agentType、包含 `/`、包含 `\` 或包含 `..` 的 agentType，避免 path traversal。（`src/registry.rs`）

registry 文件名固定为 `<type>.md`，读取失败是 error，typo 不会静默降级为无 system framing。（`src/registry.rs`; `SPEC.md`）

frontmatter stripping 只处理文件开头的 `---` block，并支持 LF 与 CRLF 结尾。（`src/registry.rs`）

如果 closing fence 位于 EOF 且没有 trailing newline，`strip_frontmatter` 返回空 body，避免 YAML 泄漏进 prompt。（`src/registry.rs`; `SPEC.md`）

解析出的 markdown body 会 `trim()` 后作为 prefix 拼到 agent prompt 前，中间用两个 newline 分隔。（`src/registry.rs`; `src/engine.rs`）

因为 registry prefix 在 journal key 之前拼入 effective prompt，修改 `<type>.md` 会改变 key，并让 resume 正确 miss 后重跑。（`src/engine.rs`; `src/journal.rs`）

## 第 4 章：Performance & TUI：从逐事件重绘到可交互的 60fps

codex-flow 的 TUI 性能问题不是单个 widget 太慢，而是旧路径把每个 engine event 都变成一次完整 redraw；M1 把目标定为 `16ms` 帧、key-immediate draw、PaneCache、WrapCache 和滚动语义修复。（`SPEC.md`; `RENDER_OPT_SPEC.md`）

旧模型可以概括为 `PerEvent(old)`：agent streaming 越密，draw 次数越接近事件数；bench harness 直接在 `src/tui.rs` 中用同一 synthetic load 比较 old/new 策略。（`src/tui.rs`）

新模型是 `Coalesced(new)`：data events 只 `mark_dirty()`，到 frame boundary 才 `should_draw_on_tick()`；key events 调 `on_key()` 后立刻 `should_draw_now()`，所以人手操作不等下一帧。（`src/render_opt.rs`; `src/tui.rs`）

帧间隔由 `RenderScheduler::FRAME = Duration::from_millis(16)` 固定，`run_tui` 用 `tokio::time::interval(RenderScheduler::FRAME)` 接入主循环，并设置 missed tick 为 `Skip`。（`src/render_opt.rs`; `src/tui.rs`）

这比 `RENDER_OPT_SPEC.md` 中记录的旧 `33ms` ticker 少一半等待上限，同时仍保留 data-update throttle：高频 agent 输出不会逐条立即刷屏。（`RENDER_OPT_SPEC.md`; `src/tui.rs`）

`codex-flow bench` 是隐藏子命令，入口在 `src/main.rs`，调用 `codex_flow::tui::bench::run(agents, per)`，默认负载是 `16 x 200`。（`src/main.rs`; `src/tui.rs`）

bench 的 synthetic load 是一个 phase、N 个 agents、每个 agent 产生 `per` 条 round-robin streaming detail events，viewport 固定为 `160x45`。（`src/tui.rs`）

bench 的 old strategy 是每个 event 后 `draw()`；new strategy 是按 `burst = agents.max(1)` 累积后 draw，一轮 agent 更新折叠成一帧。（`src/tui.rs`）

| command | events | draws old -> new | total render old -> new | speedup | source |
|---|---:|---:|---:|---:|---|
| `codex-flow bench 16 200` release | 3218 | 3218 -> 202 | 423.84ms -> 25.92ms | 16.4x less, 即约 16.7x 量级 | `codex-flow bench`; `src/tui.rs` |
| `codex-flow bench 104 50` release | 5306 | 5306 -> 52 | 890.03ms -> 8.58ms | 103.8x less, 即约 104x | `codex-flow bench`; `src/tui.rs` |

这组数字说明主要收益来自 draw count 的折叠：`16` agents 时 draw 数约少 `15.9x`，`104` agents 时 draw 数约少 `102.0x`。（`codex-flow bench`; `src/tui.rs`）

单次 draw 的平均成本没有被 new strategy 神奇消除；release bench 中 `16`-agent 平均 draw 是 `131.7us -> 128.3us`，`104`-agent 是 `167.7us -> 165.0us`，核心变化是“少画”。（`codex-flow bench`; `src/tui.rs`）

### RenderScheduler：把 when 和 what 分开

`RenderScheduler` 只负责决定“何时画”，不接触 terminal；它有 `dirty` 和 `immediate` 两个 bit，测试覆盖 fresh、data、key、frame 四类行为。（`src/render_opt.rs`; `RENDER_OPT_SPEC.md`）

`mark_dirty()` 只置 dirty，因此 data event 不会触发 now draw；`should_draw_on_tick()` 在 frame boundary 消费 dirty。（`src/render_opt.rs`）

`on_key()` 同时置 dirty 和 immediate；`should_draw_now()` 消费 immediate 和 dirty，避免 key 后又在同一帧重复画一次。（`src/render_opt.rs`）

`run_tui` 的 channel branch 先 `rx.recv()`，再最多 `try_recv()` `512` 条已排队 event；这让 producer burst 变成一次 dirty 标记，而不是 `512` 次 wake/draw。（`src/tui.rs`）

key branch 也不是逐键画：await 到第一枚 key 后，用 `keys.next().now_or_never()` 非阻塞抽干已缓冲 key，最多 `256` 枚，然后一次 immediate draw。（`src/tui.rs`）

这个 key-burst coalescing 是 R1.5 的组成部分，因为 terminal wheel/trackpad 会把一次滚动翻译成大量方向键事件。（`SPEC.md`; `src/tui.rs`）

### PaneCache 和 WrapCache：缓存真正昂贵的中间态

`PaneCache` 的纯单元按 `(Rect, Buffer)` 缓存 pane 输出；area 不变且 `force=false` 时不再执行 render closure，只把缓存 buffer merge 到目标 buffer。（`src/render_opt.rs`; `RENDER_OPT_SPEC.md`）

`PaneCache` 测试覆盖 first rebuild、same-area hit、force rebuild、area-change rebuild 和 offset-aware `Buffer::merge`，说明缓存不会把 pane 画到错误坐标。（`src/render_opt.rs`）

当前 `draw()` 把 `PaneCache` 用在 Steps pane：`steps_ver` 没变时复用 cached Steps buffer，避免 streaming frame 重建左侧 phase 列表。（`src/tui.rs`）

Agents pane 在运行中高度 volatile，源码注释明确说“miss every frame”的 cache 只会增加 buffer copy，因此没有强行缓存 Agents pane。（`src/tui.rs`）

Detail pane 的大头不是边框，而是 wrapping；所以 current implementation 把 Detail 优化放在 `WrapCache`，而不是给整个 Detail pane 做 `PaneCache`。（`src/tui.rs`; `src/render_opt.rs`）

`wrap_width(line, width)` 用 `unicode_width::UnicodeWidthChar` 按 display columns 计算宽度，CJK 字符按 `2` 列处理，且不会切断 char。（`src/render_opt.rs`）

测试把 `"你好世界"` 在 width `4` 下 wrap 为 `["你好", "世界"]`，并验证每行 display width 不超过 `4`。（`src/render_opt.rs`）

`WrapCache` 的 key 是 `(content_version, width)`；同一个 version/width 只 build 一次，version 或 width 变化才 rebuild。（`src/render_opt.rs`）

`draw_detail()` 使用 pane inner width 预先 wrap，然后把 visible lines 直接喂给 `Paragraph::new(lines)`，没有再启用 ratatui 的 runtime wrap。（`src/tui.rs`）

Detail buffer 还有硬上限：每个 agent 的 detail 超过 `MAX_DETAIL = 2000` 行时保留 tail，防止长时间运行把 draw 输入无限放大。（`src/tui.rs`）

### scroll-freeze：400 个滚动键不再重包全量 Detail

R1.5 记录的现场 bug 是：terminal 把滚轮/触控板动作变成方向键风暴，tmux 每格 `3` 个 key 加上 momentum，旧逻辑对 `400`-key burst 重 wrap 全量 detail，实测冻结 `35.9s`。（`SPEC.md`; `src/tui.rs`）

修复的第一点是 scroll key 不失效 cache：`on_key` 的 invalidation tuple 是 `(focus, steps_sel, agents_sel, collapse_gen)`，不包含 `detail_scroll`。（`src/tui.rs`）

因此 `Down`、`PageDown`、`End`、`Up` 这类纯滚动只改 scroll offset，不 bump `steps_ver/detail_ver`，`scroll_keys_do_not_invalidate_render_caches` 测试直接钉住这个行为。（`src/tui.rs`）

修复的第二点是 clamp write-back：`draw_detail()` 算出 `max_scroll` 后把 `app.detail_scroll = scroll` 写回，滚到底后继续 Down 不会积累看不见的 overshoot debt。（`src/tui.rs`）

`overscroll_clamps_and_up_responds_immediately` 测试覆盖了这个交互：End 后 draw 得到真实 max，继续 `50` 次 Down 仍停在 max，再按一次 Up 必须移动到 `max - 1`。（`src/tui.rs`）

修复的第三点是 visible-window-only materialization：`draw_detail()` 只把 `wrapped[scroll..scroll+view_h]` 转成 `Vec<Line>`，draw 成本是 O(view height)，不是 O(total wrapped lines)。（`src/tui.rs`）

修复的第四点是 key-burst coalescing：`run_tui` 的 key branch 抽干最多 `256` 个 buffered key 后只 draw 一次，避免把同一触控板动作乘上 full redraw 成本。（`src/tui.rs`）

本次回归测试 `cargo test bottom_scroll_burst_stays_responsive -- --nocapture` 输出 `400 bottom-scroll key+draw cycles took 521.21075ms`，即约 `0.52s/0.51s` 量级。（`cargo test`; `src/tui.rs`）

同一 bug 的项目记录是 `400`-key burst 旧路径 `35.9s`；按本次 `0.521s` 回归数估算，用户可感知冻结从十秒级降到半秒级。（`SPEC.md`; `src/tui.rs`）

### M7：Grouped Agents pane 不破坏 R1.5

M7 把子 workflow agents 从扁平列表改成一级分组：`agent(prompt, {group: "name"})` 是纯外观字段，事件层 `AgentSpawned` 透传 `group: Option<String>`。（`SPEC.md`; `src/event.rs`）

TUI 的 visible-row model 是 `Row::Header { first } | Row::Agent { idx }`，并且 Row 是 index-only + Copy，避免在 `256`-key drain 中反复 clone group name。（`src/tui.rs`; `SPEC.md`）

`rows_for_selected_step()` 每帧从 selected step 的 agent indices 派生可见行：组头锚在 group 首次出现处，成员折到 header 下，未分组 agent 保持 spawn 顺序与组头交错。（`src/tui.rs`; `SPEC.md`）

折叠状态存为 `HashMap<StepId, HashSet<String>>`；header Enter/Right 切换 collapse，`collapse_gen` bump 后参与 invalidation tuple。（`src/tui.rs`; `SPEC.md`）

导航按 visible rows 而不是 raw agent count：Down 的边界取 `rows_for_selected_step().len()`，所以展开 header 增加行数、折叠 header 减少行数都能正确限制 selection。（`src/tui.rs`）

Header rollup 是单次 O(n) pass 汇总 statuses 和 tokens；状态规则是任一 Failed 则 Failed，否则任一非 Done 则 Running，全部 Done 才 Done。（`src/tui.rs`; `SPEC.md`）

header 文案显示 arrow、group name、`done/total` 和 token sum；member label 在固定 `28` 列 label field 内缩进，token 列仍对齐。（`src/tui.rs`; `SPEC.md`）

无 group 的 workflow 维持旧渲染形态；SPEC 要求“逐像素一致”，源码通过未分组行仍走 `Row::Agent` 和原 label/token layout 来满足这个约束。（`SPEC.md`; `src/tui.rs`）

M7 最关键的 R1.5 回归点是 pinned Detail：进入 Detail 时保存真实 `agents` 下标到 `detail_agent`，而不是之后再按 visible row 位置查找。（`src/tui.rs`; `SPEC.md`）

这个 pin 修复了“早先展开组插入新 agent 后 visible row shift，Detail 指向漂移，`detail_ver` 停跳，`WrapCache` 冻住”的问题。（`src/tui.rs`; `SPEC.md`）

`detail_pin_survives_row_shift_from_earlier_group_spawn` 测试证明 late agent 插入早先 group 后，drilled agent 的 stream 仍会 bump `detail_ver`，非 drilled agent 不会误失效 Detail。（`src/tui.rs`）

`spawn_into_earlier_group_keeps_selection_identity` 则覆盖尚未进入 Detail 的 selection：新增 row 插到 selection 上方时，selection identity 保持在原 agent，只是 visible position 顺移。（`src/tui.rs`）

SPEC 的 M7 实况还记录 grouped pane 复测带来约 `+~3%` bench 开销、`max 3.07ms << 16ms` frame budget；这说明分组模型没有吃掉 M1 的 60fps 预算。（`SPEC.md`）

### 结论

TUI 性能优化的本质是把 redraw 从“事件驱动”改成“frame 驱动 + key immediate”，并把 scroll offset 从 expensive cache key 中移除。（`src/render_opt.rs`; `src/tui.rs`; `SPEC.md`）

`RenderScheduler` 控制 draw cadence，`PaneCache` 避免稳定 pane 重建，`WrapCache` 避免 Detail 每帧重包，R1.5 又把 overscroll、visible window 和 key burst 三个交互细节补齐。（`src/render_opt.rs`; `src/tui.rs`; `SPEC.md`）

bench 数字对应这个结构：`16`-agent 级别是约 `16.7x` 渲染时间下降，高 agent 数可到约 `104x`，而 scroll-freeze 从 `35.9s` 降到约 `0.51s`。（`codex-flow bench`; `cargo test`; `src/tui.rs`; `SPEC.md`）

## 第 5 章：开发方法论复盘

本章讨论的不是抽象流程，而是 codex-flow 在 `SPEC.md`、`RENDER_OPT_SPEC.md` 和 `src/` 中留下的可审计开发轨迹。（`SPEC.md`; `RENDER_OPT_SPEC.md`; `src/`）

codex-flow 的方法论核心是把 spec-driven、TDD、plan workflow 和 per-module review 串成一个闭环，每一轮发现最终回写到 spec 或测试。（`SPEC.md`; `RENDER_OPT_SPEC.md`; `src/tui.rs`）

这套闭环尤其适合 agent-built software，因为 agent 的并行产出很快，但只有单一事实源、失败测试和只读复核能压住漂移。（`SPEC.md`; `skill/codex-flow-workflow/references/patterns.md`）

### SPEC.md 作为唯一事实源

`SPEC.md` 开头直接声明“单一事实源”，并要求每条 Requirement 可测、TDD 先红后绿、Plan 由 spec 派生。（`SPEC.md`）

这个文件不是需求草稿，而是同时记录目标、约束、实现顺序、验收标准、已决策事项和评审定案的工程 ledger。（`SPEC.md`）

设计不变量规定“编排在代码、不在模型”，所以 JavaScript workflow 决定下一步跑什么，模型只在 `agent()` 边界内提供非确定性输出。（`SPEC.md`; `src/js/prelude.js`）

设计不变量规定“agent 输出即数据”，所以非 schema 返回 string、schema 返回已校验对象，prelude 也把非 schema prompt 追加 return-value convention。（`SPEC.md`; `src/js/prelude.js`）

设计不变量规定“失败不静默、不炸全局”，所以 `parallel/pipeline` 的失败坍缩为 `null`，run 末尾由 `FailureTracker` 汇总 `FAILURES`。（`SPEC.md`; `src/js/prelude.js`; `src/cli.rs`）

设计不变量规定“可重放→可恢复”，所以 `journal.rs` 用 deterministic key 记录成功 agent 结果，并让 `--resume` 命中时回放而不重新 spawn。（`SPEC.md`; `src/journal.rs`; `src/engine.rs`）

M1 到 M7 的每个模块都在 `SPEC.md` 中有 Requirement、验收和边界，其中 M1/M2/M3 是 P0 或独立可靠性基础。（`SPEC.md`）

`SPEC.md` 的“模块依赖与并行轴”把 `engine.rs` 和 `prelude.js` 标成公共枢纽，要求这些实现串行落，避免多 agent 并行写同一核心文件。（`SPEC.md`）

同一段还把 plan 生成和 codex review 标成“只读 spec+code”和“只读各模块 diff”，这让并行 agent 主要用于扩展认知而不是争抢写权限。（`SPEC.md`; `skill/codex-flow-workflow/references/patterns.md`）

“全局验收”要求 `cargo build` 干净、`cargo test` 全绿、每模块经 codex review 无 P0 遗留，并要求 P0/P1 差距清零。（`SPEC.md`）

“已决策”区把 plan workflow 的 open questions 收敛为确定规则，例如环境变量统一 `CODEX_FLOW_*`、model-facing 文本统一 English。（`SPEC.md`）

“评审定案”区把 review findings 分成“已修（代码）”和“入约（文档化、不改码）”，所以 review verdict 不会停留在聊天记录里。（`SPEC.md`）

### TDD：先让 bug 可复现，再让实现变绿

`RENDER_OPT_SPEC.md` 明确写出“Tests are written first (red), then implementation (green)”，并把 B.1/B.2/B.3 拆成纯逻辑单元。（`RENDER_OPT_SPEC.md`）

B.1 的测试目标是 `RenderScheduler`：`FRAME` 必须是 `16ms`，data event 只 mark dirty，key event 立即 redraw。（`RENDER_OPT_SPEC.md`; `src/render_opt.rs`）

B.2 的测试目标是 `PaneCache`：同 area 且未 force 时不再调用 render closure，area 变化或 force 才 rebuild。（`RENDER_OPT_SPEC.md`; `src/render_opt.rs`）

B.3 的测试目标是 `wrap_width` 与 `WrapCache`：ASCII、CJK、短行、空行和 `(version,width)` cache 都被单测钉住。（`RENDER_OPT_SPEC.md`; `src/render_opt.rs`）

滚动冻结的 TDD 更典型：`SPEC.md` 把 `2026-06-10` 的 freeze 定案写入 R1.5，并记录 `400` 个滚动键曾造成 `35.9s` 冻结。（`SPEC.md`; `src/tui.rs`）

`src/tui.rs` 的 repro tests 构造 `heavy_app()`，向一个 agent 写入 `2000` 行混合中文与 ASCII 的长输出，再进入 Detail pane。（`src/tui.rs`）

`overscroll_clamps_and_up_responds_immediately` 先按 `End`，再连续 `50` 次 `Down`，要求 `detail_scroll` 不能积累越界债务，并且一次 `Up` 必须移动。（`src/tui.rs`）

`scroll_keys_do_not_invalidate_render_caches` 连续执行 `Down/Down/PageDown/End/Up`，要求 `steps_ver` 和 `detail_ver` 不因纯滚动键变化。（`src/tui.rs`）

`bottom_scroll_burst_stays_responsive` 模拟 `400` 次 bottom-scroll key+draw cycle，并把接受阈值设为小于 `1` 秒。（`src/tui.rs`）

这些测试推动了实现约束：纯滚动键不 bump cache version，`draw_detail()` 每帧 clamp 写回 `detail_scroll`，并只物化可见窗口。（`SPEC.md`; `src/tui.rs`）

TDD 的收益不是“多写测试”，而是把用户感知的 freeze 拆成 cache invalidation、overscroll debt 和 O(viewport) draw 三个可验证合同。（`SPEC.md`; `src/tui.rs`; `RENDER_OPT_SPEC.md`）

### Plan workflow：并行只读规划扩展 spec

codex-flow 的 workflow DSL 支持 `parallel(thunks)`、`pipeline(items,...stages)`、`sandbox:"read-only"` 和 schema 化结果，这使 plan agents 可以并行读 spec/code 而不写文件。（`skill/codex-flow-workflow/SKILL.md`; `skill/codex-flow-workflow/references/dsl-reference.md`）

`patterns.md` 的 parallel audit 示例是一 module 一个 read-only agent，再由 summary agent 合并 findings；这正是把多人读代码变成可复用 workflow 的形式。（`skill/codex-flow-workflow/references/patterns.md`）

`SPEC.md` 明确说 plan 生成是“只读 spec+code”，并把 plan 输出收敛到“已决策”，说明规划阶段的产物最终要回到唯一事实源。（`SPEC.md`）

plan workflow 捕捉到 Detail pin 的必要性：`SPEC.md` 记录 `detail_agent` pin 真实 agents 下标，`src/tui.rs` 在 Enter 时保存 append-only index。（`SPEC.md`; `src/tui.rs`）

这个 pin 修掉的是位置式 lookup 的类别错误：visible rows 会因早先 group 新增 agent 而移动，但 append-only agent index 不会移动。（`SPEC.md`; `src/tui.rs`）

对应回归测试 `detail_pin_survives_row_shift_from_earlier_group_spawn` 先 pin flat agent `b`，再向更早的 `g1` 插入 `late` agent，要求 pinned agent 的 stream 仍 bump `detail_ver`。（`src/tui.rs`）

plan workflow 也捕捉到 `collapse_gen` invalidation gap：折叠组头不改变 focus/selection，却改变 Agents pane 的可见行模型。（`SPEC.md`; `src/tui.rs`）

实现把 invalidation tuple 扩为 `(focus,steps_sel,agents_sel,collapse_gen)`，测试 `header_enter_toggles_collapse_bumps_gen_and_bounds_down` 要求折叠后 `detail_ver` 变化。（`SPEC.md`; `src/tui.rs`）

plan workflow 还捕捉到 serde silent-drop：如果 `JsAgentSpec` 丢掉 `group` 字段，prelude 继续发送 group 时 Rust 侧会静默忽略。（`SPEC.md`; `src/engine.rs`）

`js_agent_spec_group_roundtrip` 用 `serde_json::from_value` 验证 `group:"models.py"` 和 `group:null`，把这个 silent-drop 风险变成单测。（`src/engine.rs`）

这些 planner finding 都没有直接变成实现口头约定，而是被写进 `SPEC.md` 的 M7 已决策和对应 Rust/JS 测试。（`SPEC.md`; `src/tui.rs`; `src/engine.rs`; `src/js/prelude.js`）

### Per-module cross-model review：只读复核补上边界条件

read-only gpt-5.5/cross-model review 在仓库内的持久记录是 `SPEC.md` 的 codex review verdict，执行形态与 `patterns.md` 的 read-only per-module audit 一致。（`SPEC.md`; `skill/codex-flow-workflow/references/patterns.md`）

backoff shift overflow 被源码吸收：`src/codex.rs` 把 `800u64 << (attempt - 1)` 改为 `(attempt - 1).min(13)` 后再 cap 到 `8000ms`。（`src/codex.rs`）

unbounded mpsc drain starving keys 被 TUI loop 吸收：engine event drain 设置 `MAX_DRAIN: usize = 512`，key burst drain 设置 `MAX_KEY_DRAIN: usize = 256`。（`src/tui.rs`）

scrollbar u16 truncation 被 Detail state 吸收：`detail_scroll` 使用 `usize`，并只在 ratatui 边界处理显示，避免大 wrapped/CJK 内容提前截断。（`src/tui.rs`）

FailureTracker retry-recovery false positives 被 `Done` 事件修正：`FailureTracker::observe` 在 agent 后续 `Done` 时从 failed 列表移除该 id。（`src/cli.rs`）

对应测试 `tracker_recovered_agent_not_listed` 先观察 transient `Failed`，再观察 `Done`，要求 failures 为空。（`src/cli.rs`）

`--concurrency` eating positionals 被 CLI parser 修正：只有下一 token 能 parse 为 usize 时才同时 remove flag/value，否则只 remove flag，保留 workflow path。（`src/main.rs`）

budget queue overshoot 被评审定案为 M4#1：`op_agent` 前有 fast-path gate，`run_agent` 拿到 semaphore permit 后再复查 `over_budget`。（`SPEC.md`; `src/engine.rs`; `src/codex.rs`）

journal torn-tail 被评审定案为 M5#2：`append()` 写新 JSONL 前检查文件末字节，若不是 `\n` 就先补 newline。（`SPEC.md`; `src/journal.rs`）

`torn_tail_is_healed_on_append` 写入一条无 trailing newline 的截断记录，再 append `k1`，要求 `k1` 可 load 而 `k0` 被跳过。（`src/journal.rs`）

premature worktree creation 被评审定案为 M6#1：engine 只构造 `WorktreeSpec`，runner 在拿到 permit 之后异步 `git worktree add`。（`SPEC.md`; `src/engine.rs`; `src/codex.rs`; `src/worktree.rs`）

worktree teardown race 被 M6#2 吸收：`WorktreeSpec` 和 `WorktreeGuard` 持有 `Arc<TempDir>`，Drop 时执行 `git worktree remove --force`。（`SPEC.md`; `src/worktree.rs`）

selection drift 被评审定案为 M7#1：`apply(AgentSpawned)` 先捕获当前 visible Row 身份，push 后再按 Row 身份找回新位置。（`SPEC.md`; `src/tui.rs`）

`spawn_into_earlier_group_keeps_selection_identity` 构造 `a(g1), b(flat), c(g1), d(g2)`，再向 `g1` 插入 `late`，要求 selection 仍指向 `b`。（`src/tui.rs`）

empty-group misattribution 被评审定案为 M7#2：`prelude.js` 拒绝 `group !== null` 且非 non-empty string 的 opts，避免 nameless header 和 `/label` failure。（`SPEC.md`; `src/js/prelude.js`）

review 的价值在于覆盖“实现看似合理但边界错”的点：queue depth、torn JSONL、visible row drift、空 group 都不是 happy path 单测自然会先想到的情况。（`SPEC.md`; `src/codex.rs`; `src/journal.rs`; `src/tui.rs`; `src/js/prelude.js`）

### 方法论给 agent-built software 的启示

第一，agent 可以并行读，但写入系统事实必须收敛到一个文件；codex-flow 用 `SPEC.md` 记录 Requirement、已决策和评审定案。（`SPEC.md`）

第二，spec 需要包含负面约束；例如 M7 明确“不做”子 workflow 内调 `phase()`、多级嵌套和 label 前缀自动分组。（`SPEC.md`）

第三，TDD 要从用户可感知故障反推最小合同；scroll freeze 最终被拆成 `400`-key burst、overscroll clamp、cache invalidation 和 visible-window draw。（`SPEC.md`; `src/tui.rs`）

第四，planner agent 的最好产物不是代码，而是把遗漏的合同写成可测 checklist；`detail_agent`、`collapse_gen` 和 serde roundtrip 都体现了这一点。（`SPEC.md`; `src/tui.rs`; `src/engine.rs`）

第五，review agent 的最好产物不是泛泛建议，而是能落成“已修代码”或“入约文档”的 verdict；`SPEC.md` 的评审定案正按这个二分组织。（`SPEC.md`）

第六，预算、journal、worktree、selection 这类跨模块状态最需要二次复核，因为它们的 bug 通常只在排队、恢复、异步插入或 teardown 时出现。（`src/codex.rs`; `src/journal.rs`; `src/worktree.rs`; `src/tui.rs`）

第七，agent-built software 应该默认把“静默失败”当 P0 风险：invalid args 不再 silent null，missing agentType 是 error，journal unreadable 要 Note 降级。（`SPEC.md`; `src/cli.rs`; `src/registry.rs`; `src/engine.rs`）

第八，resume 和 budget 必须显式定义成本模型；这里 `budget.spent()` 只计本进程 live output tokens，journal replay 免费且绕过 budget gate。（`SPEC.md`; `src/js/prelude.js`; `src/engine.rs`）

第九，UI 性能合同也应写进 spec，而不是靠肉眼验收；`16ms` frame、`512` event drain、`256` key drain、`3.07ms` max bench 都是可讨论的数字。（`SPEC.md`; `RENDER_OPT_SPEC.md`; `src/tui.rs`）

第十，codex-flow 的经验说明 agent 不是替代工程纪律，而是放大工程纪律；spec、tests、read-only planners 和 read-only reviews 越硬，agent 并行越可控。（`SPEC.md`; `RENDER_OPT_SPEC.md`; `skill/codex-flow-workflow/references/patterns.md`）
