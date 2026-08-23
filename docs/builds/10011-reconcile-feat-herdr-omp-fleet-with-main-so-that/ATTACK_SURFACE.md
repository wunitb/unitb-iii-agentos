# Build 10011 Attack Surface

## Worker identity discovery

An absent identity response is unknown state, not proof that zero workers are
connected. Treating it as empty could start duplicates and collide on function
registration. The merged bootstrap polls within a bounded deadline, checks
engine liveness, and shuts down anything it started when the engine never
answers. Fake-based regressions assert that no worker launch occurs on this
path and distinguish it from a valid answered-empty set.

## LLM tool boundary

Engine function metadata and model output are untrusted JSON boundaries. The
router only forwards entries with a non-empty tool ID, supplies a safe empty
object schema when metadata has no schema, and translates declarations into
provider-native wrappers. Provider-specific output is reduced to a typed
`callId`, function `id`, and `arguments` value before agent-core performs its
capability check and invocation. OpenAI JSON-string arguments are parsed; an
invalid string remains data rather than being interpolated or executed.

## Provider transport

The secure local Codex path still uses a no-proxy HTTP client and accepts only
loopback IP literals for its configurable base URL. Gemini credentials are
sent as the native API query parameter only to the configured Google base URL;
other OpenAI-compatible credentials remain bearer tokens and Anthropic uses
its native key header. No new URL comes from a completion payload.

## Merge and dependency integrity

Conflict resolution retained the previously validated lockfiles and portable
upgrade behavior. Frozen Bun installation and a clean status check detect lock
drift. The repository marker scan covers tracked and visible untracked files so
an accidentally retained merge fragment cannot compile or ship unnoticed.
