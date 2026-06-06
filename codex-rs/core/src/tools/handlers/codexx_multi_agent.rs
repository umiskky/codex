//! Codexx-specific enhanced multi-agent namespace.
//!
//! Official multi_agent_v1 and MultiAgentV2 handlers remain available as compatibility code.
//! This module exposes the Codexx enhancements under a separate namespace.

use crate::agent::exceeds_thread_spawn_depth_limit;
use crate::agent::next_thread_spawn_depth;
use crate::agent::status::is_final;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::*;
use crate::tools::handlers::multi_agents_spec::CODEXX_MULTI_AGENT_NAMESPACE;
use crate::tools::handlers::multi_agents_spec::CODEXX_MULTI_AGENT_NAMESPACE_DESCRIPTION;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_codexx_register_agent_tool;
use crate::tools::handlers::multi_agents_spec::create_codexx_resume_agent_tool;
use crate::tools::handlers::multi_agents_spec::create_codexx_wait_agent_status_tool;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v2;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabResumeBeginEvent;
use codex_protocol::protocol::CollabResumeEndEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::CollabWaitingEndEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;
use tokio::time::timeout_at;

pub(crate) struct RegisterAgentHandler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for RegisterAgentHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(CODEXX_MULTI_AGENT_NAMESPACE, "register_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_codexx_register_agent_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        handle_register_agent(invocation)
            .await
            .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for RegisterAgentHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Default)]
pub(crate) struct SpawnAgentHandler {
    options: SpawnAgentToolOptions,
}

impl SpawnAgentHandler {
    pub(crate) fn new(options: SpawnAgentToolOptions) -> Self {
        Self { options }
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for SpawnAgentHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(CODEXX_MULTI_AGENT_NAMESPACE, "spawn_agent")
    }

    fn spec(&self) -> ToolSpec {
        codexx_namespace_spec(create_spawn_agent_tool_v2(self.options.clone()))
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        crate::tools::handlers::multi_agents_v2::SpawnAgentHandler::new_with_runtime_agent_roles(
            self.options.clone(),
        )
        .handle(invocation)
        .await
    }
}

impl CoreToolRuntime for SpawnAgentHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

pub(crate) struct ResumeAgentHandler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for ResumeAgentHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(CODEXX_MULTI_AGENT_NAMESPACE, "resume_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_codexx_resume_agent_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        handle_resume_agent(invocation).await.map(boxed_tool_output)
    }
}

impl CoreToolRuntime for ResumeAgentHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct ResumeAgentArgs {
    target: String,
}

#[derive(Debug, Clone)]
struct ResolvedCodexxAgentTarget {
    thread_id: ThreadId,
    agent_path: Option<AgentPath>,
    label: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ResumeAgentResult {
    pub(crate) status: AgentStatus,
}

impl ToolOutput for ResumeAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "resume_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "resume_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "resume_agent")
    }
}

async fn handle_resume_agent(
    invocation: ToolInvocation,
) -> Result<ResumeAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: ResumeAgentArgs = parse_arguments(&arguments)?;
    let resolved_target = resolve_codexx_agent_target(&session, &turn, &args.target).await?;
    let receiver_thread_id = resolved_target.thread_id;
    let receiver_agent = session
        .services
        .agent_control
        .get_agent_metadata(receiver_thread_id)
        .unwrap_or_default();
    let child_depth = next_thread_spawn_depth(&turn.session_source);
    let max_depth = turn.config.agent_max_depth;
    if exceeds_thread_spawn_depth_limit(child_depth, max_depth) {
        return Err(FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string(),
        ));
    }

    session
        .send_event(
            &turn,
            CollabResumeBeginEvent {
                call_id: call_id.clone(),
                started_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                sender_thread_id: session.thread_id,
                receiver_thread_id,
                receiver_agent_nickname: receiver_agent.agent_nickname.clone(),
                receiver_agent_role: receiver_agent.agent_role.clone(),
            }
            .into(),
        )
        .await;

    let mut status = session
        .services
        .agent_control
        .get_status(receiver_thread_id)
        .await;
    let (receiver_agent, error) = if matches!(status, AgentStatus::NotFound) {
        match Box::pin(try_resume_closed_agent(
            &session,
            &turn,
            receiver_thread_id,
            child_depth,
            resolved_target.agent_path,
        ))
        .await
        {
            Ok(()) => {
                status = session
                    .services
                    .agent_control
                    .get_status(receiver_thread_id)
                    .await;
                (
                    session
                        .services
                        .agent_control
                        .get_agent_metadata(receiver_thread_id)
                        .unwrap_or(receiver_agent),
                    None,
                )
            }
            Err(err) => {
                status = session
                    .services
                    .agent_control
                    .get_status(receiver_thread_id)
                    .await;
                (receiver_agent, Some(err))
            }
        }
    } else {
        (receiver_agent, None)
    };
    session
        .send_event(
            &turn,
            CollabResumeEndEvent {
                call_id,
                completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                sender_thread_id: session.thread_id(),
                receiver_thread_id,
                receiver_agent_nickname: receiver_agent.agent_nickname,
                receiver_agent_role: receiver_agent.agent_role,
                status: status.clone(),
            }
            .into(),
        )
        .await;

    if let Some(err) = error {
        return Err(err);
    }
    turn.session_telemetry
        .counter("codexx.multi_agent.resume", /*inc*/ 1, &[]);

    Ok(ResumeAgentResult { status })
}

async fn try_resume_closed_agent(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    receiver_thread_id: ThreadId,
    child_depth: i32,
    agent_path: Option<AgentPath>,
) -> Result<(), FunctionCallError> {
    let config = build_agent_resume_config(turn.as_ref())?;
    Box::pin(session.services.agent_control.resume_agent_from_rollout(
        config,
        receiver_thread_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: session.thread_id(),
            depth: child_depth,
            agent_path,
            agent_nickname: None,
            agent_role: None,
        }),
    ))
    .await
    .map(|_| ())
    .map_err(|err| collab_agent_error(receiver_thread_id, err))
}

async fn resolve_codexx_agent_target(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    target: &str,
) -> Result<ResolvedCodexxAgentTarget, FunctionCallError> {
    session
        .services
        .agent_control
        .register_session_root(session.thread_id, turn.parent_thread_id);

    if let Ok(thread_id) = ThreadId::from_string(target) {
        let metadata = session.services.agent_control.get_agent_metadata(thread_id);
        let label = metadata
            .as_ref()
            .and_then(|metadata| metadata.agent_path.as_ref())
            .map(ToString::to_string)
            .unwrap_or_else(|| thread_id.to_string());
        return Ok(ResolvedCodexxAgentTarget {
            thread_id,
            agent_path: metadata.and_then(|metadata| metadata.agent_path),
            label,
        });
    }

    let current_agent_path = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let agent_path = current_agent_path
        .resolve(target)
        .map_err(FunctionCallError::RespondToModel)?;
    let thread_id = session
        .services
        .agent_control
        .resolve_agent_path_including_stored(session.thread_id, &agent_path)
        .await
        .map_err(|err| match err {
            CodexErr::UnsupportedOperation(message) => FunctionCallError::RespondToModel(message),
            other => FunctionCallError::RespondToModel(other.to_string()),
        })?;

    Ok(ResolvedCodexxAgentTarget {
        thread_id,
        label: agent_path.to_string(),
        agent_path: Some(agent_path),
    })
}

#[derive(Default)]
pub(crate) struct WaitAgentStatusHandler {
    options: WaitAgentTimeoutOptions,
}

impl WaitAgentStatusHandler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for WaitAgentStatusHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(CODEXX_MULTI_AGENT_NAMESPACE, "wait_agent_status")
    }

    fn spec(&self) -> ToolSpec {
        create_codexx_wait_agent_status_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        handle_wait_agent_status(invocation)
            .await
            .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for WaitAgentStatusHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct WaitAgentStatusArgs {
    #[serde(default)]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentStatusResult {
    pub(crate) status: HashMap<String, AgentStatus>,
    pub(crate) timed_out: bool,
}

impl ToolOutput for WaitAgentStatusResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "wait_agent_status")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, None, "wait_agent_status")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent_status")
    }
}

async fn handle_wait_agent_status(
    invocation: ToolInvocation,
) -> Result<WaitAgentStatusResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: WaitAgentStatusArgs = parse_arguments(&arguments)?;
    if args.targets.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "agent ids must be non-empty".to_string(),
        ));
    }
    let mut resolved_targets = Vec::with_capacity(args.targets.len());
    for target in &args.targets {
        resolved_targets.push(resolve_codexx_agent_target(&session, &turn, target).await?);
    }
    let receiver_thread_ids = resolved_targets
        .iter()
        .map(|target| target.thread_id)
        .collect::<Vec<_>>();

    let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
    let mut target_by_thread_id = HashMap::with_capacity(receiver_thread_ids.len());
    for resolved_target in &resolved_targets {
        let receiver_thread_id = resolved_target.thread_id;
        let agent_metadata = session
            .services
            .agent_control
            .get_agent_metadata(receiver_thread_id)
            .unwrap_or_default();
        target_by_thread_id.insert(receiver_thread_id, resolved_target.label.clone());
        receiver_agents.push(codex_protocol::protocol::CollabAgentRef {
            thread_id: receiver_thread_id,
            agent_nickname: agent_metadata.agent_nickname,
            agent_role: agent_metadata.agent_role,
        });
    }

    let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let timeout_ms = match timeout_ms {
        ms if ms <= 0 => {
            return Err(FunctionCallError::RespondToModel(
                "timeout_ms must be greater than zero".to_owned(),
            ));
        }
        ms => ms.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS),
    };

    session
        .send_event(
            &turn,
            CollabWaitingBeginEvent {
                started_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                sender_thread_id: session.thread_id,
                receiver_thread_ids: receiver_thread_ids.clone(),
                receiver_agents: receiver_agents.clone(),
                call_id: call_id.clone(),
            }
            .into(),
        )
        .await;

    let mut status_rxs = Vec::with_capacity(receiver_thread_ids.len());
    let mut initial_final_statuses = Vec::new();
    for id in &receiver_thread_ids {
        match session.services.agent_control.subscribe_status(*id).await {
            Ok(rx) => {
                let status = rx.borrow().clone();
                if is_final(&status) {
                    initial_final_statuses.push((*id, status));
                }
                status_rxs.push((*id, rx));
            }
            Err(CodexErr::ThreadNotFound(_)) => {
                initial_final_statuses.push((*id, AgentStatus::NotFound));
            }
            Err(err) => {
                let mut statuses = HashMap::with_capacity(1);
                statuses.insert(*id, session.services.agent_control.get_status(*id).await);
                session
                    .send_event(
                        &turn,
                        CollabWaitingEndEvent {
                            sender_thread_id: session.thread_id,
                            call_id: call_id.clone(),
                            completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                            agent_statuses: build_wait_agent_statuses(&statuses, &receiver_agents),
                            statuses,
                        }
                        .into(),
                    )
                    .await;
                return Err(collab_agent_error(*id, err));
            }
        }
    }

    let statuses = if !initial_final_statuses.is_empty() {
        initial_final_statuses
    } else {
        let mut futures = FuturesUnordered::new();
        for (id, rx) in status_rxs.into_iter() {
            let session = session.clone();
            futures.push(wait_for_final_status(session, id, rx));
        }
        let mut results = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            match timeout_at(deadline, futures.next()).await {
                Ok(Some(Some(result))) => {
                    results.push(result);
                    break;
                }
                Ok(Some(None)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        if !results.is_empty() {
            loop {
                match futures.next().now_or_never() {
                    Some(Some(Some(result))) => results.push(result),
                    Some(Some(None)) => continue,
                    Some(None) | None => break,
                }
            }
        }
        results
    };

    let timed_out = statuses.is_empty();
    let statuses_by_id = statuses.clone().into_iter().collect::<HashMap<_, _>>();
    let agent_statuses = build_wait_agent_statuses(&statuses_by_id, &receiver_agents);
    let result = WaitAgentStatusResult {
        status: statuses
            .into_iter()
            .filter_map(|(thread_id, status)| {
                target_by_thread_id
                    .get(&thread_id)
                    .cloned()
                    .map(|target| (target, status))
            })
            .collect(),
        timed_out,
    };

    session
        .send_event(
            &turn,
            CollabWaitingEndEvent {
                sender_thread_id: session.thread_id,
                call_id,
                completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                agent_statuses,
                statuses: statuses_by_id,
            }
            .into(),
        )
        .await;

    Ok(result)
}

async fn wait_for_final_status(
    session: Arc<Session>,
    id: ThreadId,
    mut rx: Receiver<AgentStatus>,
) -> Option<(ThreadId, AgentStatus)> {
    loop {
        if rx.changed().await.is_err() {
            return Some((id, session.services.agent_control.get_status(id).await));
        }
        let status = rx.borrow().clone();
        if is_final(&status) {
            return Some((id, status));
        }
    }
}

fn codexx_namespace_spec(spec: ToolSpec) -> ToolSpec {
    match spec {
        ToolSpec::Function(tool) => ToolSpec::Namespace(ResponsesApiNamespace {
            name: CODEXX_MULTI_AGENT_NAMESPACE.to_string(),
            description: CODEXX_MULTI_AGENT_NAMESPACE_DESCRIPTION.to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(tool)],
        }),
        spec => spec,
    }
}
