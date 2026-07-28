# Evaluation runner

`runner.mjs` is AC's host-neutral orchestration layer for opt-in live
acceptance suites. It owns the repeated harness mechanics:

- scenario discovery and selection;
- explicit live-cost opt-in and CI refusal;
- build → deterministic preflight → live-scenario ordering;
- child-process supervision and signal forwarding;
- expensive-scenario classification and summary reporting.

`checks.mjs` carries the matching direct-scenario opt-in guard and a tiny
boolean-check accumulator. It deliberately knows nothing about a host's event
or artifact shapes.

The consuming host supplies all variation: a plan of setup/preflight/scenario
commands around its own opaque target, its scenario filename convention,
default and expensive cases, prompts, fixture data, protocol driver, and
assertions. The target may be a binary, URL, service, library fixture, or
anything else; AC never inspects it. Plan creation is a pure description step:
dry-run invokes it without live-cost opt-in, while setup, validation, preflight,
and scenarios run only after the opt-in/CI guard. An asynchronous validator
receives `{ signal }` and should stop promptly when it is aborted. In
particular, this directory must never contain a consuming application's names,
paths, wire methods, deployment assumptions, or expected artifacts.

Run the hermetic unit checks with:

```sh
node support/evals/runner.test.mjs
```
