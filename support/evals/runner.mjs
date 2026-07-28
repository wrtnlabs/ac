/**
 * Generic live-evaluation suite orchestration.
 *
 * The agent kit owns the repeated mechanics: scenario discovery and selection,
 * explicit live-cost opt-in, CI refusal, one optional build, one deterministic
 * preflight, child supervision, signal forwarding, and a compact summary.
 *
 * Hosts own every noun and policy choice. They provide the suite name, scenario
 * filenames, commands, environment keys, and default/expensive classifications.
 * This module deliberately knows nothing about a host's daemon, protocol,
 * prompts, fixtures, or assertions.
 */

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { liveEvaluationRefusal } from "./checks.mjs";

export function discoverEvalScenarios({
  directory,
  filenamePattern,
  idFromFilename,
  aliases = new Map(),
  defaultScenario,
  expensiveScenarios = new Set(),
}) {
  const scenarios = fs
    .readdirSync(directory, { withFileTypes: true })
    .filter((entry) => entry.isFile() && filenamePattern.test(entry.name))
    .map((entry) => {
      const id = aliases.get(entry.name) ?? idFromFilename(entry.name);
      return {
        id,
        file: path.join(directory, entry.name),
        expensive: expensiveScenarios.has(id),
      };
    })
    .sort((left, right) => {
      if (left.id === defaultScenario) return -1;
      if (right.id === defaultScenario) return 1;
      if (left.expensive !== right.expensive) return left.expensive ? 1 : -1;
      return left.id.localeCompare(right.id);
    });

  const seen = new Set();
  for (const scenario of scenarios) {
    if (seen.has(scenario.id)) {
      throw new Error(`duplicate live eval scenario id: ${scenario.id}`);
    }
    seen.add(scenario.id);
  }
  return scenarios;
}

export function parseEvalArgs(argv) {
  const options = {
    all: false,
    includeExpensive: false,
    only: [],
    list: false,
    dryRun: false,
    help: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--") continue;
    if (arg === "--all" || arg === "--full") {
      options.all = true;
      continue;
    }
    if (arg === "--include-expensive") {
      options.includeExpensive = true;
      continue;
    }
    if (arg === "--list") {
      options.list = true;
      continue;
    }
    if (arg === "--dry-run") {
      options.dryRun = true;
      continue;
    }
    if (arg === "--help" || arg === "-h") {
      options.help = true;
      continue;
    }
    if (arg === "--only") {
      const value = argv[index + 1];
      if (!value || value.startsWith("--")) {
        throw new Error("--only requires a comma-separated scenario list");
      }
      options.only.push(
        ...value
          .split(",")
          .map((item) => item.trim())
          .filter(Boolean),
      );
      index += 1;
      continue;
    }
    throw new Error(`unknown argument: ${arg}`);
  }

  if (options.all && options.only.length > 0) {
    throw new Error("--full/--all cannot be combined with --only");
  }
  return options;
}

export function selectEvalScenarios(scenarios, options, defaultScenario) {
  const byId = new Map(scenarios.map((scenario) => [scenario.id, scenario]));
  let selected;

  if (options.only.length > 0) {
    const unknown = options.only.filter((id) => !byId.has(id));
    if (unknown.length > 0) {
      throw new Error(`unknown scenario${unknown.length === 1 ? "" : "s"}: ${unknown.join(", ")}`);
    }
    selected = options.only.map((id) => byId.get(id));
  } else if (options.all) {
    selected = scenarios.filter((scenario) => options.includeExpensive || !scenario.expensive);
  } else {
    const regular = byId.get(defaultScenario);
    if (!regular) {
      throw new Error(`default scenario is missing: ${defaultScenario}`);
    }
    selected = [regular];
    if (options.includeExpensive) {
      selected.push(...scenarios.filter((scenario) => scenario.expensive));
    }
  }

  const unique = [...new Map(selected.map((scenario) => [scenario.id, scenario])).values()];
  if (unique.length === 0) {
    throw new Error("no live eval scenarios were selected");
  }
  return unique;
}

function elapsed(startedAt) {
  return `${((Date.now() - startedAt) / 1000).toFixed(1)}s`;
}

function printScenarioList(scenarios, defaultScenario) {
  console.log("Live eval scenarios:");
  for (const scenario of scenarios) {
    const flags = [
      scenario.id === defaultScenario ? "regular default" : null,
      scenario.expensive ? "expensive" : null,
    ].filter(Boolean);
    console.log(
      `  ${scenario.id.padEnd(20)} ${flags.length > 0 ? `[${flags.join(", ")}] ` : ""}${path.basename(scenario.file)}`,
    );
  }
}

/**
 * Run a host-supplied suite through one common orchestration path.
 *
 * Command objects have `{label, command, args, cwd?, env?}`. `createPlan`
 * receives the parsed options, selected scenarios, and environment, and
 * returns host-owned setup/preflight/scenario command specs around any opaque
 * target it closed over. Plan creation must be descriptive and side-effect
 * free because dry-run calls it without live-cost opt-in. AC never assumes
 * that target is a binary or process.
 */
export async function runEvalSuite({
  argv,
  parseOptions = parseEvalArgs,
  suiteName,
  usage,
  cwd,
  scenarios,
  defaultScenario,
  createPlan,
  liveOptIn = { name: "LIVE_AGENT", value: "1" },
  ciEnvironment = "CI",
  environment = process.env,
  signalTarget = process,
}) {
  let options;
  try {
    options = parseOptions(argv);
  } catch (error) {
    console.error(`eval: ${error.message}\n\n${usage}`);
    return 2;
  }

  if (options.help) {
    process.stdout.write(usage);
    return 0;
  }
  if (options.list) {
    printScenarioList(scenarios, defaultScenario);
    return 0;
  }

  let selected;
  try {
    selected = selectEvalScenarios(scenarios, options, defaultScenario);
  } catch (error) {
    console.error(`eval: ${error.message}\n\n${usage}`);
    return 2;
  }

  if (!options.dryRun) {
    const refusal = liveEvaluationRefusal({ environment, optIn: liveOptIn, ciEnvironment });
    if (refusal) {
      console.error(`eval: ${refusal}`);
      return 2;
    }
  }

  let plan;
  try {
    plan = await createPlan({ options, selected, environment });
  } catch (error) {
    console.error(`eval: ${error.message}`);
    return 2;
  }
  console.log(suiteName);
  for (const line of plan.summaryLines ?? []) console.log(`  ${line}`);
  console.log(`  scenarios: ${selected.map((scenario) => scenario.id).join(", ")}`);
  console.log(
    `  expensive scenarios: ${
      selected
        .filter((scenario) => scenario.expensive)
        .map((scenario) => scenario.id)
        .join(", ") || "none"
    }`,
  );

  if (options.dryRun) {
    const actions = [
      plan.setupCommand?.label,
      plan.preflightCommand?.label,
      selected.length > 0 ? "live scenarios" : null,
    ].filter(Boolean);
    console.log(`  actions: ${actions.join(" → ")}`);
    return 0;
  }

  let activeChild = null;
  let interruptedSignal = null;
  const validationAbort = new AbortController();
  const listeners = [];
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    const listener = () => {
      if (!interruptedSignal) {
        interruptedSignal = signal;
        validationAbort.abort(signal);
      }
      activeChild?.kill(signal);
    };
    signalTarget.once(signal, listener);
    listeners.push([signal, listener]);
  }

  const runProcess = (spec, inheritedEnvironment) => {
    const startedAt = Date.now();
    console.log(`\n━━ ${spec.label} ━━`);
    return new Promise((resolve) => {
      const child = spawn(spec.command, spec.args, {
        cwd: spec.cwd ?? cwd,
        env: { ...inheritedEnvironment, ...spec.env },
        stdio: "inherit",
      });
      activeChild = child;
      let settled = false;
      const finish = (result) => {
        if (settled) return;
        settled = true;
        if (activeChild === child) activeChild = null;
        resolve({ label: spec.label, elapsed: elapsed(startedAt), ...result });
      };
      child.once("error", (error) => finish({ code: 2, signal: null, error }));
      child.once("close", (code, signal) =>
        finish({ code: code ?? (signal ? 1 : 2), signal, error: null }),
      );
    });
  };

  const report = (results) => {
    console.log("\n━━ eval summary ━━");
    for (const result of results) {
      const status =
        result.code === 0 ? "PASS" : result.signal ? `SIGNAL ${result.signal}` : "FAIL";
      console.log(`  ${status.padEnd(10)} ${result.elapsed.padStart(7)}  ${result.label}`);
      if (result.error) console.log(`             ${result.error.message}`);
    }
    if (interruptedSignal) {
      return 128 + (os.constants.signals[interruptedSignal] ?? 0);
    }
    if (results.some((result) => result.code === 2)) return 2;
    return results.every((result) => result.code === 0) ? 0 : 1;
  };

  try {
    const results = [];
    const childEnvironment = {
      ...environment,
      ...plan.environment,
      [liveOptIn.name]: liveOptIn.value,
    };
    if (interruptedSignal) return report(results);
    if (plan.setupCommand) {
      const setup = await runProcess(plan.setupCommand, childEnvironment);
      results.push(setup);
      if (setup.code !== 0 || interruptedSignal) return report(results);
    }

    try {
      await plan.validate?.({ signal: validationAbort.signal });
    } catch (error) {
      if (interruptedSignal) return report(results);
      console.error(`eval: ${error.message}`);
      return 2;
    }
    if (interruptedSignal) return report(results);

    if (plan.preflightCommand) {
      if (interruptedSignal) return report(results);
      const preflight = await runProcess(plan.preflightCommand, childEnvironment);
      results.push(preflight);
      if (preflight.code !== 0 || interruptedSignal) return report(results);
    }

    for (const scenario of selected) {
      if (interruptedSignal) break;
      const result = await runProcess(plan.scenarioCommand(scenario), childEnvironment);
      results.push(result);
      if (interruptedSignal) break;
    }
    return report(results);
  } finally {
    for (const [signal, listener] of listeners) signalTarget.removeListener(signal, listener);
  }
}
