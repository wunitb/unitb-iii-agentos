# Build 10000 Decisions

## D-001 — Preserve the recovered implementation

The five surviving work items are already represented by focused commits and
regression coverage in this checkout. This remediation does not rewrite those
features; it supplies the missing governed evidence and verifies the required
TypeScript and Bun commands against the result.

## D-002 — Keep AgentOS-owned function names isolated

AgentOS router registrations and internal consumers use the
`agentos::llm::*` namespace. Collision handling for duplicate registrations in
the underlying iii engine remains an upstream decision, as recorded in
`docs/decisions/2026-08-22-salvage-batch.md`.

## D-003 — Prefer explicit operational truth

Bootstrap and doctor assess required canonical worker identities rather than
counts. TUI screens without registered providers say so directly. Empty
security capability collections are distinct from unavailable data. These
choices prevent a superficially healthy or populated UI from masking missing
runtime capabilities.

## D-004 — Make the artifact contract executable

The governed directory contains exactly the four required Markdown files. A
repository test validates that the directory is real, each artifact is a
regular UTF-8 file of at least 200 bytes with a Markdown heading, and every
required ISC identifier occurs as a whole token in `TRACES.md`.
