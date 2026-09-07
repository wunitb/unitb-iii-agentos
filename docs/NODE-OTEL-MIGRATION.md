# Node OpenTelemetry migration — 2026-09-06

## Scope and dependency ownership

The engine and all iii SDK pins remain **0.22.1**. Updating `@iii-dev/helpers` to its current stable 0.23.0 would still select the vulnerable OpenTelemetry 1.x dependency family, so an engine/SDK version bump alone is not remediation.

`package.json` instead pins the complete helper-facing OpenTelemetry dependency family to stable **2.11.0** and its matching **0.222.0** packages. The public `@opentelemetry/api` remains on its compatible 1.x line. This removes the old Jaeger propagator from the graph and replaces the affected core; it does not suppress either advisory or lower the audit severity threshold.

The matching Bun patch in `patches/@iii-dev%2Fhelpers@0.22.1.patch` is repository-owned compatibility code, **not an upstream-supported iii release**. It changes only the published helper's ESM and CommonJS telemetry bundles:

- Construct resources with `resourceFromAttributes`, rather than the removed `Resource` constructor.
- Pass log processors to the `LoggerProvider` constructor.
- Pass the exporter and batching settings in the new `BatchLogRecordProcessor` options object. Keeping the old two-argument constructor can silently lose logs even when traces and metrics work.

The patch does not change SDK function invocation, authentication, HTTP helpers, queue helpers, or worker registration. The package-manager-generated lockfile and committed patch are both required: do not copy an edited `node_modules` directory as the fix. Existing upstream licences remain intact.

## Reproduce and maintain

Use the repository's pinned Bun 1.3.14, with Node available for the compatibility subprocesses:

```sh
bun install --frozen-lockfile
bun test scripts/otel-compatibility.test.ts
bun run check
bun run audit
# Requires the pinned iii binary; isolated native ingestion and teardown, no model credentials.
bun run test:otel:native
```

Clean scratch installation with `--frozen-lockfile --ignore-scripts` has also been verified; the compatibility patch applies without lifecycle scripts. Tested runtimes in this continuation are Node 24.18.0 and Bun 1.3.14 on Linux AArch64. Other Node/runtime targets require their own evidence.

Keep the override family, patch, lockfile and compatibility tests together. On an SDK/helpers upgrade, re-evaluate the patch against the exact new package, regenerate the lock through Bun, repeat the clean-install and telemetry checks, and remove overrides only when the resolved upstream graph is fixed. Do not carry a patch forward merely by changing its filename or disable audits to permit an upgrade.

The governance conflict-marker scan now treats Git filenames as filesystem paths, not URL references. This keeps percent-encoded characters in the patch filename literal while retaining the full tracked/visible-untracked scan.

## Evidence and boundaries

`scripts/otel-compatibility.test.ts` launches an isolated subprocess for each Node/Bun and ESM/CommonJS combination. The subprocess uses the real pinned SDK and patched helpers with a local WebSocket fixture. It checks a correlated SDK invocation, ordinary baggage round-trip, rejection of an invalid trace-context value, resource identity, and decoded trace, metric and log payloads. It requires all three signals and bounds flushing and shutdown; credentials and provider endpoints are not inherited.

The receiver in the unit fixture is **not the iii engine**. Native acceptance is separate: `scripts/native-otel.mjs` starts the exact pinned iii binary in a scratch runtime and runs all four runtime/module combinations. Each client uses the real engine to dispatch an echo handler, then queries `engine::traces::list`, `engine::logs::list` and `engine::metrics::list` to verify stored data, IDs, resource identity, baggage and metric values. It uses portless when available, a kernel-assigned test port otherwise, disables external builtin daemons and anonymous telemetry, and verifies engine exit, listener closure and scratch removal. CI runs this as a required step after installing pinned iii. This is native-engine ingestion evidence, not real-provider acceptance or public CI evidence for the local working tree.

The earlier Node-only snapshot is preserved in [node-supply-chain-2026-09-06.json](evidence/node-supply-chain-2026-09-06.json). The completed follow-up and final source hashes are in [closure-2026-09-06.json](evidence/closure-2026-09-06.json). Root Bun/npm audits and full `cargo-deny` pass. The follow-up removes Pulse's `cron` dependency instead of adding a duplicate exception; see the [closure note](CONTINUATION-CLOSURE-2026-09-06.md) for the legacy parser comparison and permanent regression corpus. RustSec retains the existing `fxhash` unmaintained warning; that warning is not a newly discovered vulnerability.

No commit, push, PR, merge, tag, release, repository-security setting change, or real-provider traffic is part of this work.
