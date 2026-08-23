# Build 10005 Invariants

## Artifact invariants

- The governed path is a canonical on-disk directory, not a symbolic link or
  another non-directory filesystem object.
- `ATTACK_SURFACE.md`, `DECISIONS.md`, `INVARIANTS.md`, and `TRACES.md` are
  regular files and are the complete governed evidence set.
- Every artifact is valid UTF-8, is at least 200 UTF-8 bytes long, and contains
  a Markdown heading matching the artifact gate's required form.
- `TRACES.md` names `ISC-000` through `ISC-005` as whole tokens and maps each
  identifier to concrete repository evidence.

## Runtime invariants inherited from the salvage

- Checkout configuration uses iii 0.22.1's canonical `queue`, `state`, and
  `cron` worker identifiers. The queue worker owns
  `engine::queue::enqueue`; deprecated aliases neither coexist with nor replace
  canonical workers.
- AgentOS owns `agentos::llm::*` end to end. No router registration or source
  consumer uses the ecosystem-level `llm::complete`, `llm::route`,
  `llm::providers`, or `llm::usage` identifiers.
- Sessions and Security remain provider-backed. Dashboard, Agents, Skills,
  Logs, Audit Events, and Settings identify their missing routes and display
  an explicit no-provider state instead of fabricated or stale data.
- `agentos up` checks prerequisites in order, waits for engine health on port
  49134 within a bound, starts only missing processes, and launches the TUI in
  the foreground unless `--no-tui` is selected. Spawning and readiness
  observations remain mockable.
- `agentos doctor` is read-only and reports the iii binary and version, engine
  health, required connected worker identities, TUI binary presence, and the
  active config-discovery mode with precise remediation for failures.
- Non-empty `AGENTOS_CONFIG` selects explicit config. Otherwise checkout
  discovery remains available even when `AGENTOS_HOME` is set, with the
  installed runtime as fallback.
- Portability tests canonicalize expected working directories so macOS
  `/var` and `/private/var` aliases do not cause false failures.
- The committed root `bun.lock` is unchanged by repeated frozen installs, and
  TypeScript checking plus the Bun suite remain green.
