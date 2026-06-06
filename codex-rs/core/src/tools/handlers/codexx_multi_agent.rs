//! Codexx-specific enhanced multi-agent namespace.
//!
//! Official multi_agent_v1 and MultiAgentV2 handlers remain available as compatibility code.
//! This module exposes the Codexx enhancements under a separate namespace.

use crate::agent::control::ResumeAgentOptions;
use crate::agent::control::ResumeAgentScope;
use crate::agent::control::ResumeConfigRestoreMode;
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
use crate::tools::handlers::multi_agents_spec::create_codexx_list_agents_tool;
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
use codex_state::ThreadMetadata as StateThreadMetadata;
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
        crate::tools::handlers::multi_agents_v2::SpawnAgentHandler::new_for_codexx(
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

pub(crate) struct ListAgentsHandler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for ListAgentsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(CODEXX_MULTI_AGENT_NAMESPACE, "list_agents")
    }

    fn spec(&self) -> ToolSpec {
        create_codexx_list_agents_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        handle_list_agents(invocation).await.map(boxed_tool_output)
    }
}

impl CoreToolRuntime for ListAgentsHandler {
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

const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 200;
const SUMMARY_WIDTH: usize = 60;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListAgentsArgs {
    thread_id: Option<String>,
    task_name: Option<String>,
    agent_path: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ListAgentsResult {
    table: String,
    agents: Vec<ListedCodexxAgent>,
    truncated: bool,
    selection_required: bool,
}

impl ToolOutput for ListAgentsResult {
    fn log_preview(&self) -> String {
        self.table.clone()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ListedCodexxAgent {
    task_name: Option<String>,
    agent_path: Option<String>,
    thread_id: String,
    agent_type: Option<String>,
    nickname: Option<String>,
    live_status: AgentStatus,
    summary: String,
}

async fn handle_list_agents(
    invocation: ToolInvocation,
) -> Result<ListAgentsResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: ListAgentsArgs = parse_arguments(&arguments)?;
    list_codexx_agents(&session, &turn, args).await
}

async fn list_codexx_agents(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    args: ListAgentsArgs,
) -> Result<ListAgentsResult, FunctionCallError> {
    session
        .services
        .agent_control
        .register_session_root(session.thread_id, turn.parent_thread_id);

    let limit = normalized_list_limit(args.limit)?;
    let agent_path_filter = args
        .agent_path
        .as_deref()
        .map(|agent_path| normalize_agent_path_filter(&turn.session_source, agent_path))
        .transpose()?;
    let thread_id_filter = args
        .thread_id
        .as_deref()
        .map(|thread_id| {
            ThreadId::from_string(thread_id)
                .map(|thread_id| thread_id.to_string())
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "invalid thread_id {thread_id}: {err}"
                    ))
                })
        })
        .transpose()?;

    let stored_threads = session
        .services
        .agent_control
        .list_stored_agent_threads(session.thread_id)
        .await
        .map_err(|err| match err {
            CodexErr::UnsupportedOperation(message) => FunctionCallError::RespondToModel(message),
            other => FunctionCallError::RespondToModel(other.to_string()),
        })?;
    let query = args.query.as_ref().map(|query| query.to_lowercase());
    let mut agents = Vec::new();
    for metadata in stored_threads {
        if stored_agent_matches_filters(
            &metadata,
            thread_id_filter.as_deref(),
            args.task_name.as_deref(),
            agent_path_filter.as_deref(),
            query.as_deref(),
        ) {
            agents.push(listed_agent_from_metadata(session, metadata).await);
        }
    }

    agents.sort_by(|left, right| {
        left.agent_path
            .as_deref()
            .unwrap_or_default()
            .cmp(right.agent_path.as_deref().unwrap_or_default())
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    let truncated = agents.len() > limit;
    agents.truncate(limit);
    let table = format_agents_table(&agents, truncated);

    Ok(ListAgentsResult {
        table,
        agents,
        truncated,
        selection_required: false,
    })
}

fn normalized_list_limit(limit: Option<usize>) -> Result<usize, FunctionCallError> {
    match limit.unwrap_or(DEFAULT_LIST_LIMIT) {
        0 => Err(FunctionCallError::RespondToModel(
            "limit must be greater than zero".to_string(),
        )),
        limit => Ok(limit.min(MAX_LIST_LIMIT)),
    }
}

fn normalize_agent_path_filter(
    current_session_source: &SessionSource,
    agent_path: &str,
) -> Result<String, FunctionCallError> {
    if agent_path.starts_with('/') {
        return AgentPath::from_string(agent_path.to_string())
            .map(|agent_path| agent_path.to_string())
            .map_err(FunctionCallError::RespondToModel);
    }
    current_session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root)
        .resolve(agent_path)
        .map(|agent_path| agent_path.to_string())
        .map_err(FunctionCallError::RespondToModel)
}

fn stored_agent_matches_filters(
    metadata: &StateThreadMetadata,
    thread_id: Option<&str>,
    task_name: Option<&str>,
    agent_path: Option<&str>,
    query: Option<&str>,
) -> bool {
    if thread_id.is_some_and(|thread_id| metadata.id.to_string() != thread_id) {
        return false;
    }
    if task_name.is_some_and(|task_name| stored_task_name(metadata).as_deref() != Some(task_name)) {
        return false;
    }
    if agent_path.is_some_and(|agent_path| metadata.agent_path.as_deref() != Some(agent_path)) {
        return false;
    }
    if let Some(query) = query {
        let haystack = [
            metadata.id.to_string(),
            metadata.agent_path.clone().unwrap_or_default(),
            metadata.agent_role.clone().unwrap_or_default(),
            metadata.agent_nickname.clone().unwrap_or_default(),
            metadata.preview.clone().unwrap_or_default(),
            metadata.first_user_message.clone().unwrap_or_default(),
            metadata.title.clone(),
        ]
        .join("\n")
        .to_lowercase();
        if !haystack.contains(query) {
            return false;
        }
    }
    true
}

async fn listed_agent_from_metadata(
    session: &Arc<Session>,
    metadata: StateThreadMetadata,
) -> ListedCodexxAgent {
    let live_status = session.services.agent_control.get_status(metadata.id).await;
    let task_name = stored_task_name(&metadata);
    let summary = stored_agent_summary(&metadata);
    ListedCodexxAgent {
        task_name,
        agent_path: metadata.agent_path,
        thread_id: metadata.id.to_string(),
        agent_type: metadata.agent_role,
        nickname: metadata.agent_nickname,
        live_status,
        summary,
    }
}

fn stored_task_name(metadata: &StateThreadMetadata) -> Option<String> {
    metadata
        .agent_path
        .as_deref()
        .and_then(|agent_path| agent_path.rsplit('/').next())
        .filter(|task_name| !task_name.is_empty() && *task_name != "root")
        .map(ToOwned::to_owned)
}

fn stored_agent_summary(metadata: &StateThreadMetadata) -> String {
    [
        metadata.preview.as_deref(),
        metadata.first_user_message.as_deref(),
        Some(metadata.title.as_str()),
    ]
    .into_iter()
    .flatten()
    .map(|value| value.replace('\n', " ").trim().to_string())
    .find(|value| !value.is_empty())
    .unwrap_or_default()
}

fn format_agents_table(agents: &[ListedCodexxAgent], truncated: bool) -> String {
    let headers = [
        ("TASK", 18usize),
        ("AGENT PATH", 30usize),
        ("THREAD ID", 36usize),
        ("TYPE", 14usize),
        ("NICKNAME", 16usize),
        ("STATUS", 12usize),
        ("SUMMARY", SUMMARY_WIDTH),
    ];
    let mut lines = Vec::with_capacity(agents.len().saturating_add(3));
    lines.push(format_table_row(
        &headers.map(|(header, width)| (header.to_string(), width)),
    ));
    lines.push(format_table_separator(&headers.map(|(_, width)| width)));
    for agent in agents {
        lines.push(format_table_row(&[
            (
                truncate_cell(agent.task_name.as_deref().unwrap_or("-"), headers[0].1),
                headers[0].1,
            ),
            (
                truncate_cell(agent.agent_path.as_deref().unwrap_or("-"), headers[1].1),
                headers[1].1,
            ),
            (truncate_cell(&agent.thread_id, headers[2].1), headers[2].1),
            (
                truncate_cell(agent.agent_type.as_deref().unwrap_or("-"), headers[3].1),
                headers[3].1,
            ),
            (
                truncate_cell(agent.nickname.as_deref().unwrap_or("-"), headers[4].1),
                headers[4].1,
            ),
            (
                truncate_cell(&agent_status_label(&agent.live_status), headers[5].1),
                headers[5].1,
            ),
            (truncate_cell(&agent.summary, headers[6].1), headers[6].1),
        ]));
    }
    if truncated {
        lines.push(format!(
            "... truncated; pass limit up to {MAX_LIST_LIMIT} to show more rows"
        ));
    }
    lines.join("\n")
}

fn format_table_row(cells: &[(String, usize)]) -> String {
    cells
        .iter()
        .map(|(cell, width)| format!("{cell:<width$}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn format_table_separator(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("  ")
}

fn truncate_cell(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(3);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn agent_status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::PendingInit => "pending_init".to_string(),
        AgentStatus::Running => "running".to_string(),
        AgentStatus::Interrupted => "interrupted".to_string(),
        AgentStatus::Completed(_) => "completed".to_string(),
        AgentStatus::Errored(_) => "errored".to_string(),
        AgentStatus::Shutdown => "shutdown".to_string(),
        AgentStatus::NotFound => "not_found".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct ResumeAgentArgs {
    target: Option<String>,
    thread_id: Option<String>,
    task_name: Option<String>,
    agent_path: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
    resume_scope: Option<ResumeScopeArg>,
}

#[derive(Debug, Clone)]
struct ResolvedCodexxAgentTarget {
    thread_id: ThreadId,
    agent_path: Option<AgentPath>,
    label: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResumeScopeArg {
    #[serde(rename = "self")]
    SelfOnly,
    Subtree,
    All,
}

impl ResumeScopeArg {
    fn into_agent_scope(self) -> ResumeAgentScope {
        match self {
            Self::SelfOnly => ResumeAgentScope::SelfOnly,
            Self::Subtree => ResumeAgentScope::Subtree,
            Self::All => ResumeAgentScope::AllDescendants,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum ResumeAgentResult {
    Resumed {
        status: AgentStatus,
        resume_scope: ResumeScopeArg,
    },
    SelectionRequired(ListAgentsResult),
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
    let resume_scope = args.resume_scope.unwrap_or(ResumeScopeArg::SelfOnly);
    let Some(target) = resume_target_from_args(&args)? else {
        let mut list_result = list_codexx_agents(
            &session,
            &turn,
            ListAgentsArgs {
                thread_id: args.thread_id,
                task_name: args.task_name,
                agent_path: args.agent_path,
                query: args.query,
                limit: args.limit,
            },
        )
        .await?;
        list_result.selection_required = true;
        return Ok(ResumeAgentResult::SelectionRequired(list_result));
    };
    let resolved_target = resolve_codexx_agent_target(&session, &turn, &target).await?;
    let receiver_thread_id = resolved_target.thread_id;
    let receiver_agent = session
        .services
        .agent_control
        .get_agent_metadata(receiver_thread_id)
        .unwrap_or_default();
    let child_depth = next_thread_spawn_depth(&turn.session_source);
    let target_agent_path = resolved_target.agent_path.clone();
    let target_depth = target_agent_path
        .as_ref()
        .map(agent_path_depth)
        .unwrap_or(child_depth);
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
    let (mut receiver_agent, mut error) = if matches!(status, AgentStatus::NotFound) {
        match Box::pin(try_resume_closed_agent(
            &session,
            &turn,
            receiver_thread_id,
            child_depth,
            target_agent_path,
            resume_scope,
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
    if error.is_none() && resume_scope == ResumeScopeArg::All {
        match Box::pin(try_resume_all_descendants(
            &session,
            &turn,
            receiver_thread_id,
            target_depth,
        ))
        .await
        {
            Ok(()) => {
                status = session
                    .services
                    .agent_control
                    .get_status(receiver_thread_id)
                    .await;
                receiver_agent = session
                    .services
                    .agent_control
                    .get_agent_metadata(receiver_thread_id)
                    .unwrap_or(receiver_agent);
            }
            Err(err) => {
                status = session
                    .services
                    .agent_control
                    .get_status(receiver_thread_id)
                    .await;
                error = Some(err);
            }
        }
    }
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

    Ok(ResumeAgentResult::Resumed {
        status,
        resume_scope,
    })
}

fn resume_target_from_args(args: &ResumeAgentArgs) -> Result<Option<String>, FunctionCallError> {
    let explicit_targets = [
        args.thread_id.as_ref(),
        args.task_name.as_ref(),
        args.agent_path.as_ref(),
    ]
    .into_iter()
    .flatten()
    .count();
    if args.target.is_some() && explicit_targets > 0 {
        return Err(FunctionCallError::RespondToModel(
            "Provide target or one explicit target field, but not both".to_string(),
        ));
    }
    if explicit_targets > 1 {
        return Err(FunctionCallError::RespondToModel(
            "Provide only one of thread_id, task_name, or agent_path".to_string(),
        ));
    }
    Ok(args
        .target
        .clone()
        .or_else(|| args.thread_id.clone())
        .or_else(|| args.task_name.clone())
        .or_else(|| args.agent_path.clone()))
}

async fn try_resume_closed_agent(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    receiver_thread_id: ThreadId,
    child_depth: i32,
    agent_path: Option<AgentPath>,
    resume_scope: ResumeScopeArg,
) -> Result<(), FunctionCallError> {
    let config = build_agent_resume_config(turn.as_ref())?;
    Box::pin(
        session
            .services
            .agent_control
            .resume_agent_from_rollout_with_options(
                config,
                receiver_thread_id,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: session.thread_id(),
                    depth: child_depth,
                    agent_path,
                    agent_nickname: None,
                    agent_role: None,
                }),
                ResumeAgentOptions {
                    scope: resume_scope.into_agent_scope(),
                    config_restore_mode: ResumeConfigRestoreMode::FullRuntime,
                },
            ),
    )
    .await
    .map(|_| ())
    .map_err(|err| collab_agent_error(receiver_thread_id, err))
}

async fn try_resume_all_descendants(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    receiver_thread_id: ThreadId,
    target_depth: i32,
) -> Result<(), FunctionCallError> {
    let config = build_agent_resume_config(turn.as_ref())?;
    session
        .services
        .agent_control
        .resume_agent_descendants_from_rollout(
            config,
            receiver_thread_id,
            target_depth,
            ResumeConfigRestoreMode::FullRuntime,
        )
        .await
        .map(|_| ())
        .map_err(|err| collab_agent_error(receiver_thread_id, err))
}

fn agent_path_depth(agent_path: &AgentPath) -> i32 {
    agent_path
        .as_str()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
        .saturating_sub(1) as i32
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
