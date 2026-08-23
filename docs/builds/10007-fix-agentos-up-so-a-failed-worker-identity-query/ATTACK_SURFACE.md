# Build 10007 Attack Surface

This change hardens the process-registration boundary in `agentos up`. The
engine's `engine::workers::list` response is a remote observation and must be
known before the CLI decides which worker processes are absent.

## Duplicate-registration path

Previously, a failed, timed-out, or malformed worker-identity response produced
`None`, which the bootstrap path converted to an empty set. `agentos up` could
therefore launch every required worker even when those identities were already
connected but temporarily unobservable, creating duplicate registration
attempts and competing worker processes.

The bootstrap now fails closed. It retries an unanswered identity query within
the existing bounded polling budget. If no response arrives, it reports "the
engine did not report connected worker identities", tears down only processes
started by the current invocation, and does not spawn any worker. A successful
empty response remains distinct: `Some(empty)` authoritatively means no workers
are connected, so the required workers are started as before.

## Review posture

This change does not broaden process ownership, alter worker manifests, or add
new external input. The security-relevant invariant is that unknown bus state
cannot authorize worker creation; only a reported identity set can drive the
missing-worker calculation.
