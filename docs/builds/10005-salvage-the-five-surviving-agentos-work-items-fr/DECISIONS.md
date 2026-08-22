# Build 10005 Decisions

## D-001 — Preserve the recovered implementation

The six acceptance items are already represented by focused implementation
and regression coverage in this checkout. Build 10005 finalizes the missing
governed evidence and extends its executable artifact contract; it does not
duplicate or redesign the recovered runtime features.

## D-002 — Keep AgentOS-owned function names isolated

The iii 0.22.1 worker identifiers `queue`, `state`, and `cron` are canonical,
and the standalone queue worker must provide `engine::queue::enqueue` after a
live boot. AgentOS router registrations and internal consumers use
`agentos::llm::*` end to end rather than the ecosystem-level `llm::*` names.

NOTE for the principal: an engine-side error for duplicate function
registrations belongs upstream in `iii-hq/iii`. Decide whether to open an
upstream issue about the empty-assistant-turn failure mode; this delivery does
not post externally.

## D-003 — Prefer explicit operational truth

`agentos up` performs ordered, bounded startup and launches the TUI unless
`--no-tui` is selected. `agentos doctor` only observes readiness and reports a
specific remediation for each failed check. Configuration precedence is
`AGENTOS_CONFIG`, checkout discovery, then the installed runtime under
`AGENTOS_HOME`; setting `AGENTOS_HOME` alone does not suppress checkout
discovery. Portability tests canonicalize expected temporary paths before
comparing them with a child's `$PWD`.

| Screen | Route | Decision |
|---|---|---|
| Dashboard, Agents, Skills | `GET /api/dashboard/stats` | No provider in this release; display an explicit no-provider state. |
| Sessions | `GET /api/sessions` | Keep live through the memory worker provider. |
| Security | `GET /api/security` | Keep live through the security worker provider. |
| Logs | `GET /api/dashboard/logs` | No provider in this release; display an explicit no-provider state. |
| Audit Events | `GET /api/dashboard/events` | No provider in this release; display an explicit no-provider state. |
| Settings | `GET /api/settings` | No provider in this release; display an explicit no-provider state without substituting local YAML. |

The root `bun.lock` remains committed. `bun install --frozen-lockfile` is the
authoritative reproducibility check and must leave the checkout unchanged both
immediately and when repeated.

## D-004 — Make the artifact contract executable

The build-10005 directory contains exactly the four required Markdown files.
The repository test verifies a canonical real directory, regular UTF-8 files
of at least 200 bytes, Markdown headings, and whole-token trace coverage for
`ISC-000` through `ISC-005`. Its section topology follows build 10000 so the
two heading lists can be compared mechanically.
