//! The engine's `state::*` wire shapes.
//!
//! Verified against the pinned engine (iii 0.22.1) by invoking it directly:
//!
//! ```text
//! $ iii trigger state::list scope=t
//! [ { ...value... } ]                 # bare values: no key, no envelope
//! $ iii trigger state::list_groups
//! { "groups": ["t"] }
//! $ iii trigger state::update --json '{"scope":"t","key":"k","operations":[...]}'
//! Error: serialization error: missing field `ops`
//! $ iii trigger state::update --json '{... "ops":[{"type":"increment","path":"n","value":1}]}'
//! Error: serialization error: missing field `by`
//! $ iii trigger state::update --json '{... "ops":[{"type":"merge","path":"m","value":[1]}]}'
//! { "errors": [ { "code": "merge.value.not_an_object", ... } ], ... }   # 200, silent
//! ```
//!
//! Workers that hand-rolled these shapes read nothing and wrote nothing. Use
//! this module so there is one place that is right.

use serde_json::{Value, json};

/// Extracts the stored value from one `state::list` element.
///
/// The engine returns the value itself. A `{ key, value }` envelope is still
/// accepted, but only when the object has nothing else in it, so a stored
/// document that merely happens to have a `value` field is not unwrapped.
pub fn value_of(entry: &Value) -> &Value {
    match entry.as_object() {
        Some(object)
            if object.contains_key("value")
                && object.keys().all(|key| key == "value" || key == "key") =>
        {
            &object["value"]
        }
        _ => entry,
    }
}

/// The stored values of a `state::list` response.
pub fn values(response: &Value) -> Vec<&Value> {
    response
        .as_array()
        .into_iter()
        .flatten()
        .map(value_of)
        .collect()
}

/// The scope names of a `state::list_groups` response.
pub fn groups(response: &Value) -> Vec<&str> {
    response
        .get("groups")
        .and_then(Value::as_array)
        .or_else(|| response.as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// A `state::update` `set` operation.
pub fn set_op(path: &str, value: Value) -> Value {
    json!({ "type": "set", "path": path, "value": value })
}

/// A `state::update` `append` operation.
///
/// `append` takes the element itself; passing an array appends the array as one
/// nested element. `merge` cannot append to a list at all.
pub fn append_op(path: &str, value: Value) -> Value {
    json!({ "type": "append", "path": path, "value": value })
}

/// A `state::update` `increment` operation. The amount field is `by`.
pub fn increment_op(path: &str, by: i64) -> Value {
    json!({ "type": "increment", "path": path, "by": by })
}

/// A `state::update` payload. The operation list field is `ops`.
pub fn update_payload(scope: impl Into<String>, key: &str, ops: Vec<Value>) -> Value {
    json!({ "scope": scope.into(), "key": key, "ops": ops })
}

/// The per-operation error codes a `state::update` reports inside a 200 response.
///
/// A rejected operation does not fail the invocation, so this is the only way
/// to notice that nothing was written.
pub fn update_errors(response: &Value) -> Option<String> {
    let errors = response.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }
    Some(
        errors
            .iter()
            .map(|error| {
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_the_bare_array_the_engine_returns() {
        let bare = json!([{ "id": "m-1" }, { "id": "m-2" }]);
        let ids: Vec<&str> = values(&bare)
            .into_iter()
            .filter_map(|value| value["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["m-1", "m-2"]);
    }

    #[test]
    fn values_still_read_a_key_value_envelope() {
        let enveloped = json!([{ "key": "m-1", "value": { "id": "m-1" } }]);
        assert_eq!(values(&enveloped)[0]["id"], "m-1");
    }

    #[test]
    fn a_document_with_a_value_field_is_not_an_envelope() {
        let ambiguous = json!([{ "id": "m-1", "value": 7 }]);
        assert_eq!(values(&ambiguous)[0]["id"], "m-1");
        assert!(values(&Value::Null).is_empty());
    }

    #[test]
    fn groups_read_the_engine_envelope_and_a_bare_array() {
        assert_eq!(groups(&json!({ "groups": ["a", "b"] })), vec!["a", "b"]);
        assert_eq!(groups(&json!(["a"])), vec!["a"]);
        assert!(groups(&Value::Null).is_empty());
    }

    #[test]
    fn update_payload_uses_the_field_names_the_engine_requires() {
        let payload = update_payload("scope", "key", vec![increment_op("n", 2)]);
        assert!(payload.get("operations").is_none());
        assert_eq!(payload["ops"][0]["by"], 2);
        assert!(
            payload["ops"][0].get("value").is_none(),
            "increment carries `by`"
        );
        assert_eq!(append_op("m", json!({ "a": 1 }))["type"], "append");
        assert_eq!(set_op("p", json!(1))["value"], 1);
    }

    #[test]
    fn update_errors_surface_rejected_operations() {
        let rejected = json!({ "errors": [{ "code": "merge.value.not_an_object" }] });
        assert_eq!(
            update_errors(&rejected).as_deref(),
            Some("merge.value.not_an_object")
        );
        assert!(update_errors(&json!({ "errors": [] })).is_none());
        assert!(update_errors(&json!({ "new_value": {} })).is_none());
    }
}
