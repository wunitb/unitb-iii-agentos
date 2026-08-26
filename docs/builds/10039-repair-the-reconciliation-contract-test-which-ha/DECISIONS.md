# Build 10039 Decisions

## D-001 — Test delivered content instead of ancestry

The replacement case reads the four prescribed paths as Git blobs at `HEAD`.
It requires both Markdown records to be non-empty and checks the required
source marker in each implementation file. This directly tests whether the
reconciled work remains delivered after a squash merge.

The rationale and the three retired references are recorded in
`docs/decisions/2026-08-26-reconciliation-content-invariant.md`.

## D-002 — Keep the repair independent of mutable refs

`git cat-file blob HEAD:<path>` reads committed content without resolving a
remote branch or traversing ancestry. The test therefore remains deterministic
when remote-tracking references move or are pruned.

## D-003 — Preserve the rest of the contract

The existing conflict-marker scan and its two detector edge cases remain
unchanged. No earlier governed build record is revised to describe this repair.
