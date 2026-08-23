# Build 10008 Invariants

## Startup invariants

- `None` from the engine's worker-identity query is unknown state; it is never
  interpreted as evidence that zero workers are connected.
- An unanswered identity query is retried only within the existing bounded
  `poll_plan` budget. Exhausting that budget returns the same diagnostic used
  by readiness: "the engine did not report connected worker identities".
- Worker processes are not spawned until the engine reports a concrete identity
  set. `Some(empty)` remains an authoritative report and starts every required
  worker, while a partial report starts only the missing identities.
- A failed invocation tears down only processes that invocation started.

## Artifact invariants

- The build directory contains exactly `ATTACK_SURFACE.md`, `DECISIONS.md`,
  `INVARIANTS.md`, and `TRACES.md` as regular UTF-8 Markdown files.
- `TRACES.md` records `ISC-000`, `ISC-001`, and `ISC-002` as whole tokens and
  maps each acceptance item to executable or reviewable repository evidence.
