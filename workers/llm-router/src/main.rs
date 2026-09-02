use dashmap::DashMap;
use iii_sdk::errors::Error;
use iii_sdk::{InitOptions, RegisterFunction, register_worker};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Every provider request is bounded. A chat turn on the bus is budgeted at
/// 300 s (`CHAT_TIMEOUT_MS` in the streaming worker), so a provider call has to
/// finish inside that window instead of outliving the turn that pays for it.
/// Before this, both clients were built without `.timeout(...)`: an abandoned
/// trigger left the provider request running, and billing, with nobody waiting.
const PROVIDER_TIMEOUT_DEFAULT_MS: u64 = 240_000;
const PROVIDER_TIMEOUT_MAX_MS: u64 = 240_000;
const PROVIDER_TIMEOUT_MIN_MS: u64 = 1_000;
const PROVIDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PROVIDER_ERROR_BODY_LIMIT: usize = 2_000;

/// Resolve the deadline for a single provider call. A caller may ask for less
/// than the default; nobody may ask for more than `PROVIDER_TIMEOUT_MAX_MS`.
fn provider_timeout(caller_timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        caller_timeout_ms
            .unwrap_or(PROVIDER_TIMEOUT_DEFAULT_MS)
            .clamp(PROVIDER_TIMEOUT_MIN_MS, PROVIDER_TIMEOUT_MAX_MS),
    )
}

fn caller_timeout_ms(input: &Value) -> Option<u64> {
    input
        .get("timeoutMs")
        .or_else(|| input.get("timeout_ms"))
        .and_then(Value::as_u64)
}

fn provider_client(builder: reqwest::ClientBuilder) -> reqwest::Result<reqwest::Client> {
    builder
        .timeout(provider_timeout(None))
        .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
        .build()
}

struct RouterState {
    usage: DashMap<String, Usage>,
    providers: DashMap<String, ProviderConfig>,
    default_route: Option<Route>,
}

/// Order used when the operator pinned no default and the request named no
/// provider. Only providers that require a credential are listed: a keyless
/// provider such as `ollama` also needs a server running on this machine, so
/// selecting it automatically would trade one silent failure for another.
const AUTO_ROUTE_PREFERENCE: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    CODEX_PROVIDER,
    "groq",
    "deepseek",
    "mistral",
    "together",
    "fireworks",
    "openrouter",
];

impl RouterState {
    fn provider_is_available(&self, provider: &str) -> bool {
        self.providers
            .get(provider)
            .is_some_and(|config| config.is_available())
    }

    /// First provider, in the documented preference order, whose credential is
    /// actually present.
    fn available_route(&self) -> Option<Route> {
        AUTO_ROUTE_PREFERENCE.iter().find_map(|provider| {
            let config = self.providers.get(*provider)?;
            if !config.requires_credential() || !config.is_available() {
                return None;
            }
            let model = if *provider == CODEX_PROVIDER
                && config
                    .models
                    .iter()
                    .any(|model| model == DEFAULT_CODEX_MODEL)
            {
                DEFAULT_CODEX_MODEL.to_string()
            } else {
                config.models.first().cloned()?
            };
            Some(Route {
                provider: (*provider).to_string(),
                model,
            })
        })
    }

    /// Structured refusal naming the variables the operator can set, instead of
    /// a 401 from a provider the user never configured.
    fn missing_credential_error(&self) -> Error {
        let variables: Vec<String> = AUTO_ROUTE_PREFERENCE
            .iter()
            .filter_map(|provider| self.providers.get(*provider))
            .filter(|config| config.requires_credential())
            .map(|config| config.env_key.clone())
            .collect();
        Error::Handler(format!(
            "provider_credential_missing: no provider credential is configured; set one of {} in the active .env, or send an explicit provider and model",
            variables.join(", ")
        ))
    }

    fn ensure_credential(&self, provider: &str) -> Result<(), Error> {
        let config = self
            .providers
            .get(provider)
            .ok_or_else(|| Error::Handler(format!("unknown provider: {provider}")))?;
        if config.is_available() {
            return Ok(());
        }
        Err(credential_missing_error(provider, &config.env_key))
    }
}

struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FunctionCall {
    #[serde(rename = "callId")]
    call_id: String,
    id: String,
    arguments: Value,
}

impl FunctionCall {
    fn normalized(call_id: String, id: String, arguments: Value) -> Option<Self> {
        if call_id.is_empty() || id.is_empty() {
            return None;
        }
        Some(Self {
            call_id,
            id,
            arguments,
        })
    }

    fn from_normalized_value(value: Value) -> Option<Self> {
        let call: Self = serde_json::from_value(value).ok()?;
        Self::normalized(call.call_id, call.id, call.arguments)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ToolAliases {
    by_function_id: BTreeMap<String, String>,
    by_provider_name: BTreeMap<String, String>,
}

impl ToolAliases {
    fn for_request(tools: &[Value], messages: &[Value]) -> Self {
        let mut function_ids = BTreeSet::new();
        function_ids.extend(tools.iter().filter_map(agent_tool_id).map(str::to_owned));
        for message in messages {
            function_ids.extend(
                message_function_calls(message)
                    .into_iter()
                    .map(|call| call.id),
            );
        }
        Self::from_function_ids(function_ids)
    }

    fn from_function_ids(function_ids: impl IntoIterator<Item = String>) -> Self {
        let mut aliases = Self::default();
        for function_id in function_ids {
            if function_id.is_empty() || aliases.by_function_id.contains_key(&function_id) {
                continue;
            }
            let base = provider_tool_name(&function_id);
            let mut provider_name = base.clone();
            let mut discriminator = 2_u64;
            while aliases.by_provider_name.contains_key(&provider_name) {
                let suffix = format!("_{discriminator}");
                provider_name = format!("{}{}", &base[..base.len().min(64 - suffix.len())], suffix);
                discriminator += 1;
            }
            aliases
                .by_function_id
                .insert(function_id.clone(), provider_name.clone());
            aliases.by_provider_name.insert(provider_name, function_id);
        }
        aliases
    }

    fn provider_name(&self, function_id: &str) -> Option<&str> {
        self.by_function_id.get(function_id).map(String::as_str)
    }

    fn function_id(&self, provider_name: &str) -> Option<&str> {
        self.by_provider_name.get(provider_name).map(String::as_str)
    }
}

fn provider_tool_name(function_id: &str) -> String {
    let bytes = function_id.as_bytes();
    let mut encoded = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' => encoded.push(byte as char),
            b'_' => encoded.push_str("_u"),
            b':' if bytes.get(index + 1) == Some(&b':') => {
                encoded.push_str("__");
                index += 1;
            }
            b':' => encoded.push_str("_c"),
            _ => encoded.push_str(&format!("_x{byte:02x}")),
        }
        index += 1;
    }

    if encoded.len() <= 64 {
        return encoded;
    }

    // Provider names are capped at 64 characters. The full mapping remains in
    // ToolAliases; the hash only keeps long aliases stable and readable.
    let hash = function_id
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{}_h{hash:016x}", &encoded[..46])
}

struct ProviderConfig {
    base_url: String,
    env_key: String,
    driver: Driver,
    models: Vec<String>,
    /// Credential snapshot taken at boot. `None` means the provider needs a
    /// credential and the environment does not have one, so routing must refuse
    /// the provider instead of sending an unauthenticated request and reporting
    /// the provider's 401 as if the user had chosen it.
    credential: Option<String>,
}

impl ProviderConfig {
    fn requires_credential(&self) -> bool {
        !self.env_key.is_empty()
    }

    fn is_available(&self) -> bool {
        self.credential.is_some()
    }
}

/// `None` when the provider requires a credential that the environment does not
/// have; `Some("")` for a provider such as `ollama` that needs no credential.
fn provider_credential<F>(env_key: &str, get_env: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    if env_key.is_empty() {
        return Some(String::new());
    }
    get_env(env_key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn credential_missing_error(provider: &str, env_key: &str) -> Error {
    Error::Handler(format!(
        "provider_credential_missing: provider {provider} has no credential; set {env_key} in the active .env, or choose a provider whose credential is present"
    ))
}

fn provider_base_url_env(provider: &str) -> String {
    let normalized: String = provider
        .to_ascii_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("AGENTOS_{normalized}_BASE_URL")
}

/// A provider may be redirected at a gateway, but only to a URL that cannot
/// quietly exfiltrate the credential: `https` anywhere, `http` only on a
/// loopback literal, and never with embedded credentials, query or fragment.
/// Anything else keeps the compiled-in default.
fn resolve_provider_base_url(default_base_url: &str, override_value: Option<&str>) -> String {
    let Some(raw) = override_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return default_base_url.to_string();
    };
    let Ok(url) = reqwest::Url::parse(raw) else {
        return default_base_url.to_string();
    };
    let is_loopback_literal = url
        .host_str()
        .and_then(|host| {
            let host = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host);
            host.parse::<std::net::IpAddr>().ok()
        })
        .is_some_and(|host| host.is_loopback());
    let scheme_is_safe = match url.scheme() {
        "https" => true,
        "http" => is_loopback_literal,
        _ => false,
    };
    let is_safe = scheme_is_safe
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();

    if !is_safe {
        return default_base_url.to_string();
    }

    url.to_string().trim_end_matches('/').to_string()
}

const CODEX_PROVIDER: &str = "codex";
const DEFAULT_CODEX_BASE_URL: &str = "http://127.0.0.1:8317/v1";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";

fn resolve_codex_base_url(override_value: Option<&str>) -> String {
    let Some(raw) = override_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return DEFAULT_CODEX_BASE_URL.to_string();
    };

    let Some(url) = reqwest::Url::parse(raw).ok() else {
        return DEFAULT_CODEX_BASE_URL.to_string();
    };
    let is_loopback_literal = url
        .host_str()
        .and_then(|host| {
            let host = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host);
            host.parse::<std::net::IpAddr>().ok()
        })
        .is_some_and(|host| host.is_loopback());
    let is_safe = matches!(url.scheme(), "http" | "https")
        && is_loopback_literal
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();

    if !is_safe {
        return DEFAULT_CODEX_BASE_URL.to_string();
    }

    url.to_string().trim_end_matches('/').to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    provider: String,
    model: String,
}

#[derive(Clone, Copy)]
enum Driver {
    Anthropic,
    OpenAiCompat,
    Gemini,
    #[allow(dead_code)]
    Bedrock,
}

fn default_providers() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    Driver,
    &'static [&'static str],
)> {
    vec![
        (
            "anthropic",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            Driver::Anthropic,
            &[
                "claude-opus-4-20250514",
                "claude-sonnet-4-20250514",
                "claude-haiku-4-5-20251001",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
            ],
        ),
        (
            "openai",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            Driver::OpenAiCompat,
            &["gpt-4o", "gpt-4o-mini", "o1", "o3-mini"],
        ),
        (
            "google",
            "https://generativelanguage.googleapis.com/v1beta",
            "GOOGLE_API_KEY",
            Driver::Gemini,
            &["gemini-2.0-flash", "gemini-2.0-pro"],
        ),
        (
            "groq",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
            Driver::OpenAiCompat,
            &["llama-3.3-70b-versatile", "mixtral-8x7b-32768"],
        ),
        (
            "together",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
            Driver::OpenAiCompat,
            &[
                "meta-llama/Llama-3.3-70B-Instruct",
                "mistralai/Mixtral-8x22B-Instruct-v0.1",
            ],
        ),
        (
            "deepseek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
            Driver::OpenAiCompat,
            &["deepseek-chat", "deepseek-reasoner"],
        ),
        (
            "mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
            Driver::OpenAiCompat,
            &["mistral-large-latest", "mistral-small-latest"],
        ),
        (
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
            "FIREWORKS_API_KEY",
            Driver::OpenAiCompat,
            &["accounts/fireworks/models/llama-v3p3-70b-instruct"],
        ),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            Driver::OpenAiCompat,
            &[
                "anthropic/claude-opus-4-20250514",
                "google/gemini-2.0-flash-001",
            ],
        ),
        (
            "ollama",
            "http://localhost:11434/v1",
            "",
            Driver::OpenAiCompat,
            &["llama3.3", "qwen2.5", "deepseek-r1"],
        ),
        (
            CODEX_PROVIDER,
            DEFAULT_CODEX_BASE_URL,
            "CODEX_PROXY_API_KEY",
            Driver::OpenAiCompat,
            &["gpt-5.4", "gpt-5.4-mini", "gpt-5.6-sol"],
        ),
    ]
}

struct RuntimeDefaultResolution {
    route: Option<Route>,
    disabled_provider: Option<String>,
}

/// Env key of a provider in the compiled-in catalogue.
fn provider_env_key(provider: &str) -> Option<&'static str> {
    default_providers()
        .into_iter()
        .find(|(name, ..)| *name == provider)
        .map(|(_, _, env_key, _, _)| env_key)
}

/// Model used when a default provider is pinned without a model.
fn provider_default_model(provider: &str) -> Option<&'static str> {
    if provider == CODEX_PROVIDER {
        return Some(DEFAULT_CODEX_MODEL);
    }
    default_providers()
        .into_iter()
        .find(|(name, ..)| *name == provider)
        .and_then(|(_, _, _, _, models)| models.first().copied())
}

/// Turn `AGENTOS_DEFAULT_PROVIDER` / `AGENTOS_DEFAULT_MODEL` into a route, but
/// only when the credential *that provider* needs is present. The old version
/// gated every default on `CODEX_PROXY_API_KEY`, so an operator who pinned
/// `openai` with a valid `OPENAI_API_KEY` silently lost the default and was
/// routed to Anthropic instead.
fn resolve_runtime_default<F>(get_env: F) -> RuntimeDefaultResolution
where
    F: Fn(&str) -> Option<String>,
{
    let configured = |name: &str| {
        get_env(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let provider = configured("AGENTOS_DEFAULT_PROVIDER");
    let model = configured("AGENTOS_DEFAULT_MODEL");
    let default_requested = provider.is_some() || model.is_some();
    let provider_name = provider.unwrap_or_else(|| CODEX_PROVIDER.to_string());
    let disabled = |provider_name: String| RuntimeDefaultResolution {
        route: None,
        disabled_provider: default_requested.then_some(provider_name),
    };

    let Some(env_key) = provider_env_key(&provider_name) else {
        return RuntimeDefaultResolution {
            route: None,
            disabled_provider: Some(provider_name),
        };
    };
    if provider_credential(env_key, &configured).is_none() {
        return disabled(provider_name);
    }
    let Some(model) = model.or_else(|| provider_default_model(&provider_name).map(str::to_string))
    else {
        return disabled(provider_name);
    };

    RuntimeDefaultResolution {
        route: Some(Route {
            provider: provider_name,
            model,
        }),
        disabled_provider: None,
    }
}

fn score_complexity(messages: &[Value], tools: &[Value]) -> u32 {
    let mut score: u32 = 0;
    if let Some(last) = messages.last() {
        let content = last["content"].as_str().unwrap_or("");
        score += (content.len() as u32) / 100;
        if content.contains("```") || content.contains("function") || content.contains("class") {
            score += 20;
        }
        if content.contains("analyze") || content.contains("compare") || content.contains("design")
        {
            score += 15;
        }
    }
    score += (tools.len() as u32) * 5;
    if messages.len() > 10 {
        score += 10;
    }
    score
}

fn route_field<'a>(input: &'a Value, name: &str) -> Result<Option<&'a str>, Error> {
    match input.get(name) {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(Value::String(_)) => Ok(None),
        Some(_) => Err(Error::Handler(format!("{name} must be a string"))),
    }
}

fn model_alias(model: &str) -> Option<(&'static str, &'static str)> {
    match model {
        "opus" | "claude-opus" => Some(("anthropic", "claude-opus-4-20250514")),
        "sonnet" | "claude-sonnet" => Some(("anthropic", "claude-sonnet-4-20250514")),
        "haiku" | "claude-haiku" => Some(("anthropic", "claude-haiku-4-5-20251001")),
        "gpt-4o" => Some(("openai", "gpt-4o")),
        "gemini" => Some(("google", "gemini-2.0-flash")),
        _ => None,
    }
}

fn select_model(complexity: u32, preferred: Option<&str>) -> (&'static str, &'static str) {
    if let Some(preferred) = preferred
        && let Some(route) = model_alias(preferred)
    {
        return route;
    }
    match complexity {
        0..=10 => ("anthropic", "claude-haiku-4-5-20251001"),
        11..=40 => ("anthropic", "claude-sonnet-4-20250514"),
        _ => ("anthropic", "claude-opus-4-20250514"),
    }
}

/// Name resolution: which provider and model does this request mean? An
/// explicitly requested provider/model is resolved as asked even when its
/// credential is absent, so `agentos::llm::route` stays a catalogue query;
/// `resolve_complete_route` is where a call is refused. Automatic selection is
/// the exception: choosing *for* the caller has to consider which credential
/// exists, or the answer is a guess.
fn resolve_route(state: &RouterState, input: &Value) -> Result<Route, Error> {
    let explicit_provider = route_field(input, "provider")?;
    let requested_model = route_field(input, "model")?;
    let explicit_model = requested_model
        .filter(|value| *value != "agentos-default")
        .map(|model| {
            model_alias(model)
                .map(|(_, canonical)| canonical)
                .unwrap_or(model)
        });

    if let (Some(provider), Some(model)) = (explicit_provider, explicit_model) {
        let config = state
            .providers
            .get(provider)
            .ok_or_else(|| Error::Handler(format!("unknown provider: {provider}")))?;
        if !config.models.iter().any(|candidate| candidate == model) {
            return Err(Error::Handler(format!(
                "model {model} is not registered for provider {provider}"
            )));
        }
        return Ok(Route {
            provider: provider.into(),
            model: model.into(),
        });
    }

    if let Some(model) = explicit_model {
        let owners: Vec<String> = state
            .providers
            .iter()
            .filter(|entry| entry.models.iter().any(|candidate| candidate == model))
            .map(|entry| entry.key().clone())
            .collect();
        return match owners.as_slice() {
            [provider] => Ok(Route {
                provider: provider.clone(),
                model: model.into(),
            }),
            [] => Err(Error::Handler(format!("unknown model: {model}"))),
            _ => Err(Error::Handler(format!("ambiguous model: {model}"))),
        };
    }

    if explicit_provider.is_some() {
        return Err(Error::Handler("provider requires model".into()));
    }

    if let Some(route) = &state.default_route {
        let config = state.providers.get(&route.provider).ok_or_else(|| {
            Error::Handler(format!("unknown default provider: {}", route.provider))
        })?;
        let model = model_alias(&route.model)
            .map(|(_, canonical)| canonical)
            .unwrap_or(route.model.as_str());
        if !config.models.iter().any(|candidate| candidate == model) {
            return Err(Error::Handler(format!(
                "model {} is not registered for provider {}",
                route.model, route.provider
            )));
        }
        return Ok(Route {
            provider: route.provider.clone(),
            model: model.into(),
        });
    }

    // Automatic routing. `select_model` only ever names Anthropic models, so it
    // may be used only when the Anthropic credential exists; otherwise route to
    // a provider the operator actually configured, and refuse with the missing
    // variable names when there is none.
    let messages = input["messages"].as_array().cloned().unwrap_or_default();
    let tools = input["tools"].as_array().cloned().unwrap_or_default();
    let complexity = score_complexity(&messages, &tools);
    let (provider, model) = select_model(complexity, None);
    if state.provider_is_available(provider) {
        return Ok(Route {
            provider: provider.into(),
            model: model.into(),
        });
    }
    state
        .available_route()
        .ok_or_else(|| state.missing_credential_error())
}

fn has_explicit_route(input: &Value) -> bool {
    let provider_explicit = match input.get("provider") {
        None => false,
        Some(Value::String(value)) => !value.is_empty(),
        Some(_) => true,
    };
    let model_explicit = match input.get("model") {
        None => false,
        Some(Value::String(value)) => !value.is_empty() && value != "agentos-default",
        Some(_) => true,
    };
    provider_explicit || model_explicit
}

/// Route a call that is about to be made: resolve the name, then refuse it here
/// if the provider has no credential, naming the variable to set. The old code
/// sent the request anyway with an empty key and reported the provider's 401.
fn resolve_complete_route(state: &RouterState, input: &Value) -> Result<Route, Error> {
    let route = if state.default_route.is_none()
        && !has_explicit_route(input)
        && state.provider_is_available("anthropic")
    {
        resolve_route(
            state,
            &json!({
                "provider": "anthropic",
                "model": "claude-sonnet-4-20250514",
            }),
        )?
    } else {
        resolve_route(state, input)?
    };
    state.ensure_credential(&route.provider)?;
    Ok(route)
}

fn client_for_provider<'a>(
    provider: &str,
    shared_client: &'a reqwest::Client,
    direct_client: &'a reqwest::Client,
) -> &'a reqwest::Client {
    if provider == CODEX_PROVIDER {
        direct_client
    } else {
        shared_client
    }
}

fn completion_tools(input: &Value) -> Vec<Value> {
    input["tools"]
        .as_array()
        .or_else(|| input["functions"].as_array())
        .cloned()
        .unwrap_or_default()
}

fn tool_field<'a>(tool: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| tool.get(*name))
}

fn agent_tool_id(tool: &Value) -> Option<&str> {
    tool_field(tool, &["id", "function_id", "functionId", "name"])
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn agent_tool_schema(tool: &Value) -> Value {
    tool_field(
        tool,
        &[
            "request_format",
            "requestFormat",
            "input_schema",
            "inputSchema",
            "parameters",
        ],
    )
    .filter(|schema| !schema.is_null())
    .cloned()
    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }))
}

fn anthropic_tools(tools: &[Value], aliases: &ToolAliases) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let mut native = serde_json::Map::new();
            native.insert(
                "name".into(),
                json!(aliases.provider_name(agent_tool_id(tool)?)?),
            );
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                native.insert("description".into(), json!(description));
            }
            native.insert("input_schema".into(), agent_tool_schema(tool));
            Some(Value::Object(native))
        })
        .collect()
}

fn openai_tools(tools: &[Value], aliases: &ToolAliases) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let mut function = serde_json::Map::new();
            function.insert(
                "name".into(),
                json!(aliases.provider_name(agent_tool_id(tool)?)?),
            );
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                function.insert("description".into(), json!(description));
            }
            function.insert("parameters".into(), agent_tool_schema(tool));
            Some(json!({ "type": "function", "function": function }))
        })
        .collect()
}

fn gemini_tools(tools: &[Value], aliases: &ToolAliases) -> Vec<Value> {
    let declarations: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            let mut declaration = serde_json::Map::new();
            declaration.insert(
                "name".into(),
                json!(aliases.provider_name(agent_tool_id(tool)?)?),
            );
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                declaration.insert("description".into(), json!(description));
            }
            declaration.insert("parameters".into(), agent_tool_schema(tool));
            Some(Value::Object(declaration))
        })
        .collect();
    if declarations.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "functionDeclarations": declarations })]
    }
}

fn message_function_calls(message: &Value) -> Vec<FunctionCall> {
    message
        .get("tool_calls")
        .or_else(|| message.get("toolCalls"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| FunctionCall::from_normalized_value(call.clone()))
        .collect()
}

fn message_tool_call_id(message: &Value) -> Option<&str> {
    message
        .get("tool_call_id")
        .or_else(|| message.get("toolCallId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn remove_assistant_tool_calls(message: &mut Value) {
    if let Some(object) = message.as_object_mut() {
        object.remove("tool_calls");
        object.remove("toolCalls");
    }
}

fn anthropic_messages(messages: &[Value], aliases: &ToolAliases) -> Vec<Value> {
    let mut provider_messages: Vec<Value> = Vec::with_capacity(messages.len());

    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                let calls = message_function_calls(message);
                if calls.is_empty() {
                    let mut native = message.clone();
                    remove_assistant_tool_calls(&mut native);
                    provider_messages.push(native);
                    continue;
                }

                let mut content = Vec::new();
                if let Some(text) = message.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    content.push(json!({ "type": "text", "text": text }));
                }
                content.extend(calls.into_iter().filter_map(|call| {
                    Some(json!({
                        "type": "tool_use",
                        "id": call.call_id,
                        "name": aliases.provider_name(&call.id)?,
                        "input": call.arguments,
                    }))
                }));
                provider_messages.push(json!({ "role": "assistant", "content": content }));
            }
            Some("tool") => {
                let Some(call_id) = message_tool_call_id(message) else {
                    continue;
                };
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": message.get("content").cloned().unwrap_or(Value::Null),
                });

                let appended = provider_messages.last_mut().is_some_and(|previous| {
                    if previous.get("role").and_then(Value::as_str) != Some("user") {
                        return false;
                    }
                    let Some(content) = previous.get_mut("content").and_then(Value::as_array_mut)
                    else {
                        return false;
                    };
                    if !content
                        .iter()
                        .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
                    {
                        return false;
                    }
                    content.push(block.clone());
                    true
                });
                if !appended {
                    provider_messages.push(json!({ "role": "user", "content": [block] }));
                }
            }
            _ => provider_messages.push(message.clone()),
        }
    }

    provider_messages
}

fn openai_messages(messages: &[Value], aliases: &ToolAliases) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut native = message.clone();
            let calls = message_function_calls(message);
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                remove_assistant_tool_calls(&mut native);
                let calls: Vec<Value> = calls
                    .into_iter()
                    .filter_map(|call| {
                        Some(json!({
                            "id": call.call_id,
                            "type": "function",
                            "function": {
                                "name": aliases.provider_name(&call.id)?,
                                "arguments": serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "null".to_string()),
                            },
                        }))
                    })
                    .collect();
                if !calls.is_empty() {
                    native["tool_calls"] = Value::Array(calls);
                }
            }
            if message.get("role").and_then(Value::as_str) == Some("tool")
                && let Some(call_id) = message_tool_call_id(message)
            {
                native["tool_call_id"] = json!(call_id);
                if let Some(object) = native.as_object_mut() {
                    object.remove("toolCallId");
                }
            }
            native
        })
        .collect()
}

fn gemini_function_response(content: Option<&Value>) -> Value {
    let response = match content {
        Some(Value::String(content)) => {
            serde_json::from_str(content).unwrap_or_else(|_| json!(content))
        }
        Some(content) => content.clone(),
        None => Value::Null,
    };
    if response.is_object() {
        response
    } else {
        json!({ "result": response })
    }
}

fn gemini_messages(messages: &[Value], aliases: &ToolAliases) -> Vec<Value> {
    let mut call_names = BTreeMap::new();
    let mut contents: Vec<Value> = Vec::with_capacity(messages.len());

    for message in messages {
        let calls = message_function_calls(message);
        for call in &calls {
            if let Some(provider_name) = aliases.provider_name(&call.id) {
                call_names.insert(call.call_id.clone(), provider_name.to_string());
            }
        }

        match message.get("role").and_then(Value::as_str) {
            Some("assistant") if !calls.is_empty() => {
                let mut parts = Vec::new();
                if let Some(text) = message.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    parts.push(json!({ "text": text }));
                }
                parts.extend(calls.into_iter().filter_map(|call| {
                    Some(json!({
                        "functionCall": {
                            "id": call.call_id,
                            "name": aliases.provider_name(&call.id)?,
                            "args": call.arguments,
                        },
                    }))
                }));
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Some("tool") => {
                let Some(call_id) = message_tool_call_id(message) else {
                    continue;
                };
                let Some(name) = call_names.get(call_id) else {
                    continue;
                };
                let part = json!({
                    "functionResponse": {
                        "id": call_id,
                        "name": name,
                        "response": gemini_function_response(message.get("content")),
                    },
                });
                let appended = contents.last_mut().is_some_and(|previous| {
                    if previous.get("role").and_then(Value::as_str) != Some("user") {
                        return false;
                    }
                    let Some(parts) = previous.get_mut("parts").and_then(Value::as_array_mut)
                    else {
                        return false;
                    };
                    if !parts
                        .iter()
                        .all(|item| item.get("functionResponse").is_some())
                    {
                        return false;
                    }
                    parts.push(part.clone());
                    true
                });
                if !appended {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }
            }
            Some(role) => {
                let Some(text) = message.get("content").and_then(Value::as_str) else {
                    continue;
                };
                let role = if role == "assistant" { "model" } else { "user" };
                contents.push(json!({ "role": role, "parts": [{ "text": text }] }));
            }
            None => {}
        }
    }

    contents
}

fn anthropic_request_body(
    model: &str,
    system_prompt: Option<&str>,
    messages: &[Value],
    tools: &[Value],
    max_tokens: u64,
) -> Value {
    let aliases = ToolAliases::for_request(tools, messages);
    let mut body = json!({
        "model": model,
        "messages": anthropic_messages(messages, &aliases),
        "max_tokens": max_tokens,
    });
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
        body["system"] = json!(system_prompt);
    }
    let tools = anthropic_tools(tools, &aliases);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

fn openai_request_body(
    model: &str,
    system_prompt: Option<&str>,
    messages: &[Value],
    tools: &[Value],
    max_tokens: u64,
) -> Value {
    let aliases = ToolAliases::for_request(tools, messages);
    let mut provider_messages = Vec::with_capacity(messages.len() + 1);
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
        provider_messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    provider_messages.extend(openai_messages(messages, &aliases));

    let mut body = json!({
        "model": model,
        "messages": provider_messages,
        "max_tokens": max_tokens,
    });
    let tools = openai_tools(tools, &aliases);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

fn gemini_request_body(
    system_prompt: Option<&str>,
    messages: &[Value],
    tools: &[Value],
    max_tokens: u64,
) -> Value {
    let aliases = ToolAliases::for_request(tools, messages);
    let contents = gemini_messages(messages, &aliases);
    let mut body = json!({
        "contents": contents,
        "generationConfig": { "maxOutputTokens": max_tokens },
    });
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
        body["systemInstruction"] = json!({ "parts": [{ "text": system_prompt }] });
    }
    let tools = gemini_tools(tools, &aliases);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

/// One provider call. Grouping the fields keeps the boundary honest (the three
/// drivers really do need all of them) and removes the `too_many_arguments`
/// suppressions the old positional signatures needed.
struct ProviderRequest<'a> {
    provider: &'a str,
    base_url: &'a str,
    api_key: &'a str,
    model: &'a str,
    system_prompt: Option<&'a str>,
    messages: &'a [Value],
    tools: &'a [Value],
    max_tokens: u64,
    timeout: Duration,
}

fn truncate_provider_body(body: &str) -> String {
    let body = body.trim();
    if body.chars().count() <= PROVIDER_ERROR_BODY_LIMIT {
        return body.to_string();
    }
    let head: String = body.chars().take(PROVIDER_ERROR_BODY_LIMIT).collect();
    format!("{head} [truncated]")
}

fn provider_transport_error(provider: &str, error: &reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Handler(format!(
            "provider_timeout: {provider} did not answer within the configured timeout ({error})"
        ))
    } else if error.is_connect() {
        Error::Handler(format!(
            "provider_unreachable: {provider} connection failed ({error})"
        ))
    } else {
        Error::Handler(format!("provider_request_failed: {provider} ({error})"))
    }
}

fn provider_status_error(provider: &str, status: u16, body: &str) -> Error {
    let body = truncate_provider_body(body);
    let detail = if body.is_empty() {
        "<empty response body>"
    } else {
        body.as_str()
    };
    Error::Handler(format!(
        "provider_error: {provider} returned HTTP {status}: {detail}"
    ))
}

/// Send a prepared provider request and keep the provider's own words. The old
/// code called `error_for_status()`, whose Display is
/// `HTTP status client error (400 Bad Request) for url (...)` — the message
/// that explains *why* the call failed was thrown away at three call sites.
async fn provider_json(provider: &str, request: reqwest::RequestBuilder) -> Result<Value, Error> {
    let response = request
        .send()
        .await
        .map_err(|error| provider_transport_error(provider, &error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| provider_transport_error(provider, &error))?;
    if !status.is_success() {
        return Err(provider_status_error(provider, status.as_u16(), &body));
    }
    serde_json::from_str(&body).map_err(|error| {
        Error::Handler(format!(
            "provider_invalid_response: {provider} returned HTTP {} with a body that is not JSON ({error}): {}",
            status.as_u16(),
            truncate_provider_body(&body)
        ))
    })
}

/// Anthropic's messages endpoint under a configured base URL. The default
/// catalogue entry is `https://api.anthropic.com`, and a gateway may already
/// carry the `/v1` prefix.
fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

fn openai_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

fn gemini_generate_url(base_url: &str, model: &str) -> String {
    format!(
        "{}/models/{model}:generateContent",
        base_url.trim_end_matches('/')
    )
}

async fn call_anthropic(
    client: &reqwest::Client,
    request: &ProviderRequest<'_>,
) -> Result<Value, Error> {
    let body = anthropic_request_body(
        request.model,
        request.system_prompt,
        request.messages,
        request.tools,
        request.max_tokens,
    );

    provider_json(
        request.provider,
        client
            .post(anthropic_messages_url(request.base_url))
            .header("x-api-key", request.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .timeout(request.timeout)
            .json(&body),
    )
    .await
}

async fn call_openai_compat(
    client: &reqwest::Client,
    request: &ProviderRequest<'_>,
) -> Result<Value, Error> {
    let body = openai_request_body(
        request.model,
        request.system_prompt,
        request.messages,
        request.tools,
        request.max_tokens,
    );

    let mut prepared = client
        .post(openai_completions_url(request.base_url))
        .header("content-type", "application/json")
        .timeout(request.timeout);

    if !request.api_key.is_empty() {
        prepared = prepared.header("authorization", format!("Bearer {}", request.api_key));
    }

    provider_json(request.provider, prepared.json(&body)).await
}

async fn call_gemini(
    client: &reqwest::Client,
    request: &ProviderRequest<'_>,
) -> Result<Value, Error> {
    let body = gemini_request_body(
        request.system_prompt,
        request.messages,
        request.tools,
        request.max_tokens,
    );

    provider_json(
        request.provider,
        client
            .post(gemini_generate_url(request.base_url, request.model))
            .query(&[("key", request.api_key)])
            .header("content-type", "application/json")
            .timeout(request.timeout)
            .json(&body),
    )
    .await
}

fn function_arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(arguments)) => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.clone()))
        }
        Some(arguments) => arguments.clone(),
        None => json!({}),
    }
}

fn function_calls(driver: Driver, result: &Value, aliases: &ToolAliases) -> Vec<FunctionCall> {
    match driver {
        Driver::Anthropic => result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"].as_str() == Some("tool_use"))
            .filter_map(|block| {
                FunctionCall::normalized(
                    block["id"].as_str()?.to_string(),
                    aliases.function_id(block["name"].as_str()?)?.to_string(),
                    function_arguments(block.get("input")),
                )
            })
            .collect(),
        Driver::OpenAiCompat | Driver::Bedrock => result["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice["message"]["tool_calls"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|call| {
                FunctionCall::normalized(
                    call["id"].as_str()?.to_string(),
                    aliases
                        .function_id(call["function"]["name"].as_str()?)?
                        .to_string(),
                    function_arguments(call["function"].get("arguments")),
                )
            })
            .collect(),
        Driver::Gemini => result["candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .flat_map(|(candidate_index, candidate)| {
                candidate["content"]["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(move |(part_index, part)| {
                        let call = part.get("functionCall")?;
                        let id = aliases.function_id(call["name"].as_str()?)?.to_string();
                        let call_id = match call.get("id") {
                            Some(Value::String(id)) if !id.is_empty() => id.clone(),
                            Some(Value::String(_)) | None => {
                                format!("gemini-{candidate_index}-{part_index}")
                            }
                            Some(_) => return None,
                        };
                        FunctionCall::normalized(call_id, id, function_arguments(call.get("args")))
                    })
            })
            .collect(),
    }
}

async fn route_handler(state: Arc<RouterState>, input: Value) -> Result<Value, Error> {
    let messages = input["messages"].as_array().cloned().unwrap_or_default();
    let tools = input["tools"].as_array().cloned().unwrap_or_default();
    let complexity = score_complexity(&messages, &tools);
    let route = resolve_route(&state, &input)?;
    Ok(json!({
        "provider": route.provider,
        "model": route.model,
        "complexity": complexity,
    }))
}

async fn complete_handler(
    state: Arc<RouterState>,
    shared_client: reqwest::Client,
    direct_client: reqwest::Client,
    input: Value,
) -> Result<Value, Error> {
    let route = resolve_complete_route(&state, &input)?;
    // Copy what the call needs and release the map guard: a provider call may
    // now run for minutes, and nothing else should wait behind its shard lock.
    let (base_url, env_key, driver, credential) = {
        let provider = state
            .providers
            .get(&route.provider)
            .ok_or_else(|| Error::Handler(format!("unknown provider: {}", route.provider)))?;
        (
            provider.base_url.clone(),
            provider.env_key.clone(),
            provider.driver,
            provider.credential.clone(),
        )
    };
    let api_key = credential.ok_or_else(|| credential_missing_error(&route.provider, &env_key))?;
    let model = route.model.as_str();
    let client = client_for_provider(&route.provider, &shared_client, &direct_client);
    let messages = input["messages"].as_array().cloned().unwrap_or_default();
    let tools = completion_tools(&input);
    let tool_aliases = ToolAliases::for_request(&tools, &messages);
    let system_prompt = input["systemPrompt"].as_str();
    let max_tokens = input["max_tokens"].as_u64().unwrap_or(4096);

    let request = ProviderRequest {
        provider: &route.provider,
        base_url: &base_url,
        api_key: &api_key,
        model,
        system_prompt,
        messages: &messages,
        tools: &tools,
        max_tokens,
        timeout: provider_timeout(caller_timeout_ms(&input)),
    };

    let start = Instant::now();

    let result = match driver {
        Driver::Anthropic => call_anthropic(client, &request).await?,
        Driver::OpenAiCompat | Driver::Bedrock => call_openai_compat(client, &request).await?,
        Driver::Gemini => call_gemini(client, &request).await?,
    };

    let _elapsed_ms = start.elapsed().as_millis() as u64;

    let input_tokens = result["usage"]["input_tokens"]
        .as_u64()
        .or(result["usage"]["prompt_tokens"].as_u64())
        .or(result["usageMetadata"]["promptTokenCount"].as_u64())
        .unwrap_or(0);
    let output_tokens = result["usage"]["output_tokens"]
        .as_u64()
        .or(result["usage"]["completion_tokens"].as_u64())
        .or(result["usageMetadata"]["candidatesTokenCount"].as_u64())
        .unwrap_or(0);

    let key = format!("{}:{}", route.provider, model);
    let mut usage = state.usage.entry(key).or_insert(Usage {
        input_tokens: 0,
        output_tokens: 0,
        requests: 0,
    });
    usage.input_tokens += input_tokens;
    usage.output_tokens += output_tokens;
    usage.requests += 1;

    let content = result["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"].as_str() == Some("text")))
        .and_then(|b| b["text"].as_str())
        .or_else(|| {
            result["choices"]
                .as_array()
                .and_then(|c| c.first())
                .and_then(|c| c["message"]["content"].as_str())
        })
        .or_else(|| {
            result["candidates"]
                .as_array()
                .and_then(|candidates| candidates.first())
                .and_then(|candidate| candidate["content"]["parts"].as_array())
                .and_then(|parts| parts.iter().find_map(|part| part["text"].as_str()))
        })
        .unwrap_or("");

    let tool_calls = function_calls(driver, &result, &tool_aliases);

    Ok(json!({
        "content": content,
        "model": model,
        "provider": route.provider,
        "toolCalls": tool_calls,
        "usage": {
            "input": input_tokens,
            "output": output_tokens,
            "total": input_tokens + output_tokens,
        }
    }))
}

async fn usage_handler(state: Arc<RouterState>, _input: Value) -> Result<Value, Error> {
    let mut stats = Vec::new();
    for entry in state.usage.iter() {
        let parts: Vec<&str> = entry.key().splitn(2, ':').collect();
        stats.push(json!({
            "provider": parts.first().unwrap_or(&""),
            "model": parts.get(1).unwrap_or(&""),
            "input_tokens": entry.value().input_tokens,
            "output_tokens": entry.value().output_tokens,
            "requests": entry.value().requests,
        }));
    }
    Ok(json!({ "stats": stats }))
}

async fn providers_handler(state: Arc<RouterState>, _input: Value) -> Result<Value, Error> {
    let list: Vec<Value> = state
        .providers
        .iter()
        .map(|entry| {
            let name = entry.key();
            let provider = entry.value();
            json!({
                "name": name,
                "base_url": &provider.base_url,
                "env_key": &provider.env_key,
                "models": &provider.models,
                "configured": provider.is_available(),
            })
        })
        .collect();
    Ok(json!({ "providers": list }))
}

fn models_catalog(state: &RouterState) -> Value {
    let mut models = state
        .providers
        .iter()
        .flat_map(|entry| {
            let provider = entry.key().clone();
            entry
                .value()
                .models
                .iter()
                .map(move |model| {
                    json!({
                        "id": model,
                        "provider": provider,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Value::Array(models)
}

fn provider_catalog(state: &RouterState) -> Value {
    let mut providers = state
        .providers
        .iter()
        .map(|entry| {
            let provider = entry.value();
            json!({
                "name": entry.key(),
                "available": provider.is_available(),
                "modelCount": provider.models.len(),
            })
        })
        .collect::<Vec<_>>();
    providers.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Value::Array(providers)
}

fn model_aliases() -> Value {
    json!({
        "fast": "claude-haiku-4-5-20251001",
        "balanced": "claude-sonnet-4-20250514",
        "powerful": "claude-opus-4-20250514",
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let read_env = |name: &str| std::env::var(name).ok();
    let default_resolution = resolve_runtime_default(read_env);
    if let Some(provider) = &default_resolution.disabled_provider {
        tracing::warn!(
            provider,
            env_key = provider_env_key(provider).unwrap_or("<unknown provider>"),
            "configured default provider has no credential; routing falls back to a provider whose credential is present"
        );
    }
    let state = Arc::new(RouterState {
        usage: DashMap::new(),
        providers: DashMap::new(),
        default_route: default_resolution.route,
    });

    for (name, base_url, env_key, driver, models) in default_providers() {
        state.providers.insert(
            name.to_string(),
            ProviderConfig {
                base_url: if name == CODEX_PROVIDER {
                    resolve_codex_base_url(std::env::var("CODEX_PROXY_BASE_URL").ok().as_deref())
                } else {
                    resolve_provider_base_url(
                        base_url,
                        std::env::var(provider_base_url_env(name)).ok().as_deref(),
                    )
                },
                env_key: env_key.to_string(),
                driver,
                models: models.iter().map(|s| s.to_string()).collect(),
                credential: provider_credential(env_key, &read_env),
            },
        );
    }

    match (&state.default_route, state.available_route()) {
        (Some(route), _) => tracing::info!(
            provider = %route.provider,
            model = %route.model,
            "default route pinned by configuration"
        ),
        (None, Some(route)) => tracing::info!(
            provider = %route.provider,
            model = %route.model,
            "no default pinned; unqualified requests route to the first provider whose credential is present"
        ),
        (None, None) => tracing::warn!(
            "no provider credential is configured; unqualified chat requests will fail with provider_credential_missing until one is set"
        ),
    }

    let shared_client = provider_client(reqwest::Client::builder())?;
    let direct_client = provider_client(reqwest::Client::builder().no_proxy())?;

    {
        let state = state.clone();
        iii.register_function(
            "agentos::llm::route",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                async move { route_handler(state, input).await }
            })
            .description("Route to optimal model based on complexity"),
        );
    }

    {
        let state = state.clone();
        let shared_client = shared_client.clone();
        let direct_client = direct_client.clone();
        iii.register_function(
            "agentos::llm::complete",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let shared_client = shared_client.clone();
                let direct_client = direct_client.clone();
                async move { complete_handler(state, shared_client, direct_client, input).await }
            })
            .description("Send completion request to routed provider"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "agentos::llm::usage",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                async move { usage_handler(state, input).await }
            })
            .description("Get usage stats across all providers"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "agentos::llm::providers",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                async move { providers_handler(state, input).await }
            })
            .description("List available providers and models"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "agentos::llm::models",
            RegisterFunction::new_async(move |_: Value| {
                let state = state.clone();
                async move { Ok::<Value, Error>(models_catalog(&state)) }
            })
            .description("List configured models"),
        );
    }
    {
        let state = state.clone();
        iii.register_function(
            "agentos::llm::provider_catalog",
            RegisterFunction::new_async(move |_: Value| {
                let state = state.clone();
                async move { Ok::<Value, Error>(provider_catalog(&state)) }
            })
            .description("List provider availability"),
        );
    }
    iii.register_function(
        "agentos::llm::aliases",
        RegisterFunction::new_async(
            move |_: Value| async move { Ok::<Value, Error>(model_aliases()) },
        )
        .description("List stable model aliases"),
    );

    let catalog_routes = [
        ("agentos::llm::models", "/api/models"),
        ("agentos::llm::aliases", "/api/models/aliases"),
        ("agentos::llm::provider_catalog", "/api/providers"),
    ];
    for (function_id, path) in catalog_routes {
        agentos_http_adapter::register_http_trigger(
            &iii,
            function_id,
            json!({ "api_path": path, "http_method": "GET" }),
            None,
        )?;
    }

    tracing::info!(
        "llm-router worker ready with {} providers",
        default_providers().len()
    );
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(function_ids: &[&str]) -> ToolAliases {
        ToolAliases::from_function_ids(function_ids.iter().map(|id| (*id).to_string()))
    }

    /// Every provider credentialled: the fixture the routing contract tests
    /// below were written against.
    fn test_state(default_route: Option<Route>) -> RouterState {
        state_with_credentials(default_route, None)
    }

    /// `available = Some(&[...])` restricts which provider credentials exist,
    /// which is what a real machine looks like.
    fn state_with_credentials(
        default_route: Option<Route>,
        available: Option<&[&str]>,
    ) -> RouterState {
        let providers = DashMap::new();
        for (name, base_url, env_key, driver, models) in default_providers() {
            let credential = if env_key.is_empty() {
                // Mirrors `provider_credential`: a keyless provider is always
                // "available"; it just may not have a server behind it.
                Some(String::new())
            } else {
                match available {
                    None => Some(format!("test-{name}-key")),
                    Some(available) if available.contains(&name) => {
                        Some(format!("test-{name}-key"))
                    }
                    Some(_) => None,
                }
            };
            providers.insert(
                name.to_string(),
                ProviderConfig {
                    base_url: base_url.to_string(),
                    env_key: env_key.to_string(),
                    driver,
                    models: models.iter().map(|model| model.to_string()).collect(),
                    credential,
                },
            );
        }
        RouterState {
            usage: DashMap::new(),
            providers,
            default_route,
        }
    }

    #[test]
    fn codex_provider_is_registered() {
        let (_, base_url, env_key, driver, models) = default_providers()
            .into_iter()
            .find(|(name, ..)| *name == CODEX_PROVIDER)
            .expect("codex provider");

        assert_eq!(base_url, DEFAULT_CODEX_BASE_URL);
        assert_eq!(env_key, "CODEX_PROXY_API_KEY");
        assert!(matches!(driver, Driver::OpenAiCompat));
        assert!(models.contains(&DEFAULT_CODEX_MODEL));
    }

    #[test]
    fn codex_base_url_only_accepts_loopback_literals() {
        assert_eq!(resolve_codex_base_url(None), DEFAULT_CODEX_BASE_URL);
        assert_eq!(resolve_codex_base_url(Some("   ")), DEFAULT_CODEX_BASE_URL);
        assert_eq!(
            resolve_codex_base_url(Some("http://127.0.0.2:8317/v1/")),
            "http://127.0.0.2:8317/v1"
        );
        assert_eq!(
            resolve_codex_base_url(Some("https://[::1]:8317/v1/")),
            "https://[::1]:8317/v1"
        );
        for unsafe_url in [
            "not a url",
            "ftp://127.0.0.1:8317/v1",
            "http://localhost:8317/v1",
            "http://192.168.1.10:8317/v1",
            "http://user:password@127.0.0.1:8317/v1",
            "http://127.0.0.1:8317/v1?target=https://example.com",
            "http://127.0.0.1:8317/v1#fragment",
        ] {
            assert_eq!(
                resolve_codex_base_url(Some(unsafe_url)),
                DEFAULT_CODEX_BASE_URL,
                "unsafe URL accepted: {unsafe_url}"
            );
        }
    }

    #[test]
    fn runtime_default_handles_absent_empty_enabled_and_disabled_values() {
        let absent = resolve_runtime_default(|_| None);
        assert!(absent.route.is_none());
        assert!(absent.disabled_provider.is_none());

        let whitespace = resolve_runtime_default(|name| match name {
            "CODEX_PROXY_API_KEY" | "AGENTOS_DEFAULT_PROVIDER" | "AGENTOS_DEFAULT_MODEL" => {
                Some("  ".into())
            }
            _ => None,
        });
        assert!(whitespace.route.is_none());
        assert!(whitespace.disabled_provider.is_none());

        let enabled = resolve_runtime_default(|name| match name {
            "CODEX_PROXY_API_KEY" => Some("secret".into()),
            _ => None,
        });
        assert_eq!(
            enabled.route,
            Some(Route {
                provider: CODEX_PROVIDER.into(),
                model: DEFAULT_CODEX_MODEL.into(),
            })
        );
        assert!(enabled.disabled_provider.is_none());

        let disabled = resolve_runtime_default(|name| match name {
            "AGENTOS_DEFAULT_PROVIDER" => Some("codex".into()),
            "AGENTOS_DEFAULT_MODEL" => Some(DEFAULT_CODEX_MODEL.into()),
            _ => None,
        });
        assert!(disabled.route.is_none());
        assert_eq!(disabled.disabled_provider.as_deref(), Some(CODEX_PROVIDER));
    }

    #[test]
    fn route_contract_reports_missing_unknown_and_mismatched_fields() {
        let state = test_state(None);
        for (input, message) in [
            (json!({ "provider": "codex" }), "provider requires model"),
            (json!({ "model": "missing-model" }), "unknown model"),
            (
                json!({ "provider": "codex", "model": "gpt-4o" }),
                "is not registered for provider codex",
            ),
            (
                json!({ "provider": "missing", "model": DEFAULT_CODEX_MODEL }),
                "unknown provider",
            ),
            (json!({ "provider": null }), "provider must be a string"),
        ] {
            let error = resolve_route(&state, &input).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }

        let empty = resolve_route(
            &state,
            &json!({ "provider": "", "model": "agentos-default", "messages": [] }),
        )
        .expect("empty preferences fall back to automatic routing");
        assert_eq!(empty.provider, "anthropic");
        assert!(empty.model.contains("haiku"));
    }

    #[test]
    fn configured_codex_default_and_explicit_models_resolve() {
        let state = test_state(Some(Route {
            provider: CODEX_PROVIDER.into(),
            model: DEFAULT_CODEX_MODEL.into(),
        }));
        assert_eq!(
            resolve_route(&state, &json!({})).unwrap().model,
            DEFAULT_CODEX_MODEL
        );

        let explicit = resolve_route(&state, &json!({ "model": "gpt-5.4-mini" })).unwrap();
        assert_eq!(explicit.provider, CODEX_PROVIDER);
        assert_eq!(explicit.model, "gpt-5.4-mini");
    }

    #[test]
    fn route_contract_rejects_nested_model_objects() {
        let error = resolve_route(
            &test_state(None),
            &json!({ "model": { "provider": "codex", "model": DEFAULT_CODEX_MODEL } }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("model must be a string"));
    }

    #[test]
    fn complete_without_explicit_or_runtime_route_preserves_legacy_sonnet() {
        let route = resolve_complete_route(&test_state(None), &json!({})).unwrap();
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn completion_contract_accepts_canonical_tools_and_legacy_functions() {
        let tools = vec![json!({ "name": "memory::recall" })];
        assert_eq!(completion_tools(&json!({ "tools": tools })), tools);
        assert_eq!(completion_tools(&json!({ "functions": tools })), tools);
        assert!(completion_tools(&json!({})).is_empty());
    }

    #[test]
    fn anthropic_body_preserves_system_prompt_and_agent_tools() {
        let messages = vec![json!({ "role": "user", "content": "hello" })];
        let tools = vec![json!({
            "function_id": "memory::recall",
            "description": "Recall matching memories",
            "request_format": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            },
        })];
        let body = anthropic_request_body(
            "claude-sonnet-4-20250514",
            Some("Use memory when relevant"),
            &messages,
            &tools,
            1024,
        );

        assert_eq!(body["system"], "Use memory when relevant");
        assert_eq!(body["messages"], json!(messages));
        assert_eq!(
            body["tools"],
            json!([{
                "name": "memory__recall",
                "description": "Recall matching memories",
                "input_schema": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"],
                },
            }])
        );
    }

    #[test]
    fn openai_body_preserves_system_prompt_and_agent_tools() {
        let messages = vec![json!({ "role": "user", "content": "hello" })];
        let tools = vec![json!({
            "id": "memory::recall",
            "description": "Recall matching memories",
            "inputSchema": { "type": "object", "properties": {} },
        })];
        let body = openai_request_body(
            DEFAULT_CODEX_MODEL,
            Some("Use memory when relevant"),
            &messages,
            &tools,
            1024,
        );

        assert_eq!(
            body["messages"],
            json!([
                { "role": "system", "content": "Use memory when relevant" },
                { "role": "user", "content": "hello" },
            ])
        );
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "memory__recall",
                    "description": "Recall matching memories",
                    "parameters": { "type": "object", "properties": {} },
                },
            }])
        );
    }

    #[test]
    fn gemini_body_uses_function_declarations_and_native_message_shape() {
        let body = gemini_request_body(
            Some("Use memory"),
            &[json!({ "role": "user", "content": "hello" })],
            &[json!({ "functionId": "memory::recall" })],
            512,
        );
        assert_eq!(
            body["contents"],
            json!([{ "role": "user", "parts": [{ "text": "hello" }] }])
        );
        assert_eq!(
            body["tools"],
            json!([{
                "functionDeclarations": [{
                    "name": "memory__recall",
                    "parameters": { "type": "object", "properties": {} },
                }],
            }])
        );
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Use memory");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
    }

    #[test]
    fn provider_tool_calls_normalize_to_the_agent_function_call_contract() {
        let aliases = aliases(&["memory::recall", "state::get", "queue::publish"]);
        let anthropic = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu-1",
                "name": "memory__recall",
                "input": { "query": "rust" },
            }],
        });
        assert_eq!(
            function_calls(Driver::Anthropic, &anthropic, &aliases),
            vec![FunctionCall {
                call_id: "toolu-1".into(),
                id: "memory::recall".into(),
                arguments: json!({ "query": "rust" }),
            }]
        );

        let openai = json!({
            "choices": [{ "message": { "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {
                    "name": "state__get",
                    "arguments": "{\"scope\":\"agents\"}",
                },
            }] } }],
        });
        assert_eq!(
            function_calls(Driver::OpenAiCompat, &openai, &aliases),
            vec![FunctionCall {
                call_id: "call-1".into(),
                id: "state::get".into(),
                arguments: json!({ "scope": "agents" }),
            }]
        );

        let gemini = json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "name": "queue__publish", "args": { "topic": "work" } },
            }] } }],
        });
        assert_eq!(
            function_calls(Driver::Gemini, &gemini, &aliases),
            vec![FunctionCall {
                call_id: "gemini-0-0".into(),
                id: "queue::publish".into(),
                arguments: json!({ "topic": "work" }),
            }]
        );
    }

    #[test]
    fn provider_bodies_omit_empty_or_malformed_tools_and_optional_prompts() {
        let malformed_tools = vec![
            Value::Null,
            json!({}),
            json!({ "id": "" }),
            json!({ "function_id": 7 }),
            json!({ "name": null }),
        ];

        let anthropic = anthropic_request_body("model", None, &[], &malformed_tools, 0);
        assert!(anthropic.get("system").is_none());
        assert!(anthropic.get("tools").is_none());
        assert_eq!(anthropic["messages"], json!([]));
        assert_eq!(anthropic["max_tokens"], 0);

        let openai = openai_request_body("model", Some(""), &[], &malformed_tools, 0);
        assert!(openai.get("tools").is_none());
        assert_eq!(openai["messages"], json!([]));
        assert_eq!(openai["max_tokens"], 0);

        let gemini = gemini_request_body(Some(""), &[], &malformed_tools, 0);
        assert!(gemini.get("systemInstruction").is_none());
        assert!(gemini.get("tools").is_none());
        assert_eq!(gemini["contents"], json!([]));
        assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 0);
    }

    #[test]
    fn tool_schema_aliases_and_defaults_preserve_object_boundaries() {
        let tools = vec![
            json!({ "id": "a", "requestFormat": { "type": "object", "required": [] } }),
            json!({ "function_id": "b", "input_schema": { "type": "object" } }),
            json!({ "functionId": "c", "parameters": { "type": "object", "maxProperties": 0 } }),
            json!({ "name": "d", "request_format": null }),
        ];

        let translated = openai_tools(&tools, &ToolAliases::for_request(&tools, &[]));
        assert_eq!(
            translated[0]["function"]["parameters"]["required"],
            json!([])
        );
        assert_eq!(translated[1]["function"]["parameters"]["type"], "object");
        assert_eq!(translated[2]["function"]["parameters"]["maxProperties"], 0);
        assert_eq!(
            translated[3]["function"]["parameters"],
            json!({ "type": "object", "properties": {} })
        );
    }

    #[test]
    fn provider_tool_aliases_are_valid_bidirectional_and_collision_safe() {
        let long_id = format!("worker::{}", "segment_".repeat(20));
        let aliases = ToolAliases::from_function_ids([
            "memory::recall".to_string(),
            "memory__recall".to_string(),
            long_id.clone(),
        ]);

        assert_eq!(
            aliases.provider_name("memory::recall"),
            Some("memory__recall")
        );
        assert_eq!(
            aliases.provider_name("memory__recall"),
            Some("memory_u_urecall")
        );
        for function_id in ["memory::recall", "memory__recall", long_id.as_str()] {
            let provider_name = aliases.provider_name(function_id).unwrap();
            assert!(provider_name.len() <= 64);
            assert!(
                provider_name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            );
            assert_eq!(aliases.function_id(provider_name), Some(function_id));
        }
        assert_ne!(
            aliases.provider_name("memory::recall"),
            aliases.provider_name("memory__recall")
        );
    }

    #[test]
    fn provider_tool_aliases_handle_empty_exact_boundary_and_unicode_ids() {
        assert_eq!(ToolAliases::from_function_ids([]), ToolAliases::default());
        assert_eq!(
            ToolAliases::from_function_ids([String::new()]),
            ToolAliases::default()
        );

        let exact = "a".repeat(64);
        let over = "b".repeat(65);
        let unicode = "memory::récall".to_string();
        let aliases =
            ToolAliases::from_function_ids([exact.clone(), over.clone(), unicode.clone()]);

        assert_eq!(aliases.provider_name(&exact), Some(exact.as_str()));
        for function_id in [&exact, &over, &unicode] {
            let provider_name = aliases.provider_name(function_id).unwrap();
            assert!(!provider_name.is_empty());
            assert!(provider_name.len() <= 64);
            assert!(provider_name.is_ascii());
            assert_eq!(
                aliases.function_id(provider_name),
                Some(function_id.as_str())
            );
        }
    }

    #[test]
    fn function_call_output_serializes_exactly_the_agent_contract() {
        let call = FunctionCall {
            call_id: "call-1".into(),
            id: "state::get".into(),
            arguments: json!({ "scope": "agents" }),
        };

        assert_eq!(
            serde_json::to_value(call).unwrap(),
            json!({
                "callId": "call-1",
                "id": "state::get",
                "arguments": { "scope": "agents" },
            })
        );
    }

    #[test]
    fn normalized_tool_continuations_translate_to_each_provider_schema() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    { "callId": "call-1", "id": "state::get", "arguments": { "scope": "agents" } },
                    { "callId": "call-2", "id": "queue::publish", "arguments": { "topic": "work" } },
                ],
            }),
            json!({ "role": "tool", "tool_call_id": "call-1", "content": "{\"value\":1}" }),
            json!({ "role": "tool", "tool_call_id": "call-2", "content": "published" }),
        ];

        let anthropic = anthropic_request_body("model", None, &messages, &[], 128);
        assert_eq!(
            anthropic["messages"],
            json!([
                {
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "id": "call-1", "name": "state__get", "input": { "scope": "agents" } },
                        { "type": "tool_use", "id": "call-2", "name": "queue__publish", "input": { "topic": "work" } },
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "call-1", "content": "{\"value\":1}" },
                        { "type": "tool_result", "tool_use_id": "call-2", "content": "published" },
                    ],
                },
            ])
        );

        let openai = openai_request_body("model", None, &messages, &[], 128);
        assert_eq!(
            openai["messages"],
            json!([
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call-1", "type": "function", "function": { "name": "state__get", "arguments": "{\"scope\":\"agents\"}" } },
                        { "id": "call-2", "type": "function", "function": { "name": "queue__publish", "arguments": "{\"topic\":\"work\"}" } },
                    ],
                },
                { "role": "tool", "tool_call_id": "call-1", "content": "{\"value\":1}" },
                { "role": "tool", "tool_call_id": "call-2", "content": "published" },
            ])
        );

        let gemini = gemini_request_body(None, &messages, &[], 128);
        assert_eq!(
            gemini["contents"],
            json!([
                {
                    "role": "model",
                    "parts": [
                        { "functionCall": { "id": "call-1", "name": "state__get", "args": { "scope": "agents" } } },
                        { "functionCall": { "id": "call-2", "name": "queue__publish", "args": { "topic": "work" } } },
                    ],
                },
                {
                    "role": "user",
                    "parts": [
                        { "functionResponse": { "id": "call-1", "name": "state__get", "response": { "value": 1 } } },
                        { "functionResponse": { "id": "call-2", "name": "queue__publish", "response": { "result": "published" } } },
                    ],
                },
            ])
        );
    }

    #[test]
    fn provider_adapters_omit_all_invalid_assistant_tool_calls() {
        let messages = vec![json!({
            "role": "assistant",
            "content": "kept text",
            "tool_calls": [
                null,
                { "callId": "", "id": "state::get", "arguments": {} },
                { "callId": "call-2", "id": "", "arguments": {} },
            ],
        })];

        let anthropic = anthropic_request_body("model", None, &messages, &[], 128);
        assert_eq!(
            anthropic,
            json!({
                "model": "model",
                "messages": [{ "role": "assistant", "content": "kept text" }],
                "max_tokens": 128,
            })
        );
        assert!(anthropic["messages"][0].get("tool_calls").is_none());
        assert!(anthropic["messages"][0].get("toolCalls").is_none());

        let openai = openai_request_body("model", None, &messages, &[], 128);
        assert_eq!(
            openai,
            json!({
                "model": "model",
                "messages": [{ "role": "assistant", "content": "kept text" }],
                "max_tokens": 128,
            })
        );
        assert!(openai["messages"][0].get("tool_calls").is_none());
        assert!(openai["messages"][0].get("toolCalls").is_none());
    }

    #[test]
    fn provider_adapters_strip_empty_null_and_non_array_assistant_tool_call_values() {
        let messages = vec![
            json!({ "role": "assistant", "content": "no key" }),
            json!({ "role": "assistant", "content": "empty", "tool_calls": [] }),
            json!({ "role": "assistant", "content": "null", "toolCalls": null }),
            json!({ "role": "assistant", "content": "object", "tool_calls": {} }),
        ];
        let expected_messages = json!([
            { "role": "assistant", "content": "no key" },
            { "role": "assistant", "content": "empty" },
            { "role": "assistant", "content": "null" },
            { "role": "assistant", "content": "object" },
        ]);

        assert_eq!(
            anthropic_request_body("model", None, &messages, &[], 0),
            json!({
                "model": "model",
                "messages": expected_messages,
                "max_tokens": 0,
            })
        );
        assert_eq!(
            openai_request_body("model", None, &messages, &[], 0),
            json!({
                "model": "model",
                "messages": expected_messages,
                "max_tokens": 0,
            })
        );
    }

    #[test]
    fn provider_adapters_omit_normalized_calls_when_provider_alias_resolution_fails() {
        let messages = vec![json!({
            "role": "assistant",
            "content": "kept text",
            "tool_calls": [
                { "callId": "call-1", "id": "state::get", "arguments": {} },
            ],
        })];
        let aliases = ToolAliases::default();

        assert_eq!(
            anthropic_messages(&messages, &aliases),
            vec![json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "kept text" }],
            })]
        );
        assert_eq!(
            openai_messages(&messages, &aliases),
            vec![json!({ "role": "assistant", "content": "kept text" })]
        );
    }

    #[test]
    fn provider_adapters_emit_only_valid_assistant_tool_calls_from_mixed_arrays() {
        let messages = vec![json!({
            "role": "assistant",
            "content": "kept text",
            "toolCalls": [
                { "callId": "", "id": "state::get", "arguments": {} },
                { "callId": "call-1", "id": "state::get", "arguments": { "scope": "agents" } },
                { "callId": "call-2", "arguments": {} },
            ],
        })];

        assert_eq!(
            anthropic_request_body("model", None, &messages, &[], 128),
            json!({
                "model": "model",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "kept text" },
                        { "type": "tool_use", "id": "call-1", "name": "state__get", "input": { "scope": "agents" } },
                    ],
                }],
                "max_tokens": 128,
            })
        );

        assert_eq!(
            openai_request_body("model", None, &messages, &[], 128),
            json!({
                "model": "model",
                "messages": [{
                    "role": "assistant",
                    "content": "kept text",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "state__get",
                            "arguments": "{\"scope\":\"agents\"}",
                        },
                    }],
                }],
                "max_tokens": 128,
            })
        );
    }

    #[test]
    fn provider_adapters_preserve_all_valid_assistant_tool_calls() {
        let messages = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                { "callId": "call-1", "id": "state::get", "arguments": { "scope": "agents" } },
                { "callId": "call-2", "id": "queue::publish", "arguments": { "topic": "work" } },
            ],
        })];

        assert_eq!(
            anthropic_request_body("model", None, &messages, &[], 128),
            json!({
                "model": "model",
                "messages": [{
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "id": "call-1", "name": "state__get", "input": { "scope": "agents" } },
                        { "type": "tool_use", "id": "call-2", "name": "queue__publish", "input": { "topic": "work" } },
                    ],
                }],
                "max_tokens": 128,
            })
        );

        assert_eq!(
            openai_request_body("model", None, &messages, &[], 128),
            json!({
                "model": "model",
                "messages": [{
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call-1", "type": "function", "function": { "name": "state__get", "arguments": "{\"scope\":\"agents\"}" } },
                        { "id": "call-2", "type": "function", "function": { "name": "queue__publish", "arguments": "{\"topic\":\"work\"}" } },
                    ],
                }],
                "max_tokens": 128,
            })
        );
    }

    #[test]
    fn normalized_continuation_aliases_reject_missing_empty_and_malformed_ids() {
        let aliased = json!({
            "role": "assistant",
            "toolCalls": [
                { "callId": "call-1", "id": "state::get", "arguments": null },
            ],
        });
        assert_eq!(
            message_function_calls(&aliased),
            vec![FunctionCall {
                call_id: "call-1".into(),
                id: "state::get".into(),
                arguments: Value::Null,
            }]
        );

        for malformed in [
            json!({}),
            json!({ "tool_calls": null }),
            json!({ "tool_calls": {} }),
            json!({ "tool_calls": [null, {}, { "callId": "call-1" }] }),
            json!({
                "tool_calls": [
                    { "callId": "", "id": "state::get", "arguments": {} },
                    { "callId": "call-1", "id": "", "arguments": {} },
                ],
            }),
        ] {
            assert!(message_function_calls(&malformed).is_empty());
        }

        assert_eq!(
            message_tool_call_id(&json!({ "toolCallId": "call-1" })),
            Some("call-1")
        );
        for malformed in [
            json!({}),
            json!({ "tool_call_id": null }),
            json!({ "tool_call_id": 7 }),
            json!({ "tool_call_id": "" }),
        ] {
            assert_eq!(message_tool_call_id(&malformed), None);
        }

        assert_eq!(gemini_function_response(None), json!({ "result": null }));
        assert_eq!(
            gemini_function_response(Some(&json!(""))),
            json!({ "result": "" })
        );
    }

    #[test]
    fn normalized_function_call_rejects_empty_call_id() {
        assert_eq!(
            FunctionCall::normalized(String::new(), "state::get".into(), json!({})),
            None
        );
    }

    #[test]
    fn normalized_function_call_rejects_empty_function_id() {
        assert_eq!(
            FunctionCall::normalized("call-1".into(), String::new(), json!({})),
            None
        );
    }

    #[test]
    fn normalized_function_call_value_handles_boundaries_and_invalid_shapes() {
        assert_eq!(
            FunctionCall::from_normalized_value(
                json!({ "callId": "c", "id": "x", "arguments": null }),
            ),
            Some(FunctionCall {
                call_id: "c".into(),
                id: "x".into(),
                arguments: Value::Null,
            })
        );

        for malformed in [
            Value::Null,
            json!({}),
            json!({ "callId": null, "id": "state::get", "arguments": {} }),
            json!({ "callId": "call-1", "id": null, "arguments": {} }),
            json!({ "callId": 1, "id": "state::get", "arguments": {} }),
        ] {
            assert_eq!(FunctionCall::from_normalized_value(malformed), None);
        }
    }

    #[test]
    fn function_call_normalization_handles_empty_missing_and_malformed_fields() {
        let aliases = aliases(&["state::get", "state::set", "queue::publish"]);
        for driver in [
            Driver::Anthropic,
            Driver::OpenAiCompat,
            Driver::Gemini,
            Driver::Bedrock,
        ] {
            assert!(function_calls(driver, &json!({}), &aliases).is_empty());
        }

        let anthropic = json!({
            "content": [
                { "type": "text", "text": "not a call" },
                { "type": "tool_use", "name": "missing-id", "input": {} },
                { "type": "tool_use", "id": "missing-name", "input": {} },
                { "type": "tool_use", "id": "valid", "name": "state__get" },
            ],
        });
        assert_eq!(
            function_calls(Driver::Anthropic, &anthropic, &aliases),
            vec![FunctionCall {
                call_id: "valid".into(),
                id: "state::get".into(),
                arguments: json!({}),
            }]
        );

        let openai = json!({
            "choices": [{ "message": { "tool_calls": [
                { "id": "missing-name", "function": { "arguments": "{}" } },
                { "function": { "name": "missing-id", "arguments": "{}" } },
                {
                    "id": "raw-arguments",
                    "function": { "name": "state__set", "arguments": "not-json" },
                },
            ] } }],
        });
        let expected = vec![FunctionCall {
            call_id: "raw-arguments".into(),
            id: "state::set".into(),
            arguments: json!("not-json"),
        }];
        assert_eq!(
            function_calls(Driver::OpenAiCompat, &openai, &aliases),
            expected
        );
        assert_eq!(function_calls(Driver::Bedrock, &openai, &aliases), expected);

        let gemini = json!({
            "candidates": [
                { "content": { "parts": [{ "text": "not a call" }] } },
                { "content": { "parts": [
                    { "functionCall": { "args": {} } },
                    { "functionCall": { "id": "provider-id", "name": "queue__publish" } },
                ] } },
            ],
        });
        assert_eq!(
            function_calls(Driver::Gemini, &gemini, &aliases),
            vec![FunctionCall {
                call_id: "provider-id".into(),
                id: "queue::publish".into(),
                arguments: json!({}),
            }]
        );
    }

    #[test]
    fn anthropic_function_call_normalization_rejects_empty_ids_and_unknown_aliases() {
        let aliases = aliases(&["state::get"]);
        let anthropic = json!({
            "content": [
                { "type": "tool_use", "id": "", "name": "state__get", "input": {} },
                { "type": "tool_use", "id": "call-1", "name": "", "input": {} },
                { "type": "tool_use", "id": "call-2", "name": "unknown", "input": {} },
            ],
        });
        assert!(function_calls(Driver::Anthropic, &anthropic, &aliases).is_empty());
    }

    #[test]
    fn openai_function_call_normalization_rejects_empty_ids_and_unknown_aliases() {
        let aliases = aliases(&["state::get"]);
        let openai = json!({
            "choices": [{ "message": { "tool_calls": [
                { "id": "", "function": { "name": "state__get", "arguments": null } },
                { "id": "call-1", "function": { "name": "", "arguments": "{}" } },
                { "id": "call-2", "function": { "name": "unknown", "arguments": "{}" } },
            ] } }],
        });
        assert!(function_calls(Driver::OpenAiCompat, &openai, &aliases).is_empty());
    }

    #[test]
    fn gemini_function_call_normalization_replaces_empty_ids_and_rejects_unknown_aliases() {
        let aliases = aliases(&["state::get"]);
        let gemini = json!({
            "candidates": [{ "content": { "parts": [
                { "functionCall": { "id": "", "name": "state__get", "args": null } },
                { "functionCall": { "id": "call-1", "name": "" } },
                { "functionCall": { "id": "call-2", "name": "unknown" } },
            ] } }],
        });
        assert_eq!(
            function_calls(Driver::Gemini, &gemini, &aliases),
            vec![FunctionCall {
                call_id: "gemini-0-0".into(),
                id: "state::get".into(),
                arguments: Value::Null,
            }]
        );
    }

    #[test]
    fn test_score_complexity_empty_messages() {
        let messages: Vec<Value> = vec![];
        let tools: Vec<Value> = vec![];
        assert_eq!(score_complexity(&messages, &tools), 0);
    }

    #[test]
    fn test_score_complexity_short_message() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let tools: Vec<Value> = vec![];
        assert_eq!(score_complexity(&messages, &tools), 0);
    }

    #[test]
    fn test_score_complexity_long_message() {
        let content = "a".repeat(500);
        let messages = vec![json!({"role": "user", "content": content})];
        let tools: Vec<Value> = vec![];
        assert_eq!(score_complexity(&messages, &tools), 5);
    }

    #[test]
    fn test_score_complexity_code_content() {
        let messages = vec![
            json!({"role": "user", "content": "Please write a function to sort items ```code```"}),
        ];
        let tools: Vec<Value> = vec![];
        let score = score_complexity(&messages, &tools);
        assert!(score >= 20);
    }

    #[test]
    fn test_score_complexity_analysis_keywords() {
        let messages =
            vec![json!({"role": "user", "content": "Please analyze and compare these designs"})];
        let tools: Vec<Value> = vec![];
        let score = score_complexity(&messages, &tools);
        assert!(score >= 15);
    }

    #[test]
    fn test_score_complexity_with_tools() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let tools = vec![json!({"name": "tool1"}), json!({"name": "tool2"})];
        assert_eq!(score_complexity(&messages, &tools), 10);
    }

    #[test]
    fn test_score_complexity_many_messages() {
        let messages: Vec<Value> = (0..11)
            .map(|i| json!({"role": "user", "content": format!("msg {}", i)}))
            .collect();
        let tools: Vec<Value> = vec![];
        let score = score_complexity(&messages, &tools);
        assert!(score >= 10);
    }

    #[test]
    fn test_score_complexity_uses_last_message() {
        let messages = vec![
            json!({"role": "user", "content": "simple"}),
            json!({"role": "user", "content": "Please analyze this complex function and compare it"}),
        ];
        let score = score_complexity(&messages, &[]);
        assert!(score >= 15);
    }

    #[test]
    fn test_score_complexity_class_keyword() {
        let messages =
            vec![json!({"role": "user", "content": "Define a class for the data model"})];
        let score = score_complexity(&messages, &[]);
        assert!(score >= 20);
    }

    #[test]
    fn test_select_model_low_complexity() {
        let (provider, model) = select_model(5, None);
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_select_model_medium_complexity() {
        let (provider, model) = select_model(25, None);
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_select_model_high_complexity() {
        let (provider, model) = select_model(50, None);
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-20250514");
    }

    #[test]
    fn test_select_model_boundary_10() {
        let (_, model) = select_model(10, None);
        assert_eq!(model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_select_model_boundary_11() {
        let (_, model) = select_model(11, None);
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_select_model_boundary_40() {
        let (_, model) = select_model(40, None);
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_select_model_boundary_41() {
        let (_, model) = select_model(41, None);
        assert_eq!(model, "claude-opus-4-20250514");
    }

    #[test]
    fn test_select_model_preferred_opus() {
        let (provider, model) = select_model(0, Some("opus"));
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-20250514");
    }

    #[test]
    fn test_select_model_preferred_sonnet() {
        let (_, model) = select_model(0, Some("sonnet"));
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_select_model_preferred_haiku() {
        let (_, model) = select_model(100, Some("haiku"));
        assert_eq!(model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_select_model_preferred_claude_opus() {
        let (_, model) = select_model(0, Some("claude-opus"));
        assert_eq!(model, "claude-opus-4-20250514");
    }

    #[test]
    fn test_select_model_preferred_claude_sonnet() {
        let (_, model) = select_model(0, Some("claude-sonnet"));
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_select_model_preferred_claude_haiku() {
        let (_, model) = select_model(0, Some("claude-haiku"));
        assert_eq!(model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_select_model_preferred_gpt4o() {
        let (provider, model) = select_model(0, Some("gpt-4o"));
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_select_model_preferred_gemini() {
        let (provider, model) = select_model(0, Some("gemini"));
        assert_eq!(provider, "google");
        assert_eq!(model, "gemini-2.0-flash");
    }

    #[test]
    fn test_select_model_unknown_preferred_falls_through() {
        let (provider, _) = select_model(5, Some("unknown-model"));
        assert_eq!(provider, "anthropic");
    }

    #[test]
    fn test_default_providers_count() {
        let providers = default_providers();
        assert_eq!(providers.len(), 11);
    }

    #[test]
    fn test_default_providers_anthropic_exists() {
        let providers = default_providers();
        let anthropic = providers.iter().find(|p| p.0 == "anthropic");
        assert!(anthropic.is_some());
        let (_, base_url, env_key, _, models) = anthropic.unwrap();
        assert_eq!(*base_url, "https://api.anthropic.com");
        assert_eq!(*env_key, "ANTHROPIC_API_KEY");
        assert!(models.len() >= 3);
    }

    #[test]
    fn test_default_providers_openai_exists() {
        let providers = default_providers();
        assert!(providers.iter().any(|p| p.0 == "openai"));
    }

    #[test]
    fn test_default_providers_google_exists() {
        let providers = default_providers();
        assert!(providers.iter().any(|p| p.0 == "google"));
    }

    #[test]
    fn test_default_providers_ollama_no_env_key() {
        let providers = default_providers();
        let ollama = providers.iter().find(|p| p.0 == "ollama").unwrap();
        assert_eq!(ollama.2, "");
    }

    #[test]
    fn test_default_providers_all_have_models() {
        for (name, _, _, _, models) in default_providers() {
            assert!(!models.is_empty(), "Provider {} has no models", name);
        }
    }

    #[test]
    fn test_driver_clone() {
        let d = Driver::Anthropic;
        let cloned = d;
        assert!(matches!(cloned, Driver::Anthropic));
    }

    #[test]
    fn test_score_complexity_design_keyword() {
        let messages = vec![json!({"role": "user", "content": "Help me design a new API"})];
        let score = score_complexity(&messages, &[]);
        assert!(score >= 15);
    }

    #[test]
    fn test_score_complexity_all_bonuses() {
        let content = format!(
            "{} analyze compare design function class ```code```",
            "a".repeat(2000)
        );
        let messages: Vec<Value> = (0..12)
            .map(|_| json!({"role": "user", "content": &content}))
            .collect();
        let tools: Vec<Value> = (0..5)
            .map(|i| json!({"name": format!("tool{}", i)}))
            .collect();
        let score = score_complexity(&messages, &tools);
        assert!(score > 50);
    }

    #[test]
    fn test_usage_key_format() {
        let provider_name = "anthropic";
        let model = "claude-sonnet-4-20250514";
        let key = format!("{}:{}", provider_name, model);
        assert_eq!(key, "anthropic:claude-sonnet-4-20250514");

        let parts: Vec<&str> = key.splitn(2, ':').collect();
        assert_eq!(parts[0], "anthropic");
        assert_eq!(parts[1], "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_score_complexity_no_content_field() {
        let messages = vec![json!({"role": "user"})];
        let score = score_complexity(&messages, &[]);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_score_complexity_null_content() {
        let messages = vec![json!({"role": "user", "content": null})];
        let score = score_complexity(&messages, &[]);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_score_complexity_whitespace_only() {
        let messages = vec![json!({"role": "user", "content": "   \t\n  "})];
        let score = score_complexity(&messages, &[]);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_score_complexity_exactly_100_chars() {
        let content = "x".repeat(100);
        let messages = vec![json!({"role": "user", "content": content})];
        let score = score_complexity(&messages, &[]);
        assert_eq!(score, 1);
    }

    #[test]
    fn test_score_complexity_99_chars() {
        let content = "x".repeat(99);
        let messages = vec![json!({"role": "user", "content": content})];
        let score = score_complexity(&messages, &[]);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_score_complexity_exactly_10_messages() {
        let messages: Vec<Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("m{}", i)}))
            .collect();
        let score = score_complexity(&messages, &[]);
        assert_eq!(score, 0);
    }

    #[test]
    fn test_select_model_boundary_0() {
        let (_, model) = select_model(0, None);
        assert_eq!(model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_select_model_very_high_complexity() {
        let (provider, model) = select_model(1000, None);
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-20250514");
    }

    #[test]
    fn test_select_model_empty_preferred_falls_through() {
        let (_, model) = select_model(5, Some(""));
        assert_eq!(model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_select_model_all_named_preferences() {
        let prefs = vec![
            ("opus", "claude-opus-4-20250514"),
            ("claude-opus", "claude-opus-4-20250514"),
            ("sonnet", "claude-sonnet-4-20250514"),
            ("claude-sonnet", "claude-sonnet-4-20250514"),
            ("haiku", "claude-haiku-4-5-20251001"),
            ("claude-haiku", "claude-haiku-4-5-20251001"),
            ("gpt-4o", "gpt-4o"),
            ("gemini", "gemini-2.0-flash"),
        ];
        for (pref, expected_model) in prefs {
            let (_, model) = select_model(50, Some(pref));
            assert_eq!(
                model, expected_model,
                "Preference '{}' should select model '{}'",
                pref, expected_model
            );
        }
    }

    #[test]
    fn test_default_providers_all_have_non_empty_base_url() {
        for (name, base_url, _, _, _) in default_providers() {
            assert!(!base_url.is_empty(), "Provider {} has empty base_url", name);
        }
    }

    #[test]
    fn test_default_providers_env_key_format() {
        for (name, _, env_key, _, _) in default_providers() {
            if !env_key.is_empty() {
                assert!(
                    env_key.ends_with("_KEY") || env_key.ends_with("_API_KEY"),
                    "Provider {} env_key '{}' doesn't follow convention",
                    name,
                    env_key
                );
            }
        }
    }

    #[test]
    fn test_default_providers_model_count_per_provider() {
        let providers = default_providers();
        for (name, _, _, _, models) in &providers {
            assert!(
                !models.is_empty(),
                "Provider {} should have at least 1 model",
                name
            );
        }
        let anthropic = providers.iter().find(|p| p.0 == "anthropic").unwrap();
        assert_eq!(anthropic.4.len(), 5);
        let openai = providers.iter().find(|p| p.0 == "openai").unwrap();
        assert_eq!(openai.4.len(), 4);
    }

    #[test]
    fn test_usage_key_with_colon_in_model_name() {
        let key = format!("{}:{}", "openrouter", "anthropic/claude-opus-4-20250514");
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        assert_eq!(parts[0], "openrouter");
        assert_eq!(parts[1], "anthropic/claude-opus-4-20250514");
    }

    #[test]
    fn test_usage_key_splitn_preserves_colons_in_value() {
        let key = "provider:model:with:colons";
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "provider");
        assert_eq!(parts[1], "model:with:colons");
    }

    #[test]
    fn test_default_providers_unique_names() {
        let providers = default_providers();
        let names: Vec<&str> = providers.iter().map(|p| p.0).collect();
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(names.len(), deduped.len());
    }

    #[test]
    fn test_default_providers_groq_exists() {
        assert!(default_providers().iter().any(|p| p.0 == "groq"));
    }

    #[test]
    fn test_default_providers_deepseek_exists() {
        assert!(default_providers().iter().any(|p| p.0 == "deepseek"));
    }

    #[test]
    fn test_default_providers_together_exists() {
        assert!(default_providers().iter().any(|p| p.0 == "together"));
    }

    #[test]
    fn test_default_providers_fireworks_exists() {
        assert!(default_providers().iter().any(|p| p.0 == "fireworks"));
    }

    #[test]
    fn test_score_complexity_combined_code_and_analysis() {
        let messages = vec![json!({"role": "user", "content": "analyze this function ```code```"})];
        let score = score_complexity(&messages, &[]);
        assert!(
            score >= 35,
            "Expected >= 35 for code+analysis, got {}",
            score
        );
    }

    #[test]
    fn test_score_complexity_compare_keyword_alone() {
        let messages = vec![json!({"role": "user", "content": "compare option A with option B"})];
        let score = score_complexity(&messages, &[]);
        assert!(score >= 15);
    }

    #[test]
    fn test_score_complexity_function_keyword_alone() {
        let messages =
            vec![json!({"role": "user", "content": "write a function that adds two numbers"})];
        let score = score_complexity(&messages, &[]);
        assert!(score >= 20);
    }

    #[test]
    fn test_score_complexity_exactly_11_messages() {
        let messages: Vec<Value> = (0..11)
            .map(|i| json!({"role": "user", "content": format!("m{}", i)}))
            .collect();
        let score = score_complexity(&messages, &[]);
        assert!(score >= 10);
    }

    #[test]
    fn test_score_complexity_one_tool_adds_5() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let tools = vec![json!({"name": "t"})];
        let s1 = score_complexity(&messages, &[]);
        let s2 = score_complexity(&messages, &tools);
        assert_eq!(s2 - s1, 5);
    }

    #[test]
    fn test_score_complexity_ten_tools() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let tools: Vec<Value> = (0..10)
            .map(|i| json!({"name": format!("t{}", i)}))
            .collect();
        let score = score_complexity(&messages, &tools);
        assert!(score >= 50);
    }

    #[test]
    fn test_score_complexity_200_chars_gives_2() {
        let content = "y".repeat(200);
        let messages = vec![json!({"role": "user", "content": content})];
        assert_eq!(score_complexity(&messages, &[]), 2);
    }

    #[test]
    fn test_select_model_boundary_exact_ranges() {
        for c in 0..=10 {
            let (_, model) = select_model(c, None);
            assert_eq!(
                model, "claude-haiku-4-5-20251001",
                "complexity {} should be haiku",
                c
            );
        }
        for c in 11..=40 {
            let (_, model) = select_model(c, None);
            assert_eq!(
                model, "claude-sonnet-4-20250514",
                "complexity {} should be sonnet",
                c
            );
        }
        for c in [41, 50, 100, 255, u32::MAX] {
            let (_, model) = select_model(c, None);
            assert_eq!(
                model, "claude-opus-4-20250514",
                "complexity {} should be opus",
                c
            );
        }
    }

    #[test]
    fn test_select_model_preferred_overrides_complexity() {
        let (_, model) = select_model(100, Some("haiku"));
        assert_eq!(model, "claude-haiku-4-5-20251001");

        let (_, model) = select_model(0, Some("opus"));
        assert_eq!(model, "claude-opus-4-20250514");
    }

    #[test]
    fn test_default_providers_openai_details() {
        let providers = default_providers();
        let openai = providers.iter().find(|p| p.0 == "openai").unwrap();
        assert_eq!(openai.1, "https://api.openai.com/v1");
        assert_eq!(openai.2, "OPENAI_API_KEY");
        assert!(openai.4.contains(&"gpt-4o"));
    }

    #[test]
    fn test_default_providers_google_details() {
        let providers = default_providers();
        let google = providers.iter().find(|p| p.0 == "google").unwrap();
        assert_eq!(google.2, "GOOGLE_API_KEY");
        assert!(google.4.contains(&"gemini-2.0-flash"));
    }

    #[test]
    fn test_default_providers_groq_details() {
        let providers = default_providers();
        let groq = providers.iter().find(|p| p.0 == "groq").unwrap();
        assert_eq!(groq.2, "GROQ_API_KEY");
        assert!(groq.4.contains(&"llama-3.3-70b-versatile"));
    }

    #[test]
    fn test_default_providers_deepseek_details() {
        let providers = default_providers();
        let ds = providers.iter().find(|p| p.0 == "deepseek").unwrap();
        assert_eq!(ds.2, "DEEPSEEK_API_KEY");
        assert!(ds.4.contains(&"deepseek-chat"));
        assert!(ds.4.contains(&"deepseek-reasoner"));
    }

    #[test]
    fn test_default_providers_mistral_details() {
        let providers = default_providers();
        let mistral = providers.iter().find(|p| p.0 == "mistral").unwrap();
        assert_eq!(mistral.2, "MISTRAL_API_KEY");
    }

    #[test]
    fn test_default_providers_openrouter_details() {
        let providers = default_providers();
        let or = providers.iter().find(|p| p.0 == "openrouter").unwrap();
        assert_eq!(or.2, "OPENROUTER_API_KEY");
        assert!(or.4.iter().any(|m| m.contains("anthropic/")));
    }

    #[test]
    fn test_default_providers_ollama_local() {
        let providers = default_providers();
        let ollama = providers.iter().find(|p| p.0 == "ollama").unwrap();
        assert!(ollama.1.contains("localhost"));
        assert!(ollama.2.is_empty());
    }

    #[test]
    fn test_usage_key_empty_provider() {
        let key = format!("{}:{}", "", "model-name");
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        assert_eq!(parts[0], "");
        assert_eq!(parts[1], "model-name");
    }

    #[test]
    fn test_usage_key_no_colon() {
        let key = "just-a-key";
        let parts: Vec<&str> = key.splitn(2, ':').collect();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], "just-a-key");
    }

    fn extract_anthropic_text(result: &Value) -> &str {
        result["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"].as_str() == Some("text")))
            .and_then(|b| b["text"].as_str())
            .or_else(|| {
                result["choices"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|c| c["message"]["content"].as_str())
            })
            .unwrap_or("")
    }

    #[test]
    fn test_content_extraction_tool_use_first_then_text() {
        let result = json!({
            "content": [
                {"type": "tool_use", "id": "t1", "name": "search", "input": {"q": "rust"}},
                {"type": "text", "text": "Here is what I found."},
            ]
        });
        assert_eq!(extract_anthropic_text(&result), "Here is what I found.");
    }

    #[test]
    fn test_content_extraction_text_only() {
        let result = json!({"content": [{"type": "text", "text": "just text"}]});
        assert_eq!(extract_anthropic_text(&result), "just text");
    }

    #[test]
    fn test_content_extraction_only_tool_use_returns_empty() {
        let result =
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "x", "input": {}}]});
        assert_eq!(extract_anthropic_text(&result), "");
    }

    #[test]
    fn test_content_extraction_falls_through_to_openai_choices() {
        let result = json!({
            "choices": [{"message": {"content": "openai-style content"}}]
        });
        assert_eq!(extract_anthropic_text(&result), "openai-style content");
    }

    // ----- provider transport contract -------------------------------------
    //
    // A fake provider server proves the three claims the review made about the
    // transport: a response slower than the old 30 s bus ceiling completes, a
    // 4xx body reaches the caller, and a hung provider ends in a timeout
    // instead of running (and billing) forever.

    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct FakeProvider {
        addr: SocketAddr,
        handle: tokio::task::JoinHandle<()>,
    }

    impl FakeProvider {
        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for FakeProvider {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn spawn_fake_provider(
        status: &'static str,
        body: &'static str,
        delay: Duration,
    ) -> FakeProvider {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake provider");
        let addr = listener.local_addr().expect("fake provider address");
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut scratch = vec![0_u8; 16 * 1024];
                    let _ = stream.read(&mut scratch).await;
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        FakeProvider { addr, handle }
    }

    fn provider_request<'a>(
        provider: &'a str,
        base_url: &'a str,
        messages: &'a [Value],
        timeout: Duration,
    ) -> ProviderRequest<'a> {
        ProviderRequest {
            provider,
            base_url,
            api_key: "test-key",
            model: "test-model",
            system_prompt: None,
            messages,
            tools: &[],
            max_tokens: 64,
            timeout,
        }
    }

    const SLOW_PROVIDER_DELAY: Duration = Duration::from_secs(45);

    #[tokio::test]
    async fn a_provider_answer_slower_than_the_legacy_30s_ceiling_still_completes() {
        let server = spawn_fake_provider(
            "200 OK",
            r#"{"content":[{"type":"text","text":"late but complete"}],"usage":{"input_tokens":3,"output_tokens":4}}"#,
            SLOW_PROVIDER_DELAY,
        )
        .await;
        let client = provider_client(reqwest::Client::builder()).expect("client");
        let base_url = server.base_url();
        let messages = vec![json!({ "role": "user", "content": "hello" })];
        let request = provider_request(
            "anthropic",
            &base_url,
            &messages,
            provider_timeout(Some(120_000)),
        );

        let result = call_anthropic(&client, &request)
            .await
            .expect("a 45s provider answer must reach the caller");
        assert_eq!(result["content"][0]["text"], "late but complete");
    }

    #[tokio::test]
    async fn a_hung_provider_ends_in_a_timeout_instead_of_running_forever() {
        let server = spawn_fake_provider("200 OK", "{}", Duration::from_secs(3_600)).await;
        let client = provider_client(reqwest::Client::builder()).expect("client");
        let base_url = server.base_url();
        let messages = vec![json!({ "role": "user", "content": "hello" })];
        let request =
            provider_request("anthropic", &base_url, &messages, provider_timeout(Some(1)));

        let error = call_anthropic(&client, &request)
            .await
            .expect_err("a hung provider must not hang the worker")
            .to_string();
        assert!(error.contains("provider_timeout"), "{error}");
        assert!(error.contains("anthropic"), "{error}");
    }

    #[tokio::test]
    async fn provider_4xx_bodies_reach_the_caller_for_every_driver() {
        let messages = vec![json!({ "role": "user", "content": "hello" })];
        let client = provider_client(reqwest::Client::builder()).expect("client");

        let anthropic = spawn_fake_provider(
            "429 Too Many Requests",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"anthropic says: workspace rate limit exceeded"}}"#,
            Duration::ZERO,
        )
        .await;
        let base_url = anthropic.base_url();
        let error = call_anthropic(
            &client,
            &provider_request("anthropic", &base_url, &messages, provider_timeout(None)),
        )
        .await
        .expect_err("429 must surface")
        .to_string();
        assert!(error.contains("429"), "{error}");
        assert!(
            error.contains("workspace rate limit exceeded"),
            "provider body was dropped: {error}"
        );

        let openai = spawn_fake_provider(
            "400 Bad Request",
            r#"{"error":{"message":"openai says: this model does not support tools","code":"unsupported"}}"#,
            Duration::ZERO,
        )
        .await;
        let base_url = openai.base_url();
        let error = call_openai_compat(
            &client,
            &provider_request("openai", &base_url, &messages, provider_timeout(None)),
        )
        .await
        .expect_err("400 must surface")
        .to_string();
        assert!(error.contains("400"), "{error}");
        assert!(
            error.contains("this model does not support tools"),
            "provider body was dropped: {error}"
        );

        let google = spawn_fake_provider(
            "403 Forbidden",
            r#"{"error":{"status":"PERMISSION_DENIED","message":"google says: API key not valid"}}"#,
            Duration::ZERO,
        )
        .await;
        let base_url = google.base_url();
        let error = call_gemini(
            &client,
            &provider_request("google", &base_url, &messages, provider_timeout(None)),
        )
        .await
        .expect_err("403 must surface")
        .to_string();
        assert!(error.contains("403"), "{error}");
        assert!(
            error.contains("API key not valid"),
            "provider body was dropped: {error}"
        );
    }

    #[tokio::test]
    async fn a_non_json_provider_body_is_reported_with_its_content() {
        let server =
            spawn_fake_provider("200 OK", "<html>gateway timeout</html>", Duration::ZERO).await;
        let client = provider_client(reqwest::Client::builder()).expect("client");
        let base_url = server.base_url();
        let messages = vec![json!({ "role": "user", "content": "hello" })];

        let error = call_openai_compat(
            &client,
            &provider_request("openai", &base_url, &messages, provider_timeout(None)),
        )
        .await
        .expect_err("a non-JSON body must not be reported as a decode error alone")
        .to_string();
        assert!(error.contains("provider_invalid_response"), "{error}");
        assert!(error.contains("gateway timeout"), "{error}");
    }

    #[tokio::test]
    async fn every_driver_posts_to_the_configured_base_url() {
        // Each driver answers from a loopback fake server; reaching it at all
        // proves the request did not go to the compiled-in vendor host.
        let messages = vec![json!({ "role": "user", "content": "hello" })];
        let client = provider_client(reqwest::Client::builder()).expect("client");

        let anthropic = spawn_fake_provider(
            "200 OK",
            r#"{"content":[{"type":"text","text":"gateway"}]}"#,
            Duration::ZERO,
        )
        .await;
        let base_url = anthropic.base_url();
        let result = call_anthropic(
            &client,
            &provider_request("anthropic", &base_url, &messages, provider_timeout(None)),
        )
        .await
        .expect("anthropic base_url must be honoured");
        assert_eq!(result["content"][0]["text"], "gateway");

        let gemini = spawn_fake_provider(
            "200 OK",
            r#"{"candidates":[{"content":{"parts":[{"text":"gateway"}]}}]}"#,
            Duration::ZERO,
        )
        .await;
        let base_url = gemini.base_url();
        let result = call_gemini(
            &client,
            &provider_request("google", &base_url, &messages, provider_timeout(None)),
        )
        .await
        .expect("gemini base_url must be honoured");
        assert_eq!(
            result["candidates"][0]["content"]["parts"][0]["text"],
            "gateway"
        );
    }

    #[test]
    fn provider_timeout_is_bounded_and_honours_a_smaller_caller_budget() {
        assert_eq!(
            provider_timeout(None),
            Duration::from_millis(PROVIDER_TIMEOUT_DEFAULT_MS)
        );
        assert!(provider_timeout(None) > Duration::from_secs(45));
        assert_eq!(provider_timeout(Some(45_000)), Duration::from_secs(45));
        assert_eq!(
            provider_timeout(Some(0)),
            Duration::from_millis(PROVIDER_TIMEOUT_MIN_MS)
        );
        assert_eq!(
            provider_timeout(Some(u64::MAX)),
            Duration::from_millis(PROVIDER_TIMEOUT_MAX_MS)
        );
        assert!(Duration::from_millis(PROVIDER_TIMEOUT_MAX_MS) <= Duration::from_secs(240));

        assert_eq!(
            caller_timeout_ms(&json!({ "timeoutMs": 5_000 })),
            Some(5_000)
        );
        assert_eq!(
            caller_timeout_ms(&json!({ "timeout_ms": 5_000 })),
            Some(5_000)
        );
        assert_eq!(caller_timeout_ms(&json!({ "timeoutMs": "soon" })), None);
        assert_eq!(caller_timeout_ms(&json!({})), None);
    }

    #[test]
    fn provider_urls_are_built_from_the_configured_base_url() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://gateway.internal/anthropic/"),
            "https://gateway.internal/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://gateway.internal/v1"),
            "https://gateway.internal/v1/messages"
        );
        assert_eq!(
            openai_completions_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            gemini_generate_url("https://gen.googleapis.com/v1beta/", "gemini-2.0-flash"),
            "https://gen.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent"
        );
    }

    #[test]
    fn provider_base_url_overrides_are_namespaced_and_safe() {
        assert_eq!(
            provider_base_url_env("anthropic"),
            "AGENTOS_ANTHROPIC_BASE_URL"
        );
        assert_eq!(
            provider_base_url_env("open-router"),
            "AGENTOS_OPEN_ROUTER_BASE_URL"
        );

        let default_url = "https://api.anthropic.com";
        assert_eq!(
            resolve_provider_base_url(default_url, Some("https://gateway.example.com/anthropic")),
            "https://gateway.example.com/anthropic"
        );
        assert_eq!(
            resolve_provider_base_url(default_url, Some("http://127.0.0.1:8080/v1")),
            "http://127.0.0.1:8080/v1"
        );
        for rejected in [
            "",
            "   ",
            "not-a-url",
            "http://gateway.example.com",
            "ftp://gateway.example.com",
            "https://user:pass@gateway.example.com",
            "https://gateway.example.com?key=leak",
            "https://gateway.example.com#leak",
        ] {
            assert_eq!(
                resolve_provider_base_url(default_url, Some(rejected)),
                default_url,
                "accepted {rejected}"
            );
        }
        assert_eq!(resolve_provider_base_url(default_url, None), default_url);
    }

    #[test]
    fn provider_errors_keep_the_body_and_stay_bounded() {
        let error = provider_status_error("openai", 402, "  insufficient credit  ").to_string();
        assert!(error.contains("provider_error"), "{error}");
        assert!(error.contains("openai"), "{error}");
        assert!(error.contains("402"), "{error}");
        assert!(error.contains("insufficient credit"), "{error}");

        let empty = provider_status_error("openai", 500, "   ").to_string();
        assert!(empty.contains("<empty response body>"), "{empty}");

        let long = "x".repeat(PROVIDER_ERROR_BODY_LIMIT * 2);
        let truncated = truncate_provider_body(&long);
        assert!(truncated.ends_with("[truncated]"), "{truncated}");
        assert!(truncated.chars().count() < long.len());
    }

    // ----- credential-aware routing ----------------------------------------

    #[test]
    fn runtime_default_follows_the_pinned_provider_credential() {
        let pinned_openai = resolve_runtime_default(|name| match name {
            "AGENTOS_DEFAULT_PROVIDER" => Some("openai".into()),
            "OPENAI_API_KEY" => Some("secret".into()),
            _ => None,
        });
        assert_eq!(
            pinned_openai.route,
            Some(Route {
                provider: "openai".into(),
                model: "gpt-4o".into(),
            }),
            "a pinned provider with its own credential must survive an empty CODEX_PROXY_API_KEY"
        );
        assert!(pinned_openai.disabled_provider.is_none());

        let pinned_without_credential = resolve_runtime_default(|name| match name {
            "AGENTOS_DEFAULT_PROVIDER" => Some("openai".into()),
            _ => None,
        });
        assert!(pinned_without_credential.route.is_none());
        assert_eq!(
            pinned_without_credential.disabled_provider.as_deref(),
            Some("openai")
        );

        let unknown_provider = resolve_runtime_default(|name| match name {
            "AGENTOS_DEFAULT_PROVIDER" => Some("does-not-exist".into()),
            _ => None,
        });
        assert!(unknown_provider.route.is_none());
        assert_eq!(
            unknown_provider.disabled_provider.as_deref(),
            Some("does-not-exist")
        );

        let keyless = resolve_runtime_default(|name| match name {
            "AGENTOS_DEFAULT_PROVIDER" => Some("ollama".into()),
            _ => None,
        });
        assert_eq!(
            keyless.route,
            Some(Route {
                provider: "ollama".into(),
                model: "llama3.3".into(),
            })
        );
    }

    #[test]
    fn automatic_routing_uses_a_provider_whose_credential_exists() {
        let openai_only = state_with_credentials(None, Some(&["openai"]));
        let route = resolve_route(&openai_only, &json!({ "messages": [] }))
            .expect("a configured provider must be routable");
        assert_eq!(route.provider, "openai");
        assert_eq!(route.model, "gpt-4o");

        let google_only = state_with_credentials(None, Some(&["google"]));
        let route = resolve_route(&google_only, &json!({ "messages": [] })).expect("google route");
        assert_eq!(route.provider, "google");

        // Anthropic keeps the complexity ladder when its credential is there.
        let anthropic_only = state_with_credentials(None, Some(&["anthropic"]));
        let route = resolve_route(&anthropic_only, &json!({ "messages": [] })).expect("route");
        assert_eq!(route.provider, "anthropic");
        assert!(route.model.contains("haiku"), "{}", route.model);
    }

    #[test]
    fn automatic_routing_ignores_providers_that_need_no_credential() {
        // `ollama` is always "available" but needs a server on this machine, so
        // it must never be chosen automatically.
        let nothing = state_with_credentials(None, Some(&[]));
        let error = resolve_route(&nothing, &json!({ "messages": [] }))
            .expect_err("no credential must not silently route to Anthropic")
            .to_string();
        assert!(error.contains("provider_credential_missing"), "{error}");
        for variable in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "CODEX_PROXY_API_KEY",
        ] {
            assert!(error.contains(variable), "{variable} missing from: {error}");
        }
        assert!(!error.contains("OLLAMA"), "{error}");

        let ollama_is_still_explicitly_routable = resolve_route(
            &nothing,
            &json!({ "provider": "ollama", "model": "llama3.3" }),
        )
        .expect("an explicit keyless provider stays usable");
        assert_eq!(ollama_is_still_explicitly_routable.provider, "ollama");
    }

    #[test]
    fn a_call_to_an_unconfigured_provider_is_refused_by_variable_name() {
        let openai_only = state_with_credentials(None, Some(&["openai"]));

        let explicit = resolve_complete_route(
            &openai_only,
            &json!({ "provider": "anthropic", "model": "claude-sonnet-4-20250514" }),
        )
        .expect_err("an unconfigured provider must not produce a silent 401")
        .to_string();
        assert!(
            explicit.contains("provider_credential_missing"),
            "{explicit}"
        );
        assert!(explicit.contains("ANTHROPIC_API_KEY"), "{explicit}");

        let by_model =
            resolve_complete_route(&openai_only, &json!({ "model": "gemini-2.0-flash" }))
                .expect_err("model-only routing must check the owner's credential")
                .to_string();
        assert!(by_model.contains("GOOGLE_API_KEY"), "{by_model}");

        let pinned_default = state_with_credentials(
            Some(Route {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-20250514".into(),
            }),
            Some(&["openai"]),
        );
        let default_error = resolve_complete_route(&pinned_default, &json!({}))
            .expect_err("a pinned default without a credential must be refused")
            .to_string();
        assert!(
            default_error.contains("ANTHROPIC_API_KEY"),
            "{default_error}"
        );
    }

    #[test]
    fn naming_a_model_stays_a_catalogue_query_even_without_a_credential() {
        // `agentos::llm::route` answers "where would this go?"; it must not
        // start failing for callers that only want the mapping.
        let nothing = state_with_credentials(None, Some(&[]));
        let explicit = resolve_route(
            &nothing,
            &json!({ "provider": "anthropic", "model": "claude-sonnet-4-20250514" }),
        )
        .expect("explicit naming resolves without a credential");
        assert_eq!(explicit.provider, "anthropic");

        let by_model = resolve_route(&nothing, &json!({ "model": "haiku" }))
            .expect("alias naming resolves without a credential");
        assert_eq!(by_model.provider, "anthropic");
        assert!(by_model.model.contains("haiku"), "{}", by_model.model);
    }

    #[test]
    fn complete_route_prefers_a_configured_provider_over_legacy_sonnet() {
        let openai_only = state_with_credentials(None, Some(&["openai"]));
        let route = resolve_complete_route(&openai_only, &json!({}))
            .expect("completion must route to the configured provider");
        assert_eq!(route.provider, "openai");

        let nothing = state_with_credentials(None, Some(&[]));
        let error = resolve_complete_route(&nothing, &json!({}))
            .expect_err("completion without any credential must be refused")
            .to_string();
        assert!(error.contains("provider_credential_missing"), "{error}");
    }

    #[test]
    fn provider_credential_snapshots_reject_blank_values() {
        let env = |name: &str| match name {
            "SET_KEY" => Some(" secret ".to_string()),
            "BLANK_KEY" => Some("   ".to_string()),
            _ => None,
        };
        assert_eq!(provider_credential("SET_KEY", &env), Some("secret".into()));
        assert_eq!(provider_credential("BLANK_KEY", &env), None);
        assert_eq!(provider_credential("ABSENT_KEY", &env), None);
        assert_eq!(provider_credential("", &env), Some(String::new()));
    }

    #[test]
    fn provider_catalogs_report_the_credential_snapshot() {
        let openai_only = state_with_credentials(None, Some(&["openai"]));
        let catalog = provider_catalog(&openai_only);
        let entry = |name: &str| {
            catalog
                .as_array()
                .expect("catalog array")
                .iter()
                .find(|provider| provider["name"] == name)
                .cloned()
                .expect("provider entry")
        };
        assert_eq!(entry("openai")["available"], json!(true));
        assert_eq!(entry("anthropic")["available"], json!(false));
        assert_eq!(entry("ollama")["available"], json!(true));
    }
}
