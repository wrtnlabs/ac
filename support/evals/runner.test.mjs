import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  discoverEvalScenarios,
  parseEvalArgs,
  runEvalSuite,
  selectEvalScenarios,
} from "./runner.mjs";
import { liveEvaluationRefusal } from "./checks.mjs";

function testEnvironment(overrides = {}) {
  const environment = { ...process.env };
  // `node --test` uses this private channel for its own child protocol. The
  // fixture commands are ordinary subprocesses, not nested test workers.
  delete environment.NODE_TEST_CONTEXT;
  // The suite correctly refuses real live evaluations in CI. These fixtures
  // exercise orchestration with inert local commands, so keep the ambient
  // runner marker from turning every non-dry fixture into an early refusal.
  delete environment.CI;
  return { ...environment, LIVE_AGENT: "1", ...overrides };
}

test("discovers, aliases, classifies, and orders scenarios", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ac-evals-"));
  try {
    for (const name of ["live-slow.mjs", "live-main.mjs", "live-aliased.mjs", "ignore.txt"]) {
      fs.writeFileSync(path.join(dir, name), "");
    }
    const scenarios = discoverEvalScenarios({
      directory: dir,
      filenamePattern: /^live-[a-z-]+\.mjs$/,
      idFromFilename: (name) => name.slice(5, -4),
      aliases: new Map([["live-aliased.mjs", "renamed"]]),
      defaultScenario: "main",
      expensiveScenarios: new Set(["slow"]),
    });
    assert.deepEqual(
      scenarios.map(({ id, expensive }) => ({ id, expensive })),
      [
        { id: "main", expensive: false },
        { id: "renamed", expensive: false },
        { id: "slow", expensive: true },
      ],
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("selection keeps expensive scenarios explicit", () => {
  const scenarios = [
    { id: "main", expensive: false },
    { id: "other", expensive: false },
    { id: "slow", expensive: true },
  ];
  assert.deepEqual(
    selectEvalScenarios(scenarios, parseEvalArgs([]), "main").map(({ id }) => id),
    ["main"],
  );
  assert.deepEqual(
    selectEvalScenarios(scenarios, parseEvalArgs(["--full"]), "main").map(({ id }) => id),
    ["main", "other"],
  );
  assert.deepEqual(
    selectEvalScenarios(
      scenarios,
      parseEvalArgs(["--full", "--include-expensive"]),
      "main",
    ).map(({ id }) => id),
    ["main", "other", "slow"],
  );
  assert.deepEqual(
    selectEvalScenarios(scenarios, parseEvalArgs(["--only", "slow"]), "main").map(({ id }) => id),
    ["slow"],
  );
  assert.throws(
    () =>
      selectEvalScenarios(
        [{ id: "slow", expensive: true }],
        parseEvalArgs(["--full"]),
        "slow",
      ),
    /no live eval scenarios were selected/,
  );
});

test("argument parser rejects ambiguous selection", () => {
  assert.throws(
    () => parseEvalArgs(["--full", "--only", "main"]),
    /cannot be combined/,
  );
  assert.throws(() => parseEvalArgs(["--bin", "./agent"]), /unknown argument/);
});

test("one live-cost validator serves suites and direct scenarios", () => {
  assert.equal(
    liveEvaluationRefusal({ environment: { CI: "1", LIVE_AGENT: "1" } }),
    "refusing to run live model evaluations in CI",
  );
  assert.match(
    liveEvaluationRefusal({ environment: {} }),
    /refusing to run without LIVE_AGENT=1/,
  );
  assert.equal(liveEvaluationRefusal({ environment: { LIVE_AGENT: "1" } }), null);
});

function fixtureSuite({
  argv = ["--full"],
  environment = testEnvironment(),
  createPlan,
  signalTarget = new EventEmitter(),
} = {}) {
  return runEvalSuite({
    argv,
    suiteName: "fixture suite",
    usage: "fixture usage",
    cwd: process.cwd(),
    scenarios: [
      { id: "main", file: "main.fixture", expensive: false },
      { id: "other", file: "other.fixture", expensive: false },
    ],
    defaultScenario: "main",
    environment,
    createPlan,
    signalTarget,
  });
}

function recordCommand(log, label, { code = 0, env } = {}) {
  const program = [
    'const fs = require("node:fs");',
    "const [log, label, code] = process.argv.slice(1);",
    "fs.appendFileSync(log, JSON.stringify({",
    "  label,",
    "  inherited: process.env.AC_EVAL_INHERITED,",
    "  planned: process.env.AC_EVAL_PLANNED,",
    "  specific: process.env.AC_EVAL_SPECIFIC,",
    '}) + "\\n");',
    "process.exit(Number(code));",
  ].join("\n");
  return {
    label,
    command: process.execPath,
    args: ["-e", program, log, label, String(code)],
    env,
  };
}

test("suite orchestration orders commands and composes environments", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ac-eval-suite-"));
  const log = path.join(dir, "events.jsonl");
  try {
    const code = await fixtureSuite({
      environment: {
        ...testEnvironment({ AC_EVAL_INHERITED: "inherited" }),
      },
      createPlan() {
        return {
          environment: { AC_EVAL_PLANNED: "planned" },
          setupCommand: recordCommand(log, "setup"),
          validate() {
            fs.appendFileSync(log, `${JSON.stringify({ label: "validate" })}\n`);
          },
          preflightCommand: recordCommand(log, "preflight", {
            env: { AC_EVAL_SPECIFIC: "preflight-only" },
          }),
          scenarioCommand: (scenario) => recordCommand(log, `scenario:${scenario.id}`),
        };
      },
    });

    assert.equal(code, 0);
    const events = fs
      .readFileSync(log, "utf8")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.deepEqual(
      events.map((event) => event.label),
      ["setup", "validate", "preflight", "scenario:main", "scenario:other"],
    );
    for (const event of events.filter((event) => event.label !== "validate")) {
      assert.equal(event.inherited, "inherited");
      assert.equal(event.planned, "planned");
    }
    assert.equal(events[2].specific, "preflight-only");
    assert.equal(events[3].specific, undefined);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("cost refusal precedes plan creation while dry-run stays descriptive", async () => {
  let planCalls = 0;
  const createPlan = () => {
    planCalls += 1;
    return { scenarioCommand: () => assert.fail("dry-run must not launch commands") };
  };

  const refused = await fixtureSuite({ argv: [], environment: {}, createPlan });
  assert.equal(refused, 2);
  assert.equal(planCalls, 0);

  const dry = await fixtureSuite({ argv: ["--dry-run"], environment: {}, createPlan });
  assert.equal(dry, 0);
  assert.equal(planCalls, 1);
});

test("a failing setup short-circuits validation, preflight, and scenarios", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ac-eval-fail-"));
  const log = path.join(dir, "events.jsonl");
  let validated = false;
  try {
    const code = await fixtureSuite({
      createPlan() {
        return {
          setupCommand: recordCommand(log, "setup", { code: 1 }),
          validate() {
            validated = true;
          },
          preflightCommand: recordCommand(log, "preflight"),
          scenarioCommand: (scenario) => recordCommand(log, `scenario:${scenario.id}`),
        };
      },
    });

    assert.equal(code, 1);
    assert.equal(validated, false);
    assert.deepEqual(
      fs
        .readFileSync(log, "utf8")
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line).label),
      ["setup"],
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("an interrupt during async validation aborts it and launches no child", async () => {
  const signalTarget = new EventEmitter();
  let validationAborted = false;
  let preflightLaunched = false;

  const code = await fixtureSuite({
    signalTarget,
    createPlan() {
      return {
        async validate({ signal }) {
          signalTarget.emit("SIGTERM");
          await new Promise((resolve) => setImmediate(resolve));
          validationAborted = signal.aborted;
        },
        preflightCommand: {
          label: "preflight",
          command: process.execPath,
          args: ["-e", "process.exit(0)"],
        },
        scenarioCommand() {
          preflightLaunched = true;
          throw new Error("scenario command must not be constructed after an interrupt");
        },
      };
    },
  });

  assert.equal(code, 143);
  assert.equal(validationAborted, true);
  assert.equal(preflightLaunched, false);
});
