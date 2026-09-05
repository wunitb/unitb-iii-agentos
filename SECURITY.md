# Security policy

## Supported version

AgentOS `0.2.x` receives security fixes. Older releases are not supported.
The repository pins iii `v0.22.1`; do not assume a newer engine or SDK is wire-compatible.

## Reporting a vulnerability

Do not publish exploit details in a normal issue. GitHub private vulnerability reporting is the intended confidential path **only when it is enabled** for this repository: open the repository **Security** tab and choose **Report a vulnerability**. The maintainer must verify that this feature is enabled before publishing `v0.2.0`; this document does not claim that the setting has already been checked.

Use a normal GitHub issue only for non-sensitive hardening requests. This project does not publish a private security email address.

Include the affected commit or release, a minimal reproduction, impact, and any suggested containment. Never include live credentials or personal data.

## Supported threat model

AgentOS is a single-operator product on a trusted host. The engine bus is bound to loopback. The default configuration arms tiered bus RBAC through `agentos-bus-authd`: callers without the shared bearer are the untrusted tier; callers with `AGENTOS_API_KEY` and registered AgentOS workers receive broader policy tiers. Exact sensitive calls and registration families are denied to the untrusted tier.

This is **not** a hostile local multi-user boundary:

- `AGENTOS_API_KEY` is one shared operator credential. Shipped Rust workers receive it through the worker environment policy. It identifies operator-trusted processes; it does not give each worker a separate identity or isolate processes running as the same OS user.
- The untrusted tier deliberately retains some generic registry and state access for iii compatibility. In particular, RBAC is not a complete state-mutation boundary. Do not expose port 49134 to a LAN, tailnet, container peer, or another untrusted user.
- HTTP bearer checks, principal propagation, and the bus gate narrow remote and deputy authority. They are defence in depth inside the single-operator boundary, not a replacement for OS accounts, file permissions, or network isolation.

## Execution and supply-chain boundaries

The process bridge is disabled unless `AGENTOS_ENABLE_PROCESS_BRIDGE=1`. Enabling it grants full executable authority to any bearer holder that can reach its trusted functions. The bridge checks a named executable basename and clears the child environment before adding an explicit baseline, but it does **not** bind an executable inode, sandbox arguments, or make general-purpose executors safe. Timeout and cancellation stop the direct child; descendants may survive.

Direct caller-chosen `mcp::connect` commands are refused. `integration::add` loads trusted manifests from `integrations/`. Those manifests can run tools such as `npx`, so enabling a catalog entry accepts its package-registry, package-name, version-resolution, install-script, and transitive-dependency supply-chain risk. Review and pin manifests before production use.

The `shell`, `console`, and `harness` registry workers remain opt-in because their exposure is broader than the default product boundary.

## Provider and engine evidence

Credential-free tests use the in-tree fake Anthropic endpoint. They prove request shape and multi-turn history without network egress. They do not prove a real provider account, billing path, rate limits, or production egress.

iii `v0.23` is the latest stable upstream line, but AgentOS `0.2.0` stays on `v0.22.1`. Compatibility is deferred until the RBAC hook protocol, SDK wire shapes, registry lock, worker assets on every supported platform, boot lifecycle, and full test matrix are validated together. Do not change `.iii-version` alone.
