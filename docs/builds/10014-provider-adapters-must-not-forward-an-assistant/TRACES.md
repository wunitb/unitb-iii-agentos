# Build 10014 Traceability

| Criterion | Behavior | Evidence |
|---|---|---|
| ISC-000 | OpenAI-compatible and Anthropic request construction removes both raw assistant tool-call key spellings and emits only non-empty provider-native translations of normalized calls. | `remove_assistant_tool_calls`, `openai_messages`, and `anthropic_messages` in `workers/llm-router/src/main.rs`. |
| ISC-001 | Both adapters have exact full-payload assertions for all-invalid, mixed-validity, and all-valid assistant arrays; the all-invalid assertions also check that neither raw key exists. | `provider_adapters_omit_all_invalid_assistant_tool_calls`, `provider_adapters_emit_only_valid_assistant_tool_calls_from_mixed_arrays`, and `provider_adapters_preserve_all_valid_assistant_tool_calls`. |
| ISC-002 | The governed evidence records the invariant, decision, trace, and precise filtering boundary. | `INVARIANTS.md`, `DECISIONS.md`, `TRACES.md`, and `ATTACK_SURFACE.md` in this directory. |

The delivery report records the executed Rust and TypeScript checks; this document maps criteria to implementation evidence rather than replacing those checks.
