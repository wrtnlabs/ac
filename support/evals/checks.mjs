/**
 * Small host-neutral helpers shared by live suites and direct scenarios.
 *
 * Product drivers and assertions stay in the host. These helpers only enforce
 * the live-cost boundary and aggregate boolean checks into one readable result.
 */

export function liveEvaluationRefusal({
  environment = process.env,
  optIn = { name: "LIVE_AGENT", value: "1" },
  ciEnvironment = "CI",
} = {}) {
  if (environment[ciEnvironment]) {
    return "refusing to run live model evaluations in CI";
  }
  if (environment[optIn.name] !== optIn.value) {
    return `refusing to run without ${optIn.name}=${optIn.value} (live evaluations may consume credits)`;
  }
  return null;
}

export function requireLiveOptIn(
  name,
  {
    environment = process.env,
    optIn = { name: "LIVE_AGENT", value: "1" },
    ciEnvironment = "CI",
  } = {},
) {
  const refusal = liveEvaluationRefusal({ environment, optIn, ciEnvironment });
  if (refusal) {
    console.error(`${name}: ${refusal}.`);
    process.exit(2);
  }
}

export function createChecks() {
  const failures = [];
  const notes = [];
  return {
    failures,
    notes,
    check(condition, message) {
      if (!condition) failures.push(message);
      return condition;
    },
    note(message) {
      notes.push(message);
    },
    report(name, lines = []) {
      console.log(`\n── ${name} summary ${"─".repeat(Math.max(0, 44 - name.length))}`);
      for (const line of lines) console.log(line);
      console.log("─".repeat(62));
      for (const note of notes) console.log(`  note: ${note}`);
      if (failures.length > 0) {
        console.error(`\n${name}: FAIL`);
        for (const failure of failures) console.error(`  ✗ ${failure}`);
        process.exit(1);
      }
      console.log(`${name}: PASS`);
    },
  };
}
