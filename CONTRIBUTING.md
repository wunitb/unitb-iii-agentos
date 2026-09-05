# Contributing

The contributor workflow, scope rules, and worker conventions live in [`identity/CONTRIBUTING.md`](identity/CONTRIBUTING.md). Read that guide before opening a change.

Use a focused branch and pull request. Add a failing test before changing behavior, run the relevant Rust and Bun gates, and state what you did not test. Do not commit credentials, generated runtime state, or provider transcripts.

For vulnerabilities, follow [`SECURITY.md`](SECURITY.md); do not place exploit details in a public issue.
