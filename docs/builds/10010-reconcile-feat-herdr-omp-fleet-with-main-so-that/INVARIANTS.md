# Build 10010 Invariants

## History and repository integrity

- `origin/main`, `origin/issue/1897372b-remediate-artifact-1`, and commit
  `238b423` remain ancestors of the delivered branch. The reconciliation uses
  merge ancestry instead of copying either line of work into an unrelated
  snapshot.
- No tracked or untracked repository file contains an unresolved Git conflict
  marker. Generated dependency and compilation directories are excluded from
  the repository-wide scan.
- Frozen Bun dependency installation is reproducible and leaves the worktree
  byte-for-byte clean.

## Preserved behavior

- Engine configuration and worker manifests use iii 0.22.1's canonical
  identities `state`, `queue`, and `cron`; deprecated `iii-*` identities do
  not return.
- The AgentOS LLM router and every internal consumer use the owned
  `agentos::llm::*` namespace, protecting them from ecosystem-level router
  collisions.
- `agentos up` treats a successful but unanswered worker-identity query as
  unknown readiness and fails closed. Its deterministic `Fake` tests cover
  silent, unrelated, partial, and complete identity reports.
- Upgrade and install behavior remains portable: runtime state survives an
  upgrade, installed paths are relocatable, and expected temporary paths are
  canonicalized before assertions compare them with child-process working
  directories.
- The real iii 0.22.1 boot regression, the Rust workspace, the TypeScript
  checker, and the complete Bun suite remain required release gates.
