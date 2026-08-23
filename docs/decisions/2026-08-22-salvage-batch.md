# Retired OMP fleet salvage decisions

## LLM namespace

AgentOS owns the `agentos::llm::*` namespace. Its router registrations and internal consumers no longer use the ecosystem-level `llm::*` identifiers.

NOTE for the principal: the complementary behavior change belongs upstream in `iii-hq/iii`. When two workers register the same function identifier, the engine should return an explicit collision error rather than allowing a trigger to produce an empty assistant turn. Decide whether to open that upstream issue; this repository change does not post externally.

## TUI provider decisions

| TUI surface | Route | Decision |
|---|---|---|
| Dashboard, Agents, Skills | `GET /api/dashboard/stats` | No provider in this release; render an explicit no-provider state. |
| Sessions | `GET /api/sessions` | Keep live; the memory worker registers the provider. |
| Security | `GET /api/security` | Keep live; the security worker registers the provider. |
| Logs | `GET /api/dashboard/logs` | No provider in this release; render an explicit no-provider state. |
| Audit Events | `GET /api/dashboard/events` | No provider in this release; render an explicit no-provider state. |
| Settings | `GET /api/settings` | No provider in this release; render an explicit no-provider state and remove the misleading local YAML fallback. |
