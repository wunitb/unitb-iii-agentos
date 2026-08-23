# Build 10013 Invariants

## History and repository integrity

- `origin/main`, `origin/issue/1897372b-remediate-artifact-1`, and commit
  `238b423` remain ancestors of the delivered commit.
- Repository files contain no unresolved Git conflict-marker lines.
- Both reconciliation merge parents remain in history; this follow-up changes
  only normalized tool-call validation and its governed evidence.

## Preserved runtime behavior

- Engine configuration and worker manifests use canonical `state`, `queue`,
  and `cron` identities, while all LLM registrations and consumers use
  `agentos::llm::*`.
- An unanswered worker-identity query fails closed without spawning duplicate
  workers. Portable install paths remain caller-anchored, and upgrades retain
  operator-owned configuration, environment, and runtime data.
- Agent tools are emitted in each provider's native schema and provider output
  is normalized as `FunctionCall { callId, id, arguments }`.

## Normalized function-call boundary

- Empty normalized call IDs or function IDs never reach capability checks or
  tool invocation.
- Gemini's optional call ID is synthesized deterministically when missing or
  empty; malformed non-string IDs and unknown function aliases are rejected.
- If every raw call in a completion is invalid, agent-core stops the tool loop
  without replaying malformed calls or requesting another completion.
