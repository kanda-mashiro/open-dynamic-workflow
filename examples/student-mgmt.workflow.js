// Workflow: drive codex (as the sub-agent) to BUILD a student management
// system, then drive a second codex agent to VERIFY it. This is the /goal:
// tmux -> codex-flow -> this workflow -> agent() -> codex exec -> built system.
//
// args: { dir: "<absolute output dir>" }  (where codex should create the app)

const SPEC = `用 Python 3.9 标准库(http.server + json + sqlite3,禁止 pip 装任何三方库)实现一个学生管理系统。

必须满足:
1. 单文件 app.py,运行 python3 app.py 后在 127.0.0.1:8099 起 HTTP 服务,常驻不退出。
2. 数据持久化到同目录 students.db(SQLite)。字段 id(自增) name(str) age(int) major(str)。
3. REST 接口(请求/响应均 JSON):
   POST   /api/students        {name,age,major} -> 201 含 id 的对象
   GET    /api/students        -> 200 数组
   GET    /api/students/{id}   -> 200 或 404
   PUT    /api/students/{id}   {name,age,major} -> 200 或 404
   DELETE /api/students/{id}   -> 204 或 404
   GET    /health             -> 200 {"status":"ok"}
4. 无效 JSON / 缺字段 -> 400 不崩溃;未知路径 -> 404。
5. 写一个 verify.sh:后台启动服务、等就绪、依次 健康检查->创建->列表->查单个->更新->删除->确认已删,
   每步 curl 校验状态码与内容,全通过最后一行打印 ACCEPTANCE: PASS,否则打印 ACCEPTANCE: FAIL 并非零退出;结束要关掉后台服务。

请把 app.py 和 verify.sh 写到当前工作目录。写完后自己运行一次 bash verify.sh 确认输出 ACCEPTANCE: PASS。`;

export default async function run(args) {
  const dir = (args && args.dir) || ".";
  log(`student-mgmt workflow start; build dir = ${dir}`);

  // ── Step 1: BUILD — one codex agent writes the system in `dir` ──
  phase("Build");
  const buildOut = await agent(SPEC, {
    label: "build:student-mgmt",
    cwd: dir,                    // codex -C <dir>: write files here
    sandbox: "danger-full-access",
  });
  log(`build agent finished (${buildOut.length} chars of summary)`);

  // ── Step 2: VERIFY — a second codex agent runs verify.sh and reports ──
  phase("Verify");
  const verifyOut = await agent(
    `在当前目录运行 \`bash verify.sh\` 并把它的完整输出原样返回。` +
      `只返回脚本输出,不要加任何解释。`,
    {
      label: "verify:run",
      cwd: dir,
      sandbox: "danger-full-access",
    },
  );
  log("verify agent finished");

  const passed = /ACCEPTANCE:\s*PASS/.test(verifyOut);
  return {
    dir,
    passed,
    verifyTail: verifyOut.slice(-400),
  };
}
