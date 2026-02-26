use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade},
    },
    response::Response as AxumResponse,
    routing::get,
};
use futures_util::{FutureExt, SinkExt, StreamExt, stream::SplitSink};
use runtime::{AgentRuntime, RuntimeStreamEventSender};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use types::{
    Context, GATEWAY_PROTOCOL_VERSION, GatewayAssistantDelta, GatewayCancelActiveTurn,
    GatewayClientFrame, GatewayClientHello, GatewayErrorFrame, GatewayHealthCheck,
    GatewayHealthStatus, GatewayHelloAck, GatewaySendTurn, GatewayServerFrame, GatewaySession,
    GatewaySessionCreated, GatewaySessionList, GatewaySessionSummary, GatewaySessionSwitched,
    GatewayTurnCancelled, GatewayTurnCompleted, GatewayTurnProgress, GatewayTurnStarted,
    GatewayTurnState, GatewayTurnStatus, InlineMedia, Message, MessageRole, ModelId, ProviderId,
    Response, RuntimeError, SessionRecord, SessionStore, StartupStatusReport, StreamItem,
};

mod session;
mod turn_runner;

#[cfg(test)]
mod tests;

use runtime::SchedulerNotifier;
pub use session::SessionState as GatewaySessionState;
use session::{ActiveTurnState, SessionState, UserState};
pub use turn_runner::{GatewayTurnRunner, RuntimeGatewayTurnRunner};

const WS_ROUTE: &str = "/ws";
const GATEWAY_CHANNEL_ID: &str = "tui";
const EVENT_BUFFER_CAPACITY: usize = 1024;
const MAX_INLINE_ATTACHMENTS_PER_TURN: usize = 4;
const MAX_INLINE_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_INLINE_ATTACHMENT_TOTAL_BYTES: usize =
    MAX_INLINE_ATTACHMENTS_PER_TURN * MAX_INLINE_ATTACHMENT_BYTES;

/// Default maximum number of concurrent top-level turns per user.
const DEFAULT_MAX_CONCURRENT_TURNS: u32 = 3;

pub struct GatewayServer {
    turn_runner: Arc<dyn GatewayTurnRunner>,
    startup_status: Option<StartupStatusReport>,
    /// Users keyed by user_id, each containing multiple sessions.
    users: RwLock<HashMap<String, Arc<UserState>>>,
    next_connection_id: AtomicU64,
    session_store: Option<Arc<dyn SessionStore>>,
    max_concurrent_turns: u32,
}

impl GatewayServer {
    pub fn new(turn_runner: Arc<dyn GatewayTurnRunner>) -> Self {
        Self::with_options(turn_runner, None, None, DEFAULT_MAX_CONCURRENT_TURNS)
    }

    pub fn with_startup_status(
        turn_runner: Arc<dyn GatewayTurnRunner>,
        startup_status: StartupStatusReport,
    ) -> Self {
        Self::with_options(
            turn_runner,
            Some(startup_status),
            None,
            DEFAULT_MAX_CONCURRENT_TURNS,
        )
    }

    pub fn with_session_store(
        turn_runner: Arc<dyn GatewayTurnRunner>,
        startup_status: Option<StartupStatusReport>,
        session_store: Arc<dyn SessionStore>,
    ) -> Self {
        Self::with_options(
            turn_runner,
            startup_status,
            Some(session_store),
            DEFAULT_MAX_CONCURRENT_TURNS,
        )
    }

    pub fn with_options(
        turn_runner: Arc<dyn GatewayTurnRunner>,
        startup_status: Option<StartupStatusReport>,
        session_store: Option<Arc<dyn SessionStore>>,
        max_concurrent_turns: u32,
    ) -> Self {
        Self {
            turn_runner,
            startup_status,
            users: RwLock::new(HashMap::new()),
            next_connection_id: AtomicU64::new(1),
            session_store,
            max_concurrent_turns,
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(WS_ROUTE, get(Self::upgrade_websocket))
            .with_state(self)
    }

    // -----------------------------------------------------------------------
    // Internal API (D10) — used by both the WebSocket handler and in-process
    // channel adapters (Telegram, Discord, etc.)
    // -----------------------------------------------------------------------

    /// Create a new session for a user or retrieve an existing one by ID.
    ///
    /// If `session_id` is `Some`, looks up the existing session (in-memory
    /// first, then falls back to the session store for resumed sessions).
    /// If `session_id` is `None`, creates a fresh session with a new UUID v7.
    pub async fn create_or_get_session(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        agent_name: &str,
        channel_origin: &str,
    ) -> Result<Arc<SessionState>, String> {
        let user = self.get_or_create_user(user_id).await;

        if let Some(id) = session_id {
            // Try to find in-memory first.
            let sessions = user.sessions.read().await;
            if let Some(existing) = sessions.get(id) {
                return Ok(Arc::clone(existing));
            }
            drop(sessions);

            // Try resuming from the session store.
            if let Some(store) = &self.session_store
                && let Ok(Some(record)) = store.get_session(id).await
            {
                let session = Arc::new(SessionState::new(
                    record.session_id.clone(),
                    record.user_id.clone(),
                    record.agent_name.clone(),
                    record.parent_session_id.clone(),
                    record.channel_origin.clone(),
                ));
                let mut sessions = user.sessions.write().await;
                sessions.insert(record.session_id.clone(), Arc::clone(&session));
                tracing::info!(
                    user_id = %user_id,
                    session_id = %id,
                    "gateway session resumed from store"
                );
                return Ok(session);
            }

            return Err(format!("session `{id}` not found for user `{user_id}`"));
        }

        // Create a new session.
        let new_session_id = uuid::Uuid::now_v7().to_string();
        let session = Arc::new(SessionState::new(
            new_session_id.clone(),
            user_id.to_owned(),
            agent_name.to_owned(),
            None,
            channel_origin.to_owned(),
        ));
        let mut sessions = user.sessions.write().await;
        sessions.insert(new_session_id.clone(), Arc::clone(&session));
        drop(sessions);

        // Persist to session store.
        if let Some(store) = &self.session_store {
            let record = SessionRecord {
                session_id: new_session_id.clone(),
                user_id: user_id.to_owned(),
                agent_name: agent_name.to_owned(),
                display_name: None,
                channel_origin: channel_origin.to_owned(),
                parent_session_id: None,
                created_at: chrono_now(),
                last_active_at: chrono_now(),
                archived: false,
            };
            if let Err(e) = store.create_session(&record).await {
                tracing::warn!(
                    session_id = %new_session_id,
                    error = %e,
                    "failed to persist session record"
                );
            }
        }

        tracing::info!(
            user_id = %user_id,
            session_id = %new_session_id,
            "gateway session created"
        );
        Ok(session)
    }

    /// Resolve a session by exact ID or unique prefix (like Docker short IDs).
    ///
    /// Checks in-memory sessions first, then the session store. Returns the
    /// matching session if exactly one prefix-match is found; an error if the
    /// prefix is ambiguous or matches nothing.
    async fn resolve_session_by_prefix(
        &self,
        user_id: &str,
        prefix: &str,
    ) -> Result<Arc<SessionState>, String> {
        let user = match self.users.read().await.get(user_id).cloned() {
            Some(u) => u,
            None => return Err(format!("session `{prefix}` not found for user `{user_id}`")),
        };

        // Try exact match first (fast path).
        let sessions = user.sessions.read().await;
        if let Some(existing) = sessions.get(prefix) {
            return Ok(Arc::clone(existing));
        }

        // Prefix match against in-memory sessions.
        let matches: Vec<_> = sessions
            .iter()
            .filter(|(id, _)| id.starts_with(prefix))
            .map(|(_, s)| Arc::clone(s))
            .collect();
        drop(sessions);

        if matches.len() == 1 {
            return Ok(matches.into_iter().next().unwrap());
        }
        if matches.len() > 1 {
            return Err(format!(
                "ambiguous session prefix `{prefix}`: matches {} sessions",
                matches.len()
            ));
        }

        // Fall back to session store for persisted sessions not yet in memory.
        if let Some(store) = &self.session_store {
            // Try exact match in store.
            if let Ok(Some(record)) = store.get_session(prefix).await {
                return self
                    .create_or_get_session(
                        user_id,
                        Some(&record.session_id),
                        &record.agent_name,
                        GATEWAY_CHANNEL_ID,
                    )
                    .await;
            }

            // Prefix match in store.
            if let Ok(all) = store.list_sessions(user_id, false).await {
                let store_matches: Vec<_> = all
                    .iter()
                    .filter(|r| r.session_id.starts_with(prefix))
                    .collect();

                if store_matches.len() == 1 {
                    return self
                        .create_or_get_session(
                            user_id,
                            Some(&store_matches[0].session_id),
                            &store_matches[0].agent_name,
                            GATEWAY_CHANNEL_ID,
                        )
                        .await;
                }
                if store_matches.len() > 1 {
                    return Err(format!(
                        "ambiguous session prefix `{prefix}`: matches {} sessions",
                        store_matches.len()
                    ));
                }
            }
        }

        Err(format!("session `{prefix}` not found for user `{user_id}`"))
    }

    /// Submit a user turn to a session.
    ///
    /// Returns `None` on success (the turn was started), or `Some(error_frame)`
    /// if the turn could not be started (active turn exists, concurrency limit).
    pub async fn submit_turn(
        &self,
        session: &Arc<SessionState>,
        send_turn: GatewaySendTurn,
    ) -> Option<GatewayServerFrame> {
        self.start_turn(session, send_turn).await
    }

    /// Cancel the active turn on a session.
    pub async fn cancel_session_turn(
        &self,
        session: &Arc<SessionState>,
        cancel_turn: GatewayCancelActiveTurn,
    ) -> Option<GatewayServerFrame> {
        self.cancel_turn(session, cancel_turn).await
    }

    /// Subscribe to server-sent events for a session.
    pub fn subscribe_events(
        &self,
        session: &Arc<SessionState>,
    ) -> broadcast::Receiver<GatewayServerFrame> {
        session.events.subscribe()
    }

    /// List all sessions for a user.
    pub async fn list_user_sessions(
        &self,
        user_id: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionRecord>, String> {
        if let Some(store) = &self.session_store {
            store
                .list_sessions(user_id, include_archived)
                .await
                .map_err(|e| e.to_string())
        } else {
            // Fallback: list from in-memory state.
            let users = self.users.read().await;
            if let Some(user) = users.get(user_id) {
                let sessions = user.sessions.read().await;
                let records = sessions
                    .values()
                    .map(|s| SessionRecord {
                        session_id: s.session_id.clone(),
                        user_id: s.user_id.clone(),
                        agent_name: s.agent_name.clone(),
                        display_name: None,
                        channel_origin: GATEWAY_CHANNEL_ID.to_owned(),
                        parent_session_id: s.parent_session_id.clone(),
                        created_at: String::new(),
                        last_active_at: String::new(),
                        archived: false,
                    })
                    .collect();
                Ok(records)
            } else {
                Ok(Vec::new())
            }
        }
    }

    // -----------------------------------------------------------------------
    // WebSocket handling
    // -----------------------------------------------------------------------

    async fn upgrade_websocket(
        State(server): State<Arc<Self>>,
        ws: WebSocketUpgrade,
    ) -> AxumResponse {
        ws.on_upgrade(move |socket| async move {
            server.handle_socket(socket).await;
        })
    }

    async fn handle_socket(self: Arc<Self>, socket: WebSocket) {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let (mut sender, mut receiver) = socket.split();

        let Some(first_message) = receiver.next().await else {
            return;
        };

        let first_message = match first_message {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(%connection_id, %error, "failed to read first websocket message");
                return;
            }
        };

        let hello = match parse_client_frame(first_message) {
            Ok(GatewayClientFrame::Hello(hello)) => hello,
            Ok(_) => {
                let _ = send_error_frame(
                    &mut sender,
                    None,
                    None,
                    None,
                    "expected hello as first frame",
                )
                .await;
                return;
            }
            Err(error) => {
                let _ = send_error_frame(&mut sender, None, None, None, error).await;
                return;
            }
        };

        if hello.protocol_version != GATEWAY_PROTOCOL_VERSION {
            let _ = send_error_frame(
                &mut sender,
                Some(hello.request_id),
                None,
                None,
                format!(
                    "unsupported protocol version {}; expected {}",
                    hello.protocol_version, GATEWAY_PROTOCOL_VERSION
                ),
            )
            .await;
            return;
        }

        let session = match self.resolve_session(&hello).await {
            Ok(session) => session,
            Err(message) => {
                let _ = send_error_frame(&mut sender, Some(hello.request_id), None, None, message)
                    .await;
                return;
            }
        };

        let mut updates = session.events.subscribe();
        let active_turn = session.active_turn_status().await;
        let gateway_session = session.gateway_session();

        let hello_ack = GatewayServerFrame::HelloAck(GatewayHelloAck {
            request_id: hello.request_id,
            protocol_version: GATEWAY_PROTOCOL_VERSION,
            session: gateway_session.clone(),
            active_turn,
        });
        if send_server_frame(&mut sender, &hello_ack).await.is_err() {
            return;
        }

        if session.active_turn_status().await.is_none()
            && let Some(outcome) = session.latest_terminal_frame().await
            && send_server_frame(&mut sender, &outcome).await.is_err()
        {
            return;
        }

        // Track this connection's active session for send/cancel routing.
        let mut active_session = Arc::clone(&session);

        loop {
            tokio::select! {
                incoming = receiver.next() => {
                    let Some(incoming) = incoming else {
                        break;
                    };
                    let incoming = match incoming {
                        Ok(message) => message,
                        Err(error) => {
                            tracing::debug!(%connection_id, %error, "websocket receive failed");
                            break;
                        }
                    };

                    match incoming {
                        AxumWsMessage::Close(_) => break,
                        AxumWsMessage::Ping(payload) => {
                            if sender.send(AxumWsMessage::Pong(payload)).await.is_err() {
                                break;
                            }
                        }
                        AxumWsMessage::Pong(_) => {}
                        message @ (AxumWsMessage::Text(_) | AxumWsMessage::Binary(_)) => {
                            let gateway_session = active_session.gateway_session();
                            let frame = match parse_client_frame(message) {
                                Ok(frame) => frame,
                                Err(error) => {
                                    if send_error_frame(&mut sender, None, Some(gateway_session), None, error).await.is_err() {
                                        break;
                                    }
                                    continue;
                                }
                            };

                            if self
                                .handle_client_frame(
                                    frame,
                                    &mut active_session,
                                    &mut updates,
                                    &mut sender,
                                )
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                update = updates.recv() => {
                    match update {
                        Ok(frame) => {
                            if send_server_frame(&mut sender, &frame).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            tracing::debug!(%connection_id, "gateway subscriber lagged; dropping stale updates");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    }

    async fn handle_client_frame(
        &self,
        frame: GatewayClientFrame,
        active_session: &mut Arc<SessionState>,
        updates: &mut broadcast::Receiver<GatewayServerFrame>,
        sender: &mut SplitSink<WebSocket, AxumWsMessage>,
    ) -> Result<(), ()> {
        let gateway_session = active_session.gateway_session();
        match frame {
            GatewayClientFrame::Hello(_) => {
                send_error_frame(
                    sender,
                    None,
                    Some(gateway_session),
                    None,
                    "hello already negotiated for this websocket connection",
                )
                .await
            }
            GatewayClientFrame::SendTurn(send_turn) => {
                if send_turn.session_id != active_session.session_id {
                    return send_error_frame(
                        sender,
                        Some(send_turn.request_id),
                        Some(gateway_session),
                        None,
                        "session_id does not match active session",
                    )
                    .await;
                }
                if let Some(error_frame) = self.start_turn(active_session, send_turn).await {
                    send_server_frame(sender, &error_frame).await
                } else {
                    Ok(())
                }
            }
            GatewayClientFrame::CancelActiveTurn(cancel_turn) => {
                if cancel_turn.session_id != active_session.session_id {
                    return send_error_frame(
                        sender,
                        Some(cancel_turn.request_id),
                        Some(gateway_session),
                        None,
                        "session_id does not match active session",
                    )
                    .await;
                }
                if let Some(error_frame) = self.cancel_turn(active_session, cancel_turn).await {
                    send_server_frame(sender, &error_frame).await
                } else {
                    Ok(())
                }
            }
            GatewayClientFrame::HealthCheck(health_check) => {
                let frame = self
                    .health_status(Arc::clone(active_session), health_check)
                    .await;
                send_server_frame(sender, &frame).await
            }
            GatewayClientFrame::CreateSession(create) => {
                let user_id = active_session.user_id.clone();
                let agent_name = create.agent_name.as_deref().unwrap_or("default");
                match self
                    .create_or_get_session(&user_id, None, agent_name, GATEWAY_CHANNEL_ID)
                    .await
                {
                    Ok(new_session) => {
                        // Update display_name in the session store if provided.
                        if let Some(ref name) = create.display_name
                            && let Some(store) = &self.session_store
                            && let Err(e) = store
                                .update_display_name(&new_session.session_id, name)
                                .await
                        {
                            tracing::warn!(
                                session_id = %new_session.session_id,
                                error = %e,
                                "failed to update session display_name"
                            );
                        }

                        // Switch the connection's active session.
                        *updates = new_session.events.subscribe();
                        *active_session = Arc::clone(&new_session);

                        let frame = GatewayServerFrame::SessionCreated(GatewaySessionCreated {
                            request_id: create.request_id,
                            session: new_session.gateway_session(),
                            display_name: create.display_name,
                            agent_name: new_session.agent_name.clone(),
                        });
                        send_server_frame(sender, &frame).await
                    }
                    Err(msg) => {
                        send_error_frame(
                            sender,
                            Some(create.request_id),
                            Some(gateway_session),
                            None,
                            msg,
                        )
                        .await
                    }
                }
            }
            GatewayClientFrame::ListSessions(list) => {
                let user_id = active_session.user_id.clone();
                match self
                    .list_user_sessions(&user_id, list.include_archived)
                    .await
                {
                    Ok(records) => {
                        let sessions = records
                            .into_iter()
                            .filter(|r| {
                                // Filter out subagent sessions unless requested.
                                list.include_subagent_sessions || r.parent_session_id.is_none()
                            })
                            .map(|r| GatewaySessionSummary {
                                session_id: r.session_id,
                                agent_name: r.agent_name,
                                display_name: r.display_name,
                                channel_origin: r.channel_origin,
                                created_at: r.created_at,
                                last_active_at: r.last_active_at,
                                archived: r.archived,
                            })
                            .collect();
                        let frame = GatewayServerFrame::SessionList(GatewaySessionList {
                            request_id: list.request_id,
                            sessions,
                        });
                        send_server_frame(sender, &frame).await
                    }
                    Err(msg) => {
                        send_error_frame(
                            sender,
                            Some(list.request_id),
                            Some(gateway_session),
                            None,
                            msg,
                        )
                        .await
                    }
                }
            }
            GatewayClientFrame::SwitchSession(switch) => {
                let user_id = active_session.user_id.clone();
                match self
                    .resolve_session_by_prefix(&user_id, &switch.session_id)
                    .await
                {
                    Ok(target_session) => {
                        // Unsubscribe from old session, subscribe to new one.
                        *updates = target_session.events.subscribe();
                        let active_turn = target_session.active_turn_status().await;
                        *active_session = Arc::clone(&target_session);

                        let frame = GatewayServerFrame::SessionSwitched(GatewaySessionSwitched {
                            request_id: switch.request_id,
                            session: target_session.gateway_session(),
                            active_turn,
                        });
                        send_server_frame(sender, &frame).await
                    }
                    Err(msg) => {
                        send_error_frame(
                            sender,
                            Some(switch.request_id),
                            Some(gateway_session),
                            None,
                            msg,
                        )
                        .await
                    }
                }
            }
        }
    }

    /// Resolve the session for a Hello handshake.
    ///
    /// - `create_new_session: true` → always create a fresh session.
    /// - `session_id: Some(id)` → find existing (or resume from store).
    /// - Neither → for backward compat, get-or-create the default session.
    async fn resolve_session(
        &self,
        hello: &GatewayClientHello,
    ) -> Result<Arc<SessionState>, String> {
        if hello.create_new_session {
            return self
                .create_or_get_session(&hello.user_id, None, "default", GATEWAY_CHANNEL_ID)
                .await;
        }

        if let Some(ref session_id) = hello.session_id {
            return self
                .create_or_get_session(
                    &hello.user_id,
                    Some(session_id),
                    "default",
                    GATEWAY_CHANNEL_ID,
                )
                .await;
        }

        // Backward-compat: get the default session or create one with a
        // deterministic ID (matching the old `default_session_id` behavior).
        let user = self.get_or_create_user(&hello.user_id).await;
        let default_id = default_session_id(&hello.user_id);

        // Try in-memory.
        {
            let sessions = user.sessions.read().await;
            if let Some(existing) = sessions.get(&default_id) {
                return Ok(Arc::clone(existing));
            }
        }

        // Try session store.
        if let Some(store) = &self.session_store
            && let Ok(Some(record)) = store.get_session(&default_id).await
        {
            let session = Arc::new(SessionState::new(
                record.session_id.clone(),
                record.user_id.clone(),
                record.agent_name.clone(),
                record.parent_session_id.clone(),
                record.channel_origin.clone(),
            ));
            let mut sessions = user.sessions.write().await;
            sessions.insert(default_id, Arc::clone(&session));
            return Ok(session);
        }

        // Create the default session.
        let session = Arc::new(SessionState::new(
            default_id.clone(),
            hello.user_id.clone(),
            "default".to_owned(),
            None,
            GATEWAY_CHANNEL_ID.to_owned(),
        ));
        let mut sessions = user.sessions.write().await;
        sessions.insert(default_id.clone(), Arc::clone(&session));
        drop(sessions);

        // Persist.
        if let Some(store) = &self.session_store {
            let record = SessionRecord {
                session_id: default_id.clone(),
                user_id: hello.user_id.clone(),
                agent_name: "default".to_owned(),
                display_name: None,
                channel_origin: GATEWAY_CHANNEL_ID.to_owned(),
                parent_session_id: None,
                created_at: chrono_now(),
                last_active_at: chrono_now(),
                archived: false,
            };
            if let Err(e) = store.create_session(&record).await {
                tracing::warn!(session_id = %default_id, error = %e, "failed to persist default session");
            }
        }

        tracing::info!(
            user_id = %hello.user_id,
            session_id = %default_id,
            "gateway session created (default)"
        );
        Ok(session)
    }

    async fn start_turn(
        &self,
        session: &Arc<SessionState>,
        send_turn: GatewaySendTurn,
    ) -> Option<GatewayServerFrame> {
        if let Err(message) = validate_inline_attachments(&send_turn.attachments) {
            return Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(send_turn.request_id),
                session: Some(session.gateway_session()),
                turn: None,
                message,
            }));
        }

        let cancellation = CancellationToken::new();
        {
            let mut active_turn = session.active_turn.lock().await;
            if let Some(existing) = active_turn.as_ref() {
                return Some(GatewayServerFrame::Error(GatewayErrorFrame {
                    request_id: Some(send_turn.request_id),
                    session: Some(session.gateway_session()),
                    turn: Some(GatewayTurnStatus {
                        turn_id: existing.turn_id.clone(),
                        state: GatewayTurnState::Running,
                    }),
                    message: "an active turn is already running".to_owned(),
                }));
            }
            *active_turn = Some(ActiveTurnState {
                turn_id: send_turn.turn_id.clone(),
                cancellation: cancellation.clone(),
            });
        }
        *session.latest_terminal_frame.lock().await = None;

        // Track concurrent turns per user.
        let user = self.get_or_create_user(&session.user_id).await;
        let current = user.increment_concurrent_turns();
        if current > self.max_concurrent_turns {
            user.decrement_concurrent_turns();
            {
                let mut active_turn = session.active_turn.lock().await;
                *active_turn = None;
            }
            return Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(send_turn.request_id),
                session: Some(session.gateway_session()),
                turn: None,
                message: format!(
                    "too many concurrent turns (limit: {})",
                    self.max_concurrent_turns
                ),
            }));
        }

        let running_turn = GatewayTurnStatus {
            turn_id: send_turn.turn_id.clone(),
            state: GatewayTurnState::Running,
        };
        session.publish(GatewayServerFrame::TurnStarted(GatewayTurnStarted {
            request_id: send_turn.request_id.clone(),
            session: session.gateway_session(),
            turn: running_turn.clone(),
        }));
        tracing::info!(
            turn_id = %send_turn.turn_id,
            session_id = %session.session_id,
            "turn started"
        );

        let runtime = Arc::clone(&self.turn_runner);
        let session = Arc::clone(session);
        let user_for_decrement = Arc::clone(&user);
        let session_store = self.session_store.clone();
        tokio::spawn(async move {
            let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamItem>();
            let channel_capabilities = Some(types::ChannelCapabilities::from_channel_origin(
                &session.channel_origin,
            ));
            let input = turn_runner::UserTurnInput {
                prompt: send_turn.prompt,
                attachments: send_turn.attachments,
            };
            let runtime_future = runtime.run_turn(
                &session.user_id,
                &session.session_id,
                input,
                cancellation,
                delta_tx,
                channel_capabilities,
            );
            tokio::pin!(runtime_future);

            let inner_result: Result<Result<Response, RuntimeError>, _> =
                std::panic::AssertUnwindSafe(async {
                    loop {
                        tokio::select! {
                            maybe_item = delta_rx.recv() => {
                                match maybe_item {
                                    Some(StreamItem::Text(delta)) => {
                                        session.publish(GatewayServerFrame::AssistantDelta(GatewayAssistantDelta {
                                            request_id: send_turn.request_id.clone(),
                                            session: session.gateway_session(),
                                            turn: running_turn.clone(),
                                            delta,
                                        }));
                                    }
                                    Some(StreamItem::Progress(progress)) => {
                                        session.publish(GatewayServerFrame::TurnProgress(GatewayTurnProgress {
                                            request_id: send_turn.request_id.clone(),
                                            session: session.gateway_session(),
                                            turn: running_turn.clone(),
                                            progress,
                                        }));
                                    }
                                    Some(StreamItem::Media(attachment)) => {
                                        session.publish(GatewayServerFrame::MediaAttachment(types::GatewayMediaAttachment {
                                            request_id: send_turn.request_id.clone(),
                                            session: session.gateway_session(),
                                            attachment,
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                            result = &mut runtime_future => {
                                return result;
                            }
                        }
                    }
                })
                .catch_unwind()
                .await;

            let terminal_frame = match inner_result {
                Ok(Ok(response)) => {
                    tracing::info!(turn_id = %send_turn.turn_id, "turn completed");
                    GatewayServerFrame::TurnCompleted(GatewayTurnCompleted {
                        request_id: send_turn.request_id.clone(),
                        session: session.gateway_session(),
                        turn: GatewayTurnStatus {
                            turn_id: send_turn.turn_id.clone(),
                            state: GatewayTurnState::Completed,
                        },
                        response,
                    })
                }
                Ok(Err(RuntimeError::Cancelled)) => {
                    tracing::info!(turn_id = %send_turn.turn_id, "turn cancelled");
                    GatewayServerFrame::TurnCancelled(GatewayTurnCancelled {
                        request_id: send_turn.request_id.clone(),
                        session: session.gateway_session(),
                        turn: GatewayTurnStatus {
                            turn_id: send_turn.turn_id.clone(),
                            state: GatewayTurnState::Cancelled,
                        },
                    })
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        turn_id = %send_turn.turn_id,
                        %error,
                        "turn failed"
                    );
                    GatewayServerFrame::Error(GatewayErrorFrame {
                        request_id: Some(send_turn.request_id.clone()),
                        session: Some(session.gateway_session()),
                        turn: Some(GatewayTurnStatus {
                            turn_id: send_turn.turn_id.clone(),
                            state: GatewayTurnState::Failed,
                        }),
                        message: error.to_string(),
                    })
                }
                Err(panic_payload) => {
                    let panic_message = panic_payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic_payload.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    tracing::error!(
                        turn_id = %send_turn.turn_id,
                        panic = %panic_message,
                        "turn panicked"
                    );
                    GatewayServerFrame::Error(GatewayErrorFrame {
                        request_id: Some(send_turn.request_id.clone()),
                        session: Some(session.gateway_session()),
                        turn: Some(GatewayTurnStatus {
                            turn_id: send_turn.turn_id.clone(),
                            state: GatewayTurnState::Failed,
                        }),
                        message: format!("internal error: turn panicked: {panic_message}"),
                    })
                }
            };

            {
                let mut active_turn = session.active_turn.lock().await;
                if active_turn
                    .as_ref()
                    .is_some_and(|turn| turn.turn_id == send_turn.turn_id)
                {
                    *active_turn = None;
                }
            }
            *session.latest_terminal_frame.lock().await = Some(terminal_frame.clone());
            session.publish(terminal_frame);

            // Decrement concurrent turn count after turn completes.
            user_for_decrement.decrement_concurrent_turns();

            // Touch session in store on turn completion.
            if let Some(store) = session_store
                && let Err(e) = store.touch_session(&session.session_id).await
            {
                tracing::warn!(
                    session_id = %session.session_id,
                    error = %e,
                    "failed to touch session after turn completion"
                );
            }
        });

        None
    }

    async fn cancel_turn(
        &self,
        session: &Arc<SessionState>,
        cancel_turn: GatewayCancelActiveTurn,
    ) -> Option<GatewayServerFrame> {
        let active_turn = session.active_turn.lock().await.clone();
        match active_turn {
            Some(active_turn) if active_turn.turn_id == cancel_turn.turn_id => {
                active_turn.cancellation.cancel();
                None
            }
            Some(active_turn) => Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(cancel_turn.request_id),
                session: Some(session.gateway_session()),
                turn: Some(GatewayTurnStatus {
                    turn_id: active_turn.turn_id,
                    state: GatewayTurnState::Running,
                }),
                message: "cancel request does not match active turn".to_owned(),
            })),
            None => Some(GatewayServerFrame::Error(GatewayErrorFrame {
                request_id: Some(cancel_turn.request_id),
                session: Some(session.gateway_session()),
                turn: None,
                message: "no active turn to cancel".to_owned(),
            })),
        }
    }

    async fn health_status(
        &self,
        session: Arc<SessionState>,
        health_check: GatewayHealthCheck,
    ) -> GatewayServerFrame {
        let startup_status = self.startup_status.clone();
        let message = startup_status
            .as_ref()
            .filter(|status| status.is_degraded())
            .map(|status| {
                format!(
                    "{GATEWAY_CHANNEL_ID} gateway ready with degraded startup: {}",
                    status
                        .degraded_reasons
                        .iter()
                        .map(|reason| reason.detail.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            })
            .unwrap_or_else(|| format!("{GATEWAY_CHANNEL_ID} gateway ready"));
        tracing::info!(
            user_id = %session.user_id,
            session_id = %session.session_id,
            startup_degraded = startup_status.as_ref().is_some_and(StartupStatusReport::is_degraded),
            "gateway health check handled"
        );
        GatewayServerFrame::HealthStatus(GatewayHealthStatus {
            request_id: health_check.request_id,
            healthy: true,
            session: Some(session.gateway_session()),
            active_turn: session.active_turn_status().await,
            startup_status,
            message: Some(message),
        })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn get_or_create_user(&self, user_id: &str) -> Arc<UserState> {
        // Fast path: read lock.
        {
            let users = self.users.read().await;
            if let Some(user) = users.get(user_id) {
                return Arc::clone(user);
            }
        }
        // Slow path: write lock.
        let mut users = self.users.write().await;
        users
            .entry(user_id.to_owned())
            .or_insert_with(|| Arc::new(UserState::new(user_id.to_owned())))
            .clone()
    }
}

impl SchedulerNotifier for GatewayServer {
    fn notify_user(&self, user_id: &str, frame: GatewayServerFrame) {
        if let Ok(users) = self.users.try_read()
            && let Some(user) = users.get(user_id)
            && let Ok(sessions) = user.sessions.try_read()
        {
            for session in sessions.values() {
                session.publish(frame.clone());
            }
        }
    }
}

fn default_session_id(user_id: &str) -> String {
    let normalized = user_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("runtime-{normalized}")
}

fn validate_inline_attachments(attachments: &[InlineMedia]) -> Result<(), String> {
    if attachments.len() > MAX_INLINE_ATTACHMENTS_PER_TURN {
        return Err(format!(
            "too many attachments (limit: {MAX_INLINE_ATTACHMENTS_PER_TURN})"
        ));
    }

    let mut total = 0usize;
    for attachment in attachments {
        if !is_valid_mime_type(attachment.mime_type.as_str()) {
            return Err(format!(
                "invalid attachment mime_type `{}`",
                attachment.mime_type
            ));
        }

        let size = attachment.data.len();
        if size > MAX_INLINE_ATTACHMENT_BYTES {
            return Err(format!(
                "attachment too large ({} bytes, limit: {} bytes)",
                size, MAX_INLINE_ATTACHMENT_BYTES
            ));
        }

        total = total.saturating_add(size);
        if total > MAX_INLINE_ATTACHMENT_TOTAL_BYTES {
            return Err(format!(
                "total attachment payload too large ({} bytes, limit: {} bytes)",
                total, MAX_INLINE_ATTACHMENT_TOTAL_BYTES
            ));
        }
    }
    Ok(())
}

fn is_valid_mime_type(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 255 {
        return false;
    }
    let Some((kind, subtype)) = trimmed.split_once('/') else {
        return false;
    };
    if kind.is_empty() || subtype.is_empty() {
        return false;
    }
    trimmed.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'+' | b'_')
    })
}

fn chrono_now() -> String {
    // Simple ISO-8601 timestamp without external chrono dependency.
    // The gateway crate doesn't need chrono; we produce a basic timestamp
    // suitable for the `TEXT` column type in SQLite.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as basic "seconds since epoch" — the session store can parse
    // this or use it as-is. For proper ISO-8601, the store implementation
    // handles the formatting (using `datetime('now')` in SQL).
    now.to_string()
}

fn parse_client_frame(message: AxumWsMessage) -> Result<GatewayClientFrame, String> {
    match message {
        AxumWsMessage::Text(payload) => {
            serde_json::from_str::<GatewayClientFrame>(payload.as_ref()).map_err(|error| {
                format!("failed to decode client frame from websocket text payload: {error}")
            })
        }
        AxumWsMessage::Binary(payload) => serde_json::from_slice::<GatewayClientFrame>(&payload)
            .map_err(|error| {
                format!("failed to decode client frame from websocket binary payload: {error}")
            }),
        _ => Err("unsupported websocket message type".to_owned()),
    }
}

async fn send_server_frame(
    sender: &mut SplitSink<WebSocket, AxumWsMessage>,
    frame: &GatewayServerFrame,
) -> Result<(), ()> {
    let payload = serde_json::to_string(frame).map_err(|error| {
        tracing::error!(%error, "failed to encode gateway server frame");
    })?;
    sender
        .send(AxumWsMessage::Text(payload.into()))
        .await
        .map_err(|error| {
            tracing::debug!(%error, "failed to send gateway websocket frame");
        })
}

async fn send_error_frame(
    sender: &mut SplitSink<WebSocket, AxumWsMessage>,
    request_id: Option<String>,
    session: Option<GatewaySession>,
    turn: Option<GatewayTurnStatus>,
    message: impl Into<String>,
) -> Result<(), ()> {
    send_server_frame(
        sender,
        &GatewayServerFrame::Error(GatewayErrorFrame {
            request_id,
            session,
            turn,
            message: message.into(),
        }),
    )
    .await
}
