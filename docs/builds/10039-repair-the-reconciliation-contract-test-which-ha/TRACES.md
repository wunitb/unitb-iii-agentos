# Build 10039 Traces

## Acceptance traceability

| Criterion | Result evidence |
|---|---|
| ISC-000 | This exact governed directory contains the four required substantive files; validation commands and observed exit codes are recorded below. |
| ISC-001 | The focused Bun suite runs the replacement plus the three unchanged cases. |
| ISC-002 | The replacement reads all four prescribed blobs from `HEAD`; the isolated negative probe below demonstrates failure when one is deleted. |
| ISC-003 | `docs/decisions/2026-08-26-reconciliation-content-invariant.md` retires the ancestry invariant and documents its replacement. |
| ISC-004 | The final diff is restricted to the contract test, dated decision, and this governed directory; the requested script and example suites pass. |

## Validation commands

| Command | Observed result |
|---|---|
| `bun test tests/reconciliation_contract.test.ts` | Exit 0 on every positive run, including the final run; 4 passed, 0 failed. |
| `bun run test:scripts` before dependency setup | Exit 127; `vitest: command not found`. |
| `bun run test:examples` before dependency setup | Exit 127; `vitest: command not found`. |
| `bun install --frozen-lockfile` | Exit 0; installed the locked root dependencies without modifying a lockfile. |
| `bun run test:scripts` after dependency setup | Exit 0; 1 file and 9 tests passed. |
| `bun run test:examples` after dependency setup | Exit 0; 1 file and 2 tests passed. |
| `git log -1 --format='%H %s' main -- <four prescribed paths>` | Exit 0; main reported `ef44bd7a26e32cedfc085f5b8472de0a405cd48f`. |
| `git worktree add --detach <temporary-worktree> HEAD` | Exit 0; created an isolated worktree at the implementation commit. |
| Delete `docs/decisions/2026-08-22-salvage-batch.md`, then `git commit` in the isolated worktree | Exit 0; created throwaway commit `5490d3b8c00149ea100b419a600672ad01f9dbf0`. |
| `bun test tests/reconciliation_contract.test.ts` on the throwaway commit | Exit 1; 3 passed and 1 failed with `required reconciled artifact is absent from HEAD: docs/decisions/2026-08-22-salvage-batch.md`. |
| `git worktree remove <temporary-worktree>` | Exit 0; removed the isolated probe worktree. |
| `git merge-base --is-ancestor 5490d3b8c00149ea100b419a600672ad01f9dbf0 HEAD` on the result branch | Exit 1, confirming the throwaway deletion commit is not an ancestor of the result branch. |
| `git diff --check main` | Exit 0; no whitespace errors. |
| `git diff --name-only main` | Exit 0; reported only the test, dated decision, and four files in this build's exact governed directory. |
| `find docs/builds/10039-repair-the-reconciliation-contract-test-which-ha -maxdepth 1 -type f` with filename and byte-count checks | Exit 0; exactly the four required substantive files were present. |

The negative probe exercised the committed replacement test, not a modified
copy of the test. Its only mutation was the prescribed artifact deletion.
