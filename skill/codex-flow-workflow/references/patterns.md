# codex-flow workflow patterns (copy-paste starting points)

Each is a complete `*.workflow.js`. Adapt prompts/paths to the user's task.

## 1. Parallel audit (fan-out → collect)

One agent per module, all concurrent, then a summary.

```js
export default async function run(args) {
  const modules = args?.modules ?? ["auth", "api", "db"];

  phase("Audit");
  const findings = await parallel(
    modules.map(m => () =>
      agent(`Audit the ${m} module for security and correctness bugs. ` +
            `Return concise findings.`,
        { label: `audit:${m}`, sandbox: "read-only",
          schema: { type:"object", required:["module","issues"], properties:{
            module:{type:"string"},
            issues:{type:"array", items:{type:"string"}} } } })
    )
  );

  phase("Summary");
  const report = await agent(
    `Merge these per-module findings into one prioritized report:\n` +
    JSON.stringify(findings.filter(Boolean)),
    { label: "summary", sandbox: "read-only" });

  return { modules: modules.length, report };
}
```

## 2. Build → verify pipeline (two phases, second agent checks the first)

```js
export default async function run(args) {
  const dir = args.dir;                       // absolute output dir

  phase("Build");
  await agent(
    `In the current directory build <DESCRIBE THE SYSTEM>. ` +
    `Also write verify.sh that exits 0 and prints "ACCEPTANCE: PASS" iff it works. ` +
    `Then run bash verify.sh yourself.`,
    { label: "build", cwd: dir, sandbox: "danger-full-access" });

  phase("Verify");
  const out = await agent(
    `Run \`bash verify.sh\` in the current directory and return its full output verbatim.`,
    { label: "verify", cwd: dir, sandbox: "danger-full-access" });

  return { dir, passed: /ACCEPTANCE:\s*PASS/.test(out), tail: out.slice(-300) };
}
```

## 3. Per-file fix with worktree-style isolation (pipeline, no barrier)

```js
export default async function run(args) {
  const files = args.files;                   // ["src/a.ts", "src/b.ts", ...]

  phase("Fix");
  const results = await pipeline(
    files,
    // stage 1: review (read-only)
    (f) => agent(`Review ${f}; list concrete fixes.`,
      { label: `review:${f}`, sandbox: "read-only" }),
    // stage 2: apply, each in its own working dir
    (review, f) => agent(`Apply these fixes to ${f}:\n${review}`,
      { label: `fix:${f}`, cwd: args.dir, sandbox: "workspace-write" }),
  );

  return { fixed: results.filter(Boolean).length };
}
```

## 4. Loop until a target count (accumulate)

```js
export default async function run(args) {
  const target = args?.target ?? 10;
  phase("Hunt");
  const bugs = [];
  while (bugs.length < target) {
    const r = await agent("Find ONE new bug not already in this list: " +
      JSON.stringify(bugs.map(b => b.title)),
      { schema: { type:"object", required:["title","file"], properties:{
        title:{type:"string"}, file:{type:"string"} } } });
    bugs.push(r);
    log(`${bugs.length}/${target}`);
  }
  return { bugs };
}
```

## 5. Adversarial verify (N skeptics per claim)

```js
export default async function run(args) {
  phase("Find");
  const found = await agent("List suspected bugs.", {
    schema: { type:"object", required:["bugs"], properties:{
      bugs:{type:"array", items:{type:"object", properties:{
        desc:{type:"string"}}}}}}});

  phase("Verify");
  const judged = await parallel(found.bugs.map(b => () =>
    parallel([0,1,2].map(v => () =>
      agent(`Try to REFUTE this bug; default refuted=true if unsure: ${b.desc}`,
        { label:`v${v}`, sandbox:"read-only",
          schema:{type:"object", required:["refuted"], properties:{refuted:{type:"boolean"}}}})
    )).then(votes => ({ b, real: votes.filter(Boolean).filter(v=>!v.refuted).length >= 2 }))
  ));

  return { confirmed: judged.filter(j => j.real).map(j => j.b) };
}
```

## Reminders

- Fan out with **thunks**: `items.map(x => () => agent(...))`.
- Thread data forward by **interpolating into the next prompt** (no shared memory).
- Give each agent everything it needs in its prompt (fresh context).
- **Independently verify** checkable outcomes after the run.
