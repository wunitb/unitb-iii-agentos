# Build 10003 Attack Surface

Build 10003 changes governed documentation, repository ignore policy, and a
lockfile regression assertion. The recovered features it records cross several
runtime trust boundaries that reviewers should continue to protect.

## Process and configuration boundaries

- `agentos up` resolves and launches an iii executable, release worker
  binaries, and optionally the TUI. Executable paths, inherited environment,
  working directories, timeouts, process ownership, and failure cleanup are
  security-relevant. It must never terminate a process it did not start.
- Engine health on port 49134 and connected worker identity data are remote
  observations. Readiness is based on the required canonical identity set, not
  merely a count that unrelated workers could satisfy.
- `AGENTOS_CONFIG`, `AGENTOS_HOME`, checkout paths, and dotenv input cross from
  operator-controlled state into runtime discovery. Explicit configuration has
  precedence, paths are normalized, malformed input fails clearly, and doctor
  output must not reveal secrets.

## Registration and HTTP boundaries

- Worker and function identifiers select executable handlers. Canonical iii
  worker names prevent alias collisions, and the AgentOS-owned LLM namespace
  prevents another ecosystem router from silently receiving a request.
- Sessions and Security responses are untrusted remote JSON. The TUI must
  render data rather than interpret it as terminal control or executable input,
  and protected requests must construct a valid bearer header without logging
  the credential.
- Unsupported TUI routes are not queried. An explicit no-provider state avoids
  presenting absence, stale local configuration, or fabricated values as live
  service data.

## Repository and supply-chain boundaries

- `bun.lock` pins the root JavaScript dependency graph. Frozen installation is
  expected to fail rather than silently resolve a graph different from the
  committed lockfile, and repeated installation must be idempotent.
- `.gitignore` is a visibility boundary, not just a convenience list. Removing
  the AgentField-specific pattern makes runner output visible to operators and
  prevents this product repository from adopting policy for an external
  orchestration system.

These artifacts contain no credentials, live service responses, or claims of
external publication. The upstream duplicate-registration proposal is recorded
for principal review only.
