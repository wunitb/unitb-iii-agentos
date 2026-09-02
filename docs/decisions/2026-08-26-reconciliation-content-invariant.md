# Reconciliation content invariant (2026-08-26)

## Decision

The ancestry-based invariant recorded by build 10013 is retired. The old test
required `origin/main`, `origin/issue/1897372b-remediate-artifact-1`, and
`238b423` to be ancestors of `HEAD`. `origin/main` remains resolvable but moves
as main advances. The remediation branch was deleted from origin on 2026-08-24,
making `origin/issue/1897372b-remediate-artifact-1` unresolvable in a fresh
clone. Commit `238b423` may still resolve in an existing clone, but its changes
were squash-merged, so that pre-merge commit is permanently unreachable as an
ancestor of the squash commit. Consequently, the old assertion cannot hold
again on the delivered result branch.

## Replacement invariant

The contract now reads the following committed blobs directly from `HEAD`:

- non-empty `docs/decisions/2026-08-22-salvage-batch.md`;
- `workers/llm-router/src/main.rs`, containing `agentos::llm`; and
- `crates/cli/src/bootstrap.rs`, containing `connected_worker_ids`.

These checks enforce the old invariant's intent by testing the delivered
salvage evidence, namespace migration, and fail-closed startup implementation.
Unlike branch names and pre-squash ancestry, those committed contents disappear
when the reconciled work is removed, causing the contract to fail.


## Amendment 2026-09-02

The `docs/builds/…/INVARIANTS.md` entry was dropped when `docs/builds/` left this
repository. Those directories recorded how work was produced, not what the product
does, and a public product repository is the wrong home for them; they are archived
outside the repo. The remaining three checks still enforce the invariant's intent —
delivered content, not reachable history — so the contract is unchanged in substance.
