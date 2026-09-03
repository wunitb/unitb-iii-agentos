//! Who is asking (contract T1).
//!
//! The product has one credential, `AGENTOS_API_KEY`, and no per-user identity.
//! Before this module every worker that keeps per-agent data read
//! `input["agentId"]` and believed it: whoever could reach the bus or the HTTP
//! edge chose whose memory to read, overwrite or evict simply by naming the
//! agent, and a model could do the same through a tool call. This module is
//! the one place that decides who a call is FROM; the payload's `agentId` is
//! only ever what the call is ABOUT.
//!
//! # The two principals that exist today
//!
//! * [`Principal::Operator`] — the payload carries a valid
//!   `headers.authorization: Bearer $AGENTOS_API_KEY`. That is the HTTP edge
//!   (the adapter forwards request headers verbatim) or an in-tree worker that
//!   attached the same bearer because it acts system-wide. One key, one tenant:
//!   the operator is root and may name any agent. Nothing narrower can be
//!   checked because nothing narrower exists — per-user credentials are a
//!   product decision, not something this module invents.
//! * [`Principal::Agent`] — the payload carries `principal: {"agentId": ...}`.
//!   A trusted worker sets it when it acts on behalf of exactly one resolved
//!   agent (agent-core sets it from the authenticated route's agent and
//!   overwrites whatever the model wrote into a tool call). Its trust basis is
//!   the bus tier — only a trusted session can reach a `memory::*`/`vault::*`
//!   handler at all — not the field itself.
//!
//! Neither present, or a bearer that does not match: the call is refused. A
//! missing principal fails closed.
//!
//! # Acting on another agent
//!
//! The agent an operation acts on is the principal's own agent. A payload that
//! names a different `agentId` is cross-agent access, allowed for an agent
//! principal only when the capability store holds the exact grant
//! [`crate::policy::act_as_grant`] for that target — checked through
//! `security::check_capability`, the single capability reader. The operator
//! needs no grant.
//!
//! # What this module deliberately does not do
//!
//! It reads no environment and opens no socket: the expected bearer is a
//! parameter, so `agentos-bus-auth` (which already depends on this crate) can
//! keep owning the credential, and the grant check takes a [`TriggerBus`] so a
//! test drives it through [`crate::fake::FakeBus`].

use crate::bus::TriggerBus;
use crate::policy::act_as_grant;
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerRequest;
use serde_json::{Value, json};
use std::fmt;

/// Payload key carrying an agent principal.
pub const PRINCIPAL_KEY: &str = "principal";

/// Function families whose handlers resolve a principal (contract T1), and
/// which a deputy therefore has to label before dispatching a call on behalf of
/// an agent.
///
/// `memory`, `vault`, `lifecycle` and `wasm` keep per-agent data or act on it.
/// `agent` and `workflow` are DEPUTIES: `agent::chat` runs a whole turn as the
/// agent it names and `workflow::run` dispatches steps as the agents the
/// definition names, so a model tool call into either that went out unlabelled
/// would let the model pick whose turn, memory and capabilities it runs with.
/// Labelled, both bind what they run to the caller's principal and demand the
/// exact `grant::act_as::<target>` for anyone else.
///
/// Deliberately a list rather than "everything": some upstream and in-tree
/// request structs are `deny_unknown_fields` (`hand::*`), so an unconditional
/// extra field would turn every such tool call into a deserialisation error.
pub const PRINCIPAL_FAMILIES: [&str; 6] =
    ["memory", "vault", "lifecycle", "wasm", "agent", "workflow"];

/// Who a call is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// Presented the product bearer. Root: may name any agent.
    Operator,
    /// A trusted worker acting on behalf of exactly this agent.
    Agent(String),
}

impl Principal {
    /// The agent this principal is confined to, if any.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Principal::Operator => None,
            Principal::Agent(id) => Some(id),
        }
    }

    /// True for the unconfined operator.
    pub fn is_operator(&self) -> bool {
        matches!(self, Principal::Operator)
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::Operator => f.write_str("operator"),
            Principal::Agent(id) => write!(f, "agent {id}"),
        }
    }
}

/// Why no principal could be resolved. Every variant is a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalError {
    /// No `principal` and no bearer at all.
    Missing,
    /// A bearer was presented and does not match, or no bearer is configured.
    Unauthorized,
    /// A `principal` field is present but is not `{"agentId": "<non-empty>"}`.
    Malformed,
}

impl fmt::Display for PrincipalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrincipalError::Missing => f.write_str(
                "Unauthorized: principal required (present the AgentOS bearer, or a principal.agentId set by a trusted worker)",
            ),
            PrincipalError::Unauthorized => f.write_str("Unauthorized"),
            PrincipalError::Malformed => {
                f.write_str("principal must be an object with a non-empty agentId")
            }
        }
    }
}

impl From<PrincipalError> for Error {
    fn from(error: PrincipalError) -> Self {
        Error::Handler(error.to_string())
    }
}

/// Resolve the principal of one invocation.
///
/// `expected_bearer` is the configured `AGENTOS_API_KEY`; `None` (unset or
/// empty) means no operator can exist, which is the fail-closed answer for a
/// stack that lost its key. Precedence: an explicit `principal` narrows even
/// when a bearer is also present — a worker that confines itself stays
/// confined — and a malformed `principal` never widens to the operator.
pub fn resolve(input: &Value, expected_bearer: Option<&str>) -> Result<Principal, PrincipalError> {
    if let Some(principal) = input.get(PRINCIPAL_KEY) {
        return principal
            .as_object()
            .and_then(|object| object.get("agentId"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| Principal::Agent(id.to_string()))
            .ok_or(PrincipalError::Malformed);
    }

    let has_headers = input
        .get("headers")
        .and_then(Value::as_object)
        .is_some_and(|headers| !headers.is_empty());
    match expected_bearer.filter(|key| !key.is_empty()) {
        Some(expected) if crate::is_authorized(input, expected) => Ok(Principal::Operator),
        _ if has_headers => Err(PrincipalError::Unauthorized),
        _ => Err(PrincipalError::Missing),
    }
}

/// The agent a payload asks about: `agentId`, else `agent`, non-empty.
pub fn requested_agent(input: &Value) -> Option<&str> {
    input
        .get("agentId")
        .or_else(|| input.get("agent"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// The local half of [`acting_agent`], with no bus call, so the rule is
/// testable on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acting {
    /// Act as this agent; nothing further to check.
    Allowed(String),
    /// Agent principal `agent` asked to act on `target`; only the grant
    /// [`act_as_grant`]`(target)` allows it.
    NeedsGrant { agent: String, target: String },
}

/// Decide, without a bus, which agent an operation acts on.
///
/// The operator acts on whatever the payload names, or `default` when it names
/// nothing (the pre-tenancy behaviour, now confined to the one principal that
/// is allowed it). An agent principal acts on itself; naming another agent
/// needs the grant.
pub fn plan_acting_agent(principal: &Principal, requested: Option<&str>, default: &str) -> Acting {
    match principal {
        Principal::Operator => Acting::Allowed(requested.unwrap_or(default).to_string()),
        Principal::Agent(agent) => match requested {
            Some(target) if target != agent => Acting::NeedsGrant {
                agent: agent.clone(),
                target: target.to_string(),
            },
            _ => Acting::Allowed(agent.clone()),
        },
    }
}

/// `security::check_capability` answers `{"allowed": true}` for a grant it
/// holds and an error otherwise; anything that is not an explicit `true` is a
/// denial, and so is a bus error (an unreachable security worker must not
/// become a grant).
fn grant_denial(result: &Result<Value, Error>) -> Option<String> {
    match result {
        Ok(value) if value.get("allowed").and_then(Value::as_bool) == Some(true) => None,
        Ok(value) => Some(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("not granted")
                .to_string(),
        ),
        Err(error) => Some(error.to_string()),
    }
}

/// Resolve which agent `principal` may act on for this payload, consulting the
/// capability store only for cross-agent access.
///
/// Same-agent and operator calls never touch the bus.
pub async fn acting_agent(
    bus: &dyn TriggerBus,
    principal: &Principal,
    input: &Value,
    default: &str,
) -> Result<String, Error> {
    match plan_acting_agent(principal, requested_agent(input), default) {
        Acting::Allowed(agent) => Ok(agent),
        Acting::NeedsGrant { agent, target } => {
            let grant = act_as_grant(&target);
            let result = bus
                .trigger(TriggerRequest {
                    function_id: "security::check_capability".to_string(),
                    payload: json!({ "agentId": &agent, "resource": &grant }),
                    action: None,
                    timeout_ms: None,
                })
                .await;
            match grant_denial(&result) {
                None => Ok(target),
                Some(reason) => Err(Error::Handler(format!(
                    "agent {agent} may not act on agent {target}: {grant} is not granted ({reason})"
                ))),
            }
        }
    }
}

/// Refuse anything but the operator or a bare (cron-shaped) call.
///
/// System-wide maintenance — `memory::evict`, `memory::consolidate`,
/// `lifecycle::check_all` — is fired by the engine's credential-less cron
/// worker with a cron event as payload. That caller has no principal and
/// cannot get one (see `agentos_bus_auth::policy`), so a payload with NO
/// principal is accepted here by design and says so. What is refused is an
/// AGENT principal: a deputy explicitly acting for one agent must not run a
/// job that touches every agent's data.
pub fn refuse_agent_principal(
    input: &Value,
    expected_bearer: Option<&str>,
    operation: &str,
) -> Result<(), Error> {
    match resolve(input, expected_bearer) {
        Ok(Principal::Agent(agent)) => Err(Error::Handler(format!(
            "{operation} is system-wide maintenance; agent {agent} may not run it"
        ))),
        Ok(Principal::Operator) | Err(PrincipalError::Missing) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// True when `function_id` is handled by a worker that resolves a principal.
pub fn resolves_principal(function_id: &str) -> bool {
    let Some((family, rest)) = function_id.split_once("::") else {
        return false;
    };
    !rest.is_empty() && PRINCIPAL_FAMILIES.contains(&family)
}

/// The `principal` value for a call made on behalf of `agent_id`.
pub fn as_agent(agent_id: &str) -> Value {
    json!({ "agentId": agent_id })
}

/// Label `payload` as made on behalf of `agent_id` when `function_id` resolves
/// a principal, OVERWRITING any `principal` already there — the deputy decides
/// who it acts for, never the model or the caller that supplied the arguments.
///
/// Calls into other families are returned unchanged (see
/// [`PRINCIPAL_FAMILIES`]). A non-object payload cannot carry a label and is
/// returned unchanged too; such a call then fails closed at the handler.
pub fn attach_agent(function_id: &str, mut payload: Value, agent_id: &str) -> Value {
    if !resolves_principal(function_id) {
        return payload;
    }
    if let Some(object) = payload.as_object_mut() {
        object.insert(PRINCIPAL_KEY.to_string(), as_agent(agent_id));
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeBus;

    fn bearer(token: &str) -> Value {
        json!({ "headers": { "Authorization": format!("Bearer {token}") } })
    }

    #[test]
    fn a_valid_bearer_is_the_operator() {
        assert_eq!(resolve(&bearer("k"), Some("k")), Ok(Principal::Operator));
        let mut with_agent = bearer("k");
        with_agent["agentId"] = json!("anyone");
        assert_eq!(
            resolve(&with_agent, Some("k")),
            Ok(Principal::Operator),
            "the operator names agents; it is not one"
        );
    }

    #[test]
    fn a_principal_field_is_an_agent_and_narrows_even_beside_a_bearer() {
        let input = json!({ "principal": { "agentId": "a-1" }, "agentId": "a-2" });
        assert_eq!(resolve(&input, None), Ok(Principal::Agent("a-1".into())));

        let mut confined = bearer("k");
        confined["principal"] = as_agent("a-1");
        assert_eq!(
            resolve(&confined, Some("k")),
            Ok(Principal::Agent("a-1".into())),
            "a worker that confines itself stays confined"
        );
    }

    #[test]
    fn nothing_resolves_to_nothing() {
        assert_eq!(
            resolve(&json!({ "agentId": "a-1" }), Some("k")),
            Err(PrincipalError::Missing),
            "a payload agentId is what the call is about, never who it is from"
        );
        assert_eq!(resolve(&json!({}), Some("k")), Err(PrincipalError::Missing));
        assert_eq!(
            resolve(&json!({ "headers": {} }), Some("k")),
            Err(PrincipalError::Missing)
        );
    }

    #[test]
    fn a_wrong_or_unconfigured_bearer_is_refused() {
        assert_eq!(
            resolve(&bearer("wrong"), Some("k")),
            Err(PrincipalError::Unauthorized)
        );
        assert_eq!(
            resolve(&bearer("k"), None),
            Err(PrincipalError::Unauthorized),
            "no configured key means nobody is the operator"
        );
        assert_eq!(
            resolve(&bearer("k"), Some("")),
            Err(PrincipalError::Unauthorized)
        );
        assert_eq!(
            resolve(&json!({ "headers": { "authorization": "k" } }), Some("k")),
            Err(PrincipalError::Unauthorized),
            "the scheme is required"
        );
    }

    #[test]
    fn a_malformed_principal_never_widens_to_the_operator() {
        for principal in [
            json!(null),
            json!("a-1"),
            json!({}),
            json!({ "agentId": "" }),
            json!({ "agentId": 7 }),
            json!({ "agent": "a-1" }),
        ] {
            let mut input = bearer("k");
            input["principal"] = principal.clone();
            assert_eq!(
                resolve(&input, Some("k")),
                Err(PrincipalError::Malformed),
                "{principal} must be refused, not ignored"
            );
        }
    }

    #[test]
    fn the_operator_acts_on_whatever_is_named_or_the_default() {
        assert_eq!(
            plan_acting_agent(&Principal::Operator, Some("a-2"), "default"),
            Acting::Allowed("a-2".into())
        );
        assert_eq!(
            plan_acting_agent(&Principal::Operator, None, "default"),
            Acting::Allowed("default".into())
        );
    }

    #[test]
    fn an_agent_acts_on_itself_and_needs_a_grant_for_anyone_else() {
        let agent = Principal::Agent("a-1".into());
        assert_eq!(
            plan_acting_agent(&agent, None, "default"),
            Acting::Allowed("a-1".into()),
            "the default is never an agent's escape hatch"
        );
        assert_eq!(
            plan_acting_agent(&agent, Some("a-1"), "default"),
            Acting::Allowed("a-1".into())
        );
        assert_eq!(
            plan_acting_agent(&agent, Some("a-2"), "default"),
            Acting::NeedsGrant {
                agent: "a-1".into(),
                target: "a-2".into()
            }
        );
    }

    #[test]
    fn requested_agent_reads_both_spellings_and_ignores_empties() {
        assert_eq!(requested_agent(&json!({ "agentId": "a" })), Some("a"));
        assert_eq!(requested_agent(&json!({ "agent": "b" })), Some("b"));
        assert_eq!(
            requested_agent(&json!({ "agentId": "a", "agent": "b" })),
            Some("a")
        );
        assert_eq!(requested_agent(&json!({ "agentId": "" })), None);
        assert_eq!(requested_agent(&json!({ "agentId": 1 })), None);
    }

    #[tokio::test]
    async fn cross_agent_access_asks_the_capability_reader_for_the_exact_grant() {
        let bus = FakeBus::new();
        bus.on("security::check_capability", |input| {
            if input["agentId"] == "a-1" && input["resource"] == "grant::act_as::a-2" {
                Ok(json!({ "allowed": true, "reason": "granted" }))
            } else {
                Err(Error::Handler(format!("Agent {} denied", input["agentId"])))
            }
        });
        let agent = Principal::Agent("a-1".into());

        let target = acting_agent(&bus, &agent, &json!({ "agentId": "a-2" }), "default")
            .await
            .expect("granted");
        assert_eq!(target, "a-2");
        assert_eq!(bus.call_count("security::check_capability"), 1);

        let error = acting_agent(&bus, &agent, &json!({ "agentId": "a-3" }), "default")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("grant::act_as::a-3"), "{error}");
    }

    #[tokio::test]
    async fn same_agent_and_operator_calls_never_touch_the_bus() {
        let bus = FakeBus::new();
        assert_eq!(
            acting_agent(
                &bus,
                &Principal::Agent("a-1".into()),
                &json!({ "agentId": "a-1" }),
                "d"
            )
            .await
            .unwrap(),
            "a-1"
        );
        assert_eq!(
            acting_agent(
                &bus,
                &Principal::Operator,
                &json!({ "agentId": "a-9" }),
                "d"
            )
            .await
            .unwrap(),
            "a-9"
        );
        assert!(bus.calls().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_or_non_committal_reader_is_a_denial() {
        let bus = FakeBus::new();
        let agent = Principal::Agent("a-1".into());
        let input = json!({ "agentId": "a-2" });
        assert!(
            acting_agent(&bus, &agent, &input, "d").await.is_err(),
            "no handler"
        );

        bus.on_value(
            "security::check_capability",
            json!({ "allowed": false, "reason": "no" }),
        );
        assert!(acting_agent(&bus, &agent, &input, "d").await.is_err());

        bus.on_value("security::check_capability", json!({ "reason": "granted" }));
        assert!(
            acting_agent(&bus, &agent, &input, "d").await.is_err(),
            "only an explicit allowed: true is a grant"
        );
    }

    #[test]
    fn maintenance_admits_the_operator_and_a_bare_cron_event_but_not_an_agent() {
        assert!(refuse_agent_principal(&json!({}), Some("k"), "memory::evict").is_ok());
        assert!(refuse_agent_principal(&json!({ "cap": 0 }), None, "memory::evict").is_ok());
        assert!(refuse_agent_principal(&bearer("k"), Some("k"), "memory::evict").is_ok());
        let error = refuse_agent_principal(
            &json!({ "principal": { "agentId": "a-1" } }),
            Some("k"),
            "memory::evict",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("system-wide"), "{error}");
        assert!(refuse_agent_principal(&bearer("wrong"), Some("k"), "memory::evict").is_err());
        assert!(
            refuse_agent_principal(&json!({ "principal": {} }), Some("k"), "memory::evict")
                .is_err()
        );
    }

    #[test]
    fn attach_agent_labels_only_the_families_that_resolve_one_and_overwrites() {
        let model_supplied = json!({ "agentId": "a-2", "principal": { "agentId": "a-2" } });
        let labelled = attach_agent("memory::recall", model_supplied.clone(), "a-1");
        assert_eq!(labelled["principal"], as_agent("a-1"));
        assert_eq!(
            labelled["agentId"], "a-2",
            "what the call is ABOUT is left for the handler to judge"
        );

        for function_id in [
            "vault::get",
            "lifecycle::transition",
            "wasm::execute",
            // the deputies: a model must not name whose turn or workflow runs
            "agent::chat",
            "workflow::run",
            "workflow::create",
        ] {
            assert!(resolves_principal(function_id), "{function_id}");
            assert_eq!(
                attach_agent(function_id, json!({}), "a-1")["principal"],
                as_agent("a-1")
            );
        }
        for function_id in ["hand::run", "shell::exec", "memory", "", "memoryx::recall"] {
            assert!(!resolves_principal(function_id), "{function_id}");
            assert_eq!(attach_agent(function_id, json!({}), "a-1"), json!({}));
        }
        assert_eq!(
            attach_agent("memory::store", json!("text"), "a-1"),
            json!("text")
        );
    }
}
