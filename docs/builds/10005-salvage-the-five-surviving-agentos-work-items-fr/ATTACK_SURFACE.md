# Build 10005 Attack Surface

This delivery adds governed documentation and an artifact-contract regression
test. The salvaged implementation it records crosses process, configuration,
registration, HTTP, terminal-rendering, filesystem, and dependency boundaries.

## Trust boundaries

- `agentos up` resolves and launches the iii engine, release workers, and the
  optional foreground TUI. Executable paths, inherited environment, working
  directories, timeouts, process ownership, and failure cleanup are trusted
  only for the current invocation; it must not terminate processes it did not
  start.
- Engine health on port 49134 and connected worker identities are remote
  observations. Readiness depends on the required canonical identity set, not
  on an aggregate count that unrelated workers could satisfy.
- `AGENTOS_CONFIG`, `AGENTOS_HOME`, checkout paths, and dotenv input cross from
  operator-controlled state into configuration discovery. Explicit config has
  precedence, paths are normalized, malformed input fails clearly, and
  diagnostics do not reveal credentials.
- Worker and function identifiers select executable handlers. Canonical iii
  0.22.1 worker names avoid deprecated-alias collisions, while the
  `agentos::llm::*` namespace prevents an ecosystem router from silently
  receiving AgentOS requests.
- Provider responses are untrusted remote JSON. Sessions and Security render
  provider-backed data; unsupported Dashboard, Agents, Skills, Logs, Audit
  Events, and Settings surfaces do not query absent routes or invent values.
- `bun.lock` pins the root JavaScript dependency graph. Frozen installation
  must fail instead of silently resolving a different graph and must be
  idempotent when repeated.

## Review posture

These artifacts contain static repository evidence, not credentials, live
service output, approval claims, or external-publication claims. The iii engine
collision-error proposal is recorded for principal review only. Fresh command
outcomes belong in the delivery report rather than being pre-recorded here.
