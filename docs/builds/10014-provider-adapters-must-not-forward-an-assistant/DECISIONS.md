# Build 10014 Decisions

## D-001 — Sanitize instead of dropping the assistant message

When every assistant tool call is invalid, the adapters retain the assistant message after removing `tool_calls` and `toolCalls`. This preserves ordinary assistant text while preventing malformed call data from crossing the provider boundary.

## D-002 — Remove raw keys before rebuilding native calls

OpenAI-compatible translation starts from a copy of the history message, removes both accepted input spellings, and adds `tool_calls` only when at least one normalized native call exists. Anthropic's no-valid-call branch performs the same removal; its valid-call branch already builds fresh native `tool_use` blocks. This makes the safe behavior independent of whether the input used snake case or camel case.

## D-003 — Keep the fix adapter-scoped

The change does not alter session storage, agent-core's completion loop, tool-result messages, or Gemini translation. Those paths have distinct contracts, while this defect occurs when OpenAI-compatible and Anthropic request payloads are constructed from supplied assistant history.
