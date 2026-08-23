# Build 10008 Decisions

## D-001 — Treat silence as unknown state

The worker-list query returns `Option<BTreeSet<String>>`, so its two empty-like
states carry different authority. `None` means the engine did not provide a
usable observation and cannot justify starting processes. `Some(empty)` is a
successful observation that authorizes starting all required workers.

## D-002 — Reuse the bounded readiness budget and diagnostic

Initial identity discovery uses the existing `poll_plan` timeout and polling
interval instead of introducing an independent retry policy. Persistent silence
returns the exact readiness diagnostic, "the engine did not report connected
worker identities", keeping operator guidance and program behavior aligned.

## D-003 — Preserve the focused runtime implementation

The preceding runtime fix already supplies fail-closed polling and Fake-based
regression coverage. This completion keeps that behavior unchanged and adds the
missing governed evidence rather than creating a second startup mechanism.

## D-004 — Enforce all four artifacts

The repository contract test points to build 10008, requires exactly the four
governed Markdown filenames, validates their basic file/content properties, and
checks `ISC-000`, `ISC-001`, and `ISC-002` as whole trace tokens.
