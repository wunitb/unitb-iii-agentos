# Build 10039 Invariants

## Delivered-content contract

- The reconciliation test reads four prescribed blobs from `HEAD`.
- The build-10013 invariant and the 2026-08-22 salvage decision must exist and
  be non-empty.
- The LLM router must contain `agentos::llm`, and CLI bootstrap must contain
  `connected_worker_ids`.
- Removing a required blob, emptying either document, or removing either source
  marker makes the replacement case fail.

## Preserved test behavior

The repository-wide conflict-marker case and both conflict-marker detector
edge cases remain unchanged. The repair replaces only the stale ancestry case;
it neither removes nor weakens any other test.

## Scope

This build adds its governed artifacts only at
`docs/builds/10039-repair-the-reconciliation-contract-test-which-ha/`, adds one
dated decision, and changes the reconciliation contract test. It does not edit
source code, workflows, other tests, or any earlier build record.
