# Build 10003 Decisions

## D-001 — Preserve the recovered implementation

The six acceptance items are represented by focused implementation commits and
regression tests already present in this checkout. Build 10003 supplies the
missing governed evidence and removes an unrelated ignore-policy change; it
does not duplicate or redesign the recovered features.

## D-002 — Use canonical runtime identities

The iii 0.22.1 names `queue`, `state`, and `cron` are authoritative in both the
top-level configuration and their configuration filenames. A live boot test is
retained because static alias checks alone cannot prove that the standalone
queue worker registers `engine::queue::enqueue`.

## D-003 — Isolate AgentOS LLM functions

All four router functions use `agentos::llm::*`, including internal consumers.

NOTE for the principal: an engine-side error for duplicate function
registrations belongs upstream in `iii-hq/iii`. Decide whether to open an
upstream issue about the empty-turn failure mode; this delivery does not post
externally.

## D-004 — Tell the truth on every TUI screen

| Screen | Route | Decision |
|---|---|---|
| Dashboard, Agents, Skills | `GET /api/dashboard/stats` | No provider in this release; render the route in an explicit no-provider state. |
| Sessions | `GET /api/sessions` | Keep live through the memory worker provider. |
| Security | `GET /api/security` | Keep live through the security worker provider. |
| Logs | `GET /api/dashboard/logs` | No provider in this release; render an explicit no-provider state. |
| Audit Events | `GET /api/dashboard/events` | No provider in this release; render an explicit no-provider state. |
| Settings | `GET /api/settings` | No provider in this release; render an explicit no-provider state and do not substitute local YAML. |

## D-005 — Separate orchestration from observation

`agentos up` owns ordered startup and foreground TUI launch. `agentos doctor`
only observes readiness and reports a precise remediation for each failed
check. Configuration selection follows the documented `AGENTOS_CONFIG`,
checkout, then installed-runtime precedence; `AGENTOS_HOME` alone does not
disable checkout discovery.

## D-006 — Normalize filesystem aliases in tests

Canonicalizing the expected runtime directory unconditionally is simpler and
equally correct on Linux and macOS. It fixes `/var` versus `/private/var`
without weakening the working-directory assertion.

## D-007 — Keep source-control policy product-scoped

The root lockfile remains committed and is verified by tests and repeated
frozen installs. The `.agentfield-out-*/` rule is removed because AgentField
runner state is not an AgentOS-generated product artifact; repository policy
must not silently hide it.
