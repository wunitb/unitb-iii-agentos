//! An in-memory [`TriggerBus`] for tests.
//!
//! `FakeBus` is a test double for the engine, not a mock framework: a test
//! registers plain closures as functions, handlers run against it unchanged,
//! and every call is recorded so the test can assert what was sent. There is no
//! expectation DSL, no verification order, and no partial matching.
//!
//! Two deliberate limitations, so tests do not claim more than they prove:
//! * handlers are synchronous, so this cannot exercise real concurrency;
//! * an unregistered function id is an error, exactly as the engine reports a
//!   function that no worker has registered.

use crate::bus::{BusFuture, TriggerBus};
use iii_sdk::{errors::Error, protocol::TriggerRequest};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

type Handler = Box<dyn Fn(Value) -> Result<Value, Error> + Send + Sync>;

/// One trigger observed by [`FakeBus`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    pub function_id: String,
    pub payload: Value,
    pub timeout_ms: Option<u64>,
}

/// In-memory bus that dispatches to closures registered by the test.
#[derive(Default)]
pub struct FakeBus {
    handlers: Mutex<HashMap<String, Handler>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl FakeBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler for `function_id`, replacing any previous handler.
    pub fn on<F>(&self, function_id: impl Into<String>, handler: F) -> &Self
    where
        F: Fn(Value) -> Result<Value, Error> + Send + Sync + 'static,
    {
        self.lock_handlers()
            .insert(function_id.into(), Box::new(handler));
        self
    }

    /// Register a handler that always returns `value`.
    pub fn on_value(&self, function_id: impl Into<String>, value: Value) -> &Self {
        self.on(function_id, move |_| Ok(value.clone()))
    }

    /// Register a handler that always fails with `message`.
    pub fn on_error(&self, function_id: impl Into<String>, message: impl Into<String>) -> &Self {
        let message = message.into();
        self.on(function_id, move |_| Err(Error::Handler(message.clone())))
    }

    /// Every call observed so far, in order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.lock_calls().clone()
    }

    /// Calls to one function id, in order.
    pub fn calls_to(&self, function_id: &str) -> Vec<RecordedCall> {
        self.lock_calls()
            .iter()
            .filter(|call| call.function_id == function_id)
            .cloned()
            .collect()
    }

    /// Number of calls to one function id.
    pub fn call_count(&self, function_id: &str) -> usize {
        self.calls_to(function_id).len()
    }

    fn lock_handlers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Handler>> {
        self.handlers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn lock_calls(&self) -> std::sync::MutexGuard<'_, Vec<RecordedCall>> {
        self.calls.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl TriggerBus for FakeBus {
    fn trigger(&self, request: TriggerRequest) -> BusFuture<'_> {
        self.lock_calls().push(RecordedCall {
            function_id: request.function_id.clone(),
            payload: request.payload.clone(),
            timeout_ms: request.timeout_ms,
        });

        let result = match self.lock_handlers().get(&request.function_id) {
            Some(handler) => handler(request.payload),
            None => Err(Error::Handler(format!(
                "fake bus: no handler registered for {}",
                request.function_id
            ))),
        };

        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(function_id: &str, payload: Value) -> TriggerRequest {
        TriggerRequest {
            function_id: function_id.to_string(),
            payload,
            action: None,
            timeout_ms: None,
        }
    }

    #[tokio::test]
    async fn dispatches_to_the_registered_handler_and_records_the_call() {
        let bus = FakeBus::new();
        bus.on("echo::it", |payload| Ok(json!({ "echo": payload })));

        let result = bus.trigger(request("echo::it", json!({ "a": 1 }))).await;

        assert_eq!(result.unwrap(), json!({ "echo": { "a": 1 } }));
        assert_eq!(
            bus.calls(),
            vec![RecordedCall {
                function_id: "echo::it".to_string(),
                payload: json!({ "a": 1 }),
                timeout_ms: None,
            }]
        );
    }

    #[tokio::test]
    async fn unregistered_function_ids_fail_like_the_engine() {
        let bus = FakeBus::new();

        let error = bus.trigger(request("nope::missing", json!({}))).await;

        assert!(
            error.unwrap_err().to_string().contains("nope::missing"),
            "the error should name the missing function"
        );
        assert_eq!(bus.call_count("nope::missing"), 1);
    }

    #[tokio::test]
    async fn handlers_can_hold_state_across_calls() {
        let bus = FakeBus::new();
        let seen = Mutex::new(Vec::new());
        bus.on("count::it", move |payload| {
            let mut seen = seen.lock().unwrap_or_else(|error| error.into_inner());
            seen.push(payload["n"].as_u64().unwrap_or_default());
            Ok(json!({ "total": seen.len() }))
        });

        bus.trigger(request("count::it", json!({ "n": 1 })))
            .await
            .unwrap();
        let second = bus
            .trigger(request("count::it", json!({ "n": 2 })))
            .await
            .unwrap();

        assert_eq!(second, json!({ "total": 2 }));
    }

    #[tokio::test]
    async fn on_error_reports_a_handler_failure() {
        let bus = FakeBus::new();
        bus.on_error("state::get", "state store offline");

        let error = bus.trigger(request("state::get", json!({}))).await;

        assert!(
            error
                .unwrap_err()
                .to_string()
                .contains("state store offline")
        );
    }
}
