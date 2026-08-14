# Local Codex Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route AgentOS chat through the existing local OpenAI-compatible proxy with `gpt-5.6-sol` as the default while preserving explicit provider/model overrides.

**Architecture:** Add a first-class `codex` provider to `llm-router`, resolve routes through one validated `Route` value, and pass its provider/model fields unchanged through Agent Core and Streaming. Load local secrets from the gitignored `.env`; failed local calls remain visible and never fall back silently to Anthropic.

**Tech Stack:** Rust 2024, iii-sdk 0.22.1, Tokio, serde_json, reqwest, Bash, Bun, Vitest.

## Global Constraints

- Local OpenAI-compatible endpoint: `http://127.0.0.1:8317/v1`.
- Default provider: `codex`.
- Default model: `gpt-5.6-sol`.
- Credential variable: `CODEX_PROXY_API_KEY`; never print or commit its value.
- Base URL variable: `CODEX_PROXY_BASE_URL` with the local endpoint as its default.
- Explicit provider/model input has precedence over defaults.
- A local proxy failure must not fall back to Anthropic.
- Keep the existing `openai` provider pointed at OpenAI cloud.
- Keep `.env` gitignored and mode `0600`.
- Use Bun, never npm/npx.
- iii engine remains on `49134`; REST on `3111`; WebSocket on `3112`; console on `3113`.

---

## File Map

- `workers/llm-router/src/main.rs`: provider registry, route resolution, completion provider selection, and router unit tests.
- `workers/agent-core/src/types.rs`: request-level optional provider/model fields and serde tests.
- `workers/agent-core/src/main.rs`: canonical route request and provider/model handoff through every completion iteration.
- `workers/streaming/src/main.rs`: transport-to-agent preference forwarding and direct stream route handoff.
- `scripts/dev-up.sh`: `.env` loading and model-provider readiness warning.
- `.env.example`: documented local provider variables with no live secret.
- `README.md`: local-proxy quickstart and cloud-provider alternative.
- `e2e/agentos.e2e.test.ts`: live completion model selection through `AGENTOS_E2E_MODEL`.
- `.env`: local runtime configuration only; generated during execution and never committed.

---

### Task 1: Add Validated Codex Routing

**Files:**
- Modify: `workers/llm-router/src/main.rs:8-161,238-355,372-466`
- Test: `workers/llm-router/src/main.rs` inline `#[cfg(test)]` module

**Interfaces:**
- Produces: `Route { provider: String, model: String }`.
- Produces: `resolve_route(state: &RouterState, input: &Value) -> Result<Route, Error>`.
- Produces: `runtime_default_route(get_env) -> Option<Route>` with an injectable environment lookup for deterministic tests.
- Produces: registered provider name `codex`, OpenAI-compatible driver, `CODEX_PROXY_API_KEY`, and `gpt-5.6-sol` ownership.
- Consumes: existing `select_model` only as the final legacy fallback.

- [ ] **Step 1: Write failing provider and routing tests**

Add tests that describe the new contract without mutating process environment:

```rust
#[test]
fn codex_provider_is_registered() {
    let (_, base_url, env_key, driver, models) = default_providers()
        .into_iter()
        .find(|(name, ..)| *name == "codex")
        .expect("codex provider");

    assert_eq!(base_url, "http://127.0.0.1:8317/v1");
    assert_eq!(env_key, "CODEX_PROXY_API_KEY");
    assert!(matches!(driver, Driver::OpenAiCompat));
    assert!(models.contains(&"gpt-5.6-sol"));
}

#[test]
fn configured_codex_is_the_default_route() {
    let route = runtime_default_route(|name| match name {
        "CODEX_PROXY_API_KEY" => Some("local-secret".into()),
        "AGENTOS_DEFAULT_PROVIDER" => Some("codex".into()),
        "AGENTOS_DEFAULT_MODEL" => Some("gpt-5.6-sol".into()),
        _ => None,
    });

    assert_eq!(route, Some(Route {
        provider: "codex".into(),
        model: "gpt-5.6-sol".into(),
    }));
}

#[test]
fn missing_codex_key_disables_the_local_default() {
    let route = runtime_default_route(|name| match name {
        "AGENTOS_DEFAULT_PROVIDER" => Some("codex".into()),
        "AGENTOS_DEFAULT_MODEL" => Some("gpt-5.6-sol".into()),
        _ => None,
    });

    assert_eq!(route, None);
}
```

Update tests that assert exactly ten providers to expect eleven.

- [ ] **Step 2: Run the router tests and confirm the new tests fail**

Run:

```bash
cargo test -p agentos-llm-router --release codex -- --nocapture
```

Expected: FAIL because `Route`, `runtime_default_route`, and the `codex` registry entry do not exist.

- [ ] **Step 3: Add the route type and local provider constants**

Add immediately below `ProviderConfig`:

```rust
const CODEX_PROVIDER: &str = "codex";
const DEFAULT_CODEX_BASE_URL: &str = "http://127.0.0.1:8317/v1";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    provider: String,
    model: String,
}
```

Add the provider to `default_providers()`:

```rust
(
    CODEX_PROVIDER,
    DEFAULT_CODEX_BASE_URL,
    "CODEX_PROXY_API_KEY",
    Driver::OpenAiCompat,
    &["gpt-5.4", "gpt-5.4-mini", "gpt-5.6-sol"],
),
```

- [ ] **Step 4: Implement deterministic runtime-default loading**

Add a pure helper and store its result on `RouterState`:

```rust
fn runtime_default_route<F>(get_env: F) -> Option<Route>
where
    F: Fn(&str) -> Option<String>,
{
    let key = get_env("CODEX_PROXY_API_KEY")?;
    if key.trim().is_empty() {
        return None;
    }

    Some(Route {
        provider: get_env("AGENTOS_DEFAULT_PROVIDER")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| CODEX_PROVIDER.to_string()),
        model: get_env("AGENTOS_DEFAULT_MODEL")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CODEX_MODEL.to_string()),
    })
}
```

Extend state:

```rust
struct RouterState {
    usage: DashMap<String, Usage>,
    providers: DashMap<String, ProviderConfig>,
    default_route: Option<Route>,
}
```

Initialize it with:

```rust
let default_route = runtime_default_route(|name| std::env::var(name).ok());
```

When inserting `codex`, resolve its base URL from `CODEX_PROXY_BASE_URL`, rejecting an empty override by using `DEFAULT_CODEX_BASE_URL`.

- [ ] **Step 5: Write failing route-resolution tests**

Construct a test state with the real provider registry and a local default:
```rust
fn test_state(default_route: Option<Route>) -> RouterState {
    let providers = DashMap::new();
    for (name, base_url, env_key, driver, models) in default_providers() {
        providers.insert(
            name.to_string(),
            ProviderConfig {
                base_url: base_url.to_string(),
                env_key: env_key.to_string(),
                driver,
                models: models.iter().map(|model| model.to_string()).collect(),
            },
        );
    }
    RouterState {
        usage: DashMap::new(),
        providers,
        default_route,
    }
}
```

Use it to assert precedence and errors:
```rust
#[test]
fn explicit_model_resolves_to_codex_owner() {
    let state = test_state(Some(Route {
        provider: "codex".into(),
        model: "gpt-5.6-sol".into(),
    }));
    let route = resolve_route(&state, &json!({ "model": "gpt-5.4-mini" })).unwrap();
    assert_eq!(route.provider, "codex");
    assert_eq!(route.model, "gpt-5.4-mini");
}

#[test]
fn explicit_provider_and_model_override_default() {
    let state = test_state(Some(Route {
        provider: "codex".into(),
        model: "gpt-5.6-sol".into(),
    }));
    let route = resolve_route(
        &state,
        &json!({ "provider": "openai", "model": "gpt-4o" }),
    ).unwrap();
    assert_eq!(route.provider, "openai");
    assert_eq!(route.model, "gpt-4o");
}

#[test]
fn unknown_explicit_model_is_rejected() {
    let state = test_state(None);
    let error = resolve_route(&state, &json!({ "model": "not-a-model" })).unwrap_err();
    assert!(error.to_string().contains("unknown model"));
}
```

- [ ] **Step 6: Run the route-resolution tests and confirm failure**

Run:

```bash
cargo test -p agentos-llm-router --release route -- --nocapture
```

Expected: FAIL because `RouterState::default_route` and `resolve_route` do not exist.

- [ ] **Step 7: Implement one route resolver and use it from both handlers**

Implement `resolve_route` with this exact precedence:

```rust
fn resolve_route(state: &RouterState, input: &Value) -> Result<Route, Error> {
    let explicit_provider = input["provider"].as_str().filter(|value| !value.is_empty());
    let explicit_model = input["model"]
        .as_str()
        .filter(|value| !value.is_empty() && *value != "agentos-default");

    if let (Some(provider), Some(model)) = (explicit_provider, explicit_model) {
        let config = state.providers.get(provider)
            .ok_or_else(|| Error::Handler(format!("unknown provider: {provider}")))?;
        if !config.models.iter().any(|candidate| candidate == model) {
            return Err(Error::Handler(format!(
                "model {model} is not registered for provider {provider}"
            )));
        }
        return Ok(Route { provider: provider.into(), model: model.into() });
    }

    if let Some(model) = explicit_model {
        let owners: Vec<String> = state.providers.iter()
            .filter(|entry| entry.models.iter().any(|candidate| candidate == model))
            .map(|entry| entry.key().clone())
            .collect();
        return match owners.as_slice() {
            [provider] => Ok(Route { provider: provider.clone(), model: model.into() }),
            [] => Err(Error::Handler(format!("unknown model: {model}"))),
            _ => Err(Error::Handler(format!("ambiguous model: {model}"))),
        };
    }

    if explicit_provider.is_some() {
        return Err(Error::Handler("provider requires model".into()));
    }

    if let Some(route) = &state.default_route {
        let config = state.providers.get(&route.provider)
            .ok_or_else(|| Error::Handler(format!(
                "unknown default provider: {}",
                route.provider
            )))?;
        if !config.models.iter().any(|candidate| candidate == &route.model) {
            return Err(Error::Handler(format!(
                "model {} is not registered for provider {}",
                route.model, route.provider
            )));
        }
        return Ok(route.clone());
    }

    let messages = input["messages"].as_array().cloned().unwrap_or_default();
    let tools = input["tools"].as_array().cloned().unwrap_or_default();
    let complexity = score_complexity(&messages, &tools);
    let (provider, model) = select_model(complexity, None);
    Ok(Route { provider: provider.into(), model: model.into() })
}
```

Use it in `route_handler`:

```rust
let messages = input["messages"].as_array().cloned().unwrap_or_default();
let tools = input["tools"].as_array().cloned().unwrap_or_default();
let complexity = score_complexity(&messages, &tools);
let route = resolve_route(&state, &input)?;
Ok(json!({
    "provider": route.provider,
    "model": route.model,
    "complexity": complexity,
}))
```

Use the same resolver at the start of `complete_handler`:

```rust
let route = resolve_route(&state, &input)?;
let provider = state.providers.get(&route.provider)
    .ok_or_else(|| Error::Handler(format!(
        "unknown provider: {}",
        route.provider
    )))?;
let model = route.model.as_str();
```

`complete_handler` must never call its driver until the route has been validated. `providers_handler` must treat whitespace-only keys as unconfigured.

- [ ] **Step 8: Run all router tests**

Run:

```bash
cargo test -p agentos-llm-router --release
```

Expected: all tests PASS.

- [ ] **Step 9: Commit the router change**

```bash
git add workers/llm-router/src/main.rs
git commit -m "feat: add local codex provider routing"
```

---

### Task 2: Preserve Provider and Model Across Chat Boundaries

**Files:**
- Modify: `workers/agent-core/src/types.rs:4-13,64-110`
- Modify: `workers/agent-core/src/main.rs:234-289,351-369`
- Modify: `workers/streaming/src/main.rs:30-180,293-315`
- Test: inline Rust test modules in the same files

**Interfaces:**
- Consumes: `llm::route -> { provider: string, model: string, complexity: number }`.
- Produces: `ChatRequest.provider: Option<String>` and `ChatRequest.model: Option<String>`.
- Produces: every `llm::complete` call receives string fields `provider` and `model` separately.
- Produces: HTTP `model` and optional `provider` fields reach Agent Core unchanged.

- [ ] **Step 1: Write failing ChatRequest serde tests**

Extend the optional-field fixture:

```rust
let json_val = json!({
    "agentId": "agent-2",
    "message": "Hi there",
    "sessionId": "sess-42",
    "systemPrompt": "You are a helpful assistant",
    "provider": "codex",
    "model": "gpt-5.6-sol",
});
let req: ChatRequest = serde_json::from_value(json_val).unwrap();
assert_eq!(req.provider.as_deref(), Some("codex"));
assert_eq!(req.model.as_deref(), Some("gpt-5.6-sol"));
```

- [ ] **Step 2: Run the type test and confirm failure**

Run:

```bash
cargo test -p agentos-core --release test_chat_request_with_optional_fields -- --nocapture
```

Expected: FAIL because `ChatRequest` lacks `provider` and `model`.

- [ ] **Step 3: Add optional routing fields to ChatRequest**

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub message: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(rename = "systemPrompt")]
    pub system_prompt: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}
```

Update direct `ChatRequest` constructors in tests with `provider: None` and `model: None`.

- [ ] **Step 4: Add pure route-handoff helpers and failing tests**

In Agent Core, add:

```rust
fn route_fields(route: &Value) -> Result<(String, String), Error> {
    let provider = route["provider"].as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("llm::route omitted provider".into()))?;
    let model = route["model"].as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("llm::route omitted model".into()))?;
    Ok((provider.into(), model.into()))
}
```

Test both valid extraction and rejection of the old nested-model shape:

```rust
#[test]
fn route_fields_extract_provider_and_model() {
    let fields = route_fields(&json!({
        "provider": "codex",
        "model": "gpt-5.6-sol",
    })).unwrap();
    assert_eq!(fields, ("codex".into(), "gpt-5.6-sol".into()));
}

#[test]
fn route_fields_reject_nested_model_object() {
    assert!(route_fields(&json!({ "model": { "provider": "codex" } })).is_err());
}
```

In Streaming, add a pure helper that forwards transport preferences:

```rust
fn agent_chat_payload(body: &Value, message: &str) -> Value {
    json!({
        "agentId": body["agentId"].as_str().unwrap_or("default"),
        "message": message,
        "sessionId": body.get("sessionId").cloned().unwrap_or(Value::Null),
        "provider": body.get("provider").cloned().unwrap_or(Value::Null),
        "model": body.get("model").cloned().unwrap_or(Value::Null),
    })
}
```

Add a test asserting `codex` and `gpt-5.6-sol` survive.

- [ ] **Step 5: Run targeted handoff tests and confirm failure**

Run:

```bash
cargo test -p agentos-core -p agentos-streaming --release route_fields -- --nocapture
cargo test -p agentos-streaming --release agent_chat_payload -- --nocapture
```

Expected: FAIL because the helpers do not exist.

- [ ] **Step 6: Fix Agent Core's canonical route request**

Build the route request with the router's actual field names:

```rust
let model_config = config.as_ref().and_then(|agent| agent.model.as_ref());
let preferred_provider = req.provider.as_deref().or_else(|| {
    model_config.and_then(|model| model.provider.as_deref())
});
let preferred_model = req.model.as_deref().or_else(|| {
    model_config.and_then(|model| model.model.as_deref())
});

let route = iii.trigger(TriggerRequest {
    function_id: "llm::route".into(),
    payload: json!({
        "messages": [{ "role": "user", "content": &req.message }],
        "tools": &functions,
        "provider": preferred_provider,
        "model": preferred_model,
    }),
    action: None,
    timeout_ms: None,
}).await.map_err(|error| Error::Handler(error.to_string()))?;
let (provider, model) = route_fields(&route)?;
```

Use `provider` and `model` in the first completion and every tool-loop completion:

```rust
payload: json!({
    "provider": &provider,
    "model": &model,
    "systemPrompt": &system_prompt,
    "messages": &messages,
    "functions": &functions,
}),
```

Do not clone the entire route object into `model`.

- [ ] **Step 7: Fix Streaming's direct and HTTP chat paths**

For `stream_chat`, call `llm::route` with `messages`, `tools`, `provider`, and `model`; extract the returned strings and pass both to `llm::complete`.

For `chat_completion` and `stream_sse`, use `agent_chat_payload` so HTTP preferences reach Agent Core. Keep `agentos-default` only as the response-label fallback; do not send it as a real model override when the request omitted `model`.

- [ ] **Step 8: Run Agent Core and Streaming tests**

Run:

```bash
cargo test -p agentos-core -p agentos-streaming --release
```

Expected: all tests PASS.

- [ ] **Step 9: Commit the chat-boundary fix**

```bash
git add workers/agent-core/src/types.rs workers/agent-core/src/main.rs workers/streaming/src/main.rs
git commit -m "fix: preserve llm provider across chat boundaries"
```

---

### Task 3: Load and Document Local Runtime Configuration

**Files:**
- Modify: `scripts/dev-up.sh:13-59`
- Modify: `.env.example:1-8`
- Modify: `README.md:52-67,192-203`
- Runtime only: `.env`

**Interfaces:**
- Consumes: gitignored `${ROOT}/.env`.
- Produces: exported `CODEX_PROXY_BASE_URL`, `CODEX_PROXY_API_KEY`, `AGENTOS_DEFAULT_PROVIDER`, and `AGENTOS_DEFAULT_MODEL` for every worker process.
- Treats the gitignored `.env` as the worker startup source of truth; later shell exports may override it only after the file has been loaded.

- [ ] **Step 1: Add `.env` loading before provider checks**

Immediately after `ROOT`, `PIDFILE`, and `RELEASE_DIR`:

```bash
if [[ -f "$ROOT/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "$ROOT/.env"
    set +a
fi
```

Keep `III_URL` defaulting after this block so `.env` can override it.

- [ ] **Step 2: Replace the Anthropic-only warning**

```bash
if [[ -z "${CODEX_PROXY_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "warning: no model provider credential is configured"
fi
```

Delete the inaccurate claim that the router will fall through to mocks.

- [ ] **Step 3: Update `.env.example` without live credentials**

The provider section must be:

```dotenv
ANTHROPIC_API_KEY=
OPENAI_API_KEY=
CODEX_PROXY_BASE_URL=http://127.0.0.1:8317/v1
CODEX_PROXY_API_KEY=
AGENTOS_DEFAULT_PROVIDER=codex
AGENTOS_DEFAULT_MODEL=gpt-5.6-sol
```

Never place the working proxy key in `.env.example`.

- [ ] **Step 4: Update README quickstart and verification notes**

Make local proxy the primary example:

```bash
cp .env.example .env
$EDITOR .env   # set CODEX_PROXY_API_KEY for http://127.0.0.1:8317/v1
```

State that Anthropic is optional and selected only by an explicit provider/model or when no configured local default exists. Replace `npm ci && npm run test:e2e` with `bun install && bun run test:e2e`.

- [ ] **Step 5: Create the gitignored local `.env` without printing the key**

Run this exact Bun program from the repository root. It parses the first quoted `api-keys` entry, preserves existing unrelated `.env` lines, and writes mode `0600`:

```bash
bun -e '
import { existsSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
const proxyPath = join(homedir(), ".cli-proxy-api", "config.yaml");
const lines = readFileSync(proxyPath, "utf8").split(/\r?\n/);
const section = lines.findIndex((line) => line.startsWith("api-keys:"));
let key = "";
for (let i = section + 1; i < lines.length && (lines[i].startsWith(" ") || !lines[i].trim()); i++) {
  const item = lines[i].trim();
  if (!item.startsWith("-")) continue;
  const raw = item.slice(1).trim();
  const quoted = raw.match(/^"(?:\\.|[^"\\])*"/);
  key = quoted ? JSON.parse(quoted[0]) : raw.replace(/\s+#.*$/, "").trim();
  if (key) break;
}
if (!key) throw new Error("no api-keys entry in " + proxyPath);
const managed = new Set(["CODEX_PROXY_BASE_URL", "CODEX_PROXY_API_KEY", "AGENTOS_DEFAULT_PROVIDER", "AGENTOS_DEFAULT_MODEL"]);
const existing = existsSync(".env") ? readFileSync(".env", "utf8").split(/\r?\n/) : [];
const kept = existing.filter((line) => !managed.has(line.split("=", 1)[0]));
const output = [...kept.filter(Boolean),
  "CODEX_PROXY_BASE_URL=http://127.0.0.1:8317/v1",
  "CODEX_PROXY_API_KEY=" + key,
  "AGENTOS_DEFAULT_PROVIDER=codex",
  "AGENTOS_DEFAULT_MODEL=gpt-5.6-sol",
  "",
].join("\n");
writeFileSync(".env", output, { mode: 0o600 });
chmodSync(".env", 0o600);
console.log("configured local codex provider without printing its credential");
'
```

- [ ] **Step 6: Verify shell syntax, file mode, and redacted variable presence**

Run:

```bash
bash -n scripts/dev-up.sh
stat -f '%Lp' .env
```

Expected: syntax exits `0`; mode prints `600`.

Check presence without printing values:

```bash
bun -e '
const text = await Bun.file(".env").text();
for (const name of ["CODEX_PROXY_BASE_URL", "CODEX_PROXY_API_KEY", "AGENTOS_DEFAULT_PROVIDER", "AGENTOS_DEFAULT_MODEL"]) {
  const line = text.split(/\r?\n/).find((entry) => entry.startsWith(name + "="));
  console.log(name + "=" + (line && line.length > name.length + 1 ? "present" : "missing"));
}
'
```

Expected: all four print `present`.

- [ ] **Step 7: Commit configuration documentation**

```bash
git add scripts/dev-up.sh .env.example README.md
git commit -m "docs: configure local codex runtime"
```

Do not add `.env`.

---

### Task 4: Verify End-to-End Local Chat

**Files:**
- Modify: `e2e/agentos.e2e.test.ts:6-35`
- Runtime: release binaries and persistent services

**Interfaces:**
- Consumes: local proxy and `.env` from Task 3.
- Produces: an environment-neutral live test model selected by `AGENTOS_E2E_MODEL`, defaulting to this project's `gpt-5.6-sol`.
- Proves: `/v1/chat/completions` returns a real local-model response and no Anthropic request is needed.

- [ ] **Step 1: Make the live test model explicit and configurable**

Add near `baseUrl`:

```typescript
const chatModel = process.env.AGENTOS_E2E_MODEL || "gpt-5.6-sol";
```

Use it in the completion request:

```typescript
body: JSON.stringify({
  model: chatModel,
  messages: [{ role: "user", content: "Reply with the word READY only." }],
}),
```

Keep the response-shape assertions unchanged.

- [ ] **Step 2: Run the test before restarting workers and confirm the old runtime fails**

Run:

```bash
bun run test:e2e
```

Expected before restart: the live chat assertion still fails because running workers contain the old Anthropic-default binaries.

- [ ] **Step 3: Build the affected workspace**

Run:

```bash
cargo build --workspace --release
```

Expected: release build exits `0`.

- [ ] **Step 4: Restart workers with the local `.env`**

Run:

```bash
bash scripts/dev-up.sh stop
bash scripts/dev-up.sh
```

Wait on behavior, not a fixed sleep: poll `http://127.0.0.1:3111/api/health` until it returns `200` and reports more than zero workers.

- [ ] **Step 5: Verify provider registration directly**

Run an exact iii-sdk probe and assert the returned provider record:

```bash
bun -e '
import { registerWorker } from "iii-sdk";
const sdk = registerWorker("ws://127.0.0.1:49134", { workerName: "codex-provider-smoke" });
const result = await sdk.trigger({
  function_id: "llm::providers",
  payload: {},
  timeout_ms: 30000,
});
const provider = result.providers?.find((entry) => entry.name === "codex");
if (!provider) throw new Error("codex provider missing");
if (provider.base_url !== "http://127.0.0.1:8317/v1") throw new Error("wrong codex base URL");
if (provider.configured !== true) throw new Error("codex provider not configured");
console.log(JSON.stringify({
  name: provider.name,
  base_url: provider.base_url,
  configured: provider.configured,
}));
sdk.shutdown();
'
```

Expected:

```json
{"name":"codex","base_url":"http://127.0.0.1:8317/v1","configured":true}
```

Do not print `CODEX_PROXY_API_KEY`.

- [ ] **Step 6: Run a direct local completion smoke test**

Run a direct iii-sdk completion probe:

```bash
bun -e '
import { registerWorker } from "iii-sdk";
const sdk = registerWorker("ws://127.0.0.1:49134", { workerName: "codex-completion-smoke" });
const result = await sdk.trigger({
  function_id: "llm::complete",
  payload: {
    provider: "codex",
    model: "gpt-5.6-sol",
    messages: [{ role: "user", content: "Reply with READY only." }],
  },
  timeout_ms: 120000,
});
if (typeof result.content !== "string" || !result.content.trim()) throw new Error("empty completion");
if (result.model !== "gpt-5.6-sol") throw new Error("wrong completion model");
if (typeof result.usage?.total !== "number") throw new Error("missing usage");
console.log(JSON.stringify({ model: result.model, content: result.content, usage: result.usage }));
sdk.shutdown();
'
```

Expected: non-empty `content`, model `gpt-5.6-sol`, and numeric usage fields.

- [ ] **Step 7: Run the full live E2E suite**

Run:

```bash
bun run test:e2e
```

Expected: both E2E files pass with `16/16` tests and zero failures.

- [ ] **Step 8: Launch and inspect the TUI**

Run the actual surface:

```bash
target/release/agentos-tui
```

Verify it opens against the live engine. Dismiss the known first-run overlay with `Esc` if its separate worker-count display issue remains; send one chat message and confirm a non-empty response from `gpt-5.6-sol`.

- [ ] **Step 9: Re-verify iii Desktop services**

Confirm:

- `http://127.0.0.1:3113/` returns HTTP `200`.
- `iii-desktop` has established TCP connections to `127.0.0.1:49134`.
- `iii-console` remains persistent and listening on `127.0.0.1:3113`.

- [ ] **Step 10: Commit the E2E contract**

```bash
git add e2e/agentos.e2e.test.ts
git commit -m "test: cover local codex chat path"
```

- [ ] **Step 11: Run final focused certification**

Run:

```bash
cargo test -p agentos-llm-router -p agentos-core -p agentos-streaming --release
cargo build --workspace --release
bun run test:e2e
```

Expected: every command exits `0`; no failed Rust or Vitest tests.
