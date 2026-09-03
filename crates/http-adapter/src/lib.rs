use iii_sdk::{
    IIIClient, RegisterFunction,
    builtin_triggers::{CronTriggerConfig, IIITrigger},
    errors::Error,
    protocol::{RegisterTriggerInput, TriggerRequest},
    trigger::Trigger,
};
use serde_json::{Value, json};
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub mod bus;
pub mod policy;
pub mod principal;
pub mod state;

#[cfg(any(test, feature = "fake-bus"))]
pub mod fake;

pub use bus::{BusFuture, CHAT_TIMEOUT_MS, TriggerBus};

/// Registers an HTTP trigger through a local adapter function.
///
/// iii 0.22 HTTP handlers return an HTTP response envelope. AgentOS functions
/// retain their transport-neutral return values for internal calls; the
/// adapter adds the envelope only at the HTTP boundary.
pub fn register_http_trigger(
    iii: &IIIClient,
    function_id: impl Into<String>,
    config: Value,
    metadata: Option<Value>,
) -> Result<Trigger, Error> {
    let mut config = config;
    let auth_key = resolve_http_auth(&mut config)?;
    let config = normalize_http_config(config)?;
    let function_id = function_id.into();
    let method = config["http_method"].as_str().unwrap_or_default();
    let path = config["api_path"].as_str().unwrap_or_default();
    let adapter_id = format!("agentos::http::{function_id}::{method}::{path}");
    let target_function_id = function_id.clone();
    let client = iii.clone();

    iii.register_function(
        adapter_id.clone(),
        RegisterFunction::new_async(move |request: Value| {
            let iii = client.clone();
            let function_id = target_function_id.clone();
            let auth_key = auth_key.clone();
            async move {
                if auth_key
                    .as_deref()
                    .is_some_and(|expected| !is_authorized(&request, expected))
                {
                    return Ok(json!({
                        "status_code": 401,
                        "headers": { "www-authenticate": "Bearer" },
                        "body": { "error": "unauthorized" },
                    }));
                }
                let result = iii
                    .trigger(TriggerRequest {
                        function_id,
                        payload: normalize_http_request(request),
                        action: None,
                        timeout_ms: None,
                    })
                    .await?;
                Ok(into_http_response(result))
            }
        })
        .description(format!("HTTP adapter for {function_id}")),
    );

    iii.register_trigger(RegisterTriggerInput {
        trigger_type: "http".to_string(),
        function_id: adapter_id,
        config,
        metadata,
    })
}

/// Registers a cron trigger using iii 0.22's required six-field expression.
pub fn register_cron_trigger(
    iii: &IIIClient,
    function_id: impl Into<String>,
    expression: impl AsRef<str>,
) -> Result<Trigger, Error> {
    register_cron_trigger_with_metadata(iii, function_id, expression, None)
}

/// Registers a cron trigger and passes metadata beside the cron event payload.
pub fn register_cron_trigger_with_metadata(
    iii: &IIIClient,
    function_id: impl Into<String>,
    expression: impl AsRef<str>,
    metadata: Option<Value>,
) -> Result<Trigger, Error> {
    let expression = normalize_cron_expression(expression.as_ref())?;
    let mut input = IIITrigger::Cron(CronTriggerConfig::new(expression)).for_function(function_id);
    input.metadata = metadata;
    iii.register_trigger(input)
}

fn normalize_cron_expression(expression: &str) -> Result<String, Error> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    match fields.len() {
        5 => Ok(format!("0 {}", fields.join(" "))),
        6 => Ok(fields.join(" ")),
        count => Err(Error::Handler(format!(
            "cron expression requires 5 or 6 fields, received {count}"
        ))),
    }
}

fn normalize_http_request(request: Value) -> Value {
    let Value::Object(mut payload) = request else {
        return request;
    };

    let body = payload.get("body").and_then(Value::as_object).cloned();
    let query = payload
        .get("query_params")
        .and_then(Value::as_object)
        .cloned();
    let path = payload
        .get("path_params")
        .and_then(Value::as_object)
        .cloned();

    if let Some(query) = &query {
        payload.insert("query".to_string(), Value::Object(query.clone()));
    }

    if let Some(fields) = body {
        for (key, value) in fields {
            payload.entry(key).or_insert(value);
        }
    }

    if let Some(fields) = query {
        for (key, value) in fields {
            payload
                .entry(key)
                .or_insert_with(|| scalar_query_value(value));
        }
    }

    if let Some(fields) = path {
        for (key, value) in fields {
            payload.insert(key, value);
        }
    }

    Value::Object(payload)
}

fn scalar_query_value(value: Value) -> Value {
    match value {
        Value::Array(mut values) if values.len() == 1 => values.remove(0),
        value => value,
    }
}

fn resolve_http_auth(config: &mut Value) -> Result<Option<Arc<str>>, Error> {
    let object = config
        .as_object_mut()
        .ok_or_else(|| Error::Handler("HTTP trigger config must be an object".to_string()))?;
    let auth_required = match object.remove("auth") {
        Some(Value::Bool(required)) => required,
        Some(_) => {
            return Err(Error::Handler(
                "HTTP trigger config auth must be a boolean".to_string(),
            ));
        }
        None => true,
    };

    if !auth_required {
        return Ok(None);
    }

    let key = std::env::var("AGENTOS_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            Error::Handler("AGENTOS_API_KEY is required for protected HTTP routes".to_string())
        })?;
    Ok(Some(Arc::from(key)))
}

fn is_authorized(request: &Value, expected: &str) -> bool {
    let Some(header) = request
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers.iter().find_map(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.as_str())
                    .flatten()
            })
        })
    else {
        return false;
    };
    let Some((scheme, token)) = header.split_once(' ') else {
        return false;
    };

    scheme.eq_ignore_ascii_case("bearer")
        && token.len() == expected.len()
        && bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
}

fn normalize_http_config(mut config: Value) -> Result<Value, Error> {
    let object = config
        .as_object_mut()
        .ok_or_else(|| Error::Handler("HTTP trigger config must be an object".to_string()))?;

    if let Some(path) = object.remove("path") {
        object.entry("api_path").or_insert(path);
    }
    if let Some(method) = object.remove("method") {
        object.entry("http_method").or_insert(method);
    }

    let path = object
        .get("api_path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("HTTP trigger config requires api_path".to_string()))?;
    if !path.starts_with('/') {
        object.insert("api_path".to_string(), Value::String(format!("/{path}")));
    }

    let method = object
        .get("http_method")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("HTTP trigger config requires http_method".to_string()))?;
    object.insert(
        "http_method".to_string(),
        Value::String(method.to_ascii_uppercase()),
    );

    Ok(config)
}

fn into_http_response(value: Value) -> Value {
    if value
        .as_object()
        .is_some_and(|object| object.contains_key("status_code") && object.contains_key("body"))
    {
        value
    } else {
        json!({ "status_code": 200, "body": value })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        into_http_response, is_authorized, normalize_cron_expression, normalize_http_config,
        normalize_http_request, resolve_http_auth,
    };
    use serde_json::json;

    #[test]
    fn normalizes_legacy_trigger_keys() {
        let config = normalize_http_config(json!({
            "path": "api/health",
            "method": "get",
        }))
        .unwrap();

        assert_eq!(
            config,
            json!({ "api_path": "/api/health", "http_method": "GET" })
        );
    }

    #[test]
    fn strips_agentos_auth_policy_from_engine_config() {
        let mut config = json!({
            "api_path": "/api/health",
            "http_method": "GET",
            "auth": false,
        });

        assert!(resolve_http_auth(&mut config).unwrap().is_none());
        assert_eq!(
            normalize_http_config(config).unwrap(),
            json!({ "api_path": "/api/health", "http_method": "GET" })
        );
    }

    #[test]
    fn accepts_only_matching_bearer_credentials() {
        let request = json!({
            "headers": { "Authorization": "bearer expected-key" },
        });

        assert!(is_authorized(&request, "expected-key"));
        assert!(!is_authorized(&request, "different-key"));
        assert!(!is_authorized(&json!({ "headers": {} }), "expected-key"));
        assert!(!is_authorized(
            &json!({ "headers": { "authorization": "expected-key" } }),
            "expected-key"
        ));
    }

    #[test]
    fn preserves_envelope_and_exposes_application_fields() {
        let request = json!({
            "body": { "agentId": "body-agent", "message": "hello", "items": [1] },
            "headers": { "authorization": "Bearer token" },
            "method": "POST",
            "path_params": { "agentId": "path-agent" },
            "query_params": { "limit": ["10"], "tag": ["a", "b"] },
        });

        let payload = normalize_http_request(request);

        assert_eq!(payload["agentId"], json!("path-agent"));
        assert_eq!(payload["message"], json!("hello"));
        assert_eq!(payload["items"], json!([1]));
        assert_eq!(payload["limit"], json!("10"));
        assert_eq!(payload["tag"], json!(["a", "b"]));
        assert_eq!(payload["query"]["limit"], json!(["10"]));
        assert_eq!(payload["headers"]["authorization"], json!("Bearer token"));
        assert_eq!(payload["body"]["agentId"], json!("body-agent"));
    }

    #[test]
    fn upgrades_five_field_cron_expressions() {
        assert_eq!(
            normalize_cron_expression("*/5 * * * *").unwrap(),
            "0 */5 * * * *"
        );
    }

    #[test]
    fn preserves_six_field_cron_expressions() {
        assert_eq!(
            normalize_cron_expression("0 */5 * * * *").unwrap(),
            "0 */5 * * * *"
        );
    }

    #[test]
    fn rejects_invalid_cron_expressions() {
        let error = normalize_cron_expression("every five minutes").unwrap_err();
        assert!(error.to_string().contains("requires 5 or 6 fields"));
    }

    #[test]
    fn wraps_transport_neutral_results() {
        assert_eq!(
            into_http_response(json!({ "status": "ok" })),
            json!({ "status_code": 200, "body": { "status": "ok" } })
        );
    }

    #[test]
    fn preserves_explicit_http_responses() {
        let response = json!({
            "status_code": 201,
            "headers": { "location": "/items/1" },
            "body": { "id": 1 },
        });

        assert_eq!(into_http_response(response.clone()), response);
    }
}
