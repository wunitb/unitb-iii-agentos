# Build 10014 Invariants

## Provider payload boundary

- An assistant history message's `tool_calls` or `toolCalls` value is never copied directly into an OpenAI-compatible or Anthropic request.
- Every forwarded assistant tool call has passed `FunctionCall` normalization and provider-name translation.
- A mixed array emits only the calls that normalize successfully. If no call survives, the assistant message is retained without either raw tool-call key; its non-tool content is preserved.
- OpenAI-compatible requests use only native `tool_calls` objects. Anthropic requests use only native `tool_use` content blocks.

## Preserved behavior

- Valid call IDs, function IDs, and JSON arguments retain their values across translation.
- Tool-result history and Gemini history translation are outside this change.
- Provider completion normalization and agent-core capability enforcement remain independent downstream boundaries.
