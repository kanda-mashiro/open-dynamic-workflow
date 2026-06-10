# codex-flow ⇄ Claude Code dynamic-workflow parity — SPEC

> 单一事实源。codex-flow 的目标是**完全实现 Claude Code dynamic workflow 的设计**。
> 每条 Requirement 都可测；TDD：先写测试（红）再实现（绿）。Plan 由此 spec 派生。
> 已验证的事实锚：`outputs/cc-vs-codexflow-api-review.md`（8/8 官方文档断言）。

## 设计不变量（不可违背）

- **编排在代码、不在模型**：脚本（JS）决定下一步跑什么；非确定性只存在于 `agent()` 边界内。
- **agent 输出即数据**：非 schema 返回 string，schema 返回已校验对象；脚本直接消费，不解析人话。
- **失败不静默、不炸全局**：`parallel/pipeline` 单点失败坍缩为 `null`；run 末尾汇总 failures。
- **可重放→可恢复**：脚本确定（禁 wall-clock/RNG），agent 结果按 `(prompt, opts)` journaling，resume 命中即回填。
- **精简**：不为对齐而堆 API；codex-flow 已有的优势（逐 agent sandbox、TUI 钻取、真 ES module 组合）保留。

---

## M1 — Render parity（tui.rs；独立模块）

把已测绿的 `render_opt` 单元接入渲染循环。细节见 `RENDER_OPT_SPEC.md`。

- **R1.1** 帧间隔 16ms（~60fps），取代 33ms。
- **R1.2** 按键 → 立即重绘（`RenderScheduler::should_draw_now`）；数据事件 → 仅标脏、并入下一帧（`should_draw_on_tick`），保住既有节流。
- **R1.3** `draw()` 用 `PaneCache`：pane 仅在 area 变化或标脏时重建，否则 blit 缓存。
- **R1.4** `draw_detail()` 用 `wrap_width`+`WrapCache`：按显示宽度（CJK=2）换行并缓存，喂给禁用 wrap 的 Paragraph。
- **R1.5** 滚动语义（2026-06-10 滚动冻结 bug 定案）：① 纯滚动键**不**失效缓存——版本号仅在 focus/选中变化时 bump（终端把滚轮翻成方向键风暴：tmux 每格 3 个 + 动量；逐键重 wrap 实测 400 键 35.9s 冻结）；② `detail_scroll` 每帧 clamp 写回，越底不积累隐形透支（否则 Up 看似失灵）；③ Detail 只物化可见窗口，draw 成本 O(视口) 而非 O(全部 wrapped 行)；④ `run_tui` 按键合批（`now_or_never` 抽干已缓冲键 → 一次 draw）。回归测试 `tui::tests` ×3。
- **验收**：`render_opt` 13 测绿（✓ 已达成）；`run_tui` 引用 `RenderScheduler`；`draw/draw_detail` 引用 `PaneCache/WrapCache`；`cargo build` 干净；`bench` 复测帧成本不回归。

## M2 — Runner 可靠性（codex.rs + prelude.js；P0）

schema 闭环 + 超时，干掉最常见与最恶性的失败。

- **R2.1** schema 校验进 `run_agent` 重试圈：codex 输出非合法 JSON 时视为**可重试**失败，且把错误回填进下次 attempt 的 prompt（"上次输出被拒：{err}，只输出符合 schema 的 JSON"）。
- **R2.2** per-agent 超时：`AgentSpec.timeout_ms`（DSL `opts.timeoutMs`），`run_once` 包 `tokio::time::timeout`，spawn 用 `kill_on_drop(true)`；超时计入 `last_err` 走既有重试/失败路径。
- **R2.3** prelude 删除"parse 失败即 reject"分支（校验权移交 runner）；成功路径仍 `JSON.parse`。
- **验收**：纯函数单测——`classify_outcome(text, has_schema)` 区分 成功/空输出可重试/JSON非法可重试；`build_args` 含 schema 时 argv 正确；超时用 fake-codex（sleep）集成验证返回可重试失败。

## M3 — 并发与可观测（main.rs + engine.rs；P0）

- **R3.1** 并发 = `CODEX_FLOW_CONCURRENCY` env > `--concurrency` flag > 默认 `min(16, cores-2)`（clamp≥1），取代硬编码 6。
- **R3.2** 收集 `AgentStatus::Failed` 的 {id,label,err}；run 结束在 `RESULT:` 旁输出 `FAILURES:` 列表（CLI 与 TUI 退出码/摘要）。组合器 null 语义不变。
- **R3.3** args 第二位 JSON 非法 → 报错退出（带文件名+parse错误），不再静默 `null`。
- **验收**：`resolve_concurrency(env, flag, cores)` 单测覆盖优先级与 clamp；`parse_args_json` 单测区分 缺省→null / 合法→值 / 非法→Err。

## M4 — DSL 对齐（prelude.js + engine.rs + bootstrap；P1）

- **R4.1** 非 schema 的 agent prompt 追加返回值约定："Your final message IS the return value consumed by a program. Output raw data only — no preamble/prose/fences."
- **R4.2** bootstrap 读 `user.meta`（若导出）→ 新 op `op_meta` 发 `RunMeta{name,phases}` 事件 → TUI 预声明 phase 骨架。
- **R4.3** `phase()` 把上一 step 标 `Done`（engine 内 `left_step`：phase 索引稠密，prev=step−1；无新 op、无 prelude 状态）；run 结束由 `run_workflow` 按成败把最后进入的 step 标 `Done`/`Failed`（`final_step_status`）。
- **R4.4** budget 落地：runner 已收 `AgentUpdate::Tokens{input,output}`（codex `turn.completed.usage`，event.rs 已解析）；引擎累加 output tokens 到 `EngineState`，`budget.total` 来自 args/env，`budget.spent()/remaining()` 经新 op 读取，超额时 `agent()` 抛错。
- **验收**：`resolve_step_terminal` 序列单测；budget 累加+超限单测（喂合成 Tokens 事件）；bootstrap meta 解析单测（缺省不报错）。

## M5 — Journal / Resume（新 journal.rs + engine.rs + main.rs；P1，最大工程项）

- **R5.1** 每 run 有 `run_id`；agent 成功结果 append 到 `runs/<run_id>.jsonl`，键 = `hash(prompt + canonical_json(opts))`。
- **R5.2** `--resume <run_id>`：启动加载为 map；`op_agent` 开头命中键即返回缓存（不占并发、不 spawn codex、TUI 标 cached）。
- **R5.3** 文档警告：prompt 含时间戳/随机会破缓存键（呼应 CC 禁 wall-clock/RNG 的动机）。
- **验收**：`journal_key(prompt,opts)` 确定性单测（字段序无关）；`Journal::append`+`load` 往返单测；resume 命中路径单测（命中不调 runner）。

## M6 — agentType 注册表 + worktree 隔离（engine.rs + 新 registry；P2）

- **R6.1** `opts.isolate:true`（或 `isolation:'worktree'`）→ `git worktree add` 临时目录，设为 cwd，run 末尾 `git worktree remove`。落实 engine.rs:105 的 TODO。
- **R6.2** `opts.agentType` → 载入 `~/.codex-flow/agents/<type>.md`（YAML frontmatter + 正文 system prompt），把正文作为前缀拼到 prompt 前。
- **验收**：`agent_system_prefix(type, registry)` 单测（命中/缺失）；worktree 路径生成单测（创建/清理用 tempdir 或 guarded）。

## M7 — 嵌套 workflow 的 TUI 分组（方案 B：分组 Agents 面板）

子 workflow（ES import 的函数）的 agent 现在拍平进父 step，无法按子 workflow 聚合。修法：**一级分组**，显式归属（V8 无 ambient async context，全局槽位在并行下必错——同 phase() 的既有教训；CC 同样显式：`opts.phase` 字符串分组）。

- **R7.1** DSL：`agent(prompt, {group: "name"})`——纯外观字段。约定：子 workflow 接 `ctx` 首参（`{step, group}`），spread 进每个 agent。`group` **排除在 journal 缓存键外**（同 label/step）。
- **R7.2** 事件：`AgentSpawned` 加 `group: Option<String>`；prelude/engine 透传。
- **R7.3** TUI Agents 面板两级行：组头（名字 + `done/total` + 汇总状态 + token 合计）+ 缩进 agent 行。**可见行模型**：每帧从该 step 的 agent 列表派生 `Row::Header|Agent`，组按首现顺序、未分组 agent 保持 spawn 顺序与组头交错；折叠集合 `(step, group)` 存 App。导航：Up/Dn 走可见行；Enter/Right 在组头=折/展、在 agent 行=进 Detail；Left/Esc 回 Steps。无 group 的 workflow 渲染与现状**逐像素一致**（零回归）。
- **R7.4** 汇总状态：任一 Failed→Failed；否则任一非 Done→Running；全 Done→Done。
- **R7.5** stdout 路径：agent 行前缀 `(group)`；`FAILURES:` 条目带 `group/label`。
- **不做**（定案）：子 workflow 内调 `phase()`（污染顶层 step，文档约束，v1 不加 runtime 拦截）；多级嵌套（一级封顶，同 CC）；label 前缀自动分组（约定魔法，拒绝）。
- **验收**：rollup/可见行/折叠/导航纯函数单测；无 group 零回归测试；**不破坏 R1.5**（滚动键仍不失效缓存——组折叠属选中态变化，应 bump）；嵌套 demo（4×3 agent）实测分组渲染。

---

## 模块依赖与并行轴

- **公共枢纽** = `engine.rs`（M3/M4/M5/M6 均碰）、`prelude.js`（M2/M4 碰）→ 这些实现**串行落**，避免互相踩踏。
- **完全独立** = M1（tui.rs）。
- **可无冲突并行** = ① plan 生成（只读 spec+code），② **codex review**（只读各模块 diff，多视角，可嵌套）。
- 实现顺序：**M1 → M2 → M3 → M4 → M5 → M6**（可靠性优先，journal 居中，P2 殿后）。

## 全局验收

`cargo build` 干净 + `cargo test` 全绿 + 每模块经 codex review 无 P0 遗留 + `outputs/cc-vs-codexflow-api-review.md` 的 P0/P1 差距清零。

---

## 已决策（消解 plan workflow 的 open questions；spec 仍为唯一事实源）

通用：所有 model-facing 文本（schema 重试 steering、返回值约定等）一律 **English**（CLAUDE.md）；环境覆盖统一 `CODEX_FLOW_*`；测试用例名见 plan 输出备查。

- **M2**：schema 校验=**仅 parse**（形状交给 codex `--output-schema` 服务端）；超时 `opts.timeoutMs` > `CODEX_FLOW_TIMEOUT_MS` > **默认禁用(0)**，文档建议无人值守时设；`kill_on_drop(true)` 无条件；每 attempt clone spec 注入改写后 prompt；timeout 须包 stdout 读循环+wait；fake-codex 测试 `#[cfg(unix)]`+`--test-threads=1`。
- **M3**：并发 = env `CODEX_FLOW_CONCURRENCY` > `--concurrency N` > 默认 `min(16,cores-2)` clamp≥1；**显式值不再被 16 顶**（仅默认受顶）。失败收集 `{id,label,err-best-effort=Failed 前最后一条 note}`；stdout 路径折叠 tracker，TUI 路径 App 收集退出后打印；**退出码：非法 args→2，有失败仍→0**（不破坏 RESULT: grep）。事件模型不动。
- **M4**（已实现实况）：返回值约定仅加在非 schema 的 `agent()`（prelude 单一 owner，`RETURN_CONVENTION`）；token 经 **RunnerCtx.spent (`Arc<AtomicU64>`)**，`run_once_inner` 经 `budget_delta` 累加 output tokens（避开 op 跨 await 重借用）；`budget.total` 来自 `CODEX_FLOW_BUDGET`（未设或 **0=无限**，同 timeout 约定）；超额在 op_agent spawn 前抛错（best-effort，在飞 agent 可超）；`RunMeta{name,phases}` 为 AppEvent 内联变体；ops=`op_meta`/`op_budget_total`(JSON null|number)/`op_budget_spent`(f64，2^53 内精确)；op_meta **不推进 step_seq**（phase() 仍从 0，索引对齐）；phase 终态=`left_step`(prev=step−1)+run 末 `final_step_status`；workflow 未走完 meta 声明的 phase 时，多余步保持 Pending（run 末只终态化最后**进入**的 step）。
- **M5**：`journal_key=FNV-1a(prompt + 固定字段序的{model,sandbox,schema,cwd,isolate}，None≠Some(""))`，排除 `{id,label,step}`；**禁 DefaultHasher**（随机种子破跨进程命中）；键追加 **occurrence 序号**（同 (prompt,opts) 的第 N 次调用各自成键——judge panel 等 N 独立采样不被坍缩成 1 次）；`runs/<run_id>.jsonl` 默认相对 CWD，`CODEX_FLOW_RUNS_DIR` 覆盖；run_id=**起始秒hex-pidhex**（弃 uuid，零新依赖）；`--resume <id>` 复用 id、载为缓存 map、新结果 append 同文件、命中不重 append；命中发 `AgentSpawned+Note("resumed")+Final+Done`（不占并发不 spawn，**绕过 budget gate**——回放零成本）；每条一次性整行写防交错；runs 目录打不开 → Note 警告+降级为不可 resume 的运行（不致死、不静默）。
- **M6**：`isolation:'worktree'`/`isolate:true` 在 prelude 归一为 `isolate:bool`；worktree 源仓库=`workflow_dir`，非 repo 报错不 panic；基目录=EngineState.tempdir（随 run 清，不耦合 M5）；**Drop guard 保证 error 路径也 remove**；registry=`~/.codex-flow/agents/<type>.md`，`CODEX_FLOW_AGENTS_DIR` 覆盖；frontmatter 手解析（无 yaml dep），正文作 system 前缀。
- **M7**（已实现实况）：`Row::{Header{first},Agent{idx}}` index-only Copy（组名经 `agents[first].group` 惰性取，不 clone——on_key 在 256 键 drain 内逐键重派生）；折叠集 `HashMap<StepId,HashSet<String>>`（`contains(&str)` 经 Borrow 零分配）；**Detail 钻取 PIN 真实 agents 下标**（`detail_agent`，append-only 故恒稳）——位置式查找在"agent spawn 进早先展开组"时错位、令 detail_ver 停跳、WrapCache 冻结（R1.5 borns again，有回归测试）；失效 tuple 扩为 `(focus,steps_sel,agents_sel,collapse_gen)`（折叠不改前三者）；组头聚合=单次 O(n) pass 收集 statuses/token、状态经唯一事实源 `rollup()`；成员缩进**在 28 列 label 字段内**（token 列对齐，未分组行字节级不变）；serde roundtrip 守卫测试钉死 `JsAgentSpec.group`（serde 静默丢未知字段）；resume 回放路径同发 group（冒烟实证 10/10 带前缀）；bench 复测 +~3%（噪声级，max 3.07ms ≪ 16ms 帧预算）。

## 评审定案（M4–M6 各自经 codex review，findings 已修或入约）

已修（代码）：
- M4#1 budget gate 移前漏排队：`run_agent` 拿到 permit 后**复查** `over_budget`（超支不再随队列深度无界）。
- M5#1 `journal_key` 的 None/Some("") 坍缩：optional 字段带 +/- presence tag。
- M5#2 torn-tail：append 前检查文件末字节，非 `\n` 先补行（崩溃半行不再连坏后一条）。
- M5#5 journal 不可读静默：`load` 区分 NotFound/其他错误，其他错误发 Note 降级；append 失败 Note 发**当前 agent id**（TUI 丢 id0）。
- M6#1 worktree 提前同步创建：移入 runner（`worktree.rs`），**permit 之后异步**创建（N 并发只建 N 个；被 budget 拒的不建）。
- M6#2 teardown 竞态：guard 持 `Arc<TempDir>` 保活。
- M6#3 isolate 静默丢 cwd：`isolate`+`cwd` **互斥报错**。
- M7#1 选中行漂移：spawn 进早先展开组使可见行上移，位置式 `agents_sel` 漂到错行、下次 Enter pin 错 agent——`apply(AgentSpawned)` 先捕获选中行身份（Row 是 index-only，值即稳定身份）、push 后按身份写回位置（有回归测试）。
- M7#2 `group:""` 当真组：渲染无名组头 + `/label` 失败条目——prelude 入口拒绝（TypeError，非 null 必须非空 string；失败不静默原则）。
- M6#4 EOF 处闭合 fence：识别 `\n---`(\r) 结尾，YAML 不漏进 prompt。

入约（文档化、不改码）：
- occurrence 跨 run 错位仅发生在**相同 (prompt,opts)** 的异步分支间——相同输入的样本可互换；需要稳定身份就把 item id 写进 prompt（与 CC 同约定）。
- `cwd=None` 不入 key：resume 须在同一工作目录执行。
- schema 的缓存身份=JSON.stringify 字符串（属性序变化→miss→无害重跑）。
- `budget.spent()` 只计本进程 live 消耗，回放免费（resume 后从 0 起）。
- phase() 全局槽与 CC 同构：并发分支显式传 `{step}`。
- 运行级 journal 警告（id0 Note）仅 stdout 模式可见，TUI 已知缺口。
- `--resume` 拒以 `-` 开头/以 `.js` 结尾的手工 id（机器生成 id 不受影响）。
