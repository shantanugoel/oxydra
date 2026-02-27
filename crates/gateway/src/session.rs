use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use types::{GatewayServerFrame, GatewaySession, GatewayTurnState, GatewayTurnStatus};

use crate::EVENT_BUFFER_CAPACITY;

/// Per-user state grouping all sessions for a single user.
///
/// Each user has a set of sessions (keyed by session_id) and a counter
/// tracking how many top-level turns are running concurrently.
pub(crate) struct UserState {
    #[allow(dead_code)]
    pub(crate) user_id: String,
    pub(crate) sessions: RwLock<HashMap<String, Arc<SessionState>>>,
    /// Bounded top-level turn concurrency for this user.
    /// Subagent turns do NOT acquire permits (D11).
    pub(crate) turn_semaphore: Arc<Semaphore>,
    /// Number of top-level turns currently queued for a permit.
    queued_turns: AtomicUsize,
    max_queued_turns: usize,
}

impl UserState {
    pub(crate) fn new(
        user_id: String,
        max_concurrent_turns: usize,
        max_queued_turns: usize,
    ) -> Self {
        Self {
            user_id,
            sessions: RwLock::new(HashMap::new()),
            turn_semaphore: Arc::new(Semaphore::new(max_concurrent_turns)),
            queued_turns: AtomicUsize::new(0),
            max_queued_turns,
        }
    }

    pub(crate) fn try_enqueue_turn(&self) -> bool {
        loop {
            let current = self.queued_turns.load(Ordering::Relaxed);
            if current >= self.max_queued_turns {
                return false;
            }
            if self
                .queued_turns
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub(crate) async fn acquire_turn_permit(
        &self,
        cancellation: &CancellationToken,
    ) -> Option<OwnedSemaphorePermit> {
        let acquire = Arc::clone(&self.turn_semaphore).acquire_owned();
        tokio::pin!(acquire);
        let permit = tokio::select! {
            _ = cancellation.cancelled() => None,
            result = &mut acquire => result.ok(),
        };
        self.queued_turns.fetch_sub(1, Ordering::AcqRel);
        permit
    }
}

/// Active turn state tracked per-session.
#[derive(Clone)]
pub(crate) struct ActiveTurnState {
    pub(crate) turn_id: String,
    pub(crate) cancellation: CancellationToken,
}

/// Per-session state. Each session has its own broadcast channel, active turn
/// tracker, and metadata.
pub struct SessionState {
    pub session_id: String,
    pub(crate) user_id: String,
    pub(crate) agent_name: String,
    pub(crate) parent_session_id: Option<String>,
    /// The channel that created this session (e.g. "tui", "telegram").
    pub(crate) channel_origin: String,
    #[allow(dead_code)]
    pub(crate) created_at: Instant,
    pub(crate) events: broadcast::Sender<GatewayServerFrame>,
    pub(crate) active_turn: Mutex<Option<ActiveTurnState>>,
    pub(crate) latest_terminal_frame: Mutex<Option<GatewayServerFrame>>,
    last_activity_epoch_secs: AtomicU64,
}

impl SessionState {
    pub(crate) fn new(
        session_id: String,
        user_id: String,
        agent_name: String,
        parent_session_id: Option<String>,
        channel_origin: String,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self {
            session_id,
            user_id,
            agent_name,
            parent_session_id,
            channel_origin,
            created_at: Instant::now(),
            events,
            active_turn: Mutex::new(None),
            latest_terminal_frame: Mutex::new(None),
            last_activity_epoch_secs: AtomicU64::new(epoch_now_secs()),
        }
    }

    pub(crate) fn gateway_session(&self) -> GatewaySession {
        GatewaySession {
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
        }
    }

    pub(crate) async fn active_turn_status(&self) -> Option<GatewayTurnStatus> {
        self.active_turn
            .lock()
            .await
            .as_ref()
            .map(|active_turn| GatewayTurnStatus {
                turn_id: active_turn.turn_id.clone(),
                state: GatewayTurnState::Running,
            })
    }

    pub(crate) async fn latest_terminal_frame(&self) -> Option<GatewayServerFrame> {
        self.latest_terminal_frame.lock().await.clone()
    }

    pub(crate) fn mark_active(&self) {
        self.last_activity_epoch_secs
            .store(epoch_now_secs(), Ordering::Relaxed);
    }

    pub(crate) fn idle_for_at_least(&self, ttl: Duration) -> bool {
        let last = self.last_activity_epoch_secs.load(Ordering::Relaxed);
        epoch_now_secs().saturating_sub(last) >= ttl.as_secs()
    }

    #[cfg(test)]
    pub(crate) fn rewind_last_activity_for_tests(&self, idle_secs: u64) {
        self.last_activity_epoch_secs.store(
            epoch_now_secs().saturating_sub(idle_secs),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn publish(&self, frame: GatewayServerFrame) {
        let _ = self.events.send(frame);
    }
}

fn epoch_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
