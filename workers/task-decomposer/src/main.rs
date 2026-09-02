use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};

mod types;

use types::{Task, TaskStatus, sanitize_id, strip_code_fences};

const MAX_DEPTH: u32 = 3;
const MAX_SUBTASKS: usize = 10;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn generate_task_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("t_{:x}{}", now, &suffix[..4])
}

async fn state_get(iii: &IIIClient, scope: &str, key: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".to_string(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn state_set(iii: &IIIClient, scope: &str, key: &str, value: Value) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({ "scope": scope, "key": key, "value": value }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(())
}

async fn state_list(iii: &IIIClient, scope: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({ "scope": scope }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

/// `state::list` answers a bare array of stored values: there is no `{key,
/// value}` envelope, so unwrapping a `value` field would corrupt any task
/// document that happens to carry one.
fn tasks_from_list(list: &Value) -> Vec<Value> {
    list.as_array().cloned().unwrap_or_default()
}

/// `state::update` payload that claims a pending task before its worker is
/// spawned.
///
/// The engine names the operation list `ops`; an `operations` key fails the
/// whole invocation with "missing field `ops`", which made every claim fail
/// and left `task::spawn_workers` unable to spawn anything.
fn claim_task_payload(scope: &str, task_id: &str, updated_at: u128) -> Value {
    json!({
        "scope": scope,
        "key": task_id,
        "ops": [
            { "type": "set", "path": "status", "value": "in_progress" },
            { "type": "set", "path": "updatedAt", "value": updated_at }
        ]
    })
}

/// A rejected operation still answers success at the transport level: the
/// engine reports it inside an `errors` array of an otherwise normal result.
fn update_rejection(result: &Value) -> Option<String> {
    let errors = result.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }
    Some(Value::Array(errors.clone()).to_string())
}

async fn decompose_task(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let description = input
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("description is required".into()))?
        .to_string();

    let current_depth = input.get("depth").and_then(Value::as_u64).unwrap_or(0) as u32;
    if current_depth >= MAX_DEPTH {
        return Ok::<Value, Error>(json!({
            "decomposed": false,
            "reason": "Max depth reached",
        }));
    }

    let root_id = match input.get("rootId").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => sanitize_id(s).map_err(Error::Handler)?,
        _ => generate_task_id(),
    };
    let parent_id = input
        .get("parentId")
        .and_then(Value::as_str)
        .map(sanitize_id)
        .transpose()
        .map_err(Error::Handler)?;
    let task_id = parent_id.clone().unwrap_or_else(|| root_id.clone());

    let model = input
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty());

    let mut llm_payload = json!({
        "systemPrompt": format!(
            "Decompose the following task into subtasks. Return JSON: {{ \"subtasks\": [{{ \"id\": \"<parentId>.<n>\", \"description\": \"...\" }}] }}. Maximum {MAX_SUBTASKS} subtasks. Parent ID is \"{task_id}\". Use hierarchical numbering (e.g., {task_id}.1, {task_id}.2)."
        ),
        "messages": [{ "role": "user", "content": description }],
    });
    if let Some(model) = model {
        llm_payload["model"] = json!(model);
    }

    let llm_result = iii
        .trigger(TriggerRequest {
            function_id: "agentos::llm::complete".to_string(),
            payload: llm_payload,
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let content = llm_result
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let parsed: Value = match serde_json::from_str(&strip_code_fences(content)) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(root_id = %root_id, task_id = %task_id, "Failed to parse LLM decomposition");
            return Ok::<Value, Error>(json!({
                "decomposed": false,
                "reason": "LLM parse failure",
                "rootId": root_id,
            }));
        }
    };

    let mut subtasks: Vec<(String, String)> = Vec::new();
    if let Some(arr) = parsed.get("subtasks").and_then(Value::as_array) {
        for sub in arr.iter().take(MAX_SUBTASKS) {
            let raw_id = sub.get("id").and_then(Value::as_str).unwrap_or("");
            let id = match sanitize_id(raw_id) {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(raw_id = %raw_id, "Rejected malformed subtask id from LLM");
                    continue;
                }
            };
            let desc = sub
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            subtasks.push((id, desc));
        }
    }

    let scope = format!("tasks:{root_id}");
    let edges_scope = format!("task_edges:{root_id}");

    if parent_id.is_none() {
        let now = now_ms();
        let root_task = Task {
            id: root_id.clone(),
            root_id: root_id.clone(),
            parent_id: None,
            description: description.clone(),
            status: TaskStatus::Pending,
            depth: 0,
            children: subtasks.iter().map(|(id, _)| id.clone()).collect(),
            created_at: now,
            updated_at: now,
        };
        let value = serde_json::to_value(&root_task).map_err(|e| Error::Handler(e.to_string()))?;
        state_set(iii, &scope, &root_id, value).await?;
    }

    let mut created: Vec<Task> = Vec::new();
    for (sub_id, sub_desc) in &subtasks {
        let now = now_ms();
        let task = Task {
            id: sub_id.clone(),
            root_id: root_id.clone(),
            parent_id: Some(task_id.clone()),
            description: sub_desc.clone(),
            status: TaskStatus::Pending,
            depth: current_depth + 1,
            children: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let value = serde_json::to_value(&task).map_err(|e| Error::Handler(e.to_string()))?;
        state_set(iii, &scope, sub_id, value).await?;

        state_set(
            iii,
            &edges_scope,
            &format!("{task_id}->{sub_id}"),
            json!({ "parent": task_id, "child": sub_id }),
        )
        .await?;

        created.push(task);
    }

    if let Some(parent_id) = &parent_id {
        let parent_val = state_get(iii, &scope, parent_id).await?;
        if !parent_val.is_null()
            && let Ok(mut parent_task) = serde_json::from_value::<Task>(parent_val)
        {
            let mut child_set: std::collections::BTreeSet<String> =
                parent_task.children.into_iter().collect();
            for (sub_id, _) in &subtasks {
                child_set.insert(sub_id.clone());
            }
            parent_task.children = child_set.into_iter().collect();
            parent_task.updated_at = now_ms();
            let value =
                serde_json::to_value(&parent_task).map_err(|e| Error::Handler(e.to_string()))?;
            state_set(iii, &scope, parent_id, value).await?;
        }
    }

    tracing::info!(
        root_id = %root_id,
        task_id = %task_id,
        subtask_count = created.len(),
        "Task decomposed"
    );

    Ok::<Value, Error>(json!({
        "rootId": root_id,
        "taskId": task_id,
        "subtasks": created,
    }))
}

async fn get_task(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let raw_root_id = input
        .get("rootId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId and taskId are required".into()))?;
    let raw_task_id = input
        .get("taskId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId and taskId are required".into()))?;
    let root_id = sanitize_id(raw_root_id).map_err(Error::Handler)?;
    let task_id = sanitize_id(raw_task_id).map_err(Error::Handler)?;

    let task = state_get(iii, &format!("tasks:{root_id}"), &task_id).await?;
    if task.is_null() {
        return Err(Error::Handler("Task not found".into()));
    }
    Ok(task)
}

async fn update_status(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let raw_root_id = input
        .get("rootId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId, taskId, and status are required".into()))?;
    let raw_task_id = input
        .get("taskId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId, taskId, and status are required".into()))?;
    let status_str = input
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId, taskId, and status are required".into()))?;

    let root_id = sanitize_id(raw_root_id).map_err(Error::Handler)?;
    let task_id = sanitize_id(raw_task_id).map_err(Error::Handler)?;
    let status = TaskStatus::from_str(status_str)
        .ok_or_else(|| Error::Handler(format!("Invalid status: {status_str}")))?;

    let scope = format!("tasks:{root_id}");
    let task_val = state_get(iii, &scope, &task_id).await?;
    if task_val.is_null() {
        return Err(Error::Handler("Task not found".into()));
    }
    let mut task: Task = serde_json::from_value(task_val)
        .map_err(|e| Error::Handler(format!("invalid task record: {e}")))?;

    task.status = status;
    task.updated_at = now_ms();
    let value = serde_json::to_value(&task).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, &scope, &task_id, value).await?;

    let mut parent_id_opt = task.parent_id.clone();
    while let Some(parent_id) = parent_id_opt {
        let parent_val = state_get(iii, &scope, &parent_id).await?;
        if parent_val.is_null() {
            break;
        }
        let mut parent: Task = match serde_json::from_value(parent_val) {
            Ok(p) => p,
            Err(_) => break,
        };
        if parent.children.is_empty() {
            break;
        }

        let mut siblings: Vec<Option<Task>> = Vec::with_capacity(parent.children.len());
        for child_id in &parent.children {
            let val = state_get(iii, &scope, child_id).await?;
            if val.is_null() {
                siblings.push(None);
            } else {
                siblings.push(serde_json::from_value(val).ok());
            }
        }

        let all_complete = siblings
            .iter()
            .all(|s| matches!(s, Some(t) if t.status == TaskStatus::Complete));
        let any_failed = siblings
            .iter()
            .any(|s| matches!(s, Some(t) if t.status == TaskStatus::Failed));

        let new_parent_status = if all_complete {
            Some(TaskStatus::Complete)
        } else if any_failed {
            Some(TaskStatus::Blocked)
        } else {
            None
        };

        match new_parent_status {
            None => break,
            Some(s) if s == parent.status => break,
            Some(s) => {
                parent.status = s;
                parent.updated_at = now_ms();
                let value =
                    serde_json::to_value(&parent).map_err(|e| Error::Handler(e.to_string()))?;
                state_set(iii, &scope, &parent.id.clone(), value).await?;
                parent_id_opt = parent.parent_id.clone();
            }
        }
    }

    tracing::info!(root_id = %root_id, task_id = %task_id, status = ?status, "Task status updated");

    Ok::<Value, Error>(json!({
        "taskId": task_id,
        "status": status,
        "updatedAt": task.updated_at,
    }))
}

async fn list_tasks(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let raw_root_id = input
        .get("rootId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId is required".into()))?;
    let root_id = sanitize_id(raw_root_id).map_err(Error::Handler)?;
    let status_filter = input
        .get("status")
        .and_then(Value::as_str)
        .map(String::from);

    let entries = state_list(iii, &format!("tasks:{root_id}")).await?;
    let mut tasks: Vec<Value> = tasks_from_list(&entries);

    if let Some(status) = status_filter {
        tasks.retain(|t| {
            t.get("status")
                .and_then(Value::as_str)
                .map(|s| s == status)
                .unwrap_or(false)
        });
    }

    Ok::<Value, Error>(json!({
        "rootId": root_id,
        "count": tasks.len(),
        "tasks": tasks,
    }))
}

async fn spawn_workers(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let raw_root_id = input
        .get("rootId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("rootId is required".into()))?;
    let root_id = sanitize_id(raw_root_id).map_err(Error::Handler)?;

    let entries = state_list(iii, &format!("tasks:{root_id}")).await?;

    let mut spawned = 0u64;
    let scope = format!("tasks:{root_id}");
    for task_val in tasks_from_list(&entries) {
        let status = task_val.get("status").and_then(Value::as_str).unwrap_or("");
        let children_empty = task_val
            .get("children")
            .and_then(Value::as_array)
            .map(|c| c.is_empty())
            .unwrap_or(true);
        if status != "pending" || !children_empty {
            continue;
        }
        let task_id = task_val
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if task_id.is_empty() {
            continue;
        }
        let description = task_val
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let claim = iii
            .trigger(TriggerRequest {
                function_id: "state::update".to_string(),
                payload: claim_task_payload(&scope, &task_id, now_ms()),
                action: None,
                timeout_ms: None,
            })
            .await;
        match claim {
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "skipping task: failed to claim before spawn");
                continue;
            }
            Ok(result) => {
                if let Some(rejection) = update_rejection(&result) {
                    tracing::warn!(task_id = %task_id, rejection = %rejection, "skipping task: claim was rejected");
                    continue;
                }
            }
        }

        let iii_clone = iii.clone();
        let payload = json!({
            "template": "task-worker",
            "message": description,
            "metadata": { "rootId": root_id.clone(), "taskId": task_id },
        });
        tokio::spawn(async move {
            let _ = iii_clone
                .trigger(TriggerRequest {
                    function_id: "fn::agent_spawn".to_string(),
                    payload,
                    action: None,
                    timeout_ms: None,
                })
                .await;
        });
        spawned += 1;
    }

    tracing::info!(root_id = %root_id, count = spawned, "Spawned task workers");

    Ok::<Value, Error>(json!({
        "rootId": root_id,
        "spawned": spawned,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_clone = iii.clone();
    iii.register_function(
        "task::decompose",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { decompose_task(&iii, input).await }
        })
        .description("Recursively decompose a complex task into subtasks with hierarchical IDs"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "task::get",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { get_task(&iii, input).await }
        })
        .description("Get a task by rootId and taskId"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "task::update_status",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { update_status(&iii, input).await }
        })
        .description("Update task status and propagate to parent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "task::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { list_tasks(&iii, input).await }
        })
        .description("List tasks by rootId with optional status filter"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "task::spawn_workers",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { spawn_workers(&iii, input).await }
        })
        .description("Spawn agents for pending leaf tasks"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "task::decompose".to_string(),
        json!({ "http_method": "POST", "api_path": "api/tasks/decompose" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "task::get".to_string(),
        json!({ "http_method": "POST", "api_path": "api/tasks/get" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "task::update_status".to_string(),
        json!({ "http_method": "POST", "api_path": "api/tasks/status" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "task::list".to_string(),
        json!({ "http_method": "POST", "api_path": "api/tasks/list" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "task::spawn_workers".to_string(),
        json!({ "http_method": "POST", "api_path": "api/tasks/spawn" }),
        None,
    )?;

    tracing::info!("task-decomposer worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_unique() {
        let a = generate_task_id();
        let b = generate_task_id();
        assert!(a.starts_with("t_"));
        assert!(b.starts_with("t_"));
        assert_ne!(a, b);
    }

    // --- state::list protocol (verified against iii 0.22.1) ---

    #[test]
    fn tasks_are_read_from_a_bare_list() {
        let list = json!([
            { "id": "t_1", "status": "pending", "children": [] },
            { "id": "t_2", "status": "complete", "children": [] }
        ]);
        let tasks = tasks_from_list(&list);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["id"], "t_1");
        assert_eq!(tasks[1]["status"], "complete");
    }

    #[test]
    fn a_task_carrying_its_own_value_field_survives_intact() {
        // The old reader unwrapped `entry["value"]` when present, so any task
        // document with a `value` field was replaced by that field alone.
        let list = json!([{ "id": "t_1", "status": "pending", "value": { "id": "wrong" } }]);
        let tasks = tasks_from_list(&list);
        assert_eq!(tasks[0]["id"], "t_1");
        assert_eq!(tasks[0]["status"], "pending");
    }

    #[test]
    fn a_non_array_list_response_yields_no_tasks() {
        assert!(tasks_from_list(&json!(null)).is_empty());
        assert!(tasks_from_list(&json!({ "entries": [] })).is_empty());
    }

    // --- state::update protocol (verified against iii 0.22.1) ---

    #[test]
    fn claim_payload_uses_ops_not_operations() {
        let payload = claim_task_payload("tasks:root", "t_1", 1_700_000_000_000);
        assert!(
            payload.get("operations").is_none(),
            "`operations` fails the whole invocation with `missing field ops`"
        );
        assert_eq!(payload["scope"], "tasks:root");
        assert_eq!(payload["key"], "t_1");
        assert_eq!(payload["ops"][0]["type"], "set");
        assert_eq!(payload["ops"][0]["path"], "status");
        assert_eq!(payload["ops"][0]["value"], "in_progress");
        assert_eq!(payload["ops"][1]["path"], "updatedAt");
        assert_eq!(payload["ops"][1]["value"], json!(1_700_000_000_000u128));
    }

    #[test]
    fn a_rejected_claim_is_detected_inside_a_successful_response() {
        let engine_result = json!({
            "errors": [{ "code": "set.path_invalid", "op_index": 0 }],
            "new_value": { "status": "pending" },
            "old_value": { "status": "pending" },
        });
        assert!(
            update_rejection(&engine_result)
                .expect("rejection must be reported")
                .contains("set.path_invalid")
        );
        assert_eq!(
            update_rejection(&json!({ "new_value": { "status": "in_progress" } })),
            None
        );
        assert_eq!(update_rejection(&json!({ "errors": [] })), None);
    }
}
