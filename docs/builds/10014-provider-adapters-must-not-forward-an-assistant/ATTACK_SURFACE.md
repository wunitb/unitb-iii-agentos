# Build 10014 Attack Surface

## Precisely filtered history

The llm-router filters only assistant messages in the `messages` history supplied to an outgoing OpenAI-compatible or Anthropic completion request. In `openai_messages`, both `tool_calls` and `toolCalls` are removed from each assistant message before any surviving normalized calls are rebuilt as OpenAI-native `tool_calls`. In `anthropic_messages`, an assistant message with no structurally valid calls has both raw keys removed; when calls exist, the adapter constructs new Anthropic `tool_use` blocks and includes only calls whose provider alias resolves.

This is not a global history filter. It does not rewrite persisted sessions, user or system messages, or arbitrary history before the request reaches these two adapters. Tool-result messages continue through their existing provider-specific conversion. Gemini uses its separate `gemini_messages` conversion and is not changed here.

## Relationship to agent-core

Agent-core separately filters the `toolCalls` returned by the current completion before capability checks and invocation. If that newly returned array produces no normalized `FunctionCall`, agent-core stops its continuation loop without appending that new malformed assistant turn. That behavior does not guarantee that all caller-supplied historical assistant arrays are already clean; this build closes that distinct outbound adapter boundary. This corrects build 10013's broader statement that “normalized history” was filtered without identifying which history and where.

## Remaining data boundary

Call arguments remain untrusted JSON data. This change validates call structure and function-name translation only; authorization still belongs to agent-core's capability checks, and execution still occurs through the iii function trigger boundary.
