# Stable iii 0.23 migration

## Current source and installation boundary

The source pins the independently published engine and Rust, Node and Python SDK
releases to **0.23.0**. The supported source-installation path is now
`bash scripts/oci-stack.sh build` / `up`, or `bash scripts/install.sh --oci`.
A working Podman or Docker runtime runs the Linux image; no native host engine is
started by the development or boot-smoke wrappers.

The Rust build remains on the pinned toolchain. The runtime image uses Debian
trixie: the pinned registry directory binary requires `GLIBC_2.39`, which the old
bookworm runtime did not provide. Rust binaries can still be built in bookworm.

`config.yaml` declares the native managers. `worker-compose.yaml` pins the
separate registry primitives, including HTTP and pubsub, with explicit configuration
IDs, runtime working directories and scoped credential environments. The default
cron lock adapter is local for the single owned OCI runtime; the pinned standalone
cron worker does not accept the former `kv` adapter name.

The private bootstrap manager is container-loopback-only. Policy handlers register
there before Compose and authenticated product workers start. Only the API and
fully gated edge are published, on host loopback with dynamically assigned ports.
The mandatory bare manager name is retained to prevent an extra default listener.
Readiness explicitly includes internal post-bind handlers, which iii 0.23 hides
from ordinary function discovery.

## State, recovery and upgrade policy

Use a new private `AGENTOS_OCI_HOME`, separate from an existing native installation.
Same-engine stop/rebuild/up preserves operator config, credentials and data.
Exited same-image containers recover only through verified immutable ownership;
missing records are not permission to adopt another container by name.

Different engine pins are not silently translated. Retain the complete old home
and its exact image/release, create a separate new home, and validate any application
data import against its storage contract. Do not edit a stored pin to bypass this
guard. Image-managed binaries, manifests and bundled content may be refreshed;
operator configuration is retained, with shipped-config drift reported.

The existing native release installer is retained for archived iii 0.22.x bundles.
It rejects OCI-era pins before replacing installed payload or operator state.
The published native AgentOS v0.2.0 release is historical evidence for iii 0.22.1,
not a release of this migration. This change does not retag or republish it.

## Chat and memory correctness updates

Compaction retains the session index when reading, summarizing, or updating it
fails. Session mutations are serialized per agent/session inside the single memory
worker. This is not a distributed lock: multiple independent memory writers would
require a storage-level conditional update. Summaries sort before the retained
recent messages, and history uses the role of each session occurrence even when
its content is deduplicated.

Fallback embeddings use deterministic SHA-256 hashing and identify their algorithm
as `hash-sha256-v1`. New memory entries persist `embeddingModel`. Recall compares
vectors only when both model IDs and dimensions match. Existing unlabelled or
incompatible vectors remain stored and searchable through keyword, recency,
importance and confidence scoring; they need re-embedding with a recorded model
ID to regain semantic scoring. Startup does not rewrite operator data or relabel
old randomized vectors as compatible.

Parallel/fanout batches settle all dispatched children before reporting failure.
Retries apply only to failed children and obtain authorization again. A timeout
does not prove a remote operation had no effect; workflows that retry side effects
still need an idempotent target. Consumed completion tokens are metered before
the next tool/provider call, including turns that subsequently fail.

`POST /v1/chat/completions` accepts bounded text-only `system`, `user`, and
`assistant` messages ending in the current user message. Explicit transcripts
replace automatically recalled context for that request. JSON usage fields are
`prompt_tokens`, `completion_tokens`, and `total_tokens`, accumulated across the
turn. `stream: true` returns buffered SSE after the answer is complete. This is
not full OpenAI API compatibility: multimodal content, client tool transcripts,
sampling controls, structured-output options, and streamed usage are not supported.
The AgentOS `/api/chat/stream` route remains session-based JSON and carries
`sessionPersisted`/`persistenceWarnings`; the TUI shows incomplete persistence.

`oci-stack.sh exec` has no launcher deadline, allowing long interactive TUI
sessions. The diagnostic `doctor` command retains its bounded deadline.

## Reproducing the checks

Run the ordinary Rust format, workspace Clippy, workspace tests, locked build and
`cargo deny check` gates, plus `bun run check` and `bun run audit`. Use the pinned
Bun version. Python embedding tests need the lock-constrained SDK/test dependencies.
The Node helper compatibility patch is regenerated for `@iii-dev/helpers@0.23.0`;
all trace, metric and correlated-log checks remain required. Native telemetry uses
`engine::traces::spans`, not the new compact trace-list summaries.

For the full fixture path, with a working OCI runtime:

```sh
bun run test:oci --report /tmp/agentos-oci-acceptance.json
bun scripts/assert-oci-results.ts /tmp/agentos-oci-acceptance.json
```

`--no-build` reuses an already-built image whose engine label matches the checkout.
The harness derives a test-only image from its immutable identity. Its synthetic
provider runs on container loopback; no fixture endpoint is host-published and no
real provider credentials enter the default test lane. The receipt is written only
after registry/identity checks, authenticated versus untrusted inventory checks,
fake chat and protocol validation, representative worker calls, restart/persistence
checks and verified owned teardown. An empty or inventory-only receipt is rejected.
Failed test homes are retained privately for diagnosis, not silently erased.

## Evidence limits

Local runtime acceptance covers Linux AArch64 with Podman. It does not establish
macOS/Docker or Linux x86_64 runtime behavior; those environments need their own
acceptance. A green local gate is not a merged-head CI result or publication.

The fixture lane does not certify real provider billing/cancellation, real channel
accounts, hostile same-user isolation, long-running service guarantees, optional
safety integrations, or AgentOS-to-private-memworkr production contracts. The
migration retains these boundaries rather than converting earlier deferred work
into a success claim. Real credentials and private sibling source/data are not
part of the published change.
