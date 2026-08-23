# Build 10000 Invariants

## Artifact invariants

- The governed path is a canonical on-disk directory, not a symbolic link or
  another non-directory filesystem object.
- `ATTACK_SURFACE.md`, `DECISIONS.md`, `INVARIANTS.md`, and `TRACES.md` are
  regular files and are the complete governed evidence set.
- Every artifact is valid UTF-8, is at least 200 UTF-8 bytes long, and contains
  a Markdown heading matching the artifact gate's required form.
- `TRACES.md` names `ISC-000` through `ISC-004` as whole tokens and maps each
  identifier to concrete repository evidence.

## Runtime invariants inherited from the salvage

- `agentos doctor` remains diagnostic-only; it does not install, build, start,
  or terminate processes.
- `agentos up` advances through ordered readiness stages with bounded waits and
  tears down only processes started by its own invocation when a later stage
  fails.
- Required worker readiness is set membership over canonical identities. A
  matching number of unrelated connected workers is never sufficient.
- Explicit environment variables override dotenv values, and secrets used for
  protected TUI routes are not emitted into diagnostic output.
- Canonical iii 0.22.1 worker configuration names and AgentOS-owned LLM
  function identifiers remain collision-free.
- TUI surfaces never fabricate provider results. Registered Sessions and
  Security routes stay live; unsupported dashboard, log, event, and settings
  surfaces identify their no-provider state.
