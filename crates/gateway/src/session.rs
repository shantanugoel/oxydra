use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use tokio::sync::{Mutex, RwLock, broadcast};
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
    /// Number of top-level turns currently executing for this user.
    /// Subagent turns do NOT count toward this limit (D11).
    pub(crate) concurrent_turns: AtomicU32,
}

impl UserState {
    pub(crate) fn new(user_id: String) -> Self {
        Self {
            user_id,
            sessions: RwLock::new(HashMap::new()),
            concurrent_turns: AtomicU32::new(0),
        }
    }

    pub(crate) fn increment_concurrent_turns(&self) -> u32 {
        self.concurrent_turns.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn decrement_concurrent_turns(&self) {
        self.concurrent_turns.fetch_sub(1, Ordering::Relaxed);
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

    pub(crate) fn publish(&self, frame: GatewayServerFrame) {
        let _ = self.events.send(frame);
    }
}
