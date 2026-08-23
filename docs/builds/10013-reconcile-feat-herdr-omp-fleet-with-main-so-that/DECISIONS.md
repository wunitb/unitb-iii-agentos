# Build 10013 Decisions

## D-001 — Preserve the reconciled histories

The follow-up stays on the existing reconciliation graph, retaining both
required remote tips and `238b423`. It does not recreate either line of work
as a content-only copy.

## D-002 — Retain every prior conflict decision

“Current reconciliation” denotes the branch that combined the Herdr,
portability, iii 0.22.1, and secure-routing work. “Worker-identity tip” denotes
the later fail-closed remediation. The table records every conflicting file
from those merges and the chosen resolution.

| Conflicting file | Side kept | Reason |
|---|---|---|
| `README.md` | Current reconciliation | Keep the combined documentation while allowing non-conflicting worker-identity wording to merge. |
| `bun.lock` | Current reconciliation | Retain the dependency graph validated with the reconciled AgentOS package set. |
| `crates/cli/src/main.rs` | Current reconciliation | Preserve workflow commands, caller-anchored paths, and the expanded bootstrap and doctor surface. |
| `crates/cli/tests/portability.rs` | Current reconciliation | Preserve the broader portability regressions and canonicalized temporary-path assertions. |
| `e2e/full-stack.test.ts` | Current reconciliation | Keep deferred SDK loading so iii 0.22.1 boot setup and cleanup remain controlled by the regression harness. |
| `package-lock.json` | Current reconciliation | Retain the committed compatibility lock while Bun remains authoritative for frozen root installs. |
| `scripts/dev-up.test.ts` | Current reconciliation | Keep the secure dotenv and caller-independent portable invocation coverage. |
| `workers/agent-core/src/main.rs` | Current reconciliation, then extended | Preserve routed completion and tool execution, then reject empty normalized identifiers and stop all-invalid loops. |
| `workers/agent-core/src/types.rs` | Current reconciliation | Preserve the established serialized `FunctionCall { callId, id, arguments }` contract. |
| `workers/context-monitor/src/main.rs` | Current reconciliation | Keep the extended monitor and its collision-safe AgentOS LLM call. |
| `workers/eval/src/main.rs` | Current reconciliation | Keep evaluation behavior with the migrated AgentOS LLM identifier. |
| `workers/evolve/src/main.rs` | Current reconciliation | Keep the evolution and sandbox rules with the migrated AgentOS LLM identifier. |
| `workers/llm-router/src/main.rs` | Current reconciliation, then extended | Preserve provider-native tool translation, then validate normalized identifiers while retaining Gemini ID synthesis. |
| `workers/memory/src/main.rs` | Current reconciliation | Keep the Herdr memory/session superset and AgentOS LLM namespace. |
| `workers/streaming/src/main.rs` | Current reconciliation | Preserve streaming route preferences and top-level route fields with AgentOS LLM calls. |
| `tests/artifact_contract.test.ts` | Combined | Retain existing reconciliation evidence checks and add the governed build 10013 contract. |

## D-003 — Keep Gemini's optional-ID compatibility

The common normalized constructor rejects empty identifiers. Gemini differs at
its provider boundary because the API may omit a call ID: a missing or empty
string is replaced with `gemini-<candidate-index>-<part-index>` before entering
the normalized boundary. A present non-string ID is still malformed and is
discarded.

## D-004 — Stop an all-invalid continuation

Agent-core filters provider output into typed calls before capability checks.
When filtering produces no calls, it exits the loop immediately. This avoids
adding the malformed provider array to conversation history and avoids a
second completion request that could fail provider schema validation.
