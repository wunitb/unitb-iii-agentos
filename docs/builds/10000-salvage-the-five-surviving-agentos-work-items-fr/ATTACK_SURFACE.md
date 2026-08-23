# Build 10000 Attack Surface

This delivery changes documentation only, but the salvaged work it governs
touches process startup, local configuration, HTTP authentication, worker
identity discovery, and terminal rendering. Those boundaries are the relevant
attack surface for review.

## Trust boundaries

- The bootstrap CLI launches the engine and worker binaries. Paths, inherited
  environment values, dotenv input, process ownership, timeouts, and teardown
  behavior must remain bounded to the current invocation.
- `AGENTOS_CONFIG`, `AGENTOS_HOME`, and the runtime `.env` cross from operator
  input into configuration discovery. Explicit shell values take precedence,
  malformed dotenv assignments fail with a location, and secrets must not be
  printed in diagnostics.
- Worker readiness comes from engine-reported canonical identities, not an
  untrusted aggregate count. Foreign workers cannot satisfy the required set.
- The TUI sends `AGENTOS_API_KEY` only as a valid bearer header. Invalid header
  values are rejected, and absent providers render an explicit unavailable
  state instead of presenting invented data.
- Security capability responses are remote JSON. Rendering recursively handles
  objects, arrays, scalars, empty payloads, and missing payloads without treating
  display data as terminal control instructions or executable input.

## Review posture

The governed files contain static repository evidence, not credentials, live
service output, approval claims, or remote-system assertions. Command results
are reported by the delivery run after execution rather than pre-recorded here.
