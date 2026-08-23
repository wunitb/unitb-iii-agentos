# Build 10011 Invariants

## History and repository integrity

- `origin/main`, `origin/issue/1897372b-remediate-artifact-1`, and commit
  `238b423` remain ancestors of the delivered commit.
- Repository files contain no unresolved Git conflict-marker lines.
- The merge does not replace either line of work with a content-only copy; the
  required histories are represented by real merge parents.

## Runtime compatibility

- Engine configuration uses the iii 0.22.1 canonical `state`, `queue`, and
  `cron` worker identities, and every worker manifest names its directory.
- Relative install/configuration paths remain anchored to the caller and
  canonicalized in portability assertions. Installer upgrades preserve
  operator-owned configuration, environment, and runtime data.
- `agentos up` distinguishes an answered empty identity set from an unanswered
  identity query. An unanswered query is retried within the readiness budget
  and fails without spawning workers.

## Agent LLM contract

- Every AgentOS LLM registration and consumer uses `agentos::llm::*`; no worker
  invokes the legacy ecosystem-level namespace.
- Agent tool metadata is translated to Anthropic `input_schema`, OpenAI
  `function.parameters`, or Gemini `functionDeclarations` payloads.
- Provider tool responses cross the router boundary only as
  `FunctionCall { callId, id, arguments }`, with JSON-string arguments decoded
  before the agent invokes a tool.
