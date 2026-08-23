# Build 10010 Attack Surface

The reconciliation introduces no new network protocol or credential flow, but
it preserves code that crosses process, filesystem, configuration, dependency,
and function-routing trust boundaries.

## Trust boundaries

- `agentos up` starts local executables and observes engine health plus worker
  identities. A successful command with no parseable identity answer is not
  proof of readiness; unknown, unrelated, or partial reports fail closed.
- Runtime discovery accepts operator-controlled `AGENTOS_CONFIG`,
  `AGENTOS_HOME`, checkout paths, and executable search paths. Portable
  installation must not embed a build-machine path, and upgrade must not
  replace mutable runtime state.
- Worker manifests select executable code and bus identities. Canonical
  `state`, `queue`, and `cron` identities are security-relevant because legacy
  aliases can create duplicate or misleading providers.
- Function identifiers route data to handlers on the iii bus. Restricting
  AgentOS LLM registrations and calls to `agentos::llm::*` prevents silent
  collisions with ecosystem-level `llm::*` functions.
- The full-stack boot test starts a real pinned iii 0.22.1 process. Deferred
  SDK loading, bounded readiness, and cleanup keep module initialization from
  leaking connections or processes outside the test lifecycle.
- Rust and JavaScript lockfiles constrain dependency resolution. Frozen Bun
  installation must fail on drift and must not mutate tracked or untracked
  repository state.

## Review posture

Merge ancestry is intentional and reviewable: both source histories remain
reachable, while the reconciled content matches their previously agreed tree
except for this governed evidence and its test. These artifacts contain no
secrets, live service data, external publication, or claims that verification
ran; command results belong in the final delivery report.
