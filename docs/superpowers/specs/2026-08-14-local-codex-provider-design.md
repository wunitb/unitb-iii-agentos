# Local Codex Provider Design

## Goal

Run AgentOS chat through the existing local OpenAI-compatible proxy at `http://127.0.0.1:8317/v1`, with `gpt-5.6-sol` as the default model. Cloud Anthropic must not be selected implicitly when the local provider is configured.

## Current State

- `workers/llm-router` registers cloud providers plus Ollama, but no provider for the local cli-proxy.
- `llm::complete` defaults to Anthropic whenever its input omits `provider`.
- `llm::route` returns `{ provider, model, complexity }`.
- `workers/agent-core::agent_chat` and `workers/streaming::stream_chat` call `llm::route` with legacy field names that the router does not read, then pass the entire route object as the `model` field and drop `provider`; `llm::complete` therefore falls back to Anthropic.
- `scripts/dev-up.sh` does not load the repository `.env`, despite the README instructing users to configure that file.
- The local cli-proxy is healthy on `127.0.0.1:8317`, accepts Bearer authentication, and advertises `gpt-5.6-sol`.

## Architecture

Add a first-class `codex` provider to `llm-router`:

- Driver: OpenAI-compatible.
- Base URL: `CODEX_PROXY_BASE_URL`, defaulting to `http://127.0.0.1:8317/v1`.
- Credential: `CODEX_PROXY_API_KEY`.
- Default model: `AGENTOS_DEFAULT_MODEL`, defaulting to `gpt-5.6-sol`.
- Configured state: true only when `CODEX_PROXY_API_KEY` is non-empty.

Keep the existing `openai` provider unchanged so local proxy traffic and OpenAI cloud traffic remain distinct.

## Routing Rules

Precedence, highest first:

1. Explicit `provider` and `model` supplied to `llm::complete`.
2. A model that uniquely belongs to a registered provider.
3. The configured default provider and model.
4. Existing complexity routing only when no explicit default provider is configured.

When `CODEX_PROXY_API_KEY` is configured and no explicit provider/model overrides it, routing returns:

```json
{
  "provider": "codex",
  "model": "gpt-5.6-sol"
}
```

There is no silent fallback from a failed local request to Anthropic. Proxy connection or authentication failures remain visible to the caller.

## Data Flow

1. TUI or `POST /v1/chat/completions` submits messages and optional model/provider fields.
2. Agent Core and Streaming call `llm::route` with the router's canonical `messages`, `tools`, and `model` fields.
3. `llm::route` resolves `{ provider, model, complexity }`.
4. Each caller passes `route.provider` and `route.model` as separate fields to every `llm::complete` invocation, including tool-loop iterations.
5. `llm::complete` resolves the provider using the routing precedence and calls the provider's OpenAI-compatible `/chat/completions` endpoint.
6. The existing response normalization returns AgentOS's completion shape.

## Configuration

`scripts/dev-up.sh` loads `${ROOT}/.env` before checking provider credentials or spawning workers. `.env.example` documents:

```dotenv
CODEX_PROXY_BASE_URL=http://127.0.0.1:8317/v1
CODEX_PROXY_API_KEY=
AGENTOS_DEFAULT_PROVIDER=codex
AGENTOS_DEFAULT_MODEL=gpt-5.6-sol
```

The local `.env` is generated from the already-configured cli-proxy without printing the secret. It remains gitignored. AgentOS does not parse cli-proxy's YAML directly; this avoids coupling the runtime to another application's private configuration format.

## Error Handling

- Missing or empty `CODEX_PROXY_API_KEY`: `codex` is listed but not configured and cannot become the implicit default.
- Invalid key: return the proxy's authentication failure through the existing handler error path.
- Proxy unavailable: return the connection failure; do not call a cloud provider.
- Unknown explicit provider/model: return a clear routing error rather than silently choosing Anthropic.
- Explicit cloud provider requests continue using their current provider configuration.

## Tests

Unit coverage:

- `codex` registration has the correct driver, base URL, credential variable, and supported model.
- A configured `codex` provider becomes the default route with `gpt-5.6-sol`.
- Explicit provider/model overrides the default.
- Missing local credentials retain the existing non-local routing behavior.
- Agent Core and Streaming pass canonical route inputs, then pass provider and model separately to every completion call.
- Unknown provider/model inputs fail explicitly.

Live verification:

1. Local proxy `/v1/models` returns `200` and lists `gpt-5.6-sol`.
2. `llm::providers` reports `codex` as configured.
3. `llm::complete` returns content through `codex`.
4. `POST /v1/chat/completions` returns `200` with a valid completion shape.
5. AgentOS TUI starts against the live engine.
6. Workspace release build and targeted Rust tests pass.

## iii Desktop

The Desktop connection issue is separate from model routing:

- `iii-console` owns `127.0.0.1:3113`; it was installed but not running.
- The console now runs persistently on port `3113` and serves its UI with HTTP `200`.
- `iii-desktop` connects directly to the engine bridge on `127.0.0.1:49134`; two established connections were observed after launch.
- Engine REST and WebSocket remain on ports `3111` and `3112` respectively.
