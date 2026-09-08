# Security policy

## Supported version

AgentOS `0.2.x` receives security fixes. Older releases are not supported.
The current source pins iii `v0.23.0` and uses the non-root OCI runtime.
Published native `v0.2.0` archives remain on iii `v0.22.1`; they are not migration
artifacts. Do not assume a different engine or SDK is wire-compatible.

## Reporting a vulnerability

Do not publish exploit details in a normal issue. GitHub private vulnerability reporting is enabled for `wunitb/unitb-iii-agentos`, verified through the repository API during `v0.2.0` release preparation. Use [Report a vulnerability](https://github.com/wunitb/unitb-iii-agentos/security/advisories/new), or open the repository **Security** tab and choose **Report a vulnerability**.

Use a normal GitHub issue only for non-sensitive hardening requests. This project does not publish a private security email address.

Include the affected commit or release, a minimal reproduction, impact, and any suggested containment. Never include live credentials or personal data.

## Supported threat model

AgentOS is a single-operator product on a trusted host. The OCI launcher publishes
only the API and authenticated edge bus on dynamically assigned host-loopback
ports. The raw bootstrap bus remains on private container loopback, never published. The default configuration arms tiered bus RBAC through `agentos-bus-authd`: callers without the shared bearer are the untrusted tier; callers with `AGENTOS_API_KEY` and registered AgentOS workers receive broader policy tiers. Exact sensitive calls and registration families are denied to the untrusted tier.

This is **not** a hostile local multi-user boundary:

- `AGENTOS_API_KEY` is one shared operator credential. Shipped Rust workers receive it through the worker environment policy. It identifies operator-trusted processes; it does not give each worker a separate identity or isolate processes running as the same OS user.
- The untrusted tier deliberately retains some generic registry and state access for iii compatibility. In particular, RBAC is not a complete state-mutation boundary. Do not widen the launcher's host-loopback publications or expose the container-private listeners to a LAN, tailnet, container peer, or another untrusted user.
- HTTP bearer checks, principal propagation, and the bus gate narrow remote and deputy authority. They are defence in depth inside the single-operator boundary, not a replacement for OS accounts, file permissions, or network isolation.

## Execution and supply-chain boundaries

The process bridge is disabled unless `AGENTOS_ENABLE_PROCESS_BRIDGE=1`. Enabling it grants full executable authority to any bearer holder that can reach its trusted functions. The bridge checks a named executable basename and clears the child environment before adding an explicit baseline, but it does **not** bind an executable inode, sandbox arguments, or make general-purpose executors safe. Timeout and cancellation stop the direct child; descendants may survive.

Direct caller-chosen `mcp::connect` commands are refused. `integration::add` loads trusted manifests from `integrations/`. Those manifests can run tools such as `npx`, so enabling a catalog entry accepts its package-registry, package-name, version-resolution, install-script, and transitive-dependency supply-chain risk. Review and pin manifests before production use.

The `shell`, `console`, and `harness` registry workers remain opt-in because their exposure is broader than the default product boundary.

## Provider and engine evidence

Credential-free OCI tests execute a real single chat turn against an in-tree fake
Anthropic endpoint on container loopback. They check provider request shape,
registry/access views, worker calls, restart preservation and owned teardown.
Artifact setup requires network access. These tests do not prove multi-turn
history, real provider accounts, billing, rate limits, or production egress.

Engine and SDK changes require wire-contract, RBAC, registry, lifecycle and
platform evidence. Do not change `.iii-version` alone or rewrite an existing
runtime's stored pin to bypass the separate-home migration guard. See
[the current migration boundary](docs/III-023-MIGRATION.md).
