# AgentOS 0.2.0

AgentOS 0.2.0 stabilizes the UnitB continuation on the pinned iii **0.22.1** engine. It is not an iii 0.23 migration.

## Changes

- Enable the default loopback bus boundary and preserve caller principals across agent, memory, workflow, MCP and scheduling paths. Worker environments receive only governed variables.
- Make startup, readiness, stop and restart use recorded process ownership. The Linux engine supervisor retains subreaper ownership of detached registry descendants throughout the engine lifetime.
- Bound Slack body reads and channel admission, remove legacy `init` credential copying, and keep provider errors out of public responses.
- Keep TUI chat responsive, initialize canonical agent tools, and exercise actual fake-provider chat instead of counting a skipped/preflight leaf as acceptance.
- Migrate the Node helper telemetry dependency family with a reproducible ESM/CommonJS compatibility patch. Native tests query stored traces, correlated logs and metrics from the pinned engine using both Node and Bun.
- Remove Pulse's unused future-schedule dependency in favor of private UTC due-slot membership, with a preserved legacy-parser corpus. No duplicate-dependency exception or advisory waiver was added.
- Add locked Node audit gates and native OTEL acceptance to CI. Release bundles carry checksums, SPDX SBOMs and GitHub build-provenance attestations.

## Installation and upgrade

Use the installer documented in the repository README, or download the archive matching your platform. The release workflow builds Linux x86_64, Linux AArch64 and macOS AArch64 bundles. Verify the matching checksum and GitHub attestation before installing.

Upgrades preserve operator-owned runtime configuration, `.env` and data. Stop an existing runtime before upgrading. Ownership records now pin executable device/inode, so replacing or moving the CLI binary does not invalidate ownership of an already-running supervisor. Legacy records remain readable. Supervisor cleanup gets at least five seconds even when a shorter stop grace is requested; a process group without a live recorded leader or a freshly captured descendant witness is still refused rather than signalled blindly.

The new default boundary needs a generated or configured `AGENTOS_API_KEY`; follow the onboarding/doctor guidance rather than disabling the boundary to make startup succeed.

Earlier `init` versions could copy an inherited provider key into legacy `$AGENTOS_HOME/config.toml`. Review that file's permissions and rotate affected keys when appropriate. The upgrade does not delete or print operator credential files automatically.

## Support and limits

The supported boundary is **one operator on a trusted host**, not isolation from hostile same-UID processes or a multi-tenant service. Do not expose the raw engine bus to untrusted networks. Process-bridge and executable MCP integrations retain the explicit authority described in `SECURITY.md`.

Credential-free acceptance uses isolated runtimes and a local fake provider. It does not establish live-provider billing/cancellation, real Slack/Telegram delivery, durable exactly-once semantics, or hostile-host containment. Optional safety-worker and memworkr integration work remains tracked separately in the takeover assessment; this release does not claim that every optional integration has production-service acceptance.

GitHub private vulnerability reporting is enabled; use the private reporting link in `SECURITY.md`, not a public issue, for sensitive reports.

Local pre-release evidence is retained under `docs/evidence/`. Those working-tree snapshots are historical evidence, not substitutes for the PR CI, merged commit, release workflow and attestation records associated with the published tag.
