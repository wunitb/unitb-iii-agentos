use agentos_http_adapter::TriggerBus;
use agentos_http_adapter::principal::Principal;
use agentos_http_adapter::{policy, principal};
use iii_sdk::errors::Error;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

mod types;

use types::{ErrorMode, StepMode, StepResult, Workflow, WorkflowStep, sanitize_id};

const MAX_STEP_TIMEOUT_MS: u64 = 3_600_000;
const MAX_STEP_RETRIES: u32 = 10;
const MAX_LOOP_ITERATIONS: u32 = 100;
const STATE_STARTUP_ATTEMPTS: usize = 5;

/// The bus handle every handler takes: the engine client in production, a
/// `FakeBus` in tests. `Arc` because concurrent steps are spawned.
type Bus = Arc<dyn TriggerBus>;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn interpolate(template: &str, vars: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len()
            && bytes[i] == b'{'
            && bytes[i + 1] == b'{'
            && let Some(end) = template[i + 2..].find("}}")
        {
            let key = &template[i + 2..i + 2 + end];
            if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                match vars.get(key) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(other) => out.push_str(&other.to_string()),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                i += 2 + end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn validate_workflow(workflow: &Workflow) -> Result<(), String> {
    if workflow.name.trim().is_empty() {
        return Err("workflow name is required".to_string());
    }
    if workflow.steps.is_empty() {
        return Err("workflow must contain at least one step".to_string());
    }

    let declared_agents = workflow.agents.iter().collect::<HashSet<_>>();
    if declared_agents.len() != workflow.agents.len() {
        return Err("workflow agent IDs must be unique".to_string());
    }

    let mut step_names = HashSet::new();
    let mut concurrent_policy = None;
    for step in &workflow.steps {
        let step_name = step.name.trim();
        if step_name.is_empty() {
            return Err("workflow step name is required".to_string());
        }
        if step_name.len() != step.name.len() {
            return Err(format!(
                "workflow step name has surrounding whitespace: {}",
                step.name
            ));
        }
        if !step_names.insert(step_name) {
            return Err(format!("duplicate workflow step name: {step_name}"));
        }
        if step.timeout_ms == 0 || step.timeout_ms > MAX_STEP_TIMEOUT_MS {
            return Err(format!(
                "step {step_name} timeoutMs must be between 1 and {MAX_STEP_TIMEOUT_MS}"
            ));
        }
        if step.error_mode == ErrorMode::Retry {
            let retries = step.max_retries.unwrap_or(3);
            if retries == 0 || retries > MAX_STEP_RETRIES {
                return Err(format!(
                    "step {step_name} maxRetries must be between 1 and {MAX_STEP_RETRIES}"
                ));
            }
        }
        if step.mode == StepMode::Loop {
            let iterations = step.max_iterations.unwrap_or(10);
            if iterations == 0 || iterations > MAX_LOOP_ITERATIONS {
                return Err(format!(
                    "step {step_name} maxIterations must be between 1 and {MAX_LOOP_ITERATIONS}"
                ));
            }
        }
        if let Some(agent_id) = &step.agent_id
            && !declared_agents.is_empty()
            && !declared_agents.contains(agent_id)
        {
            return Err(format!(
                "step {step_name} references undeclared agent {agent_id}"
            ));
        }
        if let Some(output_var) = &step.output_var
            && (output_var.is_empty()
                || !output_var.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                }))
        {
            return Err(format!(
                "step {step_name} has invalid outputVar {output_var}"
            ));
        }

        if matches!(step.mode, StepMode::Parallel | StepMode::Fanout) {
            let policy = (step.error_mode, step.max_retries);
            if let Some(expected) = concurrent_policy
                && expected != policy
            {
                return Err(
                    "consecutive parallel or fanout steps must use the same error policy"
                        .to_string(),
                );
            }
            concurrent_policy = Some(policy);
        } else {
            concurrent_policy = None;
        }
    }

    validate_step_dependencies(workflow, &step_names)?;
    workflow_execution_order(workflow)?;
    Ok(())
}

fn validate_step_dependencies(
    workflow: &Workflow,
    step_names: &HashSet<&str>,
) -> Result<(), String> {
    for step in &workflow.steps {
        let mut dependencies = HashSet::new();
        for dependency in &step.depends_on {
            if dependency.trim() != dependency {
                return Err(format!(
                    "step {} dependency has surrounding whitespace: {dependency}",
                    step.name
                ));
            }
            if dependency.is_empty() {
                return Err(format!("step {} has an empty dependency", step.name));
            }
            if dependency == &step.name {
                return Err(format!("step {} cannot depend on itself", step.name));
            }
            if !step_names.contains(dependency.as_str()) {
                return Err(format!(
                    "step {} depends on missing step {dependency}",
                    step.name
                ));
            }
            if !dependencies.insert(dependency) {
                return Err(format!(
                    "step {} repeats dependency {dependency}",
                    step.name
                ));
            }
        }
    }
    Ok(())
}

fn workflow_execution_order(workflow: &Workflow) -> Result<Vec<usize>, String> {
    let mut scheduled = vec![false; workflow.steps.len()];
    let mut completed = HashSet::with_capacity(workflow.steps.len());
    let mut order = Vec::with_capacity(workflow.steps.len());

    while order.len() < workflow.steps.len() {
        let mut progressed = false;
        for (index, step) in workflow.steps.iter().enumerate() {
            if scheduled[index]
                || !step
                    .depends_on
                    .iter()
                    .all(|dependency| completed.contains(dependency.as_str()))
            {
                continue;
            }
            scheduled[index] = true;
            completed.insert(step.name.as_str());
            order.push(index);
            progressed = true;
        }
        if !progressed {
            return Err("workflow dependency graph contains a cycle".to_string());
        }
    }

    Ok(order)
}

fn seed_vars(input: Value, agent_id: Option<&str>) -> Map<String, Value> {
    let mut vars = input.as_object().cloned().unwrap_or_default();
    vars.insert("input".to_string(), input);
    if let Some(agent_id) = agent_id {
        vars.insert("agentId".to_string(), Value::String(agent_id.to_string()));
    }
    vars
}

fn initial_step_states(workflow: &Workflow) -> Map<String, Value> {
    workflow
        .steps
        .iter()
        .map(|step| (step.name.clone(), json!({ "status": "pending" })))
        .collect()
}

fn set_step_status(
    states: &mut Map<String, Value>,
    step: &WorkflowStep,
    status: &str,
    error: Option<&str>,
) {
    let mut value = json!({ "status": status, "updatedAt": now_ms() });
    if let Some(error) = error {
        value["error"] = Value::String(error.to_string());
    }
    states.insert(step.name.clone(), value);
}

fn step_payload(
    step: &WorkflowStep,
    vars: &Map<String, Value>,
    fallback_agent_id: Option<&str>,
) -> Result<Value, Error> {
    let template = step.prompt_template.as_deref().unwrap_or("{{input}}");
    let prompt = interpolate(template, vars);
    let mut payload = vars.clone();
    payload.insert("prompt".to_string(), Value::String(prompt.clone()));

    if step.function_id == "agent::chat" {
        let agent_id = step
            .agent_id
            .as_deref()
            .or(fallback_agent_id)
            .ok_or_else(|| {
                Error::Handler(format!(
                    "step {} requires agentId for agent::chat",
                    step.name
                ))
            })?;
        payload.insert("agentId".to_string(), Value::String(agent_id.to_string()));
        payload.insert("message".to_string(), Value::String(prompt));
    }

    Ok(Value::Object(payload))
}

fn function_family(function_id: &str) -> &str {
    function_id.split("::").next().unwrap_or(function_id)
}

/// Whether dispatching `function_id` needs a blocking approval decision.
///
/// The family vocabulary is `agentos_http_adapter::policy` — the single shared
/// definition of contract I1. There is no local delta: `security` and `coder`
/// landed in the shared list on 2026-09-02.
fn requires_approval(function_id: &str) -> bool {
    if function_id.is_empty() {
        return false;
    }
    policy::is_deny_by_default(function_id)
}

/// The agent a step is DECLARED to run as — a candidate, not yet an authority.
///
/// A step is never exempt: the step's own `agentId`, else the `agentId` of the
/// run, else the workflow's recorded `createdBy`. `workflow.agents` is
/// deliberately NOT consulted - it is self-declared by whoever wrote the
/// definition, so honouring it would let a workflow name its own principal.
/// So are all three sources above, which is why the candidate is then BOUND to
/// the caller by [`bind_to_caller`] before anything runs as it.
fn step_principal<'a>(
    step: &'a WorkflowStep,
    workflow: &'a Workflow,
    fallback_agent_id: Option<&'a str>,
) -> Option<&'a str> {
    step.agent_id
        .as_deref()
        .or(fallback_agent_id)
        .or(workflow.created_by.as_deref())
        .filter(|id| !id.is_empty())
}

fn missing_principal_reason(step: &WorkflowStep, workflow: &Workflow) -> String {
    format!(
        "step {} cannot run: {} needs a principal, but the step declares no agentId, \
         the run supplied none and workflow {} records no createdBy",
        step.name, step.function_id, workflow.id
    )
}

fn missing_principal_error(step: &WorkflowStep, workflow: &Workflow) -> Error {
    Error::Handler(missing_principal_reason(step, workflow))
}

/// Who is running or registering this workflow (contract T1): the operator
/// (bearer), or the agent a trusted deputy labelled the call with — which is
/// what a model tool call `workflow::run` becomes. A bare bus payload has no
/// principal to bind to and is refused: `workflow::run` is reachable from the
/// untrusted tier (it is a cron target), and a cron event carries no
/// `workflowId` anyway, so nothing that worked is lost.
fn caller_principal(input: &Value) -> Result<Principal, Error> {
    let expected = agentos_bus_auth::policy::expected_api_key();
    Ok(principal::resolve(input, expected.as_deref())?)
}

/// Bind an agent a definition or run NAMES to the caller (review F1).
///
/// The step's `agentId`, the run's `agentId` and the workflow's `createdBy`
/// are all caller-supplied, and before this the capability check ran AS the
/// named agent and the dispatch was labelled with it — so any caller holding
/// `workflow::*` read any agent's memory through a step. Now the operator may
/// name anyone; an agent principal may name itself, or another agent only when
/// it holds the exact `grant::act_as::<target>`, checked through the same
/// `security::check_capability` reader as every other cross-agent access.
async fn bind_to_caller(iii: &Bus, caller: &Principal, agent_id: &str) -> Result<(), Error> {
    principal::acting_agent(
        iii.as_ref(),
        caller,
        &json!({ "agentId": agent_id }),
        agent_id,
    )
    .await
    .map(|_| ())
}

/// One run's fixed context, threaded through every dispatch site.
struct Run<'a> {
    workflow: &'a Workflow,
    caller: &'a Principal,
    /// The run's `agentId`, already bound to the caller; an agent principal's
    /// run defaults to that agent itself.
    fallback_agent_id: Option<&'a str>,
}

fn payload_digest(payload: &Value) -> String {
    let canonical = serde_json::to_string(payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// `security::check_capability` answers `{"allowed": bool, "reason": string}`.
/// Anything that is not an explicit `true` is a denial, and so is an error.
fn capability_denial(result: &Value) -> Option<String> {
    if result.get("allowed").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(
        result
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("capability denied")
            .to_string(),
    )
}

/// `approval::check` answers `{"decision": "approved"|"denied"|"required", ...}`.
/// Only an explicit `approved` may dispatch.
fn approval_denial(result: &Value) -> Option<String> {
    let decision = result
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("required");
    if decision == "approved" {
        return None;
    }
    let reason = result
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("approval not granted");
    let request_id = result
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("-");
    Some(format!("{decision}: {reason} (requestId {request_id})"))
}

async fn check_capability(iii: &Bus, agent_id: &str, function_id: &str) -> Result<(), Error> {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "security::check_capability".to_string(),
            payload: json!({
                "agentId": agent_id,
                "functionId": function_id,
                "capability": function_family(function_id),
                "resource": function_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        // Fail closed: an unreachable capability worker is a denial.
        .map_err(|error| {
            Error::Handler(format!(
                "capability check for {agent_id} -> {function_id} failed: {error}"
            ))
        })?;
    match capability_denial(&result) {
        None => Ok(()),
        Some(reason) => Err(Error::Handler(format!(
            "{agent_id} is not allowed to call {function_id}: {reason}"
        ))),
    }
}

/// What the gate decided about a step before any remote check is made.
#[derive(Debug, PartialEq, Eq)]
enum StepAuthorization<'a> {
    /// No principal could be resolved, so the step must not be dispatched.
    Refused(String),
    /// The step runs as `agent_id`, subject to the capability check and, when
    /// `needs_approval` is set, a blocking approval decision.
    Checked {
        agent_id: &'a str,
        needs_approval: bool,
    },
}

/// The local half of the authorization decision, with no remote calls, so the
/// rule "a step is never exempt" is testable on its own.
fn plan_step_authorization<'a>(
    step: &'a WorkflowStep,
    workflow: &'a Workflow,
    fallback_agent_id: Option<&'a str>,
) -> StepAuthorization<'a> {
    match step_principal(step, workflow, fallback_agent_id) {
        None => StepAuthorization::Refused(missing_principal_reason(step, workflow)),
        Some(agent_id) => StepAuthorization::Checked {
            agent_id,
            needs_approval: requires_approval(&step.function_id),
        },
    }
}

/// The payload a step is dispatched with: the interpolated variables, labelled
/// with the step's principal for the families that resolve one (contract T1).
///
/// The label OVERWRITES anything the definition or the variables put there —
/// a workflow can no more name its own principal in a payload than in
/// `workflow.agents` (see `step_principal`).
fn dispatch_payload(step: &WorkflowStep, payload: Value, agent_id: &str) -> Value {
    principal::attach_agent(&step.function_id, payload, agent_id)
}

/// Authorize one step immediately before it is dispatched, and answer the
/// principal it runs as.
///
/// The workflow worker holds a trusted engine session, so a step id reaches the
/// bus with the worker's own authority. Every dispatch site must pass through
/// here, or the definition itself becomes the authorization. Order: bind the
/// declared agent to the caller, then the agent's capability for the step id,
/// then (deny-by-default families) a blocking approval decision.
async fn authorize_step(
    iii: &Bus,
    run: &Run<'_>,
    step: &WorkflowStep,
    payload: &Value,
) -> Result<String, Error> {
    let workflow = run.workflow;
    let (agent_id, needs_approval) =
        match plan_step_authorization(step, workflow, run.fallback_agent_id) {
            StepAuthorization::Refused(reason) => return Err(Error::Handler(reason)),
            StepAuthorization::Checked {
                agent_id,
                needs_approval,
            } => (agent_id, needs_approval),
        };

    bind_to_caller(iii, run.caller, agent_id).await?;
    check_capability(iii, agent_id, &step.function_id).await?;

    if !needs_approval {
        return Ok(agent_id.to_string());
    }

    let result = iii
        .trigger(TriggerRequest {
            function_id: "approval::check".to_string(),
            payload: json!({
                "agentId": agent_id,
                "functionId": step.function_id,
                "payloadDigest": payload_digest(payload),
                "reason": format!(
                    "workflow {} step {} calls {}",
                    workflow.id, step.name, step.function_id
                ),
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        // Fail closed: an unreachable approval worker is a denial.
        .map_err(|error| {
            Error::Handler(format!(
                "approval check for {agent_id} -> {} failed: {error}",
                step.function_id
            ))
        })?;

    match approval_denial(&result) {
        None => Ok(agent_id.to_string()),
        Some(reason) => Err(Error::Handler(format!(
            "{agent_id} needs approval to call {}: {reason}",
            step.function_id
        ))),
    }
}

async fn trigger_step(
    iii: &Bus,
    run: &Run<'_>,
    step: &WorkflowStep,
    vars: &Map<String, Value>,
) -> Result<Value, Error> {
    let payload = step_payload(step, vars, run.fallback_agent_id)?;
    let agent_id = authorize_step(iii, run, step, &payload).await?;
    iii.trigger(TriggerRequest {
        function_id: step.function_id.clone(),
        payload: dispatch_payload(step, payload, &agent_id),
        action: None,
        timeout_ms: Some(step.timeout_ms),
    })
    .await
    .map_err(|error| Error::Handler(error.to_string()))
}

async fn state_get(iii: &Bus, scope: &str, key: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".to_string(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn state_set(iii: &Bus, scope: &str, key: &str, value: Value) -> Result<(), Error> {
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

/// A rejected operation still answers success at the transport level: the
/// engine reports it inside an `errors` array of an otherwise normal result,
/// so a `state::update` that changed nothing looks like a clean write.
fn update_rejection(result: &Value) -> Option<String> {
    let errors = result.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }
    Some(Value::Array(errors.clone()).to_string())
}

async fn state_update(iii: &Bus, scope: &str, key: &str, ops: Value) -> Result<(), Error> {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::update".to_string(),
            payload: json!({ "scope": scope, "key": key, "ops": ops }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    if let Some(rejection) = update_rejection(&result) {
        return Err(Error::Handler(format!(
            "state::update rejected for {scope}/{key}: {rejection}"
        )));
    }
    Ok(())
}

async fn mark_run_failed(
    iii: &Bus,
    run_id: &str,
    error: &str,
    results: &[StepResult],
    vars: &Map<String, Value>,
    step_states: &Map<String, Value>,
    next_step: usize,
) -> Result<(), Error> {
    state_update(
        iii,
        "workflow_runs",
        run_id,
        json!([
            { "type": "set", "path": "status", "value": "failed" },
            { "type": "set", "path": "failedAt", "value": now_ms() },
            { "type": "set", "path": "error", "value": error },
            { "type": "set", "path": "results", "value": results },
            { "type": "set", "path": "vars", "value": vars },
            { "type": "set", "path": "nextStep", "value": next_step },
            { "type": "set", "path": "stepStates", "value": step_states },
        ]),
    )
    .await
}

/// `workflow::create` over the bus: the caller's principal is resolved first
/// (a bare payload is refused) and an agent principal is recorded as the
/// creator whatever the definition says — `createdBy` is the last-resort
/// principal of a step, and a definition must not choose it.
async fn create_workflow(iii: &Bus, input: Value) -> Result<Value, Error> {
    let created_by = match caller_principal(&input) {
        Ok(Principal::Agent(agent)) => Some(agent),
        Ok(Principal::Operator) => None,
        Err(error) => return Err(error),
    };
    store_workflow(iii, input, created_by.as_deref()).await
}

/// Validate and store a definition. `created_by` overrides the definition's
/// own `createdBy`; `None` keeps what it says (the operator, or the in-process
/// startup loader that reads the bundled YAML files).
async fn store_workflow(iii: &Bus, input: Value, created_by: Option<&str>) -> Result<Value, Error> {
    let mut workflow_value = input;
    let object = workflow_value
        .as_object_mut()
        .ok_or_else(|| Error::Handler("workflow definition must be an object".to_string()))?;
    if let Some(agent) = created_by {
        object.insert("createdBy".to_string(), Value::String(agent.to_string()));
    }
    if object
        .get("id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        object.insert(
            "id".to_string(),
            Value::String(uuid::Uuid::new_v4().to_string()),
        );
    }

    // Record the creator when the caller names one. It is the last-resort
    // principal for a step that declares no agentId; without it such a step is
    // refused rather than dispatched with the worker's own authority.
    if object
        .get("createdBy")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(agent_id) = object
            .get("agentId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    {
        object.insert("createdBy".to_string(), Value::String(agent_id));
    }

    let workflow: Workflow = serde_json::from_value(workflow_value)
        .map_err(|error| Error::Handler(format!("invalid workflow definition: {error}")))?;
    validate_workflow(&workflow).map_err(Error::Handler)?;
    let id = workflow.id.clone();
    let mut stored =
        serde_json::to_value(workflow).map_err(|error| Error::Handler(error.to_string()))?;
    stored
        .as_object_mut()
        .expect("Workflow serializes as an object")
        .insert("createdAt".to_string(), json!(now_ms()));

    state_set(iii, "workflows", &id, stored).await?;
    Ok(json!({ "id": id }))
}

fn record_step_result(
    step: &WorkflowStep,
    output: Value,
    vars: &mut Map<String, Value>,
    results: &mut Vec<StepResult>,
    start_ms: u128,
) {
    if let Some(var) = &step.output_var {
        vars.insert(var.clone(), output.clone());
    }
    vars.insert("input".to_string(), output.clone());
    results.push(StepResult {
        step_name: step.name.clone(),
        output,
        duration_ms: now_ms().saturating_sub(start_ms),
        error: None,
    });
}

fn concurrent_group_end(workflow: &Workflow, index: usize, completed: &HashSet<String>) -> usize {
    let mode = workflow.steps[index].mode;
    if !matches!(mode, StepMode::Parallel | StepMode::Fanout) {
        return index;
    }

    let mut end = index;
    for candidate in workflow.steps.iter().skip(index + 1) {
        if candidate.mode != mode
            || !candidate
                .depends_on
                .iter()
                .all(|dependency| completed.contains(dependency))
        {
            break;
        }
        end += 1;
    }
    end
}

#[expect(
    clippy::too_many_arguments,
    reason = "step execution keeps shared workflow state explicit"
)]
async fn run_step(
    iii: &Bus,
    run: &Run<'_>,
    step: &WorkflowStep,
    vars: &mut Map<String, Value>,
    results: &mut Vec<StepResult>,
    start_ms: u128,
    i: &mut usize,
    completed: &HashSet<String>,
) -> Result<(), Error> {
    let workflow = run.workflow;
    match step.mode {
        StepMode::Sequential => {
            let output = trigger_step(iii, run, step, vars).await?;
            record_step_result(step, output, vars, results, start_ms);
        }
        StepMode::Parallel | StepMode::Fanout => {
            let group_end = concurrent_group_end(workflow, *i, completed);
            let concurrent_steps = &workflow.steps[*i..=group_end];
            let mut handles = Vec::with_capacity(concurrent_steps.len());
            for concurrent_step in concurrent_steps {
                let payload = step_payload(concurrent_step, vars, run.fallback_agent_id)?;
                // This branch dispatches without going through `trigger_step`,
                // so it has to authorize the step itself.
                let agent_id = authorize_step(iii, run, concurrent_step, &payload).await?;
                let payload = dispatch_payload(concurrent_step, payload, &agent_id);
                let iii = Arc::clone(iii);
                let function_id = concurrent_step.function_id.clone();
                let timeout_ms = concurrent_step.timeout_ms;
                handles.push(tokio::spawn(async move {
                    iii.trigger(TriggerRequest {
                        function_id,
                        payload,
                        action: None,
                        timeout_ms: Some(timeout_ms),
                    })
                    .await
                    .map_err(|error| Error::Handler(error.to_string()))
                }));
            }

            let mut concurrent_results = Vec::with_capacity(handles.len());
            for handle in handles {
                concurrent_results.push(
                    handle
                        .await
                        .map_err(|error| Error::Handler(error.to_string()))??,
                );
            }

            let result_key = if step.mode == StepMode::Fanout {
                "__fanout"
            } else {
                "__parallel"
            };
            vars.insert(
                result_key.to_string(),
                Value::Array(concurrent_results.clone()),
            );
            for (concurrent_step, output) in concurrent_steps.iter().zip(concurrent_results) {
                if let Some(var) = &concurrent_step.output_var {
                    vars.insert(var.clone(), output.clone());
                }
                results.push(StepResult {
                    step_name: concurrent_step.name.clone(),
                    output,
                    duration_ms: now_ms().saturating_sub(start_ms),
                    error: None,
                });
            }
            *i = group_end;
        }
        StepMode::Collect => {
            let mut collect_vars = vars.clone();
            collect_vars.insert(
                "fanoutResults".to_string(),
                vars.get("__fanout").cloned().unwrap_or(Value::Null),
            );
            let output = trigger_step(iii, run, step, &collect_vars).await?;
            record_step_result(step, output, vars, results, start_ms);
        }
        StepMode::Conditional => {
            let previous = vars
                .get("input")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_default();
            if step.condition.as_ref().is_some_and(|condition| {
                !previous.to_lowercase().contains(&condition.to_lowercase())
            }) {
                results.push(StepResult {
                    step_name: step.name.clone(),
                    output: Value::String("skipped".to_string()),
                    duration_ms: now_ms().saturating_sub(start_ms),
                    error: None,
                });
                return Ok(());
            }
            let output = trigger_step(iii, run, step, vars).await?;
            record_step_result(step, output, vars, results, start_ms);
        }
        StepMode::Loop => {
            let mut loop_output = Value::Null;
            for iteration in 0..step.max_iterations.unwrap_or(10) {
                let mut iteration_vars = vars.clone();
                iteration_vars.insert("iteration".to_string(), json!(iteration));
                loop_output = trigger_step(iii, run, step, &iteration_vars).await?;
                if let Some(var) = &step.output_var {
                    vars.insert(var.clone(), loop_output.clone());
                }
                vars.insert("input".to_string(), loop_output.clone());

                if step.until.as_ref().is_some_and(|until| {
                    let output = loop_output
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| loop_output.to_string());
                    output.to_lowercase().contains(&until.to_lowercase())
                }) {
                    break;
                }
            }
            record_step_result(step, loop_output, vars, results, start_ms);
        }
    }
    Ok(())
}

/// Pre-flight authorization for a whole run, so a definition that cannot be
/// authorized is refused before a run row is written.
///
/// This does NOT replace `authorize_step`: the approval decision depends on the
/// resolved payload, which only exists at dispatch time, and a step reached by
/// any other path must still be checked.
async fn authorize_workflow(iii: &Bus, run: &Run<'_>) -> Result<(), Error> {
    let workflow = run.workflow;
    let mut checked = HashSet::new();
    for step in &workflow.steps {
        if step.function_id == "agent::chat"
            && step.agent_id.as_deref().or(run.fallback_agent_id).is_none()
        {
            return Err(Error::Handler(format!(
                "step {} requires agentId for agent::chat",
                step.name
            )));
        }
        // A step without a resolvable principal is refused, never skipped: the
        // workflow worker dispatches from its own trusted session, so an
        // unchecked step would run with the worker's authority.
        let Some(agent_id) = step_principal(step, workflow, run.fallback_agent_id) else {
            return Err(missing_principal_error(step, workflow));
        };
        if !checked.insert((agent_id, step.function_id.as_str())) {
            continue;
        }
        bind_to_caller(iii, run.caller, agent_id).await?;
        check_capability(iii, agent_id, &step.function_id).await?;
    }
    Ok(())
}

async fn run_workflow(iii: &Bus, input: Value) -> Result<Value, Error> {
    // Before any state is read: a run with nobody to bind to is refused.
    let caller = caller_principal(&input)?;
    let workflow_id = input
        .get("workflowId")
        .or_else(|| input.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("workflowId is required".into()))?;
    let safe_workflow_id = sanitize_id(workflow_id).map_err(Error::Handler)?;
    let agent_id = input
        .get("agentId")
        .and_then(Value::as_str)
        .map(sanitize_id)
        .transpose()
        .map_err(Error::Handler)?;
    // The run's agent is bound to the caller up front — it seeds `vars.agentId`
    // and the run row, not only step principals. An agent's run is its own.
    let agent_id = match (&caller, agent_id) {
        (Principal::Agent(agent), None) => Some(agent.clone()),
        (_, named) => named,
    };
    if let Some(agent) = agent_id.as_deref() {
        bind_to_caller(iii, &caller, agent).await?;
    }
    let user_input = input.get("input").cloned().unwrap_or(Value::Null);

    let workflow_value = state_get(iii, "workflows", &safe_workflow_id).await?;
    if workflow_value.is_null() {
        return Err(Error::Handler(format!(
            "Workflow {safe_workflow_id} not found"
        )));
    }
    let mut workflow: Workflow = serde_json::from_value(workflow_value)
        .map_err(|error| Error::Handler(format!("invalid workflow definition: {error}")))?;
    validate_workflow(&workflow).map_err(Error::Handler)?;
    let execution_order = workflow_execution_order(&workflow).map_err(Error::Handler)?;
    workflow.steps = execution_order
        .into_iter()
        .map(|index| workflow.steps[index].clone())
        .collect();
    let run = Run {
        workflow: &workflow,
        caller: &caller,
        fallback_agent_id: agent_id.as_deref(),
    };
    authorize_workflow(iii, &run).await?;

    let run_id = uuid::Uuid::new_v4().to_string();
    let safe_run_id = sanitize_id(&run_id).map_err(Error::Handler)?;
    let mut vars = seed_vars(user_input.clone(), agent_id.as_deref());

    let mut results: Vec<StepResult> = Vec::new();
    let mut completed = HashSet::with_capacity(workflow.steps.len());
    let mut step_states = initial_step_states(&workflow);

    state_set(
        iii,
        "workflow_runs",
        &safe_run_id,
        json!({
            "runId": safe_run_id,
            "workflowId": safe_workflow_id,
            "agentId": agent_id,
            "input": user_input,
            "vars": vars,
            "results": results,
            "stepStates": step_states,
            "nextStep": 0,
            "status": "running",
            "startedAt": now_ms(),
        }),
    )
    .await?;

    let mut i = 0;
    while i < workflow.steps.len() {
        let step = workflow.steps[i].clone();
        let start_ms = now_ms();
        let batch_start = i;
        let batch_end = concurrent_group_end(&workflow, i, &completed);
        for batch_step in &workflow.steps[i..=batch_end] {
            set_step_status(&mut step_states, batch_step, "running", None);
        }
        state_update(
            iii,
            "workflow_runs",
            &safe_run_id,
            json!([
                { "type": "set", "path": "stepStates", "value": step_states },
                { "type": "set", "path": "nextStep", "value": i },
            ]),
        )
        .await?;

        let step_outcome = run_step(
            iii,
            &run,
            &step,
            &mut vars,
            &mut results,
            start_ms,
            &mut i,
            &completed,
        )
        .await;

        if let Err(err) = step_outcome {
            let err_msg = err.to_string();
            match step.error_mode {
                ErrorMode::Skip => {
                    results.push(StepResult {
                        step_name: step.name.clone(),
                        output: Value::Null,
                        duration_ms: now_ms().saturating_sub(start_ms),
                        error: Some(err_msg),
                    });
                }
                ErrorMode::Retry => {
                    let max_retries = step.max_retries.unwrap_or(3);
                    let mut last_error = err;
                    let mut retried = false;
                    for _ in 0..max_retries {
                        let vars_snapshot = vars.clone();
                        let results_len = results.len();
                        let i_snapshot = i;
                        let retry_start_ms = now_ms();
                        match run_step(
                            iii,
                            &run,
                            &step,
                            &mut vars,
                            &mut results,
                            retry_start_ms,
                            &mut i,
                            &completed,
                        )
                        .await
                        {
                            Ok(()) => {
                                retried = true;
                                break;
                            }
                            Err(error) => {
                                last_error = error;
                                vars = vars_snapshot;
                                results.truncate(results_len);
                                i = i_snapshot;
                            }
                        }
                    }
                    if !retried {
                        let error = last_error.to_string();
                        set_step_status(&mut step_states, &step, "failed", Some(&error));
                        mark_run_failed(
                            iii,
                            &safe_run_id,
                            &error,
                            &results,
                            &vars,
                            &step_states,
                            i,
                        )
                        .await?;
                        return Err(last_error);
                    }
                }
                ErrorMode::Fail => {
                    set_step_status(&mut step_states, &step, "failed", Some(&err_msg));
                    mark_run_failed(
                        iii,
                        &safe_run_id,
                        &err_msg,
                        &results,
                        &vars,
                        &step_states,
                        i,
                    )
                    .await?;
                    return Err(err);
                }
            }
        }

        for completed_step in &workflow.steps[batch_start..=i] {
            completed.insert(completed_step.name.clone());
            let result = results
                .iter()
                .rev()
                .find(|result| result.step_name == completed_step.name);
            let status = if result.is_some_and(|result| result.error.is_some()) {
                "skipped"
            } else {
                "completed"
            };
            set_step_status(&mut step_states, completed_step, status, None);
        }

        i += 1;
        state_update(
            iii,
            "workflow_runs",
            &safe_run_id,
            json!([
                { "type": "set", "path": "results", "value": results },
                { "type": "set", "path": "vars", "value": vars },
                { "type": "set", "path": "nextStep", "value": i },
                { "type": "set", "path": "stepStates", "value": step_states },
            ]),
        )
        .await?;
    }

    state_update(
        iii,
        "workflow_runs",
        &safe_run_id,
        json!([
            { "type": "set", "path": "status", "value": "completed" },
            { "type": "set", "path": "completedAt", "value": now_ms() },
            { "type": "set", "path": "results", "value": results },
            { "type": "set", "path": "vars", "value": vars },
            { "type": "set", "path": "nextStep", "value": workflow.steps.len() },
            { "type": "set", "path": "stepStates", "value": step_states },
        ]),
    )
    .await?;

    Ok::<Value, Error>(json!({
        "runId": safe_run_id,
        "results": results,
        "vars": vars,
        "stepStates": step_states,
    }))
}

async fn list_workflows(iii: &Bus) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({ "scope": "workflows" }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn get_workflow(iii: &Bus, workflow_id: &str) -> Result<Value, Error> {
    let safe_workflow_id = sanitize_id(workflow_id).map_err(Error::Handler)?;
    let workflow = state_get(iii, "workflows", &safe_workflow_id).await?;
    if workflow.is_null() {
        return Err(Error::Handler(format!(
            "Workflow {safe_workflow_id} not found"
        )));
    }
    Ok(workflow)
}

fn safe_pagination(limit: Option<i64>, offset: Option<i64>) -> (usize, usize) {
    let limit = limit.unwrap_or(50).clamp(1, 500) as usize;
    let offset = offset.unwrap_or(0).max(0) as usize;
    (limit, offset)
}

fn input_i64(input: &Value, key: &str) -> Option<i64> {
    input
        .get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn workflow_run_page(
    all: Value,
    workflow_id: &str,
    limit: usize,
    offset: usize,
) -> (Vec<Value>, usize) {
    let matching: Vec<Value> = all
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|run| run.get("workflowId").and_then(Value::as_str) == Some(workflow_id))
        .collect();
    let total = matching.len();
    let page = matching.into_iter().skip(offset).take(limit).collect();
    (page, total)
}

async fn list_runs(iii: &Bus, input: Value) -> Result<Value, Error> {
    let workflow_id = input
        .get("workflowId")
        .or_else(|| input.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("workflowId is required".into()))?;
    let safe_workflow_id = sanitize_id(workflow_id).map_err(Error::Handler)?;

    let (limit, offset) = safe_pagination(input_i64(&input, "limit"), input_i64(&input, "offset"));

    let all = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": "workflow_runs" }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let (filtered, total) = workflow_run_page(all, &safe_workflow_id, limit, offset);

    Ok::<Value, Error>(json!({
        "runs": filtered,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

async fn get_run_state(iii: &Bus, run_id: &str) -> Result<Value, Error> {
    let safe_run_id = sanitize_id(run_id).map_err(Error::Handler)?;
    let run = state_get(iii, "workflow_runs", &safe_run_id).await?;
    if run.is_null() {
        return Err(Error::Handler(format!(
            "Workflow run {safe_run_id} not found"
        )));
    }
    Ok(run)
}

fn workflow_directory() -> Result<PathBuf, Error> {
    if let Some(path) = std::env::var_os("AGENTOS_WORKFLOWS_DIR") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Ok(path);
        }
        return Err(Error::Handler(format!(
            "AGENTOS_WORKFLOWS_DIR is not a directory: {}",
            path.display()
        )));
    }

    let mut candidates = Vec::new();
    if let Ok(current_dir) = std::env::current_dir() {
        candidates.push(current_dir.join("workflows"));
        candidates.push(current_dir.join("runtime/workflows"));
        candidates.push(current_dir.join("../workflows"));
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(bin_dir) = executable.parent()
    {
        candidates.push(bin_dir.join("../workflows"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../workflows"));

    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| Error::Handler("bundled workflows directory not found".to_string()))
}

fn read_workflow_definitions(directory: &std::path::Path) -> Result<Vec<Workflow>, Error> {
    let mut paths = std::fs::read_dir(directory)
        .map_err(|error| Error::Handler(format!("{}: {error}", directory.display())))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "yaml" | "yml"))
        })
        .collect::<Vec<_>>();
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let content = std::fs::read_to_string(&path)
                .map_err(|error| Error::Handler(format!("{}: {error}", path.display())))?;
            let workflow: Workflow = serde_yaml::from_str(&content)
                .map_err(|error| Error::Handler(format!("{}: {error}", path.display())))?;
            validate_workflow(&workflow)
                .map_err(|error| Error::Handler(format!("{}: {error}", path.display())))?;
            Ok(workflow)
        })
        .collect()
}

async fn load_workflows_from_directory(
    iii: &Bus,
    directory: &std::path::Path,
) -> Result<usize, Error> {
    let workflows = read_workflow_definitions(directory)?;
    if workflows.is_empty() {
        return Err(Error::Handler(format!(
            "no workflow definitions found in {}",
            directory.display()
        )));
    }

    let mut last_error = None;
    for attempt in 0..STATE_STARTUP_ATTEMPTS {
        let mut loaded = 0;
        for workflow in &workflows {
            let input = serde_json::to_value(workflow)
                .map_err(|error| Error::Handler(error.to_string()))?;
            // In-process and bare: the bundled definitions are the operator's,
            // read from disk, and record whatever `createdBy` they declare.
            match store_workflow(iii, input, None).await {
                Ok(_) => loaded += 1,
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }
        if loaded == workflows.len() {
            return Ok(loaded);
        }
        if attempt + 1 < STATE_STARTUP_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
        }
    }

    Err(last_error
        .unwrap_or_else(|| Error::Handler("workflow state unavailable during startup".to_string())))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let bus: Bus = Arc::new(iii.clone());

    let bus_clone = Arc::clone(&bus);
    iii.register_function(
        "workflow::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = Arc::clone(&bus_clone);
            async move { create_workflow(&iii, input).await }
        })
        .description("Register a workflow definition"),
    );

    let bus_clone = Arc::clone(&bus);
    iii.register_function(
        "workflow::run",
        RegisterFunction::new_async(move |input: Value| {
            let iii = Arc::clone(&bus_clone);
            async move { run_workflow(&iii, input).await }
        })
        .description("Execute a workflow"),
    );

    let bus_clone = Arc::clone(&bus);
    iii.register_function(
        "workflow::list",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = Arc::clone(&bus_clone);
            async move { list_workflows(&iii).await }
        })
        .description("List all workflows"),
    );

    let bus_clone = Arc::clone(&bus);
    iii.register_function(
        "workflow::get",
        RegisterFunction::new_async(move |input: Value| {
            let iii = Arc::clone(&bus_clone);
            async move {
                let workflow_id = input
                    .get("workflowId")
                    .or_else(|| input.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Handler("workflowId is required".into()))?;
                get_workflow(&iii, workflow_id).await
            }
        })
        .description("Read a workflow by ID"),
    );

    let bus_clone = Arc::clone(&bus);
    iii.register_function(
        "workflow::runs",
        RegisterFunction::new_async(move |input: Value| {
            let iii = Arc::clone(&bus_clone);
            async move { list_runs(&iii, input).await }
        })
        .description("List runs for a workflow"),
    );

    let bus_clone = Arc::clone(&bus);
    iii.register_function(
        "workflow::get_run_state",
        RegisterFunction::new_async(move |input: Value| {
            let iii = Arc::clone(&bus_clone);
            async move {
                let run_id = input
                    .get("runId")
                    .or_else(|| input.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Handler("runId is required".into()))?;
                get_run_state(&iii, run_id).await
            }
        })
        .description("Read a workflow run by ID"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "workflow::run".to_string(),
        json!({ "http_method": "POST", "api_path": "api/workflows/:id/run" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "workflow::create".to_string(),
        json!({ "http_method": "POST", "api_path": "api/workflows" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "workflow::list".to_string(),
        json!({ "http_method": "GET", "api_path": "api/workflows" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "workflow::get".to_string(),
        json!({ "http_method": "GET", "api_path": "api/workflows/:id" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "workflow::runs".to_string(),
        json!({ "http_method": "GET", "api_path": "api/workflows/:id/runs" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "workflow::get_run_state".to_string(),
        json!({ "http_method": "GET", "api_path": "api/workflow-runs/:id" }),
        None,
    )?;

    let workflow_dir = workflow_directory()?;
    let loaded_workflows = load_workflows_from_directory(&bus, &workflow_dir).await?;
    tracing::info!(
        count = loaded_workflows,
        directory = %workflow_dir.display(),
        "bundled workflows loaded"
    );

    tracing::info!("workflow worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_replaces_string() {
        let mut vars = Map::new();
        vars.insert("input".into(), Value::String("hello".into()));
        assert_eq!(interpolate("hi {{input}}!", &vars), "hi hello!");
    }

    #[test]
    fn interpolate_keeps_unknown() {
        let vars = Map::new();
        assert_eq!(interpolate("{{missing}}", &vars), "{{missing}}");
    }

    #[test]
    fn interpolate_serializes_non_string() {
        let mut vars = Map::new();
        vars.insert("count".into(), json!(42));
        assert_eq!(interpolate("n={{count}}", &vars), "n=42");
    }

    #[test]
    fn safe_pagination_clamps() {
        assert_eq!(safe_pagination(Some(0), Some(-5)), (1, 0));
        assert_eq!(safe_pagination(Some(10000), Some(20)), (500, 20));
        assert_eq!(safe_pagination(None, None), (50, 0));
    }

    #[test]
    fn input_i64_accepts_http_query_strings() {
        let input = json!({ "limit": "5", "offset": 2, "invalid": "x" });
        assert_eq!(input_i64(&input, "limit"), Some(5));
        assert_eq!(input_i64(&input, "offset"), Some(2));
        assert_eq!(input_i64(&input, "invalid"), None);
    }

    #[test]
    fn workflow_run_page_filters_raw_state_values() {
        let all = json!([
            { "runId": "a-1", "workflowId": "a" },
            { "runId": "b-1", "workflowId": "b" },
            { "runId": "a-2", "workflowId": "a" },
        ]);
        let (page, total) = workflow_run_page(all, "a", 1, 1);
        assert_eq!(total, 2);
        assert_eq!(page, vec![json!({ "runId": "a-2", "workflowId": "a" })]);
    }

    #[test]
    fn bundled_workflows_parse_and_bind_every_agent_step() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../workflows");
        let workflows = read_workflow_definitions(&directory).unwrap();
        assert_eq!(workflows.len(), 3);
        for workflow in workflows {
            for step in workflow
                .steps
                .iter()
                .filter(|step| step.function_id == "agent::chat")
            {
                let agent_id = step
                    .agent_id
                    .as_ref()
                    .expect("agent step must bind agentId");
                assert!(workflow.agents.contains(agent_id));
            }
        }
    }

    #[test]
    fn agent_step_payload_uses_interpolated_message_and_step_agent() {
        let workflow: Workflow = serde_json::from_value(json!({
            "id": "wf",
            "name": "workflow",
            "description": "test",
            "agents": ["architect"],
            "steps": [{
                "name": "spec",
                "functionId": "agent::chat",
                "agentId": "architect",
                "promptTemplate": "Design {{goal}}",
                "mode": "sequential",
                "errorMode": "fail",
                "timeoutMs": 1000
            }]
        }))
        .unwrap();
        let vars = seed_vars(json!({ "goal": "a cache" }), Some("fallback"));
        let payload = step_payload(&workflow.steps[0], &vars, Some("fallback")).unwrap();
        assert_eq!(payload["agentId"], "architect");
        assert_eq!(payload["message"], "Design a cache");
        assert_eq!(payload["prompt"], "Design a cache");
    }

    #[test]
    fn validation_rejects_unbounded_step_controls() {
        let workflow: Workflow = serde_json::from_value(json!({
            "id": "wf",
            "name": "workflow",
            "description": "test",
            "steps": [{
                "name": "loop",
                "functionId": "echo::run",
                "mode": "loop",
                "errorMode": "fail",
                "timeoutMs": 3600001,
                "maxIterations": 101
            }]
        }))
        .unwrap();
        assert!(validate_workflow(&workflow).is_err());
    }

    #[test]
    fn validation_rejects_missing_dependency() {
        let workflow: Workflow = serde_json::from_value(json!({
            "id": "wf",
            "name": "workflow",
            "description": "test",
            "steps": [{
                "name": "deploy",
                "functionId": "echo::run",
                "dependsOn": ["build"],
                "mode": "sequential",
                "errorMode": "fail",
                "timeoutMs": 1000
            }]
        }))
        .unwrap();

        assert_eq!(
            validate_workflow(&workflow).unwrap_err(),
            "step deploy depends on missing step build"
        );
    }

    #[test]
    fn validation_rejects_dependency_cycle() {
        let workflow: Workflow = serde_json::from_value(json!({
            "id": "wf",
            "name": "workflow",
            "description": "test",
            "steps": [
                {
                    "name": "build",
                    "functionId": "echo::run",
                    "dependsOn": ["test"],
                    "mode": "sequential",
                    "errorMode": "fail",
                    "timeoutMs": 1000
                },
                {
                    "name": "test",
                    "functionId": "echo::run",
                    "dependsOn": ["build"],
                    "mode": "sequential",
                    "errorMode": "fail",
                    "timeoutMs": 1000
                }
            ]
        }))
        .unwrap();

        assert_eq!(
            validate_workflow(&workflow).unwrap_err(),
            "workflow dependency graph contains a cycle"
        );
    }

    #[test]
    fn dependency_order_moves_prerequisites_before_dependents() {
        let workflow: Workflow = serde_json::from_value(json!({
            "id": "wf",
            "name": "workflow",
            "description": "test",
            "steps": [
                {
                    "name": "deploy",
                    "functionId": "echo::run",
                    "dependsOn": ["build"],
                    "mode": "sequential",
                    "errorMode": "fail",
                    "timeoutMs": 1000
                },
                {
                    "name": "build",
                    "functionId": "echo::run",
                    "mode": "sequential",
                    "errorMode": "fail",
                    "timeoutMs": 1000
                }
            ]
        }))
        .unwrap();

        assert_eq!(workflow_execution_order(&workflow).unwrap(), vec![1, 0]);
    }

    // --- state protocol (verified against iii 0.22.1) ---

    #[test]
    fn a_rejected_update_is_detected_inside_a_successful_response() {
        // `state::update` answers 200 with an `errors` array when an operation
        // is refused, so a run marked failed could silently stay "running".
        let engine_result = json!({
            "errors": [{ "code": "set.path_invalid", "op_index": 2 }],
            "new_value": { "status": "running" },
            "old_value": { "status": "running" },
        });
        assert!(
            update_rejection(&engine_result)
                .expect("rejection must be reported")
                .contains("set.path_invalid")
        );
    }

    #[test]
    fn a_clean_update_reports_no_rejection() {
        assert_eq!(
            update_rejection(&json!({ "new_value": { "status": "failed" } })),
            None
        );
        assert_eq!(
            update_rejection(&json!({ "new_value": {}, "errors": [] })),
            None
        );
    }

    #[test]
    fn runs_are_paged_from_a_bare_list() {
        // `state::list` answers a bare array of stored values: no `{key,
        // value}` envelope, so a run document is filtered as it arrives.
        let all = json!([
            { "runId": "r1", "workflowId": "wf-1" },
            { "runId": "r2", "workflowId": "wf-2" },
            { "runId": "r3", "workflowId": "wf-1" }
        ]);
        let (page, total) = workflow_run_page(all, "wf-1", 10, 0);
        assert_eq!(total, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0]["runId"], "r1");
        assert_eq!(page[1]["runId"], "r3");
    }

    #[test]
    fn the_envelope_this_worker_never_receives_matches_no_run() {
        let enveloped = json!([{ "key": "r1", "value": { "workflowId": "wf-1" } }]);
        let (page, total) = workflow_run_page(enveloped, "wf-1", 10, 0);
        assert_eq!(total, 0);
        assert!(page.is_empty());
    }

    // --- bus authorization (review finding H-5) ---

    fn step_with(name: &str, function_id: &str, agent_id: Option<&str>) -> WorkflowStep {
        serde_json::from_value(json!({
            "name": name,
            "functionId": function_id,
            "agentId": agent_id,
            "mode": "sequential",
            "errorMode": "fail",
            "timeoutMs": 1000,
        }))
        .expect("step fixture")
    }

    fn workflow_with(steps: Vec<WorkflowStep>, created_by: Option<&str>) -> Workflow {
        Workflow {
            id: "wf-1".into(),
            name: "wf".into(),
            description: "d".into(),
            created_by: created_by.map(str::to_string),
            agents: vec![],
            steps,
        }
    }

    #[test]
    fn a_memory_step_is_dispatched_on_behalf_of_the_step_principal() {
        let step = step_with("Remember", "memory::store", Some("agent-1"));
        let mut vars = Map::new();
        vars.insert("input".into(), json!("x"));
        // The definition tries to name its own principal through a variable.
        vars.insert("principal".into(), json!({ "agentId": "victim" }));
        let payload = step_payload(&step, &vars, None).expect("payload");

        let dispatched = dispatch_payload(&step, payload, "agent-1");
        assert_eq!(dispatched["principal"], json!({ "agentId": "agent-1" }));
        assert_eq!(dispatched["prompt"], "x");

        // A nested workflow is a deputy too (review F2/F1): it is labelled, so
        // the inner run binds its steps to this step's principal.
        let step = step_with("Run", "workflow::run", Some("agent-1"));
        let payload = step_payload(&step, &vars, None).expect("payload");
        assert_eq!(
            dispatch_payload(&step, payload, "agent-1")["principal"],
            json!({ "agentId": "agent-1" })
        );

        // Families that resolve no principal are dispatched as built.
        let step = step_with("Hand", "hand::run", Some("agent-1"));
        let payload = step_payload(&step, &vars, None).expect("payload");
        assert_eq!(dispatch_payload(&step, payload.clone(), "agent-1"), payload);
    }

    #[test]
    fn a_step_without_any_principal_is_refused_not_exempt() {
        let step = step_with("Escalate", "mcp::connect", None);
        let workflow = workflow_with(vec![step.clone()], None);

        match plan_step_authorization(&step, &workflow, None) {
            StepAuthorization::Refused(reason) => {
                assert!(reason.contains("mcp::connect"), "{reason}");
                assert!(reason.contains("needs a principal"), "{reason}");
                assert!(reason.contains("wf-1"), "{reason}");
            }
            other => panic!("a step with no principal must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_workflow_may_not_name_its_own_principal_through_agents() {
        // `agents` is self-declared by whoever wrote the definition, so it must
        // not satisfy the principal requirement.
        let step = step_with("Escalate", "shell::exec", None);
        let mut workflow = workflow_with(vec![step.clone()], None);
        workflow.agents = vec!["admin".into()];

        assert!(matches!(
            plan_step_authorization(&step, &workflow, None),
            StepAuthorization::Refused(_)
        ));
    }

    #[test]
    fn the_principal_falls_back_step_then_run_then_creator() {
        let bare = step_with("A", "memory::store", None);
        let owned = step_with("B", "memory::store", Some("step-agent"));

        let no_creator = workflow_with(vec![bare.clone()], None);
        let with_creator = workflow_with(vec![bare.clone()], Some("creator"));

        assert_eq!(
            step_principal(&owned, &no_creator, Some("run")),
            Some("step-agent")
        );
        assert_eq!(step_principal(&bare, &no_creator, Some("run")), Some("run"));
        assert_eq!(step_principal(&bare, &with_creator, None), Some("creator"));
        assert_eq!(step_principal(&bare, &no_creator, None), None);
    }

    // --- a run is bound to its caller (review F1) — through the real handlers ---

    use agentos_http_adapter::fake::FakeBus;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    static AUTH_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_api_key<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        let _guard = AUTH_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("AGENTOS_API_KEY");
        unsafe {
            match value {
                Some(value) => std::env::set_var("AGENTOS_API_KEY", value),
                None => std::env::remove_var("AGENTOS_API_KEY"),
            }
        }
        let result = test();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("AGENTOS_API_KEY", value),
                None => std::env::remove_var("AGENTOS_API_KEY"),
            }
        }
        result
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    /// In-memory `state::*` with the engine's real shapes (`state::get` of a
    /// missing key is null, `state::list` a bare array, `state::update` takes
    /// `ops` and applies `set` by top-level path).
    #[derive(Default)]
    struct StateStore(Mutex<BTreeMap<String, BTreeMap<String, Value>>>);

    impl StateStore {
        fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeMap<String, Value>>> {
            self.0.lock().unwrap_or_else(|error| error.into_inner())
        }
        fn field(input: &Value, name: &str) -> String {
            input[name].as_str().unwrap_or_default().to_string()
        }
    }

    /// A bus with a state store, a `memory::recall` that answers with who it
    /// was asked AS, and a capability reader answering through the shared
    /// matcher: `a-1` and `a-2` hold `workflow::*` + `memory::*`; `a-granted`
    /// additionally holds exactly `grant::act_as::a-2`.
    fn workflow_bus() -> (Arc<FakeBus>, Arc<StateStore>) {
        let store = Arc::new(StateStore::default());
        let bus = Arc::new(FakeBus::new());
        let state = store.clone();
        bus.on("state::get", move |input| {
            Ok(state
                .lock()
                .get(&StateStore::field(&input, "scope"))
                .and_then(|scope| scope.get(&StateStore::field(&input, "key")))
                .cloned()
                .unwrap_or(Value::Null))
        });
        let state = store.clone();
        bus.on("state::set", move |input| {
            state
                .lock()
                .entry(StateStore::field(&input, "scope"))
                .or_default()
                .insert(StateStore::field(&input, "key"), input["value"].clone());
            Ok(json!({ "stored": true }))
        });
        let state = store.clone();
        bus.on("state::list", move |input| {
            Ok(Value::Array(
                state
                    .lock()
                    .get(&StateStore::field(&input, "scope"))
                    .map(|scope| scope.values().cloned().collect())
                    .unwrap_or_default(),
            ))
        });
        let state = store.clone();
        bus.on("state::update", move |input| {
            let mut store = state.lock();
            let scope = store.entry(StateStore::field(&input, "scope")).or_default();
            let entry = scope
                .entry(StateStore::field(&input, "key"))
                .or_insert_with(|| json!({}));
            for op in input["ops"].as_array().into_iter().flatten() {
                if op["type"] == "set"
                    && let Some(path) = op["path"].as_str()
                {
                    entry[path] = op["value"].clone();
                }
            }
            Ok(json!({ "new_value": entry.clone() }))
        });
        bus.on("memory::recall", |input| {
            Ok(json!({ "readAs": input["principal"]["agentId"], "about": input["agentId"] }))
        });
        bus.on("security::check_capability", |input| {
            let agent = input["agentId"].as_str().unwrap_or_default();
            let resource = input["resource"].as_str().unwrap_or_default();
            let tools: Vec<String> = match agent {
                "a-1" | "a-2" => vec!["workflow::*".into(), "memory::*".into()],
                "a-granted" => vec![
                    "workflow::*".into(),
                    "memory::*".into(),
                    policy::act_as_grant("a-2"),
                ],
                _ => vec![],
            };
            if policy::capabilities_grant(&tools, resource) {
                Ok(json!({ "allowed": true, "reason": "granted" }))
            } else {
                Err(Error::Handler(format!("Agent {agent} denied: {resource}")))
            }
        });
        (bus, store)
    }

    fn as_bus(bus: &Arc<FakeBus>) -> Bus {
        Arc::clone(bus) as Bus
    }

    /// A stored definition whose one step reads memory as `step_agent`.
    fn stored_recall_workflow(store: &StateStore, id: &str, step_agent: Option<&str>) {
        let step = step_with("Recall", "memory::recall", step_agent);
        let workflow = Workflow {
            id: id.into(),
            ..workflow_with(vec![step], None)
        };
        store.lock().entry("workflows".into()).or_default().insert(
            id.into(),
            serde_json::to_value(workflow).expect("definition"),
        );
    }

    fn run_as(agent: &str, workflow_id: &str, run_agent: Option<&str>) -> Value {
        let mut input =
            json!({ "workflowId": workflow_id, "principal": principal::as_agent(agent) });
        if let Some(run_agent) = run_agent {
            input["agentId"] = json!(run_agent);
        }
        input
    }

    fn grants_asked(bus: &FakeBus) -> Vec<(String, String)> {
        bus.calls_to("security::check_capability")
            .into_iter()
            .map(|call| call.payload)
            .filter(|payload| policy::is_grant(payload["resource"].as_str().unwrap_or_default()))
            .map(|payload| {
                (
                    payload["agentId"].as_str().unwrap_or_default().to_string(),
                    payload["resource"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_step_cannot_run_as_another_agent_without_the_exact_grant() {
        let (bus, store) = workflow_bus();
        let iii = as_bus(&bus);
        // The F1 scenario: a-1 (holding workflow::*) runs a definition whose
        // step names a-2. Nothing here consults a-1's right to act as a-2.
        stored_recall_workflow(&store, "wf-1", Some("a-2"));

        let outcome = run_workflow(&iii, run_as("a-1", "wf-1", None)).await;
        assert_eq!(
            bus.call_count("memory::recall"),
            0,
            "a-2's memory was read through a workflow step run by a-1"
        );
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("grant::act_as::a-2"), "{error}");
        assert!(
            store.lock().get("workflow_runs").is_none(),
            "refused before a run row is written"
        );
        assert_eq!(
            grants_asked(&bus),
            vec![("a-1".to_string(), policy::act_as_grant("a-2"))]
        );

        // The same definition run by an agent holding the exact grant: the step
        // runs AS a-2 (labelled a-2, a-2's capability for memory::recall).
        let result = run_workflow(&iii, run_as("a-granted", "wf-1", None))
            .await
            .expect("granted");
        assert_eq!(result["results"][0]["output"]["readAs"], "a-2");
        let dispatched = &bus.calls_to("memory::recall")[0].payload;
        assert_eq!(dispatched["principal"], principal::as_agent("a-2"));
        assert!(
            bus.calls_to("security::check_capability")
                .iter()
                .any(|call| call.payload["agentId"] == "a-2"
                    && call.payload["resource"] == "memory::recall")
        );
        assert!(
            grants_asked(&bus).contains(&("a-granted".to_string(), policy::act_as_grant("a-2")))
        );
    }

    #[tokio::test]
    async fn the_run_agent_and_the_creator_are_bound_to_the_caller_too() {
        let (bus, store) = workflow_bus();
        let iii = as_bus(&bus);
        // A step with no agentId falls back to the run's agentId ...
        stored_recall_workflow(&store, "wf-bare", None);

        // ... which a-1 may not set to a-2 without the grant.
        let error = run_workflow(&iii, run_as("a-1", "wf-bare", Some("a-2")))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("grant::act_as::a-2"), "{error}");
        assert_eq!(bus.call_count("memory::recall"), 0);

        // With no agentId at all, an agent's run is its own.
        let result = run_workflow(&iii, run_as("a-1", "wf-bare", None))
            .await
            .expect("own run");
        assert_eq!(result["results"][0]["output"]["readAs"], "a-1");
        assert_eq!(result["vars"]["agentId"], "a-1");
        assert!(
            grants_asked(&bus).len() == 1,
            "only the refused attempt asked the reader"
        );

        // And a definition whose `createdBy` names a-2 does not run as a-2 for
        // a-1 either: the creator is just another caller-supplied candidate.
        {
            let mut store = store.lock();
            let workflows = store.get_mut("workflows").expect("scope");
            workflows.get_mut("wf-bare").expect("wf-bare")["createdBy"] = json!("a-2");
        }
        // a-1's run defaults to a-1 before `createdBy` is consulted, so this
        // runs as a-1; the creator only matters for the operator's bare runs.
        let result = run_workflow(&iii, run_as("a-1", "wf-bare", None))
            .await
            .expect("the creator does not override the caller");
        assert_eq!(result["results"][0]["output"]["readAs"], "a-1");
        assert_eq!(
            bus.calls_to("memory::recall").last().unwrap().payload["principal"],
            principal::as_agent("a-1")
        );
    }

    #[tokio::test]
    async fn a_bare_run_is_refused_before_any_state_is_read() {
        let (bus, store) = workflow_bus();
        let iii = as_bus(&bus);
        stored_recall_workflow(&store, "wf-1", Some("a-2"));

        // The untrusted-tier variant: no bearer, no principal, any agentId.
        let outcome = run_workflow(&iii, json!({ "workflowId": "wf-1", "agentId": "a-2" })).await;
        assert!(
            bus.calls().is_empty(),
            "a bare run must be refused before the bus is touched: {:?}",
            bus.calls()
        );
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("principal required"), "{error}");

        // So is a bearer that does not match.
        let outcome = run_workflow(
            &iii,
            json!({
                "workflowId": "wf-1",
                "headers": { "authorization": "Bearer not-the-key" },
            }),
        )
        .await;
        assert!(bus.calls().is_empty());
        assert!(outcome.unwrap_err().to_string().contains("Unauthorized"));
    }

    #[test]
    fn the_operator_runs_a_workflow_as_whoever_it_names() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, store) = workflow_bus();
                let iii = as_bus(&bus);
                stored_recall_workflow(&store, "wf-1", Some("a-2"));

                let result = run_workflow(
                    &iii,
                    json!({
                        "workflowId": "wf-1",
                        "headers": { "authorization": "Bearer op-key" },
                    }),
                )
                .await
                .expect("operator run");
                assert_eq!(result["results"][0]["output"]["readAs"], "a-2");
                assert!(grants_asked(&bus).is_empty(), "the operator needs no grant");
            })
        });
    }

    #[tokio::test]
    async fn an_agent_registers_definitions_in_its_own_name_and_a_bare_create_is_refused() {
        let (bus, store) = workflow_bus();
        let iii = as_bus(&bus);
        let definition = json!({
            "id": "wf-new",
            "name": "wf",
            "description": "d",
            "createdBy": "a-2",
            "steps": [{
                "name": "Recall",
                "functionId": "memory::recall",
                "mode": "sequential",
                "errorMode": "fail",
                "timeoutMs": 1000,
            }],
        });

        let mut labelled = definition.clone();
        labelled["principal"] = principal::as_agent("a-1");
        create_workflow(&iii, labelled)
            .await
            .expect("create as a-1");
        let stored = store.lock()["workflows"]["wf-new"].clone();
        assert_eq!(
            stored["createdBy"], "a-1",
            "the definition's self-declared creator is replaced by the principal"
        );
        assert!(
            stored.get("principal").is_none(),
            "the label is not persisted"
        );

        let error = create_workflow(&iii, definition.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("principal required"), "{error}");

        // The in-process startup loader keeps storing the bundled definitions
        // as they are written.
        let mut bundled = definition;
        bundled["id"] = json!("wf-bundled");
        store_workflow(&iii, bundled, None)
            .await
            .expect("bundled definition");
        assert_eq!(store.lock()["workflows"]["wf-bundled"]["createdBy"], "a-2");
    }

    #[test]
    fn a_deny_by_default_step_needs_an_approval_decision() {
        for function_id in [
            "shell::exec",
            "bridge::invoke",
            "mcp::connect",
            "hook::register",
            "cron::create",
            "vault::read",
            "state::set",
            "engine::functions::list",
            "code::run",
            "harness::spawn",
            "browser::navigate",
            "wasm::run",
            // .W ruling 2026-09-02: no agent-chosen call has a legitimate
            // reason to reach any `security::*` or `coder::*` id.
            "security::docker_exec",
            "security::audit",
            "security::set_capabilities",
            "coder::update",
            "coder::delete",
        ] {
            let step = step_with("Escalate", function_id, Some("agent-1"));
            let workflow = workflow_with(vec![step.clone()], None);
            assert_eq!(
                plan_step_authorization(&step, &workflow, None),
                StepAuthorization::Checked {
                    agent_id: "agent-1",
                    needs_approval: true,
                },
                "{function_id} must require approval"
            );
        }
    }

    #[test]
    fn an_ordinary_allowed_step_still_runs() {
        let step = step_with("Remember", "memory::store", Some("agent-1"));
        let workflow = workflow_with(vec![step.clone()], None);

        assert_eq!(
            plan_step_authorization(&step, &workflow, None),
            StepAuthorization::Checked {
                agent_id: "agent-1",
                needs_approval: false,
            }
        );
        // and the two remote verdicts let it through
        assert_eq!(capability_denial(&json!({ "allowed": true })), None);
    }

    #[test]
    fn every_shipped_workflow_still_authorizes() {
        // Every shipped definition names an agentId on every step, so
        // fail-closed does not strand any of them.
        for path in [
            "../../workflows/feature-build.yaml",
            "../../workflows/incident-response.yaml",
            "../../workflows/mvp-sprint.yaml",
            "../../examples/crew-blog-writer.yaml",
        ] {
            let raw = std::fs::read_to_string(path).expect(path);
            let value: Value = serde_yaml::from_str(&raw).expect(path);
            let workflow: Workflow = serde_json::from_value(value).expect(path);
            for step in &workflow.steps {
                assert!(
                    matches!(
                        plan_step_authorization(step, &workflow, None),
                        StepAuthorization::Checked { .. }
                    ),
                    "{path}: step {} lost its principal",
                    step.name
                );
            }
        }
    }

    #[test]
    fn a_capability_verdict_that_is_not_true_is_a_denial() {
        // The old code discarded this response entirely.
        assert_eq!(
            capability_denial(&json!({ "allowed": false, "reason": "no such capability" })),
            Some("no such capability".to_string())
        );
        assert_eq!(
            capability_denial(&json!({})),
            Some("capability denied".to_string())
        );
        assert_eq!(
            capability_denial(&json!({ "allowed": "true" })),
            Some("capability denied".to_string())
        );
        assert_eq!(capability_denial(&json!({ "allowed": true })), None);
    }

    #[test]
    fn only_an_explicit_approval_may_dispatch() {
        assert_eq!(approval_denial(&json!({ "decision": "approved" })), None);
        for decision in ["denied", "required", "queued", ""] {
            let result = json!({ "decision": decision, "reason": "r", "requestId": "req-1" });
            let denial =
                approval_denial(&result).unwrap_or_else(|| panic!("{decision} must not dispatch"));
            assert!(denial.contains("req-1"), "{denial}");
        }
        // A response with no decision at all is not an approval.
        assert!(approval_denial(&json!({})).is_some());
    }

    #[test]
    fn the_family_vocabulary_is_the_shared_contract_i1_definition() {
        // The gate reads `agentos_http_adapter::policy`, so this worker and
        // agent-core can never disagree about what is deny-by-default.
        for family in policy::DENY_BY_DEFAULT_FAMILIES {
            assert!(
                requires_approval(&format!("{family}::anything")),
                "{family} is deny-by-default in the shared contract"
            );
        }
        assert!(!requires_approval("memory::store"));
        assert!(!requires_approval("agent::chat"));
        assert!(!requires_approval(""));
        assert!(requires_approval("shell::exec"));
    }

    #[test]
    fn the_ruled_families_stay_in_the_shared_list() {
        // .W ruled `security` and `coder` into contract I1 on 2026-09-02, and
        // chat-core landed them in `policy::DENY_BY_DEFAULT_FAMILIES`, so the
        // local delta this worker used to carry is gone. This test keeps them
        // there: dropping either one silently re-opens `security::docker_exec`
        // (root-equivalent) and `coder::update` (the shell binary's second
        // file-writing surface) to an unapproved workflow step.
        for family in ["security", "coder"] {
            assert!(
                policy::DENY_BY_DEFAULT_FAMILIES.contains(&family),
                "`{family}` was removed from policy::DENY_BY_DEFAULT_FAMILIES; \
                 that is a .W-level decision, not a refactor"
            );
            assert!(requires_approval(&format!("{family}::anything")));
        }
    }

    #[test]
    fn the_payload_digest_binds_the_approval_to_one_payload() {
        let a = payload_digest(&json!({ "cmd": "ls" }));
        let b = payload_digest(&json!({ "cmd": "rm -rf /" }));
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(a, payload_digest(&json!({ "cmd": "ls" })));
    }
}
