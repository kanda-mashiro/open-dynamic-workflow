// Minimal workflow to prove the engine: JS DSL → op_agent → real codex.
// Run: the host loads this file, calls its default export with `args`.

export default async function run(args) {
  log(`workflow start; args=${JSON.stringify(args)}`);

  phase("Greet");
  // One real codex sub-agent. Keep it cheap + deterministic.
  const reply = await agent("Reply with exactly the word: pong", {
    label: "ping",
    sandbox: "read-only",
  });

  log(`agent replied: ${reply}`);
  return { reply, ok: reply.toLowerCase().includes("pong") };
}
