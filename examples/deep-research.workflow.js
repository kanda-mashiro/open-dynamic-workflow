// deep-research.workflow.js — a Claude-Code-style deep-research harness on
// codex-flow. Five phases: Scope -> Research (parallel per angle) -> Verify
// (adversarial, N skeptics per claim) -> Synthesize. Each agent is a real
// `codex exec`; research/verify agents get a network-capable sandbox so they
// can actually fetch sources.
//
//   codex-flow examples/deep-research.workflow.js '{"question":"..."}'
//   codex-flow --tui examples/deep-research.workflow.js '{"question":"...","angles":4,"maxClaims":8,"votes":2}'
//
// args:
//   question   (string, required)  the research question
//   angles     (number, def 4)     how many search angles to decompose into
//   maxClaims  (number, def 8)     cap on claims sent to verification (cost)
//   votes      (number, def 2)     skeptic votes per claim; majority-refute kills

const SCOPE_SCHEMA = {
  type: "object", additionalProperties: false,
  required: ["question", "angles"],
  properties: {
    question: { type: "string" },
    angles: {
      type: "array", minItems: 2, maxItems: 6,
      items: {
        type: "object", additionalProperties: false,
        required: ["label", "query", "rationale"],
        properties: {
          label: { type: "string" },
          query: { type: "string" },
          rationale: { type: "string" },
        },
      },
    },
  },
};

const FINDINGS_SCHEMA = {
  type: "object", additionalProperties: false,
  required: ["angle", "claims"],
  properties: {
    angle: { type: "string" },
    claims: {
      type: "array", maxItems: 5,
      items: {
        type: "object", additionalProperties: false,
        required: ["claim", "evidence", "source", "importance"],
        properties: {
          claim: { type: "string" },
          evidence: { type: "string" },
          source: { type: "string" },
          importance: { enum: ["central", "supporting", "tangential"] },
        },
      },
    },
  },
};

const VERDICT_SCHEMA = {
  type: "object", additionalProperties: false,
  required: ["refuted", "reason", "confidence"],
  properties: {
    refuted: { type: "boolean" },
    reason: { type: "string" },
    confidence: { enum: ["high", "medium", "low"] },
  },
};

const REPORT_SCHEMA = {
  type: "object", additionalProperties: false,
  required: ["summary", "findings", "caveats", "openQuestions"],
  properties: {
    summary: { type: "string" },
    findings: {
      type: "array",
      items: {
        type: "object", additionalProperties: false,
        required: ["point", "confidence", "sources"],
        properties: {
          point: { type: "string" },
          confidence: { enum: ["high", "medium", "low"] },
          sources: { type: "array", items: { type: "string" } },
        },
      },
    },
    caveats: { type: "string" },
    openQuestions: { type: "array", items: { type: "string" } },
  },
};

const RESEARCH_HINT =
  "请用你能用的一切方式获取真实信息:内置 web 搜索工具(若可用),或用 shell 的 " +
  "`curl` 抓取你确知的权威文档/页面 URL。基于真实检索到的内容作答,每条 claim 尽量带 " +
  "evidence(简短引用)和 source(URL 或出处)。查不到可靠来源时,把该 claim 标为 " +
  "importance:tangential 并在 evidence 里写明“未独立核实”。不要编造来源。";

const impRank = { central: 0, supporting: 1, tangential: 2 };

export default async function run(args) {
  const question =
    (args && (args.question || args.q)) ||
    (typeof args === "string" ? args : null);
  if (!question) {
    return { error: "pass a question: '{\"question\":\"...\"}'" };
  }
  const nAngles = (args && args.angles) || 4;
  const maxClaims = (args && args.maxClaims) || 8;
  const votes = (args && args.votes) || 2;
  const needRefute = Math.ceil(votes / 2); // majority

  // ── Phase 1: Scope — decompose into complementary angles ──
  phase("Scope");
  const scope = await agent(
    `把下面这个研究问题拆解成 ${nAngles} 个互补的检索角度(覆盖:权威/主线、技术细节、` +
      `近期进展、反方/质疑、实践/落地 等不同侧面)。每个角度给 label、一个具体的 query、` +
      `以及一句 rationale。\n\n## 研究问题\n${question}\n\n只输出结构化结果。`,
    { label: "scope", sandbox: "read-only", schema: SCOPE_SCHEMA },
  );
  if (!scope || !scope.angles?.length) {
    return { error: "scope failed", question };
  }
  log(`分解为 ${scope.angles.length} 个角度: ${scope.angles.map((a) => a.label).join(", ")}`);

  // ── Phase 2: Research — one agent per angle, in parallel ──
  phase("Research");
  const perAngle = await parallel(
    scope.angles.map((a) => () =>
      agent(
        `你是“${a.label}”角度的研究员。\n研究问题: ${question}\n你的角度: ${a.label} — ` +
          `${a.rationale || ""}\n检索 query: ${a.query}\n\n${RESEARCH_HINT}\n\n` +
          `产出 2-5 条可证伪的 claim。只输出结构化结果。`,
        {
          label: `research:${a.label}`,
          sandbox: "danger-full-access",
          schema: FINDINGS_SCHEMA,
        },
      ),
    ),
  );
  const claims = perAngle
    .filter(Boolean)
    .flatMap((r) => (r.claims || []).map((c) => ({ ...c, angle: r.angle })))
    .sort((a, b) => impRank[a.importance] - impRank[b.importance])
    .slice(0, maxClaims);
  log(`收集到 ${claims.length} 条 claim(取 top ${maxClaims} 送验证)`);

  if (claims.length === 0) {
    return { question, summary: "未检索到任何 claim。", findings: [], stats: { angles: scope.angles.length, claims: 0 } };
  }

  // ── Phase 3: Verify — adversarial, `votes` skeptics per claim ──
  phase("Verify");
  const judged = await parallel(
    claims.map((c) => () =>
      parallel(
        Array.from({ length: votes }, (_, v) => () =>
          agent(
            `对下面这条 claim 做对抗式核查(第 ${v + 1}/${votes} 票)。要挑剔:尝试反驳它。\n\n` +
              `研究问题: ${question}\nclaim: "${c.claim}"\n` +
              `evidence: ${c.evidence || "(无)"}\nsource: ${c.source || "(无)"}\n\n` +
              `用你能用的方式查证(web/curl)。若 claim 无来源支撑/被可信来源否定/过时/明显是营销话术, ` +
              `则 refuted=true;只有当 claim 有据、当前、来源质量足够时 refuted=false;不确定时默认 refuted=true。` +
              `只输出结构化结果。`,
            { label: `verify:${c.claim.slice(0, 24)}`, sandbox: "danger-full-access", schema: VERDICT_SCHEMA },
          ),
        ),
      ).then((vs) => {
        const valid = vs.filter(Boolean);
        const refuted = valid.filter((v) => v.refuted).length;
        const survives = valid.length >= needRefute && refuted < needRefute;
        log(`"${c.claim.slice(0, 40)}…": ${valid.length - refuted}-${refuted} ${survives ? "OK" : "killed"}`);
        return { ...c, refuted, survives };
      }),
    ),
  );
  const confirmed = judged.filter((c) => c.survives);
  const killed = judged.filter((c) => !c.survives);
  log(`验证完成: ${confirmed.length} 条存活, ${killed.length} 条被否决`);

  // ── Phase 4: Synthesize — merge confirmed claims into a cited report ──
  phase("Synthesize");
  const block = confirmed
    .map((c, i) => `[${i}] (${c.angle}) ${c.claim}\n    evidence: ${c.evidence || "-"}\n    source: ${c.source || "-"}`)
    .join("\n");
  const report = await agent(
    `把下面这些通过对抗式核查的 claim 综合成一份研究报告(中文)。\n\n## 研究问题\n${question}\n\n` +
      `## 已核实的 claim\n${block || "(无)"}\n\n## 要求\n1. 合并语义重复项;` +
      `2. 归纳成若干 finding,每个给 confidence(high/medium/low)和 sources;` +
      `3. 写 3-5 句执行摘要回答问题;4. 写 caveats(不确定/来源弱/时效性);` +
      `5. 列 2-4 个开放问题。只输出结构化结果。`,
    { label: "synthesize", sandbox: "read-only", schema: REPORT_SCHEMA },
  );

  return {
    question,
    ...(report || { summary: "综合阶段失败", findings: [] }),
    refuted: killed.map((c) => c.claim),
    stats: {
      angles: scope.angles.length,
      claimsCollected: claims.length,
      confirmed: confirmed.length,
      killed: killed.length,
    },
  };
}
