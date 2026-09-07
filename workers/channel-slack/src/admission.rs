//! Bounded, in-process webhook admission.
//!
//! Accepted delivery ids are retained for a fixed TTL after their task ends,
//! up to a fixed capacity, and tasks are owned by a `JoinSet`. In-flight ids do
//! not expire. This prevents provider retries from starting a second turn in
//! this process. It is deliberately not a durable queue: a process crash can
//! lose an acknowledged task and the dedupe history.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Clone)]
pub(crate) struct Admission {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    slots: Arc<Semaphore>,
    retention: Duration,
    dedupe_capacity: usize,
}

struct State {
    accepting: bool,
    seen: HashMap<String, Seen>,
    completed: VecDeque<(String, Instant)>,
    tasks: JoinSet<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    InFlight(Instant),
    Completed(Instant),
}

/// Marks an id completed even when its future is cancelled or panics.
struct Completion {
    shared: Weak<Shared>,
    delivery_id: String,
    admitted_at: Instant,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.seen.get(&self.delivery_id) == Some(&Seen::InFlight(self.admitted_at)) {
            let completed_at = Instant::now();
            state
                .seen
                .insert(self.delivery_id.clone(), Seen::Completed(completed_at));
            state
                .completed
                .push_back((self.delivery_id.clone(), completed_at));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionDecision {
    Accepted,
    Duplicate,
    Overloaded,
}

impl Admission {
    pub(crate) fn new(max_in_flight: usize, dedupe_capacity: usize, retention: Duration) -> Self {
        assert!(max_in_flight > 0, "admission needs at least one task slot");
        assert!(dedupe_capacity > 0, "dedupe cache needs at least one entry");
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    accepting: true,
                    seen: HashMap::new(),
                    completed: VecDeque::new(),
                    tasks: JoinSet::new(),
                }),
                slots: Arc::new(Semaphore::new(max_in_flight)),
                retention,
                dedupe_capacity,
            }),
        }
    }

    /// Atomically dedupe, reserve capacity, and put the owned task in the set.
    /// A refusal does not consume the id, so the provider can retry it.
    pub(crate) fn admit<F>(&self, delivery_id: String, work: F) -> AdmissionDecision
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reap_finished(&mut state.tasks);
        purge_expired(&mut state, self.shared.retention, Instant::now());

        if !state.accepting {
            return AdmissionDecision::Overloaded;
        }
        if state.seen.contains_key(&delivery_id) {
            return AdmissionDecision::Duplicate;
        }

        let permit = match self.shared.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return AdmissionDecision::Overloaded,
        };
        if state.seen.len() >= self.shared.dedupe_capacity {
            return AdmissionDecision::Overloaded;
        }

        let admitted_at = Instant::now();
        state
            .seen
            .insert(delivery_id.clone(), Seen::InFlight(admitted_at));
        let completion = Completion {
            shared: Arc::downgrade(&self.shared),
            delivery_id,
            admitted_at,
        };
        state.tasks.spawn(async move {
            let _permit = permit;
            let _completion = completion;
            work.await;
        });
        AdmissionDecision::Accepted
    }

    /// Stop admission and abort/join every task owned by this worker.
    pub(crate) async fn shutdown(&self) {
        let mut tasks = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.accepting = false;
            state.tasks.abort_all();
            std::mem::take(&mut state.tasks)
        };
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::error!(%error, "webhook background task failed");
            }
        }
    }
}

fn purge_expired(state: &mut State, retention: Duration, now: Instant) {
    while state
        .completed
        .front()
        .is_some_and(|(_, completed_at)| now.duration_since(*completed_at) >= retention)
    {
        let (delivery_id, completed_at) = state.completed.pop_front().expect("front exists");
        if state.seen.get(&delivery_id) == Some(&Seen::Completed(completed_at)) {
            state.seen.remove(&delivery_id);
        }
    }
}

fn reap_finished(tasks: &mut JoinSet<()>) {
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            tracing::error!(%error, "webhook background task failed");
        }
    }
}
