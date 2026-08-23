# Build 10011 Decisions

## D-001 — Merge the required remote tips

The result merges both current `origin/main` and
`origin/issue/1897372b-remediate-artifact-1`. This makes the acceptance graph
properties durable while preserving the already-merged Herdr/salvage history,
including `238b423`.

## D-002 — Resolve every conflicting file deliberately

“Current reconciliation” is the pre-merge branch at `4e7d96f`; it already
contained the combined Herdr, portability, iii 0.22.1, and secure-routing work.
“Worker-identity tip” is the later PR #12 remediation. These are the complete
conflict decisions from both merges.

| Conflicting file | Side kept | Reason |
|---|---|---|
| `README.md` | Current reconciliation | Its documentation is the combined superset; non-conflicting PR #12 wording still merged automatically. |
| `bun.lock` | Current reconciliation | Keep the dependency state already validated with the merged AgentOS package set instead of selecting main's older add/add variant. |
| `crates/cli/src/main.rs` | Current reconciliation | Preserve workflow commands together with caller-anchored portable paths and the extended bootstrap/doctor surface. |
| `crates/cli/tests/portability.rs` | Current reconciliation | Preserve the broader regressions and the required canonicalized temporary-runtime path assertions. |
| `e2e/full-stack.test.ts` | Current reconciliation | Keep deferred iii SDK loading so the real 0.22.1 boot regression controls setup and cleanup safely. |
| `package-lock.json` | Current reconciliation | Retain the committed compatibility lock instead of main's deletion; the Bun lock remains authoritative for frozen Bun installs. |
| `scripts/dev-up.test.ts` | Current reconciliation | Keep the expanded secure dotenv and portable invocation cases already paired with the reconciled script. |
| `workers/agent-core/src/main.rs` | Current reconciliation | Preserve routed provider/model propagation, AgentOS-owned LLM identifiers, system prompts, and agent tool forwarding. |
| `workers/agent-core/src/types.rs` | Current reconciliation | Preserve the established `FunctionCall { callId, id, arguments }` boundary and its larger edge-case suite. |
| `workers/context-monitor/src/main.rs` | Current reconciliation | Keep the extended monitoring implementation and collision-safe AgentOS LLM consumer call. |
| `workers/eval/src/main.rs` | Current reconciliation | Keep evaluation behavior while retaining the migrated AgentOS LLM function ID. |
| `workers/evolve/src/main.rs` | Current reconciliation | Keep the extended evolution and sandbox rules with the migrated AgentOS LLM function ID. |
| `workers/llm-router/src/main.rs` | Current reconciliation, then extended | Preserve secure Codex routing and provider catalogs, then finish native tool translation and normalized function-call output. |
| `workers/memory/src/main.rs` | Current reconciliation | Keep the Herdr memory/session superset and its collision-safe AgentOS LLM consumer. |
| `workers/streaming/src/main.rs` | Current reconciliation | Preserve streaming route preferences and top-level route fields with collision-safe AgentOS LLM calls. |
| `tests/artifact_contract.test.ts` | Combined | Retain build 10010 reconciliation checks and add build 10008 worker-identity evidence checks from the remediation tip. |

## D-003 — Finish chat migration at the router boundary

The remaining orchestrator and task-decomposer callers now target
`agentos::llm::chat`. The router registers that canonical function as a chat
alias over the same completion pipeline, and the namespace test treats `chat`
like `complete`, `route`, `providers`, and `usage` in its repository scan.

## D-004 — Translate tools, normalize calls

The router accepts engine/agent tool metadata aliases at its internal boundary
and emits each external provider's native declaration shape. Anthropic,
OpenAI-compatible, and Gemini response shapes are converted back into one
typed `FunctionCall` contract so agent-core never depends on provider JSON.
