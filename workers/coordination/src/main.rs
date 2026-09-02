use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde_json::{Value, json};

mod types;

use types::{
    Channel, CreateChannelRequest, PinRequest, Post, PostRequest, ReadRequest, ReplyRequest,
    sanitize_id,
};

const MAX_POSTS_PER_CHANNEL: usize = 1000;
const MAX_PINNED: usize = 25;
static POST_COUNTER_INIT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn fire_and_forget(iii: &IIIClient, function_id: &str, payload: Value) {
    let iii = iii.clone();
    let function_id = function_id.to_string();
    tokio::spawn(async move {
        let _ = iii
            .trigger(TriggerRequest {
                function_id,
                payload,
                action: None,
                timeout_ms: None,
            })
            .await;
    });
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

/// `state::list` answers a bare array of the stored values: there is no
/// `{key, value}` envelope. Channels are newest first.
fn channels_newest_first(mut channels: Vec<Value>) -> Vec<Value> {
    channels.sort_by(|a, b| {
        let ta = a.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    channels
}

/// Posts from a `state::list` response, oldest first. Same bare-array shape as
/// `channels_newest_first`.
fn posts_oldest_first(mut posts: Vec<Value>) -> Vec<Value> {
    posts.sort_by(|a, b| {
        let ta = a.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
        ta.cmp(&tb)
    });
    posts
}

fn post_count_from_update(result: &Value) -> Result<(bool, usize), Error> {
    if let Some(errors) = result.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        return Err(Error::Handler(format!(
            "Post counter update failed: {}",
            Value::Array(errors.clone())
        )));
    }

    let count = result
        .get("new_value")
        .and_then(|value| value.get("count"))
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| Error::Handler("Post counter returned an invalid value".into()))?;
    let was_missing = result.get("old_value").is_none_or(Value::is_null);
    Ok((was_missing, count))
}

async fn change_post_count(
    iii: &IIIClient,
    posts_scope: &str,
    delta: i64,
) -> Result<(bool, usize), Error> {
    let (operation, by) = if delta >= 0 {
        ("increment", delta)
    } else {
        ("decrement", -delta)
    };
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::update".to_string(),
            payload: json!({
                "scope": "coord_post_counts",
                "key": posts_scope,
                "ops": [{ "type": operation, "path": "count", "by": by }],
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|error| Error::Handler(error.to_string()))?;
    post_count_from_update(&result)
}

async fn release_post_slot(iii: &IIIClient, posts_scope: &str) -> Result<(), Error> {
    change_post_count(iii, posts_scope, -1).await.map(|_| ())
}

/// Atomically reserves one post slot before either `post` or `reply` persists.
async fn reserve_post_slot(iii: &IIIClient, posts_scope: &str) -> Result<(), Error> {
    let _initialization_guard = POST_COUNTER_INIT_LOCK.lock().await;
    let (was_missing, mut count) = change_post_count(iii, posts_scope, 1).await?;

    if was_missing {
        let existing = match state_list(iii, posts_scope).await {
            Ok(existing) => existing.len(),
            Err(error) => {
                let _ = release_post_slot(iii, posts_scope).await;
                return Err(error);
            }
        };
        if existing > 0 {
            match change_post_count(iii, posts_scope, existing as i64).await {
                Ok((_, reconciled_count)) => count = reconciled_count,
                Err(error) => {
                    let _ = release_post_slot(iii, posts_scope).await;
                    return Err(error);
                }
            }
        }
    }

    if count > MAX_POSTS_PER_CHANNEL {
        release_post_slot(iii, posts_scope).await?;
        return Err(Error::Handler("Channel has reached the post limit".into()));
    }
    Ok(())
}

fn merge_path_into_request(mut body: Value, input: &Value) -> Value {
    if let (Some(path), Some(obj)) = (input.get("path"), body.as_object_mut())
        && let Some(channel_id) = path.get("channelId").and_then(|v| v.as_str())
    {
        obj.entry("channelId".to_string())
            .or_insert_with(|| Value::String(channel_id.to_string()));
    }
    body
}

async fn create_channel(iii: &IIIClient, req: CreateChannelRequest) -> Result<Value, Error> {
    let name = req
        .name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Error::Handler("name and agentId are required".into()))?;
    let agent_id = req
        .agent_id
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::Handler("name and agentId are required".into()))?;

    let safe_name = sanitize_id(&name).map_err(Error::Handler)?;
    let safe_agent = sanitize_id(&agent_id).map_err(Error::Handler)?;

    let channel_id = uuid::Uuid::new_v4().to_string();
    let channel = Channel {
        id: channel_id.clone(),
        name: safe_name.clone(),
        topic: req.topic.unwrap_or_default(),
        created_by: safe_agent,
        created_at: now_ms(),
        pinned: vec![],
    };

    let value = serde_json::to_value(&channel).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "coord_channels", &channel_id, value).await?;

    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("coord:{channel_id}"),
            "data": { "type": "channel_created", "channelId": channel_id, "name": safe_name },
        }),
    );

    Ok::<Value, Error>(json!({
        "channelId": channel_id,
        "name": safe_name,
    }))
}

async fn post(iii: &IIIClient, req: PostRequest) -> Result<Value, Error> {
    let channel_id = req
        .channel_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("channelId, agentId, and content are required".into()))?;
    let agent_id = req
        .agent_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("channelId, agentId, and content are required".into()))?;
    let content = req
        .content
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("channelId, agentId, and content are required".into()))?;

    let safe_channel_id = sanitize_id(&channel_id).map_err(Error::Handler)?;

    if state_get(iii, "coord_channels", &safe_channel_id)
        .await?
        .is_none()
    {
        return Err(Error::Handler("Channel not found".into()));
    }

    let posts_scope = format!("coord_posts:{safe_channel_id}");
    let post_id = uuid::Uuid::new_v4().to_string();
    let safe_agent = sanitize_id(&agent_id).map_err(Error::Handler)?;
    let post = Post {
        id: post_id.clone(),
        channel_id: safe_channel_id.clone(),
        agent_id: safe_agent.clone(),
        content,
        parent_id: None,
        created_at: now_ms(),
        metadata: req.metadata.unwrap_or_else(|| json!({})),
    };

    let value = serde_json::to_value(&post).map_err(|e| Error::Handler(e.to_string()))?;
    reserve_post_slot(iii, &posts_scope).await?;
    if let Err(error) = state_set(iii, &posts_scope, &post_id, value).await {
        if let Err(rollback_error) = release_post_slot(iii, &posts_scope).await {
            tracing::error!(%rollback_error, %posts_scope, "failed to roll back reserved post slot");
        }
        return Err(error);
    }

    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("coord:{safe_channel_id}"),
            "data": { "type": "post_created", "postId": post_id, "agentId": safe_agent },
        }),
    );

    Ok::<Value, Error>(json!({
        "postId": post_id,
        "channelId": safe_channel_id,
    }))
}

async fn reply(iii: &IIIClient, req: ReplyRequest) -> Result<Value, Error> {
    let channel_id = req.channel_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Handler("channelId, parentId, agentId, and content are required".into())
    })?;
    let parent_id = req.parent_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Handler("channelId, parentId, agentId, and content are required".into())
    })?;
    let agent_id = req.agent_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Handler("channelId, parentId, agentId, and content are required".into())
    })?;
    let content = req.content.filter(|s| !s.is_empty()).ok_or_else(|| {
        Error::Handler("channelId, parentId, agentId, and content are required".into())
    })?;

    let safe_channel_id = sanitize_id(&channel_id).map_err(Error::Handler)?;
    let safe_parent_id = sanitize_id(&parent_id).map_err(Error::Handler)?;
    let safe_agent = sanitize_id(&agent_id).map_err(Error::Handler)?;

    let posts_scope = format!("coord_posts:{safe_channel_id}");

    if state_get(iii, &posts_scope, &safe_parent_id)
        .await?
        .is_none()
    {
        return Err(Error::Handler("Parent post not found".into()));
    }

    let post_id = uuid::Uuid::new_v4().to_string();
    let reply = Post {
        id: post_id.clone(),
        channel_id: safe_channel_id.clone(),
        agent_id: safe_agent.clone(),
        content,
        parent_id: Some(safe_parent_id.clone()),
        created_at: now_ms(),
        metadata: req.metadata.unwrap_or_else(|| json!({})),
    };

    let value = serde_json::to_value(&reply).map_err(|e| Error::Handler(e.to_string()))?;
    reserve_post_slot(iii, &posts_scope).await?;
    if let Err(error) = state_set(iii, &posts_scope, &post_id, value).await {
        if let Err(rollback_error) = release_post_slot(iii, &posts_scope).await {
            tracing::error!(%rollback_error, %posts_scope, "failed to roll back reserved reply slot");
        }
        return Err(error);
    }

    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("coord:{safe_channel_id}"),
            "data": {
                "type": "reply_created",
                "postId": post_id,
                "parentId": safe_parent_id,
                "agentId": safe_agent,
            },
        }),
    );

    Ok::<Value, Error>(json!({
        "postId": post_id,
        "parentId": safe_parent_id,
        "channelId": safe_channel_id,
    }))
}

async fn list_channels(iii: &IIIClient) -> Result<Value, Error> {
    let raw = state_list(iii, "coord_channels").await?;
    Ok::<Value, Error>(Value::Array(channels_newest_first(raw)))
}

async fn read(iii: &IIIClient, req: ReadRequest) -> Result<Value, Error> {
    let channel_id = req
        .channel_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("channelId is required".into()))?;
    let safe_channel_id = sanitize_id(&channel_id).map_err(Error::Handler)?;

    let posts_scope = format!("coord_posts:{safe_channel_id}");
    let raw = state_list(iii, &posts_scope).await?;

    let mut posts: Vec<Value> = posts_oldest_first(raw);

    if let Some(thread_id) = req.thread_id {
        let safe_thread = sanitize_id(&thread_id).map_err(Error::Handler)?;
        posts.retain(|p| {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let parent = p.get("parentId").and_then(|v| v.as_str()).unwrap_or("");
            id == safe_thread || parent == safe_thread
        });
    }

    let cap = req.limit.filter(|&n| n > 0).unwrap_or(100);
    if posts.len() > cap {
        let start = posts.len() - cap;
        posts = posts.split_off(start);
    }

    Ok::<Value, Error>(Value::Array(posts))
}

async fn pin(iii: &IIIClient, req: PinRequest) -> Result<Value, Error> {
    let channel_id = req
        .channel_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("channelId and postId are required".into()))?;
    let post_id = req
        .post_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("channelId and postId are required".into()))?;

    let safe_channel_id = sanitize_id(&channel_id).map_err(Error::Handler)?;
    let safe_post_id = sanitize_id(&post_id).map_err(Error::Handler)?;

    let channel_val = state_get(iii, "coord_channels", &safe_channel_id)
        .await?
        .ok_or_else(|| Error::Handler("Channel not found".into()))?;
    let mut channel: Channel =
        serde_json::from_value(channel_val).map_err(|e| Error::Handler(e.to_string()))?;

    let posts_scope = format!("coord_posts:{safe_channel_id}");
    if state_get(iii, &posts_scope, &safe_post_id).await?.is_none() {
        return Err(Error::Handler("Post not found".into()));
    }

    let unpin = req.unpin.unwrap_or(false);
    if unpin {
        channel.pinned.retain(|id| id != &safe_post_id);
    } else if !channel.pinned.contains(&safe_post_id) {
        if channel.pinned.len() >= MAX_PINNED {
            return Err(Error::Handler(format!(
                "Maximum {MAX_PINNED} pinned posts per channel"
            )));
        }
        channel.pinned.push(safe_post_id.clone());
    } else {
        return Ok::<Value, Error>(json!({
            "channelId": safe_channel_id,
            "pinned": channel.pinned,
        }));
    }

    let value = serde_json::to_value(&channel).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "coord_channels", &safe_channel_id, value).await?;

    Ok::<Value, Error>(json!({
        "channelId": safe_channel_id,
        "pinned": channel.pinned,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_clone = iii.clone();
    iii.register_function(
        "coord::create_channel",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or_else(|| input.clone());
                let req: CreateChannelRequest =
                    serde_json::from_value(body).map_err(|e| Error::Handler(e.to_string()))?;
                create_channel(&iii, req).await
            }
        })
        .description("Create a coordination channel for agent communication"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "coord::post",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or_else(|| input.clone());
                let body = merge_path_into_request(body, &input);
                let req: PostRequest =
                    serde_json::from_value(body).map_err(|e| Error::Handler(e.to_string()))?;
                post(&iii, req).await
            }
        })
        .description("Post a message to a coordination channel"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "coord::reply",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or_else(|| input.clone());
                let body = merge_path_into_request(body, &input);
                let req: ReplyRequest =
                    serde_json::from_value(body).map_err(|e| Error::Handler(e.to_string()))?;
                reply(&iii, req).await
            }
        })
        .description("Reply to a post in a coordination channel (threaded)"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "coord::list_channels",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move { list_channels(&iii).await }
        })
        .description("List all coordination channels"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "coord::read",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input
                    .get("body")
                    .cloned()
                    .or_else(|| input.get("query").cloned())
                    .unwrap_or_else(|| input.clone());
                let body = merge_path_into_request(body, &input);
                let req: ReadRequest =
                    serde_json::from_value(body).map_err(|e| Error::Handler(e.to_string()))?;
                read(&iii, req).await
            }
        })
        .description("Read messages in a coordination channel"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "coord::pin",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or_else(|| input.clone());
                let body = merge_path_into_request(body, &input);
                let req: PinRequest =
                    serde_json::from_value(body).map_err(|e| Error::Handler(e.to_string()))?;
                pin(&iii, req).await
            }
        })
        .description("Pin or unpin a post in a coordination channel"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "coord::create_channel".to_string(),
        json!({ "api_path": "api/coord/channel", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "coord::post".to_string(),
        json!({ "api_path": "api/coord/:channelId/post", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "coord::reply".to_string(),
        json!({ "api_path": "api/coord/:channelId/reply", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "coord::list_channels".to_string(),
        json!({ "api_path": "api/coord/channels", "http_method": "GET" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "coord::read".to_string(),
        json!({ "api_path": "api/coord/:channelId", "http_method": "GET" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "coord::pin".to_string(),
        json!({ "api_path": "api/coord/:channelId/pin", "http_method": "POST" }),
        None,
    )?;

    tracing::info!("coordination worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_atomic_post_counter_updates() {
        assert_eq!(
            post_count_from_update(&json!({
                "old_value": null,
                "new_value": { "count": 1 },
            }))
            .unwrap(),
            (true, 1)
        );
        assert_eq!(
            post_count_from_update(&json!({
                "old_value": { "count": 1 },
                "new_value": { "count": 2 },
            }))
            .unwrap(),
            (false, 2)
        );
    }

    #[test]
    fn rejects_counter_errors_and_invalid_shapes() {
        assert!(
            post_count_from_update(&json!({
                "old_value": null,
                "new_value": {},
                "errors": [{ "code": "increment.type_mismatch" }],
            }))
            .is_err()
        );
        assert!(post_count_from_update(&json!({ "new_value": {} })).is_err());
    }

    // --- state::list protocol (verified against iii 0.22.1) ---

    #[test]
    fn channels_are_ordered_newest_first_from_a_bare_list() {
        let raw = vec![
            json!({ "id": "c1", "createdAt": 10 }),
            json!({ "id": "c3", "createdAt": 30 }),
            json!({ "id": "c2", "createdAt": 20 }),
        ];
        let ordered = channels_newest_first(raw);
        let ids: Vec<&str> = ordered
            .iter()
            .map(|c| c["id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(ids, vec!["c3", "c2", "c1"]);
    }

    #[test]
    fn posts_are_ordered_oldest_first_from_a_bare_list() {
        let raw = vec![
            json!({ "id": "p2", "createdAt": 20 }),
            json!({ "id": "p1", "createdAt": 10 }),
        ];
        let ordered = posts_oldest_first(raw);
        let ids: Vec<&str> = ordered
            .iter()
            .map(|p| p["id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(ids, vec!["p1", "p2"]);
    }

    #[test]
    fn a_post_carrying_its_own_value_field_survives_intact() {
        // The old reader unwrapped `entry["value"]` when present, so a post
        // whose body happened to be stored under `value` was replaced by it
        // and lost its id, author and timestamp.
        let raw = vec![json!({
            "id": "p1",
            "channelId": "c1",
            "createdAt": 10,
            "value": { "id": "wrong" },
        })];
        let ordered = posts_oldest_first(raw);
        assert_eq!(ordered[0]["id"], "p1");
        assert_eq!(ordered[0]["channelId"], "c1");
        assert_eq!(ordered[0]["value"], json!({ "id": "wrong" }));
    }
}
