use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use mockall::mock;
use serde_json::{Value, json};
#[cfg(unix)]
use shell_daemon::ShellDaemonServer;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use types::{
    CatalogProvider, Context, FunctionDecl, Memory, MemoryChunkUpsertRequest,
    MemoryChunkUpsertResponse, MemoryError, MemoryForgetRequest, MemoryHybridQueryRequest,
    MemoryHybridQueryResult, MemoryRecallRequest, MemoryRecord, MemoryRetrieval,
    MemoryStoreRequest, MemorySummaryReadRequest, MemorySummaryState, MemorySummaryWriteRequest,
    MemorySummaryWriteResult, Message, MessageRole, ModelCatalog, ModelDescriptor, ModelId,
    ModelLimits, Provider, ProviderCaps, ProviderError, ProviderId, Response,
    RunnerBootstrapEnvelope, RuntimeProgressEvent, RuntimeProgressKind, SafetyTier, SandboxTier,
    SidecarEndpoint, SidecarTransport, StreamItem, Tool, ToolCall, ToolCallDelta, ToolError,
    ToolExecutionContext, UsageUpdate,
};

use super::{AgentRuntime, RuntimeLimits};
use tools::{ToolRegistry, bootstrap_runtime_tools};

mock! {
    ProviderContract {}
    #[async_trait]
    impl Provider for ProviderContract {
        fn provider_id(&self) -> &ProviderId;
        fn catalog_provider_id(&self) -> &ProviderId;
        fn model_catalog(&self) -> &ModelCatalog;
        fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError>;
        async fn complete(&self, context: &Context) -> Result<Response, ProviderError>;
        async fn stream(
            &self,
            context: &Context,
            buffer_size: usize,
        ) -> Result<types::ProviderStream, ProviderError>;
    }
}

mock! {
    ToolContract {}
    #[async_trait]
    impl Tool for ToolContract {
        fn schema(&self) -> FunctionDecl;
        async fn execute(&self, args: &str, context: &ToolExecutionContext) -> Result<String, ToolError>;
        fn timeout(&self) -> Duration;
        fn safety_tier(&self) -> SafetyTier;
    }
}

#[derive(Debug)]
enum ProviderStep {
    Stream(Vec<Result<StreamItem, ProviderError>>),
    StreamFailure(ProviderError),
    Complete(Response),
    CompleteDelayed { response: Response, delay: Duration },
}

struct FakeProvider {
    provider_id: ProviderId,
    model_catalog: ModelCatalog,
    caps: ProviderCaps,
    steps: Mutex<VecDeque<ProviderStep>>,
}

impl FakeProvider {
    fn new(provider_id: ProviderId, model_catalog: ModelCatalog, steps: Vec<ProviderStep>) -> Self {
        // Derive caps from the first model in the catalog
        let caps = model_catalog
            .all_models()
            .next()
            .map(|(_, _, descriptor)| descriptor.to_provider_caps())
            .unwrap_or_default();
        Self {
            provider_id,
            model_catalog,
            caps,
            steps: Mutex::new(steps.into()),
        }
    }

    fn with_caps(mut self, caps: ProviderCaps) -> Self {
        self.caps = caps;
        self
    }

    fn next_step(&self) -> ProviderStep {
        self.steps
            .lock()
            .expect("test provider mutex should not be poisoned")
            .pop_front()
            .expect("test provider expected another scripted step")
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    fn model_catalog(&self) -> &ModelCatalog {
        &self.model_catalog
    }

    fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError> {
        // Validate the model exists, then return the stored caps
        self.model_catalog.validate(self.provider_id(), model)?;
        Ok(self.caps.clone())
    }

    async fn complete(&self, _context: &Context) -> Result<Response, ProviderError> {
        match self.next_step() {
            ProviderStep::Complete(response) => Ok(response),
            ProviderStep::CompleteDelayed { response, delay } => {
                sleep(delay).await;
                Ok(response)
            }
            other => Err(ProviderError::RequestFailed {
                provider: self.provider_id.clone(),
                message: format!("unexpected provider step for complete: {other:?}"),
            }),
        }
    }

    async fn stream(
        &self,
        _context: &Context,
        _buffer_size: usize,
    ) -> Result<types::ProviderStream, ProviderError> {
        match self.next_step() {
            ProviderStep::Stream(items) => {
                let (sender, receiver) = mpsc::channel(items.len().max(1));
                for item in items {
                    sender
                        .try_send(item)
                        .expect("test stream channel should accept scripted item");
                }
                drop(sender);
                Ok(receiver)
            }
            ProviderStep::StreamFailure(error) => Err(error),
            other => Err(ProviderError::RequestFailed {
                provider: self.provider_id.clone(),
                message: format!("unexpected provider step for stream: {other:?}"),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MockReadTool;

#[async_trait]
impl Tool for MockReadTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            "file_read",
            None,
            serde_json::json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let parsed: Value = serde_json::from_str(args)?;
        let path = parsed.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments {
                tool: "file_read".to_owned(),
                message: "missing `path`".to_owned(),
            }
        })?;
        Ok(format!("mock read: {path}"))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

#[derive(Debug, Clone, Copy)]
struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            "slow_tool",
            None,
            serde_json::json!({ "type": "object", "properties": {} }),
        )
    }

    async fn execute(
        &self,
        _args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        sleep(Duration::from_millis(50)).await;
        Ok("done".to_owned())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

#[derive(Debug, Clone, Copy)]
struct SensitiveOutputTool;

#[async_trait]
impl Tool for SensitiveOutputTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            "sensitive_output",
            None,
            serde_json::json!({ "type": "object", "properties": {} }),
        )
    }

    async fn execute(
        &self,
        _args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        Ok(
            "api_key=sk_live_ABC123DEF456GHI789JKL012MNO345PQR678\nartifact_id=A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v2\nstatus=ok"
                .to_owned(),
        )
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

#[derive(Default)]
struct RecordingMemory {
    records: Mutex<Vec<MemoryRecord>>,
    hybrid_query_requests: Mutex<Vec<MemoryHybridQueryRequest>>,
    hybrid_query_results: Mutex<Vec<MemoryHybridQueryResult>>,
    summary_state: Mutex<Option<MemorySummaryState>>,
    summary_write_requests: Mutex<Vec<MemorySummaryWriteRequest>>,
    force_stale_summary_write: bool,
}

impl RecordingMemory {
    fn with_records(records: Vec<MemoryRecord>) -> Self {
        Self {
            records: Mutex::new(records),
            hybrid_query_requests: Mutex::new(Vec::new()),
            hybrid_query_results: Mutex::new(Vec::new()),
            summary_state: Mutex::new(None),
            summary_write_requests: Mutex::new(Vec::new()),
            force_stale_summary_write: false,
        }
    }

    fn with_hybrid_query_results(results: Vec<MemoryHybridQueryResult>) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            hybrid_query_requests: Mutex::new(Vec::new()),
            hybrid_query_results: Mutex::new(results),
            summary_state: Mutex::new(None),
            summary_write_requests: Mutex::new(Vec::new()),
            force_stale_summary_write: false,
        }
    }

    fn with_stale_summary_writes(results: Vec<MemoryHybridQueryResult>) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            hybrid_query_requests: Mutex::new(Vec::new()),
            hybrid_query_results: Mutex::new(results),
            summary_state: Mutex::new(None),
            summary_write_requests: Mutex::new(Vec::new()),
            force_stale_summary_write: true,
        }
    }

    fn recorded_hybrid_queries(&self) -> Vec<MemoryHybridQueryRequest> {
        self.hybrid_query_requests
            .lock()
            .expect("memory test mutex should not be poisoned")
            .clone()
    }

    fn recorded_summary_writes(&self) -> Vec<MemorySummaryWriteRequest> {
        self.summary_write_requests
            .lock()
            .expect("memory test mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl Memory for RecordingMemory {
    async fn store(&self, request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError> {
        let record = MemoryRecord {
            session_id: request.session_id,
            sequence: request.sequence,
            payload: request.payload,
        };
        self.records
            .lock()
            .expect("memory test mutex should not be poisoned")
            .push(record.clone());
        Ok(record)
    }

    async fn recall(&self, request: MemoryRecallRequest) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut records: Vec<MemoryRecord> = self
            .records
            .lock()
            .expect("memory test mutex should not be poisoned")
            .iter()
            .filter(|record| record.session_id == request.session_id)
            .cloned()
            .collect();
        records.sort_by_key(|record| record.sequence);
        if let Some(limit) = request.limit {
            let keep = usize::try_from(limit).unwrap_or(usize::MAX);
            if records.len() > keep {
                records = records[records.len().saturating_sub(keep)..].to_vec();
            }
        }
        if records.is_empty() {
            return Err(MemoryError::NotFound {
                session_id: request.session_id,
            });
        }
        Ok(records)
    }

    async fn forget(&self, request: MemoryForgetRequest) -> Result<(), MemoryError> {
        let mut records = self
            .records
            .lock()
            .expect("memory test mutex should not be poisoned");
        let before = records.len();
        records.retain(|record| record.session_id != request.session_id);
        if records.len() == before {
            return Err(MemoryError::NotFound {
                session_id: request.session_id,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryRetrieval for RecordingMemory {
    async fn upsert_chunks(
        &self,
        _request: MemoryChunkUpsertRequest,
    ) -> Result<MemoryChunkUpsertResponse, MemoryError> {
        Ok(MemoryChunkUpsertResponse {
            upserted_chunks: 0,
            skipped_chunks: 0,
        })
    }

    async fn hybrid_query(
        &self,
        request: MemoryHybridQueryRequest,
    ) -> Result<Vec<MemoryHybridQueryResult>, MemoryError> {
        self.hybrid_query_requests
            .lock()
            .expect("memory test mutex should not be poisoned")
            .push(request);
        Ok(self
            .hybrid_query_results
            .lock()
            .expect("memory test mutex should not be poisoned")
            .clone())
    }

    async fn read_summary_state(
        &self,
        request: MemorySummaryReadRequest,
    ) -> Result<Option<MemorySummaryState>, MemoryError> {
        Ok(self
            .summary_state
            .lock()
            .expect("memory test mutex should not be poisoned")
            .clone()
            .filter(|state| state.session_id == request.session_id))
    }

    async fn write_summary_state(
        &self,
        request: MemorySummaryWriteRequest,
    ) -> Result<MemorySummaryWriteResult, MemoryError> {
        self.summary_write_requests
            .lock()
            .expect("memory test mutex should not be poisoned")
            .push(request.clone());
        if self.force_stale_summary_write {
            return Ok(MemorySummaryWriteResult {
                applied: false,
                current_epoch: request.expected_epoch.saturating_add(1),
            });
        }

        let mut state = self
            .summary_state
            .lock()
            .expect("memory test mutex should not be poisoned");
        let current_epoch = state.as_ref().map_or(0, |current| current.epoch);
        if current_epoch != request.expected_epoch {
            return Ok(MemorySummaryWriteResult {
                applied: false,
                current_epoch,
            });
        }
        *state = Some(MemorySummaryState {
            session_id: request.session_id,
            epoch: request.next_epoch,
            upper_sequence: request.upper_sequence,
            summary: request.summary,
            metadata: request.metadata,
        });
        Ok(MemorySummaryWriteResult {
            applied: true,
            current_epoch: request.next_epoch,
        })
    }

    async fn store_note(
        &self,
        _session_id: &str,
        _note_id: &str,
        _content: &str,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn delete_note(&self, _session_id: &str, _note_id: &str) -> Result<bool, MemoryError> {
        Ok(false)
    }
}

fn mock_provider(
    provider_id: ProviderId,
    model_id: ModelId,
    supports_streaming: bool,
) -> MockProviderContract {
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider.expect_model_catalog().return_const(test_catalog(
        provider_id,
        model_id,
        supports_streaming,
    ));
    let caps = ProviderCaps {
        supports_streaming,
        supports_tools: true,
        ..ProviderCaps::default()
    };
    provider
        .expect_capabilities()
        .returning(move |_| Ok(caps.clone()));
    provider
}

#[tokio::test]
async fn run_session_uses_complete_path_when_streaming_is_disabled() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), false),
        vec![ProviderStep::Complete(assistant_response(
            "final answer",
            vec![],
        ))],
    )
    .with_caps(ProviderCaps {
        supports_streaming: false,
        supports_tools: true,
        ..ProviderCaps::default()
    });
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime turn should complete");

    assert_eq!(response.message.content.as_deref(), Some("final answer"));
    assert_eq!(context.messages.len(), 2);
    assert!(matches!(context.messages[1].role, MessageRole::Assistant));
}

#[tokio::test]
async fn run_session_reconstructs_streamed_tool_calls_and_loops_until_done() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("checking file".to_owned())),
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("file_read".to_owned()),
                    arguments: Some("{\"path\":\"Car".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: Some("go.toml\"}".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("done".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );
    let mut tools = ToolRegistry::default();
    tools.register("file_read", MockReadTool);
    let runtime = AgentRuntime::new(Box::new(provider), tools, RuntimeLimits::default());
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime turn should complete");

    assert_eq!(response.message.content.as_deref(), Some("done"));
    assert_eq!(context.messages.len(), 4);
    assert!(matches!(context.messages[1].role, MessageRole::Assistant));
    assert_eq!(context.messages[1].tool_calls.len(), 1);
    assert!(matches!(context.messages[2].role, MessageRole::Tool));
    assert_eq!(context.messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(
        context.messages[2].content.as_deref(),
        Some("mock read: Cargo.toml")
    );
    assert!(matches!(context.messages[3].role, MessageRole::Assistant));
}

#[tokio::test]
async fn run_session_for_session_with_stream_events_forwards_provider_deltas() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![ProviderStep::Stream(vec![
            Ok(StreamItem::Text("hello".to_owned())),
            Ok(StreamItem::FinishReason("stop".to_owned())),
        ])],
    );
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();

    let response = runtime
        .run_session_for_session_with_stream_events(
            "stream-session",
            &mut context,
            &CancellationToken::new(),
            events_tx,
        )
        .await
        .expect("runtime turn should complete with stream observer");

    let mut observed = Vec::new();
    while let Some(item) = events_rx.recv().await {
        observed.push(item);
    }

    assert_eq!(response.message.content.as_deref(), Some("hello"));
    assert_eq!(
        observed,
        vec![
            StreamItem::Progress(RuntimeProgressEvent {
                kind: RuntimeProgressKind::ProviderCall,
                message: "[1/8] Calling provider".to_owned(),
                turn: 1,
                max_turns: 8,
            }),
            StreamItem::Text("hello".to_owned()),
            StreamItem::FinishReason("stop".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn run_session_executes_bash_via_bootstrap_sidecar_backend() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_bash".to_owned()),
                    name: Some("shell_exec".to_owned()),
                    arguments: Some(r#"{"command":"printf ws5-runtime"}"#.to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("done".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );

    let socket_path = temp_socket_path("runtime-sidecar");
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("test unix listener should bind");
    let server = ShellDaemonServer::default();
    let server_task = tokio::spawn({
        let server = server.clone();
        async move { server.serve_unix_listener(listener).await }
    });

    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Container,
        workspace_root: "/tmp/oxydra-runtime-test".to_owned(),
        sidecar_endpoint: Some(SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: socket_path.to_string_lossy().into_owned(),
        }),
        runtime_policy: None,
        startup_status: None,
    };
    let bootstrap_tools = bootstrap_runtime_tools(Some(&bootstrap), None).await;
    assert!(bootstrap_tools.availability.shell.is_ready());
    let runtime = AgentRuntime::new(
        Box::new(provider),
        bootstrap_tools.registry,
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should execute sidecar-backed bash tool call");
    assert_eq!(response.message.content.as_deref(), Some("done"));
    let tool_result = context
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("runtime should append bash tool result message");
    assert_eq!(tool_result.content.as_deref(), Some("ws5-runtime"));

    server_task.abort();
    let _ = std::fs::remove_file(socket_path);
}

#[tokio::test]
async fn run_session_emits_explicit_shell_disabled_error_when_sidecar_is_unavailable() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_bash".to_owned()),
                    name: Some("shell_exec".to_owned()),
                    arguments: Some(r#"{"command":"printf blocked"}"#.to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text(
                    "shell unavailable acknowledged".to_owned(),
                )),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );

    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Process,
        workspace_root: "/tmp/oxydra-runtime-test".to_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
    };
    let bootstrap_tools = bootstrap_runtime_tools(Some(&bootstrap), None).await;
    assert!(!bootstrap_tools.availability.shell.is_ready());
    let runtime = AgentRuntime::new(
        Box::new(provider),
        bootstrap_tools.registry,
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should continue after explicit disabled shell tool result");
    assert_eq!(
        response.message.content.as_deref(),
        Some("shell unavailable acknowledged")
    );
    let tool_result = context
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result should be appended for disabled bash call");
    assert!(
        tool_result
            .content
            .as_deref()
            .is_some_and(|content| content.contains("shell tool is disabled"))
    );
}

#[tokio::test]
async fn run_session_injects_security_policy_denial_for_out_of_workspace_file_access() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let workspace_root = temp_workspace_root("runtime-policy-workspace");
    let outside_root = temp_workspace_root("runtime-policy-outside");
    let outside_file = outside_root.join("outside.txt");
    std::fs::write(&outside_file, "outside").expect("outside file should be writable");
    let outside_file_path = outside_file.to_string_lossy().into_owned();
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_read".to_owned()),
                    name: Some("file_read".to_owned()),
                    arguments: Some(json!({ "path": outside_file_path }).to_string()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("policy denial handled".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );
    let bootstrap = RunnerBootstrapEnvelope {
        user_id: "alice".to_owned(),
        sandbox_tier: SandboxTier::Process,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        sidecar_endpoint: None,
        runtime_policy: None,
        startup_status: None,
    };
    let bootstrap_tools = bootstrap_runtime_tools(Some(&bootstrap), None).await;
    let runtime = AgentRuntime::new(
        Box::new(provider),
        bootstrap_tools.registry,
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should continue after security-policy denial");
    assert_eq!(
        response.message.content.as_deref(),
        Some("policy denial handled")
    );
    let tool_result = context
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result should be appended for denied path");
    assert!(tool_result.content.as_deref().is_some_and(|content| {
        content.contains("blocked by security policy")
            && content.contains("PathOutsideAllowedRoots")
    }));

    let _ = std::fs::remove_dir_all(workspace_root);
    let _ = std::fs::remove_dir_all(outside_root);
}

#[tokio::test]
async fn run_session_injects_unknown_tool_error_for_legacy_alias_after_cutover() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("legacy_call".to_owned()),
                    name: Some("read_file".to_owned()),
                    arguments: Some(r#"{"path":"Cargo.toml"}"#.to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("legacy tool rejected".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );
    let bootstrap_tools = bootstrap_runtime_tools(None, None).await;
    let runtime = AgentRuntime::new(
        Box::new(provider),
        bootstrap_tools.registry,
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should continue after unknown-tool injection");

    assert_eq!(
        response.message.content.as_deref(),
        Some("legacy tool rejected")
    );
    let tool_result = context
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("unknown tool error should be injected as tool result");
    assert!(
        tool_result
            .content
            .as_deref()
            .is_some_and(|content| content.contains("unknown tool `read_file`"))
    );
}

#[tokio::test]
async fn run_session_scrubs_credential_like_tool_output_before_context_injection() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("sensitive_call".to_owned()),
                    name: Some("sensitive_output".to_owned()),
                    arguments: Some("{}".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("scrubbed".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );
    let mut registry = ToolRegistry::new(1024);
    registry.register("sensitive_output", SensitiveOutputTool);

    let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should continue after scrubbing sensitive output");
    assert_eq!(response.message.content.as_deref(), Some("scrubbed"));

    let tool_result = context
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("tool result should be appended");
    let scrubbed = tool_result
        .content
        .as_ref()
        .expect("tool result should include scrubbed content");
    assert!(scrubbed.contains("api_key=[REDACTED]"));
    assert!(scrubbed.contains("artifact_id=[REDACTED]"));
    assert!(scrubbed.contains("status=ok"));
    assert!(!scrubbed.contains("sk_live_ABC123DEF456GHI789JKL012MNO345PQR678"));
    assert!(!scrubbed.contains("A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v2"));
}

#[tokio::test]
async fn run_session_falls_back_to_complete_after_stream_failure() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::StreamFailure(ProviderError::Transport {
                provider: provider_id.clone(),
                message: "stream failed".to_owned(),
            }),
            ProviderStep::Complete(assistant_response("fallback response", vec![])),
        ],
    );
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should fall back to complete");

    assert_eq!(
        response.message.content.as_deref(),
        Some("fallback response")
    );
    assert_eq!(context.messages.len(), 2);
}

#[tokio::test]
async fn run_session_rejects_invalid_guard_preconditions() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), false),
        vec![ProviderStep::Complete(assistant_response("unused", vec![]))],
    );
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits {
            turn_timeout: Duration::from_secs(1),
            max_turns: 0,
            max_cost: None,
            ..RuntimeLimits::default()
        },
    );
    let mut context = test_context(provider_id, model_id);

    let error = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect_err("invalid guard preconditions should fail");
    assert!(matches!(error, types::RuntimeError::BudgetExceeded));
}

#[tokio::test]
async fn run_session_supports_mockall_provider_single_turn_without_tools() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = mock_provider(provider_id.clone(), model_id.clone(), false);
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .returning(|_| Ok(assistant_response("mockall response", vec![])));

    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("mockall provider should produce one final turn");

    assert_eq!(
        response.message.content.as_deref(),
        Some("mockall response")
    );
    assert!(response.tool_calls.is_empty());
}

#[tokio::test]
async fn run_session_exposes_registered_tools_to_provider_context() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = mock_provider(provider_id.clone(), model_id.clone(), false);
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(|context| context.tools.iter().any(|tool| tool.name == "file_read"))
        .returning(|_| Ok(assistant_response("tool schema seen", vec![])));

    let mut tools = ToolRegistry::default();
    tools.register("file_read", MockReadTool);
    let runtime = AgentRuntime::new(Box::new(provider), tools, RuntimeLimits::default());
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should expose tools before provider call");

    assert_eq!(
        response.message.content.as_deref(),
        Some("tool schema seen")
    );
    assert!(context.tools.iter().any(|tool| tool.name == "file_read"));
}

#[tokio::test]
async fn run_session_for_session_persists_initial_context_and_new_turns() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), false),
        vec![ProviderStep::Complete(assistant_response(
            "final answer",
            vec![],
        ))],
    )
    .with_caps(ProviderCaps {
        supports_streaming: false,
        supports_tools: true,
        ..ProviderCaps::default()
    });
    let memory = Arc::new(RecordingMemory::default());
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    )
    .with_memory(memory.clone());
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session_for_session("session-persist", &mut context, &CancellationToken::new())
        .await
        .expect("runtime turn should complete");
    assert_eq!(response.message.content.as_deref(), Some("final answer"));

    let stored = memory
        .recall(MemoryRecallRequest {
            session_id: "session-persist".to_owned(),
            limit: None,
        })
        .await
        .expect("persisted records should be recallable");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].sequence, 1);
    assert_eq!(stored[1].sequence, 2);
    let restored_first: Message =
        serde_json::from_value(stored[0].payload.clone()).expect("payload should deserialize");
    let restored_second: Message =
        serde_json::from_value(stored[1].payload.clone()).expect("payload should deserialize");
    assert_eq!(restored_first.role, MessageRole::User);
    assert_eq!(restored_second.role, MessageRole::Assistant);
}

#[tokio::test]
async fn restore_session_hydrates_context_when_memory_is_configured() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let memory = Arc::new(RecordingMemory::with_records(vec![
        MemoryRecord {
            session_id: "session-restore".to_owned(),
            sequence: 1,
            payload: serde_json::to_value(Message {
                role: MessageRole::User,
                content: Some("hello".to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            })
            .expect("message should serialize"),
        },
        MemoryRecord {
            session_id: "session-restore".to_owned(),
            sequence: 2,
            payload: serde_json::to_value(Message {
                role: MessageRole::Assistant,
                content: Some("world".to_owned()),
                tool_calls: vec![],
                tool_call_id: None,
            })
            .expect("message should serialize"),
        },
    ]));
    let runtime = AgentRuntime::new(
        Box::new(FakeProvider::new(
            provider_id.clone(),
            test_catalog(provider_id.clone(), model_id.clone(), false),
            vec![ProviderStep::Complete(assistant_response("unused", vec![]))],
        )),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    )
    .with_memory(memory);
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![],
    };

    runtime
        .restore_session("session-restore", &mut context, None)
        .await
        .expect("restore should succeed");

    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[0].content.as_deref(), Some("hello"));
    assert_eq!(context.messages[1].content.as_deref(), Some("world"));
}

#[tokio::test]
async fn run_session_for_session_keeps_existing_behavior_without_memory_backend() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), false),
        vec![ProviderStep::Complete(assistant_response(
            "no memory configured",
            vec![],
        ))],
    )
    .with_caps(ProviderCaps {
        supports_streaming: false,
        supports_tools: true,
        ..ProviderCaps::default()
    });
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session_for_session("session-disabled", &mut context, &CancellationToken::new())
        .await
        .expect("session run should still succeed without configured memory");
    assert_eq!(
        response.message.content.as_deref(),
        Some("no memory configured")
    );
    assert_eq!(context.messages.len(), 2);
}

#[tokio::test]
async fn run_session_recovers_from_validation_error() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("file_read".to_owned()),
                    // Missing "path" property, should trigger validation error
                    arguments: Some("{}".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text(
                    "Oh I missed the path argument!".to_owned(),
                )),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );

    let mut tool = MockToolContract::new();
    tool.expect_schema().return_const(FunctionDecl::new(
        "file_read",
        None,
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" }
            }
        }),
    ));
    tool.expect_safety_tier().return_const(SafetyTier::ReadOnly);
    // Execute should not be called because validation intercepts it!
    tool.expect_execute().times(0);

    let mut registry = ToolRegistry::new(1024);
    registry.register("file_read", tool);

    let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
    let mut context = Context {
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("read".to_owned()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        tools: Vec::new(),
        model: model_id,
        provider: provider_id,
    };

    let result = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("should complete despite validation error");

    assert_eq!(
        result.message.content.as_deref(),
        Some("Oh I missed the path argument!")
    );
    // Context should contain the injected tool error
    let tool_result = context
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    let err_msg = tool_result.content.as_ref().unwrap();
    assert!(err_msg.contains("schema validation failed"));
    assert!(err_msg.contains("\"path\" is a required property"));
}

#[tokio::test]
async fn run_session_recovers_from_malformed_streamed_json_arguments() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("file_read".to_owned()),
                    arguments: Some("{\"path\":\"Cargo.toml\"".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("retrying with corrected args".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );

    let mut tool = MockToolContract::new();
    tool.expect_schema().return_const(FunctionDecl::new(
        "file_read",
        None,
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" }
            }
        }),
    ));
    tool.expect_safety_tier().return_const(SafetyTier::ReadOnly);
    tool.expect_execute().times(0);

    let mut registry = ToolRegistry::new(1024);
    registry.register("file_read", tool);

    let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
    let mut context = Context {
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("read".to_owned()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        tools: Vec::new(),
        model: model_id,
        provider: provider_id,
    };

    let result = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("should complete despite malformed JSON arguments");

    assert_eq!(
        result.message.content.as_deref(),
        Some("retrying with corrected args")
    );
    let tool_result = context
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("tool result should be injected into context");
    let err_msg = tool_result
        .content
        .as_ref()
        .expect("tool result should contain error text");
    assert!(err_msg.contains("invalid JSON arguments payload"));
    assert!(err_msg.contains("EOF while parsing"));
}

#[tokio::test]
async fn run_session_executes_readonly_tools_in_parallel() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("slow_tool".to_owned()),
                    arguments: Some("{}".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 1,
                    id: Some("call_2".to_owned()),
                    name: Some("slow_tool".to_owned()),
                    arguments: Some("{}".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("all done".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );

    let mut registry = ToolRegistry::new(1024);
    registry.register("slow_tool", SlowTool);

    let runtime = AgentRuntime::new(Box::new(provider), registry, RuntimeLimits::default());
    let mut context = Context {
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("do slow things".to_owned()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        tools: Vec::new(),
        model: model_id,
        provider: provider_id,
    };

    let start = std::time::Instant::now();
    runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .unwrap();
    let elapsed = start.elapsed();

    // SlowTool sleeps for 50ms. Two sequential would take > 100ms.
    // Parallel should take ~50ms. We give it some buffer for setup/teardown.
    assert!(
        elapsed < Duration::from_millis(90),
        "Took {:?}, not parallel!",
        elapsed
    );

    let tool_results: Vec<_> = context
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();
    assert_eq!(tool_results.len(), 2);
}

#[tokio::test]
async fn run_session_supports_mockall_tool_execution() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![
            ProviderStep::Stream(vec![
                Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_owned()),
                    name: Some("file_read".to_owned()),
                    arguments: Some("{\"path\":\"Cargo.toml\"}".to_owned()),
                    metadata: None,
                })),
                Ok(StreamItem::FinishReason("tool_calls".to_owned())),
            ]),
            ProviderStep::Stream(vec![
                Ok(StreamItem::Text("tool complete".to_owned())),
                Ok(StreamItem::FinishReason("stop".to_owned())),
            ]),
        ],
    );

    let mut tool = MockToolContract::new();
    tool.expect_schema().return_const(FunctionDecl::new(
        "file_read",
        None,
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" }
            }
        }),
    ));
    tool.expect_safety_tier().return_const(SafetyTier::ReadOnly);
    tool.expect_timeout().return_const(Duration::from_secs(1));
    tool.expect_execute()
        .times(1)
        .withf(|args, _context| args.contains("\"path\":\"Cargo.toml\""))
        .returning(|_, _| Ok("mockall read: Cargo.toml".to_owned()));

    let mut tools = ToolRegistry::default();
    tools.register("file_read", tool);
    let runtime = AgentRuntime::new(Box::new(provider), tools, RuntimeLimits::default());
    let mut context = test_context(provider_id, model_id);

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("mockall tool should execute and loop to completion");

    assert_eq!(response.message.content.as_deref(), Some("tool complete"));
    assert!(matches!(context.messages[2].role, MessageRole::Tool));
    assert_eq!(
        context.messages[2].content.as_deref(),
        Some("mockall read: Cargo.toml")
    );
}

#[tokio::test]
async fn run_session_cancels_before_provider_call() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = mock_provider(provider_id.clone(), model_id.clone(), false);
    provider.expect_stream().never();
    provider.expect_complete().never();
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits::default(),
    );
    let mut context = test_context(provider_id, model_id);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = runtime
        .run_session(&mut context, &cancellation)
        .await
        .expect_err("cancelled token should short-circuit the turn");

    assert!(matches!(error, types::RuntimeError::Cancelled));
}

#[tokio::test]
async fn run_session_cancels_during_provider_call() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), false),
        vec![ProviderStep::CompleteDelayed {
            response: assistant_response("late response", vec![]),
            delay: Duration::from_millis(250),
        }],
    )
    .with_caps(ProviderCaps {
        supports_streaming: false,
        supports_tools: true,
        ..ProviderCaps::default()
    });
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits {
            turn_timeout: Duration::from_secs(2),
            max_turns: 3,
            max_cost: None,
            ..RuntimeLimits::default()
        },
    );
    let mut context = test_context(provider_id, model_id);
    let cancellation = CancellationToken::new();
    let cancellation_clone = cancellation.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(30)).await;
        cancellation_clone.cancel();
    });

    let error = runtime
        .run_session(&mut context, &cancellation)
        .await
        .expect_err("provider await should observe cancellation");
    assert!(matches!(error, types::RuntimeError::Cancelled));
}

#[tokio::test]
async fn run_session_errors_when_provider_stage_times_out() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), false),
        vec![ProviderStep::CompleteDelayed {
            response: assistant_response("late response", vec![]),
            delay: Duration::from_millis(100),
        }],
    )
    .with_caps(ProviderCaps {
        supports_streaming: false,
        supports_tools: true,
        ..ProviderCaps::default()
    });
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits {
            turn_timeout: Duration::from_millis(10),
            max_turns: 2,
            max_cost: None,
            ..RuntimeLimits::default()
        },
    );
    let mut context = test_context(provider_id, model_id);

    let error = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect_err("provider call should be bounded by turn timeout");
    assert!(matches!(error, types::RuntimeError::BudgetExceeded));
}

#[tokio::test]
async fn run_session_errors_when_tool_stage_times_out() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![ProviderStep::Stream(vec![
            Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                index: 0,
                id: Some("call_1".to_owned()),
                name: Some("slow_tool".to_owned()),
                arguments: Some("{}".to_owned()),
                metadata: None,
            })),
            Ok(StreamItem::FinishReason("tool_calls".to_owned())),
        ])],
    );

    let mut tools = ToolRegistry::default();
    tools.register("slow_tool", SlowTool);
    let runtime = AgentRuntime::new(
        Box::new(provider),
        tools,
        RuntimeLimits {
            turn_timeout: Duration::from_millis(5),
            max_turns: 2,
            max_cost: None,
            ..RuntimeLimits::default()
        },
    );
    let mut context = test_context(provider_id, model_id);

    let error = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect_err("tool stage should respect runtime timeout");
    assert!(matches!(error, types::RuntimeError::BudgetExceeded));
}

#[tokio::test]
async fn run_session_errors_when_max_turn_budget_is_exceeded() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![ProviderStep::Stream(vec![
            Ok(StreamItem::ToolCallDelta(ToolCallDelta {
                index: 0,
                id: Some("call_1".to_owned()),
                name: Some("file_read".to_owned()),
                arguments: Some("{\"path\":\"Cargo.toml\"}".to_owned()),
                metadata: None,
            })),
            Ok(StreamItem::FinishReason("tool_calls".to_owned())),
        ])],
    );
    let mut tools = ToolRegistry::default();
    tools.register("file_read", MockReadTool);
    let runtime = AgentRuntime::new(
        Box::new(provider),
        tools,
        RuntimeLimits {
            turn_timeout: Duration::from_secs(1),
            max_turns: 1,
            max_cost: None,
            ..RuntimeLimits::default()
        },
    );
    let mut context = test_context(provider_id, model_id);

    let error = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect_err("runtime should stop after max turn budget");
    assert!(matches!(error, types::RuntimeError::BudgetExceeded));
}

#[tokio::test]
async fn run_session_errors_when_max_cost_budget_is_exceeded() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let provider = FakeProvider::new(
        provider_id.clone(),
        test_catalog(provider_id.clone(), model_id.clone(), true),
        vec![ProviderStep::Stream(vec![
            Ok(StreamItem::UsageUpdate(UsageUpdate {
                prompt_tokens: Some(4),
                completion_tokens: Some(2),
                total_tokens: Some(6),
            })),
            Ok(StreamItem::Text("expensive turn".to_owned())),
            Ok(StreamItem::FinishReason("stop".to_owned())),
        ])],
    );
    let runtime = AgentRuntime::new(
        Box::new(provider),
        ToolRegistry::default(),
        RuntimeLimits {
            turn_timeout: Duration::from_secs(1),
            max_turns: 2,
            max_cost: Some(5.0),
            ..RuntimeLimits::default()
        },
    );
    let mut context = test_context(provider_id, model_id);

    let error = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect_err("max cost guard should fail fast once budget is exceeded");
    assert!(matches!(error, types::RuntimeError::BudgetExceeded));
}

#[tokio::test]
async fn run_session_trims_provider_context_to_max_context_budget() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider
        .expect_model_catalog()
        .return_const(test_catalog_with_max_context(
            provider_id.clone(),
            model_id.clone(),
            false,
            Some(96),
        ));
    provider.expect_capabilities().returning(|_| {
        Ok(ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            max_context_tokens: Some(96),
            ..ProviderCaps::default()
        })
    });
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(|provider_context| {
            provider_context.messages.len() < 6
                && provider_context
                    .messages
                    .iter()
                    .any(|message| message.content.as_deref() == Some("latest user turn"))
        })
        .returning(|_| Ok(assistant_response("budgeted response", vec![])));

    let mut limits = RuntimeLimits::default();
    limits.context_budget.safety_buffer_tokens = 8;
    let runtime = AgentRuntime::new(Box::new(provider), ToolRegistry::default(), limits);
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![
            Message {
                role: MessageRole::System,
                content: Some("system ".repeat(120)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("older user ".repeat(90)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("older assistant ".repeat(90)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("middle user ".repeat(90)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("middle assistant ".repeat(90)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("latest user turn".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
    };

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should trim provider-facing context");

    assert_eq!(
        response.message.content.as_deref(),
        Some("budgeted response")
    );
    assert_eq!(context.messages.len(), 7);
    assert_eq!(
        context.messages[5].content.as_deref(),
        Some("latest user turn")
    );
}

#[tokio::test]
async fn run_session_uses_fallback_context_limit_when_model_cap_is_missing() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider
        .expect_model_catalog()
        .return_const(test_catalog_with_max_context(
            provider_id.clone(),
            model_id.clone(),
            false,
            None,
        ));
    provider.expect_capabilities().returning(|_| {
        Ok(ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            ..ProviderCaps::default()
        })
    });
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(|provider_context| provider_context.messages.len() <= 2)
        .returning(|_| Ok(assistant_response("fallback budget applied", vec![])));

    let mut limits = RuntimeLimits::default();
    limits.context_budget.safety_buffer_tokens = 8;
    limits.context_budget.fallback_max_context_tokens = 48;
    let runtime = AgentRuntime::new(Box::new(provider), ToolRegistry::default(), limits);
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![
            Message {
                role: MessageRole::User,
                content: Some("older user ".repeat(100)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("older assistant ".repeat(100)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("latest user".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
    };

    let response = runtime
        .run_session(&mut context, &CancellationToken::new())
        .await
        .expect("runtime should use fallback max context tokens");
    assert_eq!(
        response.message.content.as_deref(),
        Some("fallback budget applied")
    );
}

#[tokio::test]
async fn run_session_for_session_enforces_budget_with_retrieval_injection() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider
        .expect_model_catalog()
        .return_const(test_catalog_with_max_context(
            provider_id.clone(),
            model_id.clone(),
            false,
            Some(96),
        ));
    provider.expect_capabilities().returning(|_| {
        Ok(ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            max_context_tokens: Some(96),
            ..ProviderCaps::default()
        })
    });
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(|provider_context| {
            provider_context.messages.len() <= 3
                && provider_context.messages.iter().any(|message| {
                    message.role == MessageRole::System
                        && message
                            .content
                            .as_deref()
                            .is_some_and(|content| content.contains("Retrieved memory snippets:"))
                })
                && provider_context
                    .messages
                    .iter()
                    .any(|message| message.content.as_deref() == Some("latest user turn"))
                && !provider_context.messages.iter().any(|message| {
                    message
                        .content
                        .as_deref()
                        .is_some_and(|content| content.contains("older user detail"))
                })
        })
        .returning(|_| Ok(assistant_response("budgeted retrieval response", vec![])));

    let memory = Arc::new(RecordingMemory::with_hybrid_query_results(vec![
        MemoryHybridQueryResult {
            chunk_id: "chunk-budget".to_owned(),
            session_id: "session-budget-retrieval".to_owned(),
            text: "persisted budget note".to_owned(),
            score: 0.93,
            vector_score: 1.0,
            fts_score: 0.0,
            file_id: None,
            sequence_start: Some(1),
            sequence_end: Some(1),
            metadata: None,
        },
    ]));
    let mut limits = RuntimeLimits::default();
    limits.context_budget.safety_buffer_tokens = 8;
    limits.retrieval.top_k = 1;
    limits.retrieval.vector_weight = 0.7;
    limits.retrieval.fts_weight = 0.3;
    let runtime = AgentRuntime::new(Box::new(provider), ToolRegistry::default(), limits)
        .with_memory_retrieval(memory.clone());
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![
            Message {
                role: MessageRole::User,
                content: Some("older user detail ".repeat(120)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("older assistant detail ".repeat(120)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("latest user turn".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
    };

    let response = runtime
        .run_session_for_session(
            "session-budget-retrieval",
            &mut context,
            &CancellationToken::new(),
        )
        .await
        .expect("runtime should keep provider context within budget after retrieval injection");
    assert_eq!(
        response.message.content.as_deref(),
        Some("budgeted retrieval response")
    );

    let queries = memory.recorded_hybrid_queries();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].session_id, "session-budget-retrieval");
    assert_eq!(queries[0].query, "latest user turn");
}

#[tokio::test]
async fn run_session_for_session_injects_retrieved_memory_snippets() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider
        .expect_model_catalog()
        .return_const(test_catalog_with_max_context(
            provider_id.clone(),
            model_id.clone(),
            false,
            Some(256),
        ));
    provider.expect_capabilities().returning(|_| {
        Ok(ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            max_context_tokens: Some(256),
            ..ProviderCaps::default()
        })
    });
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(|provider_context| {
            provider_context.messages.iter().any(|message| {
                message.role == MessageRole::System
                    && message
                        .content
                        .as_deref()
                        .is_some_and(|content| content.contains("Retrieved memory snippets:"))
            })
        })
        .returning(|_| Ok(assistant_response("response with retrieval", vec![])));

    let memory = Arc::new(RecordingMemory::with_hybrid_query_results(vec![
        MemoryHybridQueryResult {
            chunk_id: "chunk-1".to_owned(),
            session_id: "session-retrieval".to_owned(),
            text: "Persisted memory snippet".to_owned(),
            score: 0.91,
            vector_score: 1.0,
            fts_score: 0.0,
            file_id: None,
            sequence_start: Some(1),
            sequence_end: Some(1),
            metadata: None,
        },
    ]));
    let mut limits = RuntimeLimits::default();
    limits.context_budget.safety_buffer_tokens = 8;
    limits.retrieval.top_k = 3;
    limits.retrieval.vector_weight = 0.6;
    limits.retrieval.fts_weight = 0.4;
    let runtime = AgentRuntime::new(Box::new(provider), ToolRegistry::default(), limits)
        .with_memory_retrieval(memory.clone());
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("what did we persist?".to_owned()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
    };

    let response = runtime
        .run_session_for_session("session-retrieval", &mut context, &CancellationToken::new())
        .await
        .expect("runtime should inject retrieved snippets into provider context");
    assert_eq!(
        response.message.content.as_deref(),
        Some("response with retrieval")
    );

    let retrieval_queries = memory.recorded_hybrid_queries();
    assert_eq!(retrieval_queries.len(), 1);
    let request = &retrieval_queries[0];
    assert_eq!(request.session_id, "session-retrieval");
    assert_eq!(request.query, "what did we persist?");
    assert_eq!(request.top_k, Some(3));
    assert_eq!(request.vector_weight, Some(0.6));
    assert_eq!(request.fts_weight, Some(0.4));
}

#[tokio::test]
async fn run_session_for_session_triggers_rolling_summary_when_threshold_exceeded() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let summary_prefix = format!("{}\n", super::budget::ROLLING_SUMMARY_PREFIX);
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider
        .expect_model_catalog()
        .return_const(test_catalog_with_max_context(
            provider_id.clone(),
            model_id.clone(),
            false,
            Some(256),
        ));
    provider.expect_capabilities().returning(|_| {
        Ok(ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            max_context_tokens: Some(256),
            ..ProviderCaps::default()
        })
    });
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(move |provider_context| {
            provider_context.messages.iter().any(|message| {
                message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.starts_with(summary_prefix.as_str()))
            })
        })
        .returning(|_| Ok(assistant_response("summary applied", vec![])));

    let memory = Arc::new(RecordingMemory::with_hybrid_query_results(vec![]));
    let mut limits = RuntimeLimits::default();
    limits.context_budget.trigger_ratio = 0.2;
    limits.context_budget.safety_buffer_tokens = 8;
    limits.summarization.target_ratio = 0.45;
    limits.summarization.min_turns = 4;
    let runtime = AgentRuntime::new(Box::new(provider), ToolRegistry::default(), limits)
        .with_memory_retrieval(memory.clone());
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![
            Message {
                role: MessageRole::User,
                content: Some("older user detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("older assistant detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("middle user detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("middle assistant detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("latest user question".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
    };

    let response = runtime
        .run_session_for_session("session-summary", &mut context, &CancellationToken::new())
        .await
        .expect("runtime should trigger rolling summary flow");
    assert_eq!(response.message.content.as_deref(), Some("summary applied"));

    let writes = memory.recorded_summary_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].session_id, "session-summary");
    assert_eq!(writes[0].expected_epoch, 0);
    assert!(writes[0].upper_sequence > 0);
    assert!(writes[0].summary.contains("Recent condensed turns:"));

    let summary_state = memory
        .read_summary_state(MemorySummaryReadRequest {
            session_id: "session-summary".to_owned(),
        })
        .await
        .expect("summary state should be readable")
        .expect("summary state should be persisted");
    assert_eq!(summary_state.epoch, 1);
    assert_eq!(summary_state.upper_sequence, writes[0].upper_sequence);
}

#[tokio::test]
async fn run_session_for_session_discards_stale_rolling_summary_writes() {
    let provider_id = ProviderId::from("openai");
    let model_id = ModelId::from("gpt-4o-mini");
    let summary_prefix = format!("{}\n", super::budget::ROLLING_SUMMARY_PREFIX);
    let mut provider = MockProviderContract::new();
    provider
        .expect_provider_id()
        .return_const(provider_id.clone());
    provider
        .expect_model_catalog()
        .return_const(test_catalog_with_max_context(
            provider_id.clone(),
            model_id.clone(),
            false,
            Some(256),
        ));
    provider.expect_capabilities().returning(|_| {
        Ok(ProviderCaps {
            supports_streaming: false,
            supports_tools: true,
            max_context_tokens: Some(256),
            ..ProviderCaps::default()
        })
    });
    provider.expect_stream().never();
    provider
        .expect_complete()
        .times(1)
        .withf(move |provider_context| {
            !provider_context.messages.iter().any(|message| {
                message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.starts_with(summary_prefix.as_str()))
            })
        })
        .returning(|_| Ok(assistant_response("stale ignored", vec![])));

    let memory = Arc::new(RecordingMemory::with_stale_summary_writes(vec![]));
    let mut limits = RuntimeLimits::default();
    limits.context_budget.trigger_ratio = 0.2;
    limits.context_budget.safety_buffer_tokens = 8;
    limits.summarization.target_ratio = 0.45;
    limits.summarization.min_turns = 4;
    let runtime = AgentRuntime::new(Box::new(provider), ToolRegistry::default(), limits)
        .with_memory_retrieval(memory.clone());
    let mut context = Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![
            Message {
                role: MessageRole::User,
                content: Some("older user detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("older assistant detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("middle user detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: Some("middle assistant detail ".repeat(80)),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message {
                role: MessageRole::User,
                content: Some("latest user question".to_owned()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        ],
    };

    let response = runtime
        .run_session_for_session(
            "session-summary-stale",
            &mut context,
            &CancellationToken::new(),
        )
        .await
        .expect("runtime should tolerate stale summary writes");
    assert_eq!(response.message.content.as_deref(), Some("stale ignored"));

    let writes = memory.recorded_summary_writes();
    assert_eq!(writes.len(), 1);
    let summary_state = memory
        .read_summary_state(MemorySummaryReadRequest {
            session_id: "session-summary-stale".to_owned(),
        })
        .await
        .expect("summary read should succeed");
    assert!(summary_state.is_none());
}

fn test_catalog(
    provider_id: ProviderId,
    model_id: ModelId,
    supports_streaming: bool,
) -> ModelCatalog {
    test_catalog_with_max_context(provider_id, model_id, supports_streaming, None)
}

fn test_catalog_with_max_context(
    provider_id: ProviderId,
    model_id: ModelId,
    _supports_streaming: bool,
    max_context_tokens: Option<u32>,
) -> ModelCatalog {
    let limit = ModelLimits {
        context: max_context_tokens.unwrap_or(0),
        output: 0,
    };

    let mut models = BTreeMap::new();
    models.insert(
        model_id.0.clone(),
        ModelDescriptor {
            id: model_id.0.clone(),
            name: model_id.0.clone(),
            family: None,
            attachment: false,
            reasoning: false,
            tool_call: true,
            interleaved: None,
            structured_output: false,
            temperature: false,
            knowledge: None,
            release_date: None,
            last_updated: None,
            modalities: Default::default(),
            open_weights: false,
            cost: Default::default(),
            limit,
        },
    );

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_id.0.clone(),
        CatalogProvider {
            id: provider_id.0.clone(),
            name: provider_id.0.clone(),
            env: vec![],
            api: None,
            doc: None,
            models,
        },
    );

    ModelCatalog::new(providers)
}

fn test_context(provider_id: ProviderId, model_id: ModelId) -> Context {
    Context {
        provider: provider_id,
        model: model_id,
        tools: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: Some("Read Cargo.toml".to_owned()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
    }
}

#[cfg(unix)]
fn temp_socket_path(label: &str) -> std::path::PathBuf {
    let short_label = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos()
        % 1_000_000;
    std::path::PathBuf::from(format!(
        "/tmp/oxy-{short_label}-{}-{unique}.sock",
        std::process::id()
    ))
}

fn temp_workspace_root(label: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "oxydra-{label}-workspace-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("shared")).expect("shared directory should be created");
    std::fs::create_dir_all(root.join("tmp")).expect("tmp directory should be created");
    std::fs::create_dir_all(root.join("vault")).expect("vault directory should be created");
    root
}

fn assistant_response(content: &str, tool_calls: Vec<ToolCall>) -> Response {
    Response {
        message: Message {
            role: MessageRole::Assistant,
            content: Some(content.to_owned()),
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
        },
        tool_calls,
        finish_reason: Some("stop".to_owned()),
        usage: None,
    }
}

#[test]
fn runtime_limits_default_matches_turn_guard_baseline() {
    let limits = RuntimeLimits::default();
    assert_eq!(limits.turn_timeout, Duration::from_secs(60));
    assert_eq!(limits.max_turns, 8);
    assert_eq!(limits.max_cost, None);
    assert_eq!(limits.context_budget.trigger_ratio, 0.85);
    assert_eq!(limits.context_budget.safety_buffer_tokens, 1_024);
    assert_eq!(limits.context_budget.fallback_max_context_tokens, 128_000);
    assert_eq!(limits.retrieval.top_k, 8);
    assert_eq!(limits.retrieval.vector_weight, 0.7);
    assert_eq!(limits.retrieval.fts_weight, 0.3);
    assert_eq!(limits.summarization.target_ratio, 0.5);
    assert_eq!(limits.summarization.min_turns, 6);
}

#[test]
fn runtime_limits_can_store_optional_max_cost_guard() {
    let limits = RuntimeLimits {
        turn_timeout: Duration::from_secs(15),
        max_turns: 3,
        max_cost: Some(1.25),
        ..RuntimeLimits::default()
    };
    assert_eq!(limits.max_cost, Some(1.25));
}

#[test]
fn streamed_tool_calls_accept_empty_argument_payload_as_object() {
    let provider = ProviderId::from("openai");
    let mut accumulator = super::ToolCallAccumulator::default();
    accumulator.merge(ToolCallDelta {
        index: 0,
        id: Some("call_1".to_owned()),
        name: Some("noop".to_owned()),
        arguments: None,
        metadata: None,
    });
    let tool_calls = accumulator
        .build(&provider)
        .expect("empty streamed arguments should normalize to object");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].arguments, json!({}));
}

#[test]
fn streamed_tool_calls_require_id_field() {
    let provider = ProviderId::from("openai");
    let mut accumulator = super::ToolCallAccumulator::default();
    accumulator.merge(ToolCallDelta {
        index: 0,
        id: None,
        name: Some("noop".to_owned()),
        arguments: Some("{}".to_owned()),
        metadata: None,
    });

    let error = accumulator
        .build(&provider)
        .expect_err("missing id should fail reconstruction");
    assert!(
        matches!(error, ProviderError::ResponseParse { message, .. } if message.contains("missing id"))
    );
}

#[test]
fn streamed_tool_calls_require_function_name() {
    let provider = ProviderId::from("openai");
    let mut accumulator = super::ToolCallAccumulator::default();
    accumulator.merge(ToolCallDelta {
        index: 0,
        id: Some("call_1".to_owned()),
        name: None,
        arguments: Some("{}".to_owned()),
        metadata: None,
    });

    let error = accumulator
        .build(&provider)
        .expect_err("missing function name should fail reconstruction");
    assert!(
        matches!(error, ProviderError::ResponseParse { message, .. } if message.contains("missing function name"))
    );
}

#[test]
fn try_extract_concatenated_json_objects_returns_all_objects() {
    let raw = r#"{"query":"AI doom scenario"}{"query":"p(doom) AI"}{"query":"AI risk"}"#;
    let result = super::try_extract_concatenated_json_objects(raw);
    let objects = result.expect("should extract all objects");
    assert_eq!(objects.len(), 3);
    assert_eq!(objects[0]["query"], "AI doom scenario");
    assert_eq!(objects[1]["query"], "p(doom) AI");
    assert_eq!(objects[2]["query"], "AI risk");
}

#[test]
fn try_extract_concatenated_json_objects_returns_none_for_valid_single_object() {
    let raw = r#"{"query":"normal query"}"#;
    let result = super::try_extract_concatenated_json_objects(raw);
    assert!(
        result.is_none(),
        "single valid object should not be intercepted"
    );
}

#[test]
fn try_extract_concatenated_json_objects_returns_none_for_non_object_input() {
    assert!(super::try_extract_concatenated_json_objects("not json").is_none());
    assert!(super::try_extract_concatenated_json_objects("").is_none());
    assert!(super::try_extract_concatenated_json_objects("[1,2,3]").is_none());
}

#[test]
fn try_extract_concatenated_json_objects_ignores_trailing_whitespace() {
    let raw = r#"{"query":"test"}   "#;
    let result = super::try_extract_concatenated_json_objects(raw);
    assert!(result.is_none(), "trailing whitespace is not concatenation");
}

#[test]
fn try_extract_concatenated_json_objects_returns_none_for_single_with_trailing_junk() {
    let raw = r#"{"query":"test"}not json"#;
    let result = super::try_extract_concatenated_json_objects(raw);
    assert!(
        result.is_none(),
        "single object with trailing junk should not be intercepted"
    );
}

#[test]
fn concatenated_arguments_are_fanned_out_as_parallel_tool_calls() {
    let provider = ProviderId::from("gemini");
    let mut accumulator = super::ToolCallAccumulator::default();
    accumulator.merge(ToolCallDelta {
        index: 0,
        id: Some("call_1".to_owned()),
        name: Some("web_search".to_owned()),
        arguments: Some(r#"{"query":"first query"}{"query":"second query"}"#.to_owned()),
        metadata: None,
    });

    let tool_calls = accumulator
        .build(&provider)
        .expect("concatenated arguments should be fanned out");
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0].id, "call_1_0");
    assert_eq!(tool_calls[0].name, "web_search");
    assert_eq!(tool_calls[0].arguments["query"], "first query");
    assert_eq!(tool_calls[1].id, "call_1_1");
    assert_eq!(tool_calls[1].name, "web_search");
    assert_eq!(tool_calls[1].arguments["query"], "second query");
}

// ── Scheduler executor tests ───────────────────────────────────────────

mod scheduler_executor_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;
    use types::{
        GatewayServerFrame, NotificationPolicy, RuntimeError, ScheduleCadence, ScheduleDefinition,
        ScheduleRunRecord, ScheduleRunStatus, ScheduleSearchFilters, ScheduleSearchResult,
        ScheduleStatus, SchedulerConfig, SchedulerError,
    };

    use crate::ScheduledTurnRunner;
    use crate::scheduler_executor::{SchedulerExecutor, SchedulerNotifier};

    // -- Mock ScheduledTurnRunner --

    #[derive(Clone)]
    struct MockTurnRunner {
        responses: Arc<Mutex<Vec<Result<String, RuntimeError>>>>,
    }

    impl MockTurnRunner {
        fn new(responses: Vec<Result<String, RuntimeError>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
            }
        }
    }

    #[async_trait]
    impl ScheduledTurnRunner for MockTurnRunner {
        async fn run_scheduled_turn(
            &self,
            _user_id: &str,
            _runtime_session_id: &str,
            _prompt: String,
            _cancellation: CancellationToken,
        ) -> Result<String, RuntimeError> {
            let mut responses = self.responses.lock().await;
            if responses.is_empty() {
                Ok("default response".to_owned())
            } else {
                responses.remove(0)
            }
        }
    }

    // -- Mock SchedulerStore --

    struct MockSchedulerStore {
        due: Mutex<Vec<ScheduleDefinition>>,
        recorded_runs: Mutex<Vec<ScheduleRunRecord>>,
    }

    impl MockSchedulerStore {
        fn new(due: Vec<ScheduleDefinition>) -> Self {
            Self {
                due: Mutex::new(due),
                recorded_runs: Mutex::new(Vec::new()),
            }
        }

        async fn recorded_runs(&self) -> Vec<ScheduleRunRecord> {
            self.recorded_runs.lock().await.clone()
        }
    }

    #[async_trait]
    impl memory_crate::SchedulerStore for MockSchedulerStore {
        async fn create_schedule(&self, _def: &ScheduleDefinition) -> Result<(), SchedulerError> {
            Ok(())
        }
        async fn get_schedule(
            &self,
            _schedule_id: &str,
        ) -> Result<ScheduleDefinition, SchedulerError> {
            Err(SchedulerError::NotFound {
                schedule_id: "mock".to_owned(),
            })
        }
        async fn search_schedules(
            &self,
            _user_id: &str,
            _filters: &ScheduleSearchFilters,
        ) -> Result<ScheduleSearchResult, SchedulerError> {
            Ok(ScheduleSearchResult {
                schedules: vec![],
                total_count: 0,
                offset: 0,
                limit: 20,
            })
        }
        async fn count_schedules(&self, _user_id: &str) -> Result<usize, SchedulerError> {
            Ok(0)
        }
        async fn delete_schedule(&self, _schedule_id: &str) -> Result<bool, SchedulerError> {
            Ok(true)
        }
        async fn update_schedule(
            &self,
            _schedule_id: &str,
            _patch: &types::SchedulePatch,
        ) -> Result<ScheduleDefinition, SchedulerError> {
            Err(SchedulerError::NotFound {
                schedule_id: "mock".to_owned(),
            })
        }
        async fn due_schedules(
            &self,
            _now: &str,
            _limit: usize,
        ) -> Result<Vec<ScheduleDefinition>, SchedulerError> {
            let mut due = self.due.lock().await;
            Ok(std::mem::take(&mut *due))
        }
        async fn record_run_and_reschedule(
            &self,
            _schedule_id: &str,
            run: &ScheduleRunRecord,
            _next_run_at: Option<String>,
            _new_status: Option<ScheduleStatus>,
        ) -> Result<(), SchedulerError> {
            self.recorded_runs.lock().await.push(run.clone());
            Ok(())
        }
        async fn prune_run_history(
            &self,
            _schedule_id: &str,
            _keep: usize,
        ) -> Result<(), SchedulerError> {
            Ok(())
        }
        async fn get_run_history(
            &self,
            _schedule_id: &str,
            _limit: usize,
        ) -> Result<Vec<ScheduleRunRecord>, SchedulerError> {
            Ok(vec![])
        }
    }

    // -- Mock SchedulerNotifier --

    struct MockNotifier {
        notifications: Mutex<Vec<GatewayServerFrame>>,
    }

    impl MockNotifier {
        fn new() -> Self {
            Self {
                notifications: Mutex::new(Vec::new()),
            }
        }

        async fn notifications(&self) -> Vec<GatewayServerFrame> {
            self.notifications.lock().await.clone()
        }
    }

    impl SchedulerNotifier for MockNotifier {
        fn notify_user(&self, _user_id: &str, frame: GatewayServerFrame) {
            let notifications = &self.notifications;
            // Use try_lock since we're not in async context here
            if let Ok(mut n) = notifications.try_lock() {
                n.push(frame);
            }
        }
    }

    fn test_config() -> SchedulerConfig {
        SchedulerConfig {
            enabled: true,
            poll_interval_secs: 1,
            max_concurrent: 2,
            max_schedules_per_user: 50,
            max_turns: 10,
            max_cost: 0.50,
            max_run_history: 20,
            min_interval_secs: 60,
            default_timezone: "Asia/Kolkata".to_owned(),
            auto_disable_after_failures: 5,
        }
    }

    fn test_schedule(id: &str, notification_policy: NotificationPolicy) -> ScheduleDefinition {
        let now = chrono::Utc::now().to_rfc3339();
        ScheduleDefinition {
            schedule_id: id.to_owned(),
            user_id: "alice".to_owned(),
            name: Some("Test schedule".to_owned()),
            goal: "Do the thing".to_owned(),
            cadence: ScheduleCadence::Interval { every_secs: 3600 },
            notification_policy,
            status: ScheduleStatus::Active,
            created_at: now.clone(),
            updated_at: now.clone(),
            next_run_at: Some(now.clone()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
        }
    }

    #[tokio::test]
    async fn executor_tick_dispatches_due_schedule() {
        let schedule = test_schedule("sched-1", NotificationPolicy::Always);
        let store = Arc::new(MockSchedulerStore::new(vec![schedule]));
        let runner = Arc::new(MockTurnRunner::new(vec![Ok("All done".to_owned())]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier.clone(),
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ScheduleRunStatus::Success);
        assert!(runs[0].notified);

        let notifs = notifier.notifications().await;
        assert_eq!(notifs.len(), 1);
    }

    #[tokio::test]
    async fn executor_conditional_notification_with_notify_marker() {
        let schedule = test_schedule("sched-cond", NotificationPolicy::Conditional);
        let store = Arc::new(MockSchedulerStore::new(vec![schedule]));
        let runner = Arc::new(MockTurnRunner::new(vec![Ok(
            "[NOTIFY] Important update!".to_owned()
        )]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier.clone(),
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert_eq!(runs.len(), 1);
        assert!(runs[0].notified);

        let notifs = notifier.notifications().await;
        assert_eq!(notifs.len(), 1);
        if let GatewayServerFrame::ScheduledNotification(n) = &notifs[0] {
            assert_eq!(n.message, "Important update!");
        } else {
            panic!("expected ScheduledNotification frame");
        }
    }

    #[tokio::test]
    async fn executor_conditional_notification_without_marker_is_silent() {
        let schedule = test_schedule("sched-silent", NotificationPolicy::Conditional);
        let store = Arc::new(MockSchedulerStore::new(vec![schedule]));
        let runner = Arc::new(MockTurnRunner::new(vec![Ok(
            "Nothing interesting happened".to_owned(),
        )]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier.clone(),
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].notified);

        let notifs = notifier.notifications().await;
        assert!(notifs.is_empty());
    }

    #[tokio::test]
    async fn executor_never_notification_is_silent() {
        let schedule = test_schedule("sched-never", NotificationPolicy::Never);
        let store = Arc::new(MockSchedulerStore::new(vec![schedule]));
        let runner = Arc::new(MockTurnRunner::new(vec![Ok("Done".to_owned())]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier.clone(),
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].notified);

        let notifs = notifier.notifications().await;
        assert!(notifs.is_empty());
    }

    #[tokio::test]
    async fn executor_handles_failed_turn() {
        let schedule = test_schedule("sched-fail", NotificationPolicy::Always);
        let store = Arc::new(MockSchedulerStore::new(vec![schedule]));
        let runner = Arc::new(MockTurnRunner::new(vec![Err(RuntimeError::BudgetExceeded)]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier.clone(),
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ScheduleRunStatus::Failed);
        assert!(!runs[0].notified);
    }

    #[tokio::test]
    async fn executor_one_shot_completes_after_success() {
        let now = chrono::Utc::now().to_rfc3339();
        let schedule = ScheduleDefinition {
            schedule_id: "sched-once".to_owned(),
            user_id: "alice".to_owned(),
            name: None,
            goal: "One time task".to_owned(),
            cadence: ScheduleCadence::Once { at: now.clone() },
            notification_policy: NotificationPolicy::Always,
            status: ScheduleStatus::Active,
            created_at: now.clone(),
            updated_at: now.clone(),
            next_run_at: Some(now),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
        };
        let store = Arc::new(MockSchedulerStore::new(vec![schedule]));
        let runner = Arc::new(MockTurnRunner::new(vec![Ok("Done".to_owned())]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier,
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ScheduleRunStatus::Success);
    }

    #[tokio::test]
    async fn executor_empty_tick_does_nothing() {
        let store = Arc::new(MockSchedulerStore::new(vec![]));
        let runner = Arc::new(MockTurnRunner::new(vec![]));
        let notifier = Arc::new(MockNotifier::new());

        let executor = SchedulerExecutor::new(
            store.clone(),
            runner,
            notifier.clone(),
            test_config(),
            CancellationToken::new(),
        );

        executor.tick().await;

        let runs = store.recorded_runs().await;
        assert!(runs.is_empty());

        let notifs = notifier.notifications().await;
        assert!(notifs.is_empty());
    }
}
