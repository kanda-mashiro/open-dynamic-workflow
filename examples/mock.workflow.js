// Multi-agent workflow for TUI smoke: two phases, several agents, so the TUI
// has steps to list, agents to drill into, and streamed detail to scroll.

export default async function run(args) {
  log("mock workflow start");

  phase("Scan");
  const scan = await parallel(
    ["auth", "api", "db"].map((m) => () =>
      agent(`Reply with one short line about the ${m} module.`, {
        label: `scan:${m}`,
        sandbox: "read-only",
      })
    )
  );

  phase("Report");
  const rep = await agent("Reply with one short line: summary done.", {
    label: "report",
    sandbox: "read-only",
  });

  return { scanned: scan.length, report: rep.slice(0, 40) };
}
