//! A minimal abstraction over the iii bus.
//!
//! AgentOS handlers only ever need one operation from the SDK client: trigger a
//! function by id with a JSON payload and await the JSON result. Depending on
//! [`TriggerBus`] instead of `IIIClient` keeps that dependency explicit and lets
//! tests drive real handlers through [`crate::fake::FakeBus`].

use iii_sdk::{IIIClient, errors::Error, protocol::TriggerRequest};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Timeout applied to every trigger issued on the chat path (contract I4).
///
/// The SDK default is 30 s, which caps a whole ReAct turn (tool calls plus
/// provider completions) far below the 300 s the HTTP edge advertises.
pub const CHAT_TIMEOUT_MS: u64 = 300_000;

/// Boxed future returned by [`TriggerBus::trigger`].
///
/// The trait is boxed rather than `async fn` so it stays dyn-compatible:
/// handlers take `&dyn TriggerBus` and are not generic.
pub type BusFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, Error>> + Send + 'a>>;

/// The subset of the iii client that AgentOS handlers use.
pub trait TriggerBus: Send + Sync {
    /// Trigger a function and await its result.
    fn trigger(&self, request: TriggerRequest) -> BusFuture<'_>;

    /// Trigger a function with an explicit timeout instead of the SDK default.
    fn trigger_with_timeout(
        &self,
        function_id: &str,
        payload: Value,
        timeout_ms: u64,
    ) -> BusFuture<'_> {
        self.trigger(TriggerRequest {
            function_id: function_id.to_string(),
            payload,
            action: None,
            timeout_ms: Some(timeout_ms),
        })
    }
}

impl TriggerBus for IIIClient {
    fn trigger(&self, request: TriggerRequest) -> BusFuture<'_> {
        // Path syntax resolves to the inherent `IIIClient::trigger`, not to this
        // trait method, so this does not recurse.
        Box::pin(IIIClient::trigger(self, request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeBus;
    use serde_json::json;

    #[tokio::test]
    async fn trigger_with_timeout_sets_the_requested_timeout() {
        let bus = FakeBus::new();
        bus.on_value("state::get", json!({ "ok": true }));

        let result = bus
            .trigger_with_timeout("state::get", json!({ "key": "k" }), CHAT_TIMEOUT_MS)
            .await
            .unwrap();

        assert_eq!(result, json!({ "ok": true }));
        let calls = bus.calls_to("state::get");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].timeout_ms, Some(300_000));
        assert_eq!(calls[0].payload["key"], "k");
    }

    #[test]
    fn chat_timeout_is_five_minutes() {
        assert_eq!(CHAT_TIMEOUT_MS, 300_000);
    }
}
