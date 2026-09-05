# Releasing AgentOS

Only the principal maintainer releases AgentOS. A work-package agent must not push, merge, tag, or publish.

## Preconditions

1. Merge the reviewed release branch through a pull request. Wait for every required check and for all reviews to finish.
2. On the exact merged commit, run the repository gate set documented in `README.md`. Confirm the portable bundle job stages the same launch-critical files as `.github/workflows/release.yml`.
3. Verify `Cargo.toml`, `Cargo.lock`, root Node metadata, plugin metadata, website metadata/lock, and Python embedding metadata/lock share one product version. `bun test tests/release_contract.test.ts` enforces this without hard-coding a forever-version.
4. Verify GitHub private vulnerability reporting is enabled. `SECURITY.md` is intentionally conditional until that repository setting is checked.
5. Review the public diff for secrets, local paths, conflict markers, and release-note accuracy. A fake-provider test is not evidence of a live provider.

## Tag and workflow

Create `vX.Y.Z` only at the verified commit on `main`. The release workflow rejects a tag whose `X.Y.Z` does not equal `[workspace.package].version` in `Cargo.toml`.

The workflow then:

1. builds all three supported targets with the locked Rust workspace;
2. stages the CLI, TUI, `agentos-bus-authd`, `.iii-version`, `config.yaml`, `iii.lock`, `.env.example`, `workers/env.allowlist`, manifests, configuration, and worker payloads;
3. produces a sha256 checksum, SPDX JSON SBOM, and GitHub build-provenance attestation for every archive;
4. validates each downloaded archive and attestation in an isolated home;
5. publishes only after both `build` and `validate` succeed.

Release concurrency never cancels an in-flight tag run. Every job has a bounded timeout. Do not bypass a failed job or manually upload a partial asset set.

## After publication

Verify the GitHub release contains three archives and matching checksum/SBOM assets. Verify attestations against the repository. Install one archive into a scratch home and run the credential-free doctor/boot checks. Do not run a real provider test unless the maintainer separately authorizes credentials and egress.

AgentOS `0.2.0` remains pinned to iii `v0.22.1`. iii `v0.23` compatibility is separate work; see `SECURITY.md`.
