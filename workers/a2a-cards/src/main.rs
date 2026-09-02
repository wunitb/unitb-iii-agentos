use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};

mod types;

use types::{A2aAgentCard, A2aAuthentication, A2aCapabilities, AgentSkillRef, GenerateCardRequest};

/// HTTP path of this worker's discovery document.
///
/// `GET /.well-known/agent.json` is the A2A spec location and is bound by
/// `workers/a2a` to `a2a::agent_card`. Two workers binding the same method and
/// path leaves the winner to process start order, so the card catalogue serves
/// its copy from its own path.
const WELL_KNOWN_ROUTE: &str = "api/a2a/agent-card";

fn api_url() -> String {
    std::env::var("AGENTOS_API_URL").unwrap_or_else(|_| "http://localhost:3111".to_string())
}

async fn state_get(iii: &IIIClient, scope: &str, key: &str) -> Result<Option<Value>, Error> {
    let v = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": scope, "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(if v.is_null() { None } else { Some(v) })
}

async fn state_set(iii: &IIIClient, scope: &str, key: &str, value: Value) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({ "scope": scope, "key": key, "value": value }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map(|_| ())
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn state_list(iii: &IIIClient, scope: &str) -> Result<Vec<Value>, Error> {
    let v = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": scope }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

async fn list_agent_functions(iii: &IIIClient, agent_id: &str) -> Result<Vec<String>, Error> {
    let res = iii
        .trigger(TriggerRequest {
            function_id: "agent::list_functions".to_string(),
            payload: json!({ "agentId": agent_id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(res
        .as_array()
        .cloned()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    t.get("function_id")
                        .or_else(|| t.get("id"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default())
}

/// `state::list` answers a bare array of the stored values: there is no
/// `{key, value}` envelope, so a skill document is read as it arrives.
fn skills_from_entries(entries: Vec<Value>) -> Vec<AgentSkillRef> {
    entries
        .into_iter()
        .filter_map(|skill| {
            let id = skill.get("id")?.as_str()?.to_string();
            let name = skill
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = skill
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AgentSkillRef {
                id,
                name,
                description,
            })
        })
        .take(20)
        .collect()
}

/// Agent ids from a bare `state::list` response over the `agents` scope.
fn agent_ids_from_entries(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|agent| agent.get("id")?.as_str().map(String::from))
        .collect()
}

async fn list_skill_entries(iii: &IIIClient) -> Result<Vec<AgentSkillRef>, Error> {
    Ok(skills_from_entries(state_list(iii, "skills").await?))
}

async fn generate_card(iii: &IIIClient, req: GenerateCardRequest) -> Result<Value, Error> {
    let config = state_get(iii, "agents", &req.agent_id)
        .await?
        .ok_or_else(|| Error::Handler(format!("Agent not found: {}", req.agent_id)))?;

    let function_ids = list_agent_functions(iii, &req.agent_id).await?;
    let skills = list_skill_entries(iii).await?;

    let name = config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&req.agent_id)
        .to_string();
    let description = config
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("Agent {}", req.agent_id));

    let card = A2aAgentCard {
        name,
        description,
        url: format!("{}/api/a2a/agents/{}", api_url(), req.agent_id),
        capabilities: A2aCapabilities {
            functions: function_ids.into_iter().take(50).collect(),
            streaming: true,
            push_notifications: false,
        },
        skills,
        authentication: A2aAuthentication {
            schemes: vec!["bearer".into()],
        },
        default_input_modes: vec!["text".into()],
        default_output_modes: vec!["text".into()],
    };

    let value = serde_json::to_value(&card).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "a2a_cards", &req.agent_id, value.clone()).await?;

    Ok::<Value, Error>(value)
}

async fn list_cards(iii: &IIIClient) -> Result<Value, Error> {
    let agents = state_list(iii, "agents").await?;
    let mut cards: Vec<Value> = Vec::new();

    for agent_id in agent_ids_from_entries(&agents) {
        if let Ok(card) = generate_card(iii, GenerateCardRequest { agent_id }).await {
            cards.push(card);
        }
    }

    Ok::<Value, Error>(Value::Array(cards))
}

async fn well_known(iii: &IIIClient) -> Result<Value, Error> {
    if let Some(cached) = state_get(iii, "a2a_cards", "orchestrator").await? {
        return Ok::<Value, Error>(cached);
    }

    let card = A2aAgentCard {
        name: "agentos".into(),
        description: "AI agent operating system with multi-agent orchestration".into(),
        url: format!("{}/api/a2a/agents/orchestrator", api_url()),
        capabilities: A2aCapabilities {
            functions: vec![],
            streaming: true,
            push_notifications: false,
        },
        skills: vec![],
        authentication: A2aAuthentication {
            schemes: vec!["bearer".into()],
        },
        default_input_modes: vec!["text".into()],
        default_output_modes: vec!["text".into()],
    };

    let value = serde_json::to_value(&card).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "a2a_cards", "orchestrator", value.clone()).await?;
    Ok::<Value, Error>(value)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL")
        .or_else(|_| std::env::var("III_URL"))
        .unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_clone = iii.clone();
    iii.register_function(
        "a2a::generate_card",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: GenerateCardRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                generate_card(&iii, req).await
            }
        })
        .description("Generate an A2A agent card for a specific agent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "a2a::list_cards",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move { list_cards(&iii).await }
        })
        .description("List all A2A agent cards"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "a2a::well_known",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move { well_known(&iii).await }
        })
        .description("Serve the .well-known/agent.json discovery document"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "a2a::list_cards".to_string(),
        json!({ "api_path": "api/a2a/cards", "http_method": "GET" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "a2a::generate_card".to_string(),
        json!({ "api_path": "api/a2a/cards/:agentId", "http_method": "GET" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "a2a::well_known".to_string(),
        json!({ "api_path": WELL_KNOWN_ROUTE, "http_method": "GET" }),
        None,
    )?;

    tracing::info!("a2a-cards worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- state::list protocol (verified against iii 0.22.1) ---

    #[test]
    fn skills_are_read_from_a_bare_list() {
        let entries = vec![
            json!({ "id": "s1", "name": "Search", "description": "find things" }),
            json!({ "id": "s2", "name": "Write" }),
        ];
        let skills = skills_from_entries(entries);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].id, "s1");
        assert_eq!(skills[0].name, "Search");
        assert_eq!(skills[1].description, "");
    }

    #[test]
    fn a_skill_carrying_its_own_value_field_survives_intact() {
        // The old reader unwrapped `entry["value"]` when present, so a skill
        // document with a `value` field was replaced by that field alone.
        let entries = vec![json!({
            "id": "s1",
            "name": "Search",
            "description": "find things",
            "value": { "id": "wrong", "name": "wrong" },
        })];
        let skills = skills_from_entries(entries);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "s1");
        assert_eq!(skills[0].name, "Search");
    }

    #[test]
    fn skills_are_capped_at_twenty() {
        let entries: Vec<Value> = (0..25).map(|i| json!({ "id": format!("s{i}") })).collect();
        assert_eq!(skills_from_entries(entries).len(), 20);
    }

    #[test]
    fn deleted_entries_are_skipped() {
        // `state::set value=null` leaves a null entry in the scope.
        let entries = vec![Value::Null, json!({ "id": "s1" })];
        assert_eq!(skills_from_entries(entries).len(), 1);
    }

    // --- HTTP route uniqueness ---

    #[test]
    fn the_card_catalogue_does_not_bind_the_spec_route() {
        // `workers/a2a` binds GET /.well-known/agent.json to a2a::agent_card.
        assert_ne!(WELL_KNOWN_ROUTE, ".well-known/agent.json");
        assert_eq!(WELL_KNOWN_ROUTE, "api/a2a/agent-card");
    }

    #[test]
    fn the_card_catalogue_route_stays_under_the_a2a_api_prefix() {
        assert!(WELL_KNOWN_ROUTE.starts_with("api/a2a/"));
    }

    #[test]
    fn agent_ids_are_read_from_a_bare_list() {
        let entries = vec![
            json!({ "id": "agent-1", "name": "One" }),
            json!({ "name": "no id" }),
            Value::Null,
            json!({ "id": "agent-2" }),
        ];
        assert_eq!(
            agent_ids_from_entries(&entries),
            vec!["agent-1".to_string(), "agent-2".to_string()]
        );
    }

    #[test]
    fn the_envelope_this_worker_used_to_expect_is_never_sent() {
        let enveloped = vec![json!({ "key": "agent-1", "value": { "id": "agent-1" } })];
        assert!(agent_ids_from_entries(&enveloped).is_empty());
    }
}
