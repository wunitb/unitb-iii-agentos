# Build 10013 Traceability

| Criterion | Preserved behavior | Evidence |
|---|---|---|
| ISC-000 | Required branch tips and `238b423` remain ancestors, and repository files are free of conflict markers. | Explicit `git merge-base --is-ancestor` checks plus the repository-wide marker scan. |
| ISC-001 | Canonical workers, the AgentOS LLM namespace, fail-closed startup, portable upgrades, native tool schemas, and normalized calls remain intact. Empty identifiers are rejected at the normalized boundary, with Gemini optional IDs synthesized. | Configuration and manifests, CLI portability/up tests, namespace and install tests, and worker unit tests. |
| ISC-002 | The Rust workspace, TypeScript, Bun suite, iii 0.22.1 boot regression, and frozen dependency graph remain reproducible. | `cargo +1.90.0 test --workspace`, `bunx tsc --noEmit`, `bun test`, and `bun install --frozen-lockfile` followed by a clean status comparison. |
| ISC-003 | This governed directory contains exactly the four required evidence documents and a decision for each merge conflict. | `INVARIANTS.md`, `TRACES.md`, `DECISIONS.md`, `ATTACK_SURFACE.md`, and the artifact contract test. |

## Delivery note

The delivery report records the actual command results. These traces identify
the acceptance surface without treating prose as a substitute for executable
checks.
