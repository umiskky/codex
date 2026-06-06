use super::*;
use crate::ThreadManager;
use crate::config::AgentRoleConfig;
use crate::config::DEFAULT_AGENT_MAX_DEPTH;
use crate::function_tool::FunctionCallError;
use crate::init_state_db;
use crate::session::tests::make_session_and_context;
use crate::session_prefix::format_subagent_notification_message;
use crate::thread_manager::thread_store_from_config;
use crate::tools::context::ToolOutput;
use crate::tools::handlers::codexx_multi_agent::ListAgentsHandler as CodexxListAgentsHandler;
use crate::tools::handlers::codexx_multi_agent::RegisterAgentHandler as CodexxRegisterAgentHandler;
use crate::tools::handlers::codexx_multi_agent::ResumeAgentHandler as CodexxResumeAgentHandler;
use crate::tools::handlers::codexx_multi_agent::SpawnAgentHandler as CodexxSpawnAgentHandler;
use crate::tools::handlers::codexx_multi_agent::WaitAgentStatusHandler as CodexxWaitAgentStatusHandler;
use crate::tools::handlers::multi_agents_v2::CloseAgentHandler as CloseAgentHandlerV2;
use crate::tools::handlers::multi_agents_v2::FollowupTaskHandler as FollowupTaskHandlerV2;
use crate::tools::handlers::multi_agents_v2::ListAgentsHandler as ListAgentsHandlerV2;
use crate::tools::handlers::multi_agents_v2::SendMessageHandler as SendMessageHandlerV2;
use crate::tools::handlers::multi_agents_v2::SpawnAgentHandler as SpawnAgentHandlerV2;
use crate::tools::handlers::multi_agents_v2::WaitAgentHandler as WaitAgentHandlerV2;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileSystemAccessMode;
use codex_protocol::protocol::FileSystemPath;
use codex_protocol::protocol::FileSystemSandboxEntry;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::NetworkSandboxPolicy;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use core_test_support::PathExt;
use core_test_support::TempDirExt;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
    tool_name: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    ToolInvocation {
        session,
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        source: crate::tools::context::ToolCallSource::Direct,
        payload,
    }
}

fn function_payload(args: serde_json::Value) -> ToolPayload {
    ToolPayload::Function {
        arguments: args.to_string(),
    }
}

fn parse_agent_id(id: &str) -> ThreadId {
    ThreadId::from_string(id).expect("agent id should be valid")
}

fn thread_manager() -> ThreadManager {
    ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        built_in_model_providers(/* openai_base_url */ /*openai_base_url*/ None)["openai"].clone(),
    )
}

async fn install_role_with_model_override(turn: &mut TurnContext) -> String {
    let role_name = "fork-context-role".to_string();
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn
        .config
        .codex_home
        .as_path()
        .join("fork-context-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model = "gpt-5-role-override"
model_provider = "ollama"
model_reasoning_effort = "minimal"
"#,
    )
    .await
    .expect("role config should be written");

    let mut config = (*turn.config).clone();
    config.agent_roles.insert(
        role_name.clone(),
        AgentRoleConfig {
            description: Some("Role with model overrides".to_string()),
            config_file: Some(role_config_path),
            nickname_candidates: None,
        },
    );
    turn.config = Arc::new(config);

    role_name
}

fn set_turn_config(turn: &mut TurnContext, config: crate::config::Config) {
    turn.multi_agent_version = config.multi_agent_version_from_features();
    turn.config = Arc::new(config);
}

fn expect_text_output<T>(output: T) -> (String, Option<bool>)
where
    T: ToolOutput,
{
    let response = output.to_response_item(
        "call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => {
            let content = match output.body {
                FunctionCallOutputBody::Text(text) => text,
                FunctionCallOutputBody::ContentItems(items) => {
                    codex_protocol::models::function_call_output_content_items_to_text(&items)
                        .unwrap_or_default()
                }
            };
            (content, output.success)
        }
        other => panic!("expected function output, got {other:?}"),
    }
}

async fn wait_for_state_thread(
    state_db: &crate::StateDbHandle,
    thread_id: ThreadId,
) -> codex_state::ThreadMetadata {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Some(metadata)) = state_db.get_thread(thread_id).await {
                return metadata;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("thread metadata should be persisted to sqlite")
}

#[derive(Debug, Deserialize)]
struct ListAgentsResult {
    agents: Vec<ListedAgentResult>,
}

#[derive(Debug, Deserialize)]
struct ListedAgentResult {
    agent_name: String,
    agent_status: serde_json::Value,
    last_task_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexxListAgentsResult {
    table: String,
    agents: Vec<CodexxListedAgentResult>,
    truncated: bool,
    selection_required: bool,
}

#[derive(Debug, Deserialize)]
struct CodexxListedAgentResult {
    task_name: Option<String>,
    agent_path: Option<String>,
    thread_id: String,
    agent_type: Option<String>,
    nickname: Option<String>,
    live_status: serde_json::Value,
    summary: String,
}

#[tokio::test]
async fn handler_rejects_non_function_payloads() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        ToolPayload::Custom {
            input: "hello".to_string(),
        },
    );
    let Err(err) = SpawnAgentHandler::default().handle(invocation).await else {
        panic!("payload should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_rejects_empty_message() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "   "})),
    );
    let Err(err) = SpawnAgentHandler::default().handle(invocation).await else {
        panic!("empty message should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("Empty message can't be sent to an agent".to_string())
    );
}

#[tokio::test]
async fn spawn_agent_rejects_when_message_and_items_are_both_set() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "hello",
            "items": [{"type": "mention", "name": "drive", "path": "app://drive"}]
        })),
    );
    let Err(err) = SpawnAgentHandler::default().handle(invocation).await else {
        panic!("message+items should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_uses_explorer_role_and_preserves_approval_policy() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let mut config = (*turn.config).clone();
    let provider_info =
        built_in_model_providers(/* openai_base_url */ /*openai_base_url*/ None)["ollama"].clone();
    config.model_provider_id = "ollama".to_string();
    config.model_provider = provider_info.clone();
    config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.provider = create_model_provider(provider_info, turn.auth_manager.clone());
    turn.config = Arc::new(config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "agent_type": "explorer"
        })),
    );
    let output = SpawnAgentHandler::default()
        .handle(invocation)
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = parse_agent_id(&result.agent_id);
    assert!(
        result
            .nickname
            .as_deref()
            .is_some_and(|nickname| !nickname.is_empty())
    );
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.approval_policy, AskForApproval::OnRequest);
    assert_eq!(snapshot.model_provider_id, "ollama");
}

#[tokio::test]
async fn codexx_register_agent_registers_agents_and_spawn_uses_them_without_restart() {
    #[derive(Debug, Deserialize)]
    struct RegisterAgentResult {
        agent_types: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let project = tempfile::tempdir().expect("project temp dir");
    let project_config_dir = project.path().join(".codex");
    let project_agents_dir = project_config_dir.join("agents");
    tokio::fs::create_dir_all(&project_agents_dir)
        .await
        .expect("project agents dir should be created");
    tokio::fs::write(
        project_agents_dir.join("fresh.toml"),
        r#"description = "Fresh project role"
nickname_candidates = ["Freshy"]
developer_instructions = "Use the fresh project role."
model = "gpt-5.4"
model_reasoning_effort = "minimal"
"#,
    )
    .await
    .expect("project role config should be written");

    let mut config = (*turn.config).clone();
    config.cwd = project.path().abs();
    #[allow(deprecated)]
    {
        turn.cwd = project.path().abs();
    }
    turn.config = Arc::new(config);
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let register_output = CodexxRegisterAgentHandler
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "register_agent",
            function_payload(json!({
                "agent_config_paths": [project_agents_dir.join("fresh.toml")],
            })),
        ))
        .await
        .expect("register_agent should load the project role file");
    let (register_content, _) = expect_text_output(register_output);
    let register_result: RegisterAgentResult =
        serde_json::from_str(&register_content).expect("register result should be json");
    assert_eq!(register_result.agent_types, vec!["fresh".to_string()]);

    let output = CodexxSpawnAgentHandler::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "fresh_task",
                "agent_type": "fresh",
                "fork_turns": "none"
            })),
        ))
        .await
        .expect("codexx spawn_agent should use the project role");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(result.task_name, "/root/fresh_task");
    assert_eq!(result.nickname.as_deref(), Some("Freshy"));
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "fresh_task")
        .await
        .expect("fresh task should resolve");
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(snapshot.model, "gpt-5.4");
    assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::Minimal));
}

#[tokio::test]
async fn codexx_register_agent_accepts_relative_agent_directory() {
    #[derive(Debug, Deserialize)]
    struct RegisterAgentResult {
        agent_types: Vec<String>,
    }

    let (session, mut turn) = make_session_and_context().await;
    let project = tempfile::tempdir().expect("project temp dir");
    let project_config_dir = project.path().join(".codex");
    let project_agents_dir = project_config_dir.join("agents");
    tokio::fs::create_dir_all(&project_agents_dir)
        .await
        .expect("project agents dir should be created");
    tokio::fs::write(
        project_agents_dir.join("reviewer.toml"),
        r#"description = "Reviewer role"
developer_instructions = "Review carefully."
model = "gpt-5.4"
"#,
    )
    .await
    .expect("project role config should be written");

    let mut config = (*turn.config).clone();
    config.cwd = project.path().abs();
    #[allow(deprecated)]
    {
        turn.cwd = project.path().abs();
    }
    turn.config = Arc::new(config);

    let output = CodexxRegisterAgentHandler
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "register_agent",
            function_payload(json!({
                "agent_config_paths": [".codex/agents"],
            })),
        ))
        .await
        .expect("register_agent should load relative project agent directory");
    let (content, _) = expect_text_output(output);
    let result: RegisterAgentResult =
        serde_json::from_str(&content).expect("register result should be json");
    assert_eq!(result.agent_types, vec!["reviewer".to_string()]);
}

#[tokio::test]
async fn codexx_spawn_agent_role_permissions_override_parent_runtime_permissions() {
    let (mut session, mut turn) = make_session_and_context().await;
    let project = tempfile::tempdir().expect("project temp dir");
    let project_agents_dir = project.path().join(".codex").join("agents");
    tokio::fs::create_dir_all(&project_agents_dir)
        .await
        .expect("project agents dir should be created");
    tokio::fs::write(
        project_agents_dir.join("privileged.toml"),
        r#"description = "Privileged role"
developer_instructions = "Use the privileged project role."
approval_policy = "never"
sandbox_mode = "danger-full-access"
"#,
    )
    .await
    .expect("project role config should be written");

    let mut config = (*turn.config).clone();
    config.cwd = project.path().abs();
    config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("parent approval policy should be set");
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .expect("parent permission profile should be set");
    #[allow(deprecated)]
    {
        turn.cwd = project.path().abs();
    }
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("parent turn approval policy should be set");
    turn.permission_profile = PermissionProfile::read_only();
    turn.config = Arc::new(config);

    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    CodexxRegisterAgentHandler
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "register_agent",
            function_payload(json!({
                "agent_config_paths": [project_agents_dir.join("privileged.toml")],
            })),
        ))
        .await
        .expect("register_agent should load the privileged role");

    let output = CodexxSpawnAgentHandler::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "privileged_task",
                "agent_type": "privileged",
                "fork_turns": "none"
            })),
        ))
        .await
        .expect("codexx spawn_agent should use the privileged role");
    let _ = expect_text_output(output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "privileged_task")
        .await
        .expect("privileged task should resolve");
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(snapshot.approval_policy, AskForApproval::Never);
    assert_eq!(snapshot.permission_profile, PermissionProfile::Disabled);
}

#[tokio::test]
async fn spawn_agent_fork_context_rejects_agent_type_override() {
    let (mut session, mut turn) = make_session_and_context().await;
    let role_name = install_role_with_model_override(&mut turn).await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let err = SpawnAgentHandler::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "agent_type": role_name,
                "fork_context": true
            })),
        ))
        .await
        .err()
        .expect("fork_context should reject agent_type overrides");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork.".to_string(),
        )
    );
}

#[tokio::test]
async fn spawn_agent_fork_context_rejects_child_model_overrides() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let err = SpawnAgentHandler::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "model": "gpt-5-child-override",
                "reasoning_effort": "low",
                "fork_context": true
            })),
        ))
        .await
        .err()
        .expect("forked spawn should reject child model overrides");

    assert_eq!(
        err,
            FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork.".to_string(),
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_fork_turns_all_rejects_agent_type_override() {
    let (mut session, mut turn) = make_session_and_context().await;
    let role_name = install_role_with_model_override(&mut turn).await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let turn = TurnContext {
        config: Arc::new(config),
        multi_agent_version: codex_protocol::protocol::MultiAgentVersion::V2,
        ..turn
    };

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "fork_context_v2",
                "agent_type": role_name,
                "fork_turns": "all"
            })),
        ))
        .await
        .err()
        .expect("fork_turns=all should reject agent_type overrides");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork.".to_string(),
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_defaults_to_full_fork_and_rejects_child_model_overrides() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "fork_context_v2",
                "model": "gpt-5-child-override",
                "reasoning_effort": "low"
            })),
        ))
        .await
        .err()
        .expect("default full fork should reject child model overrides");

    assert_eq!(
        err,
            FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork.".to_string(),
        )
    );
}

#[tokio::test]
async fn spawn_agent_service_tier_override_validates_the_effective_child_model() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
    }

    {
        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandler::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({
                    "message": "inspect this repo",
                    "model": "gpt-5.4",
                    "service_tier": ServiceTier::Fast.request_value()
                })),
            ))
            .await
            .expect("spawn_agent should accept a supported explicit service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(parse_agent_id(&result.agent_id))
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(
            snapshot.service_tier,
            Some(ServiceTier::Fast.request_value().to_string())
        );
    }

    {
        let (session, turn) = make_session_and_context().await;
        let err = SpawnAgentHandler::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({
                    "message": "inspect this repo",
                    "model": "gpt-5.4",
                    "service_tier": "turbo"
                })),
            ))
            .await
            .err()
            .expect("unknown service tier should be rejected");

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "Service tier `turbo` is not supported for model `gpt-5.4`. Supported service tiers: priority"
                    .to_string()
            )
        );
    }

    {
        let (session, turn) = make_session_and_context().await;
        let err = SpawnAgentHandler::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({
                    "message": "inspect this repo",
                    "model": "gpt-5.3-codex",
                    "service_tier": ServiceTier::Fast.request_value()
                })),
            ))
            .await
            .err()
            .expect("tier unsupported by the final child model should be rejected");

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "Service tier `priority` is not supported for model `gpt-5.3-codex`. Supported service tiers: none"
                    .to_string()
            )
        );
    }
}

#[tokio::test]
async fn spawn_agent_service_tier_inheritance_preserves_supported_or_configured_tiers() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
    }

    {
        let (mut session, turn) = make_session_and_context().await;
        let mut turn = turn
            .with_model("gpt-5.4".to_string(), &session.services.models_manager)
            .await;
        let mut config = (*turn.config).clone();
        config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
        turn.config = Arc::new(config);
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandler::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({"message": "inspect this repo"})),
            ))
            .await
            .expect("spawn_agent should inherit a supported parent service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(parse_agent_id(&result.agent_id))
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(
            snapshot.service_tier,
            Some(ServiceTier::Fast.request_value().to_string())
        );
    }

    {
        let (mut session, turn) = make_session_and_context().await;
        let mut turn = turn
            .with_model("gpt-5.4".to_string(), &session.services.models_manager)
            .await;
        let mut config = (*turn.config).clone();
        config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
        turn.config = Arc::new(config);
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandler::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({
                    "message": "inspect this repo",
                    "model": "gpt-5.3-codex"
                })),
            ))
            .await
            .expect("spawn_agent should clear unsupported inherited service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(parse_agent_id(&result.agent_id))
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(snapshot.service_tier, None);
    }

    {
        let (mut session, mut turn) = make_session_and_context().await;
        tokio::fs::create_dir_all(&turn.config.codex_home)
            .await
            .expect("codex home should be created");
        let role_config_path = turn
            .config
            .codex_home
            .as_path()
            .join("service-tier-role.toml");
        tokio::fs::write(
            &role_config_path,
            r#"model = "gpt-5.4"
service_tier = "priority"
"#,
        )
        .await
        .expect("role config should be written");

        let role_name = "service-tier-role".to_string();
        let mut config = (*turn.config).clone();
        config.agent_roles.insert(
            role_name.clone(),
            AgentRoleConfig {
                description: Some("Role with a child service tier".to_string()),
                config_file: Some(role_config_path),
                nickname_candidates: None,
            },
        );
        turn.config = Arc::new(config);
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandler::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({
                    "message": "inspect this repo",
                    "agent_type": role_name
                })),
            ))
            .await
            .expect("spawn_agent should preserve the child role service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(parse_agent_id(&result.agent_id))
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(
            snapshot.service_tier,
            Some(ServiceTier::Fast.request_value().to_string())
        );
    }
}

#[tokio::test]
async fn spawn_agent_role_service_tier_falls_back_to_supported_parent_tier() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
    }

    let (mut session, turn) = make_session_and_context().await;
    let mut turn = turn
        .with_model("gpt-5.4".to_string(), &session.services.models_manager)
        .await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn.config.codex_home.as_path().join("tiered-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model = "gpt-5.4"
service_tier = "turbo"
"#,
    )
    .await
    .expect("role config should be written");

    let role_name = "tiered-role".to_string();
    let mut config = (*turn.config).clone();
    config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
    config.agent_roles.insert(
        role_name.clone(),
        AgentRoleConfig {
            description: Some("Role with an unsupported child tier".to_string()),
            config_file: Some(role_config_path),
            nickname_candidates: None,
        },
    );
    turn.config = Arc::new(config);
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let output = SpawnAgentHandler::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "agent_type": role_name
            })),
        ))
        .await
        .expect("spawn_agent should fall back to the supported parent tier");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let snapshot = manager
        .get_thread(parse_agent_id(&result.agent_id))
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(
        snapshot.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
}

#[tokio::test]
async fn spawn_agent_role_service_tier_does_not_hide_invalid_spawn_request() {
    let (session, mut turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn.config.codex_home.as_path().join("tiered-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model = "gpt-5.4"
service_tier = "priority"
"#,
    )
    .await
    .expect("role config should be written");

    let role_name = "tiered-role".to_string();
    let mut config = (*turn.config).clone();
    config.agent_roles.insert(
        role_name.clone(),
        AgentRoleConfig {
            description: Some("Role with a supported child tier".to_string()),
            config_file: Some(role_config_path),
            nickname_candidates: None,
        },
    );
    turn.config = Arc::new(config);

    let result = SpawnAgentHandler::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "agent_type": role_name,
                "service_tier": "turbo"
            })),
        ))
        .await;

    assert_eq!(
        result.err(),
        Some(FunctionCallError::RespondToModel(
            "Service tier `turbo` is not supported for model `gpt-5.4`. Supported service tiers: priority"
                .to_string()
        ))
    );
}

#[tokio::test]
async fn spawn_agent_full_history_fork_accepts_explicit_service_tier() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
    }

    let (mut session, turn) = make_session_and_context().await;
    let turn = turn
        .with_model("gpt-5.4".to_string(), &session.services.models_manager)
        .await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let output = SpawnAgentHandler::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "fork_context": true,
                "service_tier": ServiceTier::Fast.request_value()
            })),
        ))
        .await
        .expect("full-history fork should accept explicit service tier");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let snapshot = manager
        .get_thread(parse_agent_id(&result.agent_id))
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(
        snapshot.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_full_history_fork_accepts_explicit_service_tier() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    let (mut session, turn) = make_session_and_context().await;
    let mut turn = turn
        .with_model("gpt-5.4".to_string(), &session.services.models_manager)
        .await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "fork_with_tier",
                "service_tier": ServiceTier::Fast.request_value()
            })),
        ))
        .await
        .expect("multi-agent v2 full-history fork should accept explicit service tier");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(
            session.thread_id,
            &turn.session_source,
            result.task_name.as_str(),
        )
        .await
        .expect("spawned task name should resolve");
    let snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(
        snapshot.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_partial_fork_turns_allows_agent_type_override() {
    let (mut session, mut turn) = make_session_and_context().await;
    let role_name = install_role_with_model_override(&mut turn).await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let turn = TurnContext {
        config: Arc::new(config),
        multi_agent_version: codex_protocol::protocol::MultiAgentVersion::V2,
        ..turn
    };

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "partial_fork",
                "agent_type": role_name,
                "fork_turns": "1"
            })),
        ))
        .await
        .expect("partial fork should allow agent_type overrides");
    let (content, _) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(result["task_name"], "/root/partial_fork");
    let agent_id = manager
        .captured_ops()
        .into_iter()
        .map(|(thread_id, _)| thread_id)
        .find(|thread_id| *thread_id != root.thread_id)
        .expect("spawned agent should receive an op");
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(snapshot.model, "gpt-5-role-override");
    assert_eq!(snapshot.model_provider_id, "ollama");
    assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::Minimal));
}

#[tokio::test]
async fn spawn_agent_returns_agent_id_without_task_name() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let output = SpawnAgentHandler::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");

    assert!(result["agent_id"].is_string());
    assert!(result.get("task_name").is_none());
    assert!(result.get("nickname").is_some());
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_spawn_requires_task_name() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("missing task_name should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("missing task_name should surface as a model-facing error");
    };
    assert!(message.contains("missing field `task_name`"));
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_legacy_items_field() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "items": [{"type": "text", "text": "inspect this repo"}],
            "task_name": "worker"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("legacy items field should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("legacy items field should surface as a model-facing error");
    };
    assert!(message.contains("unknown field `items`"));
}

#[tokio::test]
async fn spawn_agent_errors_when_manager_dropped() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "hello"})),
    );
    let Err(err) = SpawnAgentHandler::default().handle(invocation).await else {
        panic!("spawn should fail without a manager");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("collab manager unavailable".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_returns_path_and_send_message_accepts_relative_path() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(spawn_output);
    let spawn_result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn result should parse");
    assert_eq!(spawn_result.task_name, "/root/test_process");
    assert_eq!(spawn_result.nickname, None);

    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "test_process")
        .await
        .expect("relative path should resolve");
    let child_snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(
        child_snapshot.session_source.get_agent_path().as_deref(),
        Some("/root/test_process")
    );
    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child_thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == AgentPath::root()
                        && communication.recipient.as_str() == "/root/test_process"
                        && communication.other_recipients.is_empty()
                        && communication.content == "inspect this repo"
                        && communication.trigger_turn
            )
    }));

    SendMessageHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "send_message",
            function_payload(json!({
                "target": "test_process",
                "message": "continue"
            })),
        ))
        .await
        .expect("send_message should accept v2 path");

    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child_thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == AgentPath::root()
                        && communication.recipient.as_str() == "/root/test_process"
                        && communication.other_recipients.is_empty()
                        && communication.content == "continue"
                        && !communication.trigger_turn
            )
    }));
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_legacy_fork_context() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker",
                "fork_context": true
            })),
        ))
        .await
        .err()
        .expect("legacy fork_context should be rejected");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "fork_context is not supported in MultiAgentV2; use fork_turns instead".to_string()
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_invalid_fork_turns_string() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker",
                "fork_turns": "banana"
            })),
        ))
        .await
        .err()
        .expect("invalid fork_turns should be rejected");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "fork_turns must be `none`, `all`, or a positive integer string".to_string()
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_zero_fork_turns() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker",
                "fork_turns": "0"
            })),
        ))
        .await
        .err()
        .expect("zero turn count should be rejected");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "fork_turns must be `none`, `all`, or a positive integer string".to_string()
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_send_message_accepts_root_target_from_child() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });

    SendMessageHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "send_message",
            function_payload(json!({
                "target": "/root",
                "message": "done"
            })),
        ))
        .await
        .expect("send_message should accept the root agent path");

    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == root.thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == child_path
                        && communication.recipient == AgentPath::root()
                        && communication.other_recipients.is_empty()
                        && communication.content == "done"
                        && !communication.trigger_turn
            )
    }));
}

#[tokio::test]
async fn multi_agent_v2_followup_task_rejects_root_target_from_child() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path),
        agent_nickname: None,
        agent_role: None,
    });

    let Err(err) = FollowupTaskHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "followup_task",
            function_payload(json!({
                "target": "/root",
                "message": "run this",
            })),
        ))
        .await
    else {
        panic!("followup_task should reject the root target");
    };

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Follow-up tasks can't target the root agent".to_string()
        )
    );
    let root_ops = manager
        .captured_ops()
        .into_iter()
        .filter_map(|(id, op)| (id == root.thread_id).then_some(op))
        .collect::<Vec<_>>();
    assert!(!root_ops.iter().any(|op| matches!(op, Op::Interrupt)));
    assert!(
        !root_ops
            .iter()
            .any(|op| matches!(op, Op::InterAgentCommunication { .. }))
    );
}

#[tokio::test]
async fn multi_agent_v2_list_agents_returns_completed_status_and_last_task_message() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("child thread should exist");
    let child_turn = child_thread.codex.session.new_default_turn().await;
    child_thread
        .codex
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("done".to_string()),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, success) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    let agent_names = result
        .agents
        .iter()
        .map(|agent| agent.agent_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(agent_names, vec!["/root", "/root/worker"]);
    let root_agent = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root")
        .expect("root agent should be listed");
    assert_eq!(root_agent.last_task_message.as_deref(), Some("Main thread"));
    let worker = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root/worker")
        .expect("worker agent should be listed");
    assert_eq!(worker.agent_status, json!({"completed": "done"}));
    assert_eq!(
        worker.last_task_message.as_deref(),
        Some("inspect this repo")
    );
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_list_agents_filters_by_relative_path_prefix() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config.clone());
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(researcher_path),
        agent_nickname: None,
        agent_role: None,
    });

    let output = ListAgentsHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "list_agents",
            function_payload(json!({
                "path_prefix": "worker"
            })),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].agent_name, worker_path.as_str());
    assert_eq!(result.agents[0].last_task_message.as_deref(), Some("build"));
}

#[tokio::test]
async fn multi_agent_v2_list_agents_omits_closed_agents() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    session
        .services
        .agent_control
        .close_agent(agent_id)
        .await
        .expect("close_agent should succeed");

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].agent_name, "/root");
    assert_eq!(
        result.agents[0].last_task_message.as_deref(),
        Some("Main thread")
    );
}

#[tokio::test]
async fn codexx_list_agents_reads_historical_state_db_and_filters() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow multi-agent v2");
    let state_db = init_state_db(&config)
        .await
        .expect("sqlite state db should initialize");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db.clone()),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for (task_name, message) in [
        ("alpha", "Alpha summary from the first task"),
        ("beta", "Beta summary from the second task"),
    ] {
        CodexxSpawnAgentHandler::default()
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "spawn_agent",
                function_payload(json!({
                    "message": message,
                    "task_name": task_name,
                    "fork_turns": "none"
                })),
            ))
            .await
            .expect("codexx spawn_agent should succeed");
    }

    let alpha_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "alpha")
        .await
        .expect("alpha should resolve");
    let beta_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "beta")
        .await
        .expect("beta should resolve");
    wait_for_state_thread(&state_db, alpha_thread_id).await;
    wait_for_state_thread(&state_db, beta_thread_id).await;
    session
        .services
        .agent_control
        .shutdown_live_agent(alpha_thread_id)
        .await
        .expect("alpha shutdown should submit");

    let output = CodexxListAgentsHandler
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("codexx list_agents should read historical sqlite records");
    let (content, success) = expect_text_output(output);
    let result: CodexxListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");
    let rows = result
        .agents
        .iter()
        .map(|agent| {
            (
                agent.task_name.clone(),
                agent.agent_path.clone(),
                agent.thread_id.clone(),
                agent.summary.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            (
                Some("alpha".to_string()),
                Some("/root/alpha".to_string()),
                alpha_thread_id.to_string(),
                "Alpha summary from the first task".to_string(),
            ),
            (
                Some("beta".to_string()),
                Some("/root/beta".to_string()),
                beta_thread_id.to_string(),
                "Beta summary from the second task".to_string(),
            ),
        ]
    );
    assert_eq!(result.truncated, false);
    assert_eq!(result.selection_required, false);
    assert_eq!(success, Some(true));
    assert!(result.table.contains("TASK"));
    assert!(result.table.contains("THREAD ID"));
    assert!(result.table.contains("/root/alpha"));
    assert!(result.table.contains("Alpha summary from the first task"));
    let alpha = result
        .agents
        .iter()
        .find(|agent| agent.task_name.as_deref() == Some("alpha"))
        .expect("alpha row should be present");
    assert_eq!(alpha.live_status, json!("not_found"));
    assert_eq!(alpha.agent_type, None);
    assert!(alpha.nickname.is_some());

    let query_output = CodexxListAgentsHandler
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "list_agents",
            function_payload(json!({"query": "second task"})),
        ))
        .await
        .expect("query filter should succeed");
    let (query_content, _) = expect_text_output(query_output);
    let query_result: CodexxListAgentsResult =
        serde_json::from_str(&query_content).expect("query result should be json");
    assert_eq!(query_result.agents.len(), 1);
    assert_eq!(query_result.agents[0].task_name.as_deref(), Some("beta"));

    let path_output = CodexxListAgentsHandler
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "list_agents",
            function_payload(json!({"agent_path": "/root/alpha"})),
        ))
        .await
        .expect("agent_path filter should succeed");
    let (path_content, _) = expect_text_output(path_output);
    let path_result: CodexxListAgentsResult =
        serde_json::from_str(&path_content).expect("path result should be json");
    assert_eq!(path_result.agents.len(), 1);
    assert_eq!(path_result.agents[0].thread_id, alpha_thread_id.to_string());

    let id_output = CodexxListAgentsHandler
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({"thread_id": beta_thread_id.to_string()})),
        ))
        .await
        .expect("thread_id filter should succeed");
    let (id_content, _) = expect_text_output(id_output);
    let id_result: CodexxListAgentsResult =
        serde_json::from_str(&id_content).expect("id result should be json");
    assert_eq!(id_result.agents.len(), 1);
    assert_eq!(
        id_result.agents[0].agent_path.as_deref(),
        Some("/root/beta")
    );
}

#[tokio::test]
async fn multi_agent_v2_send_message_rejects_legacy_items_field() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let invocation = invocation(
        session,
        turn,
        "send_message",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [
                {"type": "mention", "name": "drive", "path": "app://google_drive"},
                {"type": "text", "text": "read the folder"}
            ]
        })),
    );

    let Err(err) = SendMessageHandlerV2.handle(invocation).await else {
        panic!("legacy items field should be rejected in v2");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("legacy items field should surface as a model-facing error");
    };
    assert!(message.contains("unknown field `items`"));
}

#[tokio::test]
async fn multi_agent_v2_send_message_rejects_interrupt_parameter() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");

    let invocation = invocation(
        session,
        turn,
        "send_message",
        function_payload(json!({
            "target": agent_id.to_string(),
            "message": "continue",
            "interrupt": true
        })),
    );

    let Err(err) = SendMessageHandlerV2.handle(invocation).await else {
        panic!("send_message interrupt parameter should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("expected model-facing parse error");
    };
    assert!(message.starts_with(
        "failed to parse function arguments: unknown field `interrupt`, expected `target` or `message`"
    ));

    let ops = manager.captured_ops();
    let ops_for_agent: Vec<&Op> = ops
        .iter()
        .filter_map(|(id, op)| (*id == agent_id).then_some(op))
        .collect();
    assert!(!ops_for_agent.iter().any(|op| matches!(op, Op::Interrupt)));
    assert!(!ops_for_agent.iter().any(|op| matches!(
        op,
        Op::InterAgentCommunication { communication }
            if communication.author == AgentPath::root()
                && communication.recipient.as_str() == "/root/worker"
                && communication.other_recipients.is_empty()
                && communication.content == "continue"
                && !communication.trigger_turn
    )));
}

#[tokio::test]
async fn multi_agent_v2_followup_task_completion_notifies_parent_on_every_turn() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    // Production spawn_agent calls happen after the parent turn has resolved
    // and stored its runtime; mirror that before using the synthetic handler.
    root.thread.codex.session.new_default_turn().await;
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let thread = manager
        .get_thread(agent_id)
        .await
        .expect("worker thread should exist");
    let worker_path = AgentPath::try_from("/root/worker").expect("worker path");

    let first_turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            first_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: first_turn.sub_id.clone(),
                last_agent_message: Some("first done".to_string()),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    FollowupTaskHandlerV2
        .handle(invocation(
            session,
            turn,
            "followup_task",
            function_payload(json!({
                "target": agent_id.to_string(),
                "message": "continue",
            })),
        ))
        .await
        .expect("followup_task should succeed");

    let second_turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            second_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: second_turn.sub_id.clone(),
                last_agent_message: Some("second done".to_string()),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    let first_notification = format_subagent_notification_message(
        worker_path.as_str(),
        &AgentStatus::Completed(Some("first done".to_string())),
    );
    let second_notification = format_subagent_notification_message(
        worker_path.as_str(),
        &AgentStatus::Completed(Some("second done".to_string())),
    );

    let notifications = timeout(Duration::from_secs(5), async {
        loop {
            let notifications = manager
                .captured_ops()
                .into_iter()
                .filter_map(|(id, op)| {
                    (id == root.thread_id)
                        .then_some(op)
                        .and_then(|op| match op {
                            Op::InterAgentCommunication { communication }
                                if communication.author == worker_path
                                    && communication.recipient == AgentPath::root()
                                    && communication.other_recipients.is_empty()
                                    && !communication.trigger_turn =>
                            {
                                Some(communication.content)
                            }
                            _ => None,
                        })
                })
                .collect::<Vec<_>>();
            let first_count = notifications
                .iter()
                .filter(|message| **message == first_notification)
                .count();
            let second_count = notifications
                .iter()
                .filter(|message| **message == second_notification)
                .count();
            if first_count == 1 && second_count == 1 {
                break notifications;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("parent should receive one completion notification per child turn");

    assert_eq!(notifications.len(), 2);
}

#[tokio::test]
async fn multi_agent_v2_followup_task_rejects_legacy_items_field() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let invocation = invocation(
        session,
        turn,
        "followup_task",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [{"type": "text", "text": "continue"}],
        })),
    );

    let Err(err) = FollowupTaskHandlerV2.handle(invocation).await else {
        panic!("legacy items field should be rejected in v2");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("legacy items field should surface as a model-facing error");
    };
    assert!(message.contains("unknown field `items`"));
}

#[tokio::test]
async fn multi_agent_v2_interrupted_turn_does_not_notify_parent() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let thread = manager
        .get_thread(agent_id)
        .await
        .expect("worker thread should exist");

    let aborted_turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            aborted_turn.as_ref(),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(aborted_turn.sub_id.clone()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            }),
        )
        .await;

    let notifications = manager
        .captured_ops()
        .into_iter()
        .filter_map(|(id, op)| {
            (id == root.thread_id)
                .then_some(op)
                .and_then(|op| match op {
                    Op::InterAgentCommunication { communication }
                        if communication.author.as_str() == "/root/worker"
                            && communication.recipient == AgentPath::root()
                            && communication.other_recipients.is_empty()
                            && !communication.trigger_turn =>
                    {
                        Some(communication.content)
                    }
                    _ => None,
                })
        })
        .collect::<Vec<_>>();

    assert_eq!(notifications, Vec::<String>::new());
}

#[tokio::test]
async fn multi_agent_v2_spawn_omits_agent_id_when_named() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");

    assert!(result.get("agent_id").is_none());
    assert_eq!(result["task_name"], "/root/test_process");
    assert!(result.get("nickname").is_none());
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_spawn_surfaces_task_name_validation_errors() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "task_name": "BadName"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("invalid agent name should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "agent_name must use only lowercase letters, digits, and underscores".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_reapplies_runtime_sandbox_after_role_config() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let expected_sandbox = turn.config.legacy_sandbox_policy();
    #[allow(deprecated)]
    let mut expected_file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&expected_sandbox, &turn.cwd);
    expected_file_system_sandbox_policy
        .entries
        .push(FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                pattern: "**/.env".to_string(),
            },
            access: FileSystemAccessMode::Deny,
        });
    let expected_network_sandbox_policy = NetworkSandboxPolicy::from(&expected_sandbox);
    let expected_permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::from_legacy_sandbox_policy(&expected_sandbox),
        &expected_file_system_sandbox_policy,
        expected_network_sandbox_policy,
    );
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.permission_profile = expected_permission_profile.clone();
    assert_ne!(
        expected_permission_profile,
        turn.config.permissions.effective_permission_profile(),
        "test requires a runtime profile override that differs from base config"
    );

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "await this command",
            "agent_type": "explorer"
        })),
    );
    let output = SpawnAgentHandler::default()
        .handle(invocation)
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = parse_agent_id(&result.agent_id);
    assert!(
        result
            .nickname
            .as_deref()
            .is_some_and(|nickname| !nickname.is_empty())
    );

    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.sandbox_policy(), expected_sandbox);
    assert_eq!(snapshot.approval_policy, AskForApproval::OnRequest);
    assert_eq!(snapshot.permission_profile, expected_permission_profile);
    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist");
    let child_turn = child_thread.codex.session.new_default_turn().await;
    assert_eq!(
        child_turn.file_system_sandbox_policy(),
        expected_file_system_sandbox_policy
    );
    assert_eq!(
        child_turn.network_sandbox_policy(),
        expected_network_sandbox_policy
    );
    assert_eq!(child_turn.permission_profile(), expected_permission_profile);
}

#[tokio::test]
async fn spawn_agent_rejects_when_depth_limit_exceeded() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let max_depth = turn.config.agent_max_depth;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: max_depth,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "hello"})),
    );
    let Err(err) = SpawnAgentHandler::default().handle(invocation).await else {
        panic!("spawn should fail when depth limit exceeded");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_allows_depth_up_to_configured_max_depth() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let mut config = (*turn.config).clone();
    config.agent_max_depth = DEFAULT_AGENT_MAX_DEPTH + 1;
    turn.config = Arc::new(config);
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: DEFAULT_AGENT_MAX_DEPTH,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "hello"})),
    );
    let output = SpawnAgentHandler::default()
        .handle(invocation)
        .await
        .expect("spawn should succeed within configured depth");
    let (content, success) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert!(!result.agent_id.is_empty());
    assert!(
        result
            .nickname
            .as_deref()
            .is_some_and(|nickname| !nickname.is_empty())
    );
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_spawn_agent_ignores_configured_max_depth() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config.agent_max_depth = 1;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    set_turn_config(&mut turn, config);
    let parent_path = AgentPath::try_from("/root/parent").expect("agent path");
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(parent_path),
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "hello",
            "task_name": "child",
            "fork_turns": "none"
        })),
    );
    let output = SpawnAgentHandlerV2::default()
        .handle(invocation)
        .await
        .expect("multi-agent v2 spawn should ignore max depth");
    let (content, success) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(result.task_name, "/root/parent/child");
    assert_eq!(result.nickname, None);
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn send_input_rejects_empty_message() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({"target": ThreadId::new().to_string(), "message": ""})),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("empty message should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("Empty message can't be sent to an agent".to_string())
    );
}

#[tokio::test]
async fn send_input_rejects_when_message_and_items_are_both_set() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({
            "target": ThreadId::new().to_string(),
            "message": "hello",
            "items": [{"type": "mention", "name": "drive", "path": "app://drive"}]
        })),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("message+items should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string()
        )
    );
}

#[tokio::test]
async fn send_input_rejects_invalid_id() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({"target": "not-a-uuid", "message": "hi"})),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("invalid id should be rejected");
    };
    let FunctionCallError::RespondToModel(msg) = err else {
        panic!("expected respond-to-model error");
    };
    assert!(msg.starts_with("invalid agent id not-a-uuid:"));
}

#[tokio::test]
async fn send_input_reports_missing_agent() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let agent_id = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({"target": agent_id.to_string(), "message": "hi"})),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("missing agent should be reported");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(format!("agent with id {agent_id} not found"))
    );
}

#[tokio::test]
async fn send_input_interrupts_before_prompt() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({
            "target": agent_id.to_string(),
            "message": "hi",
            "interrupt": true
        })),
    );
    SendInputHandler
        .handle(invocation)
        .await
        .expect("send_input should succeed");

    let ops = manager.captured_ops();
    let ops_for_agent: Vec<&Op> = ops
        .iter()
        .filter_map(|(id, op)| (*id == agent_id).then_some(op))
        .collect();
    assert_eq!(ops_for_agent.len(), 2);
    assert!(matches!(ops_for_agent[0], Op::Interrupt));
    assert!(matches!(ops_for_agent[1], Op::UserInput { .. }));

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn send_input_accepts_structured_items() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [
                {"type": "mention", "name": "drive", "path": "app://google_drive"},
                {"type": "text", "text": "read the folder"}
            ]
        })),
    );
    SendInputHandler
        .handle(invocation)
        .await
        .expect("send_input should succeed");

    let expected = Op::UserInput {
        environments: None,
        items: vec![
            UserInput::Mention {
                name: "drive".to_string(),
                path: "app://google_drive".to_string(),
            },
            UserInput::Text {
                text: "read the folder".to_string(),
                text_elements: Vec::new(),
            },
        ],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: Default::default(),
    };
    let captured = manager
        .captured_ops()
        .into_iter()
        .find(|(id, op)| *id == agent_id && *op == expected);
    assert_eq!(captured, Some((agent_id, expected)));

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn resume_agent_rejects_invalid_id() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": "not-a-uuid"})),
    );
    let Err(err) = ResumeAgentHandler.handle(invocation).await else {
        panic!("invalid id should be rejected");
    };
    let FunctionCallError::RespondToModel(msg) = err else {
        panic!("expected respond-to-model error");
    };
    assert!(msg.starts_with("invalid agent id not-a-uuid:"));
}

#[tokio::test]
async fn resume_agent_reports_missing_agent() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let agent_id = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": agent_id.to_string()})),
    );
    let Err(err) = ResumeAgentHandler.handle(invocation).await else {
        panic!("missing agent should be reported");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(format!("agent with id {agent_id} not found"))
    );
}

#[tokio::test]
async fn resume_agent_noops_for_active_agent() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let status_before = manager.agent_control().get_status(agent_id).await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": agent_id.to_string()})),
    );

    let output = ResumeAgentHandler
        .handle(invocation)
        .await
        .expect("resume_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&content).expect("resume_agent result should be json");
    assert_eq!(result.status, status_before);
    assert_eq!(success, Some(true));

    let thread_ids = manager.list_thread_ids().await;
    assert_eq!(thread_ids, vec![agent_id]);

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn resume_agent_restores_closed_agent_and_accepts_send_input() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .resume_thread_with_history(
            config.clone(),
            InitialHistory::Forked(vec![RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "materialized".to_string(),
                }],
                phase: None,
            })]),
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
            /*parent_trace*/ None,
        )
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let _ = manager
        .agent_control()
        .shutdown_live_agent(agent_id)
        .await
        .expect("shutdown agent");
    assert_eq!(
        manager.agent_control().get_status(agent_id).await,
        AgentStatus::NotFound
    );
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let resume_invocation = invocation(
        session.clone(),
        turn.clone(),
        "resume_agent",
        function_payload(json!({"id": agent_id.to_string()})),
    );
    let output = ResumeAgentHandler
        .handle(resume_invocation)
        .await
        .expect("resume_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&content).expect("resume_agent result should be json");
    assert_ne!(result.status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));

    let send_invocation = invocation(
        session,
        turn,
        "send_input",
        function_payload(json!({"target": agent_id.to_string(), "message": "hello"})),
    );
    let output = SendInputHandler
        .handle(send_invocation)
        .await
        .expect("send_input should succeed after resume");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("send_input result should be json");
    let submission_id = result
        .get("submission_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(!submission_id.is_empty());
    assert_eq!(success, Some(true));

    let _ = manager
        .agent_control()
        .shutdown_live_agent(agent_id)
        .await
        .expect("shutdown resumed agent");
}

#[tokio::test]
async fn codexx_resume_agent_resolves_task_name_and_preserves_child_model_and_effort() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    #[derive(Debug, Deserialize)]
    struct ResumeAgentResult {
        status: AgentStatus,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let child_cwd = tempfile::tempdir().expect("child cwd temp dir");
    let child_cwd = child_cwd.path().abs();
    let parent_cwd = tempfile::tempdir().expect("parent cwd temp dir");
    let parent_cwd = parent_cwd.path().abs();
    let mut child_config = (*turn.config).clone();
    let mut child_shell_environment_policy = ShellEnvironmentPolicy {
        inherit: ShellEnvironmentPolicyInherit::None,
        ..Default::default()
    };
    child_shell_environment_policy
        .r#set
        .insert("CODEXX_CHILD_ENV".to_string(), "child".to_string());
    child_config.cwd = child_cwd.clone();
    child_config.workspace_roots = vec![child_cwd.clone()];
    child_config
        .permissions
        .set_workspace_roots(vec![child_cwd.clone()]);
    child_config.permissions.shell_environment_policy = child_shell_environment_policy.clone();
    child_config
        .permissions
        .approval_policy
        .set(AskForApproval::Never)
        .expect("child approval policy should be set");
    child_config
        .permissions
        .set_permission_profile(PermissionProfile::Disabled)
        .expect("child permission profile should be set");
    #[allow(deprecated)]
    {
        turn.cwd = child_cwd.clone();
    }
    turn.approval_policy
        .set(AskForApproval::Never)
        .expect("child turn approval policy should be set");
    turn.permission_profile = PermissionProfile::Disabled;
    turn.shell_environment_policy = child_shell_environment_policy.clone();
    set_turn_config(&mut turn, child_config);

    let mut resume_turn = turn
        .with_model("gpt-parent".to_string(), &session.services.models_manager)
        .await;
    resume_turn.reasoning_effort = Some(ReasoningEffort::High);
    let mut resume_config = (*resume_turn.config).clone();
    let mut parent_shell_environment_policy = ShellEnvironmentPolicy::default();
    parent_shell_environment_policy
        .r#set
        .insert("CODEXX_PARENT_ENV".to_string(), "parent".to_string());
    resume_config.cwd = parent_cwd.clone();
    resume_config.workspace_roots = vec![parent_cwd.clone()];
    resume_config
        .permissions
        .set_workspace_roots(vec![parent_cwd.clone()]);
    resume_config.permissions.shell_environment_policy = parent_shell_environment_policy.clone();
    resume_config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("parent approval policy should be set");
    resume_config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .expect("parent permission profile should be set");
    resume_config.model_reasoning_effort = Some(ReasoningEffort::High);
    #[allow(deprecated)]
    {
        resume_turn.cwd = parent_cwd;
    }
    resume_turn
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("parent turn approval policy should be set");
    resume_turn.permission_profile = PermissionProfile::read_only();
    resume_turn.shell_environment_policy = parent_shell_environment_policy;
    set_turn_config(&mut resume_turn, resume_config);

    let session = Arc::new(session);
    let spawn_turn = Arc::new(turn);
    let spawn_output = CodexxSpawnAgentHandler::default()
        .handle(invocation(
            session.clone(),
            spawn_turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "remember your child config",
                "task_name": "resume_target",
                "model": "gpt-5.4",
                "reasoning_effort": "low",
                "fork_turns": "none"
            })),
        ))
        .await
        .expect("codexx spawn_agent should succeed");
    let (content, _) = expect_text_output(spawn_output);
    let spawn_result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(spawn_result.task_name, "/root/resume_target");
    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(
            session.thread_id,
            &spawn_turn.session_source,
            "resume_target",
        )
        .await
        .expect("resume target should resolve");

    session
        .services
        .agent_control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    assert_eq!(
        session
            .services
            .agent_control
            .get_status(child_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let resume_output = CodexxResumeAgentHandler
        .handle(invocation(
            session.clone(),
            Arc::new(resume_turn),
            "resume_agent",
            function_payload(json!({"target": "resume_target"})),
        ))
        .await
        .expect("codexx resume_agent should resolve task name");
    let (content, success) = expect_text_output(resume_output);
    let resume_result: ResumeAgentResult =
        serde_json::from_str(&content).expect("resume_agent result should be json");
    assert_ne!(resume_result.status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));

    let snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("resumed child thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.model, "gpt-5.4");
    assert_eq!(snapshot.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(snapshot.approval_policy, AskForApproval::Never);
    assert_eq!(snapshot.permission_profile, PermissionProfile::Disabled);
    assert_eq!(snapshot.cwd, child_cwd);
    assert_eq!(snapshot.workspace_roots, vec![child_cwd]);
    assert_eq!(
        snapshot.shell_environment_policy,
        child_shell_environment_policy
    );
    assert_eq!(
        snapshot.session_source.get_agent_path().as_deref(),
        Some("/root/resume_target")
    );
}

#[tokio::test]
async fn codexx_resume_agent_defaults_to_self_and_subtree_scope_restores_descendants() {
    #[derive(Debug, Deserialize)]
    struct ResumeAgentResult {
        status: AgentStatus,
    }

    let (_session, turn) = make_session_and_context().await;
    let mut config = turn.config.as_ref().clone();
    config.agent_max_depth = 3;
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow multi-agent v2");
    let state_db = init_state_db(&config)
        .await
        .expect("sqlite state db should initialize");
    let manager = ThreadManager::new(
        &config,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        /*analytics_events_client*/ None,
        thread_store_from_config(&config, Some(state_db.clone())),
        Some(state_db.clone()),
        "22222222-2222-4222-8222-222222222222".to_string(),
        /*attestation_provider*/ None,
    );

    let root = manager
        .start_thread(config)
        .await
        .expect("root thread should start");
    let root_session = root.thread.codex.session.clone();
    let root_agent_control = root_session.services.agent_control.clone();
    CodexxSpawnAgentHandler::default()
        .handle(invocation(
            root_session.clone(),
            root_session.new_default_turn().await,
            "spawn_agent",
            function_payload(json!({
                "message": "hello parent",
                "task_name": "parent",
                "fork_turns": "none"
            })),
        ))
        .await
        .expect("parent spawn should succeed");
    let root_turn = root_session.new_default_turn().await;
    let parent_thread_id = root_agent_control
        .resolve_agent_reference(root.thread_id, &root_turn.session_source, "parent")
        .await
        .expect("parent should resolve");
    let parent_thread = manager
        .get_thread(parent_thread_id)
        .await
        .expect("parent thread should exist");
    let parent_session = parent_thread.codex.session.clone();
    CodexxSpawnAgentHandler::default()
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "spawn_agent",
            function_payload(json!({
                "message": "hello child",
                "task_name": "child",
                "fork_turns": "none"
            })),
        ))
        .await
        .expect("child spawn should succeed");
    let parent_turn = parent_session.new_default_turn().await;
    let child_thread_id = root_agent_control
        .resolve_agent_reference(parent_thread_id, &parent_turn.session_source, "child")
        .await
        .expect("child should resolve");
    wait_for_state_thread(&state_db, parent_thread_id).await;
    wait_for_state_thread(&state_db, child_thread_id).await;

    for thread_id in [child_thread_id, parent_thread_id] {
        root_agent_control
            .shutdown_live_agent(thread_id)
            .await
            .expect("thread should shut down without closing its spawn edge");
    }
    assert_eq!(
        root_agent_control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        root_agent_control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );

    let self_output = CodexxResumeAgentHandler
        .handle(invocation(
            root_session.clone(),
            root_session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({"target": "parent"})),
        ))
        .await
        .expect("default resume should restore the parent only");
    let (self_content, _) = expect_text_output(self_output);
    let self_result: ResumeAgentResult =
        serde_json::from_str(&self_content).expect("resume result should be json");
    assert_ne!(self_result.status, AgentStatus::NotFound);
    assert_ne!(
        root_agent_control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        root_agent_control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );

    root_agent_control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("resumed parent should shut down without closing its spawn edge");

    let subtree_output = CodexxResumeAgentHandler
        .handle(invocation(
            root_session.clone(),
            root_session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({
                "target": "parent",
                "resume_scope": "subtree"
            })),
        ))
        .await
        .expect("subtree resume should restore open descendants");
    let (subtree_content, _) = expect_text_output(subtree_output);
    let subtree_result: ResumeAgentResult =
        serde_json::from_str(&subtree_content).expect("subtree resume result should be json");
    assert_ne!(subtree_result.status, AgentStatus::NotFound);
    assert_ne!(
        root_agent_control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        root_agent_control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    let child_snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("subtree resume should restore child thread")
        .config_snapshot()
        .await;
    assert_eq!(
        child_snapshot.session_source.get_agent_path().as_deref(),
        Some("/root/parent/child")
    );
}

#[tokio::test]
async fn codexx_resume_agent_all_scope_restores_root_descendants() {
    let (_session, turn) = make_session_and_context().await;
    let mut config = turn.config.as_ref().clone();
    config.agent_max_depth = 3;
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow multi-agent v2");
    let state_db = init_state_db(&config)
        .await
        .expect("sqlite state db should initialize");
    let manager = ThreadManager::new(
        &config,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        /*analytics_events_client*/ None,
        thread_store_from_config(&config, Some(state_db.clone())),
        Some(state_db.clone()),
        "33333333-3333-4333-8333-333333333333".to_string(),
        /*attestation_provider*/ None,
    );

    let root = manager
        .start_thread(config)
        .await
        .expect("root thread should start");
    let root_session = root.thread.codex.session.clone();
    let root_agent_control = root_session.services.agent_control.clone();
    for task_name in ["alpha", "beta"] {
        CodexxSpawnAgentHandler::default()
            .handle(invocation(
                root_session.clone(),
                root_session.new_default_turn().await,
                "spawn_agent",
                function_payload(json!({
                    "message": format!("hello {task_name}"),
                    "task_name": task_name,
                    "fork_turns": "none"
                })),
            ))
            .await
            .expect("spawn should succeed");
    }
    let root_turn = root_session.new_default_turn().await;
    let alpha_thread_id = root_agent_control
        .resolve_agent_reference(root.thread_id, &root_turn.session_source, "alpha")
        .await
        .expect("alpha should resolve");
    let beta_thread_id = root_agent_control
        .resolve_agent_reference(root.thread_id, &root_turn.session_source, "beta")
        .await
        .expect("beta should resolve");
    wait_for_state_thread(&state_db, alpha_thread_id).await;
    wait_for_state_thread(&state_db, beta_thread_id).await;

    root_agent_control
        .shutdown_live_agent(alpha_thread_id)
        .await
        .expect("alpha should shut down");
    assert_eq!(
        root_agent_control.get_status(alpha_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        root_agent_control.get_status(beta_thread_id).await,
        AgentStatus::NotFound
    );

    CodexxResumeAgentHandler
        .handle(invocation(
            root_session.clone(),
            root_session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({
                "target": "/root",
                "resume_scope": "all"
            })),
        ))
        .await
        .expect("all scope should restore unloaded root descendants");

    assert_ne!(
        root_agent_control.get_status(alpha_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        root_agent_control.get_status(beta_thread_id).await,
        AgentStatus::NotFound
    );
}

#[tokio::test]
async fn resume_agent_rejects_when_depth_limit_exceeded() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let max_depth = turn.config.agent_max_depth;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: max_depth,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": ThreadId::new().to_string()})),
    );
    let Err(err) = ResumeAgentHandler.handle(invocation).await else {
        panic!("resume should fail when depth limit exceeded");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string()
        )
    );
}

#[tokio::test]
async fn wait_agent_rejects_non_positive_timeout() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [ThreadId::new().to_string()],
            "timeout_ms": 0
        })),
    );
    let Err(err) = WaitAgentHandler::default().handle(invocation).await else {
        panic!("non-positive timeout should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("timeout_ms must be greater than zero".to_string())
    );
}

#[tokio::test]
async fn wait_agent_rejects_invalid_target() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"targets": ["invalid"]})),
    );
    let Err(err) = WaitAgentHandler::default().handle(invocation).await else {
        panic!("invalid id should be rejected");
    };
    let FunctionCallError::RespondToModel(msg) = err else {
        panic!("expected respond-to-model error");
    };
    assert!(msg.starts_with("invalid agent id invalid:"));
}

#[tokio::test]
async fn wait_agent_rejects_empty_targets() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"targets": []})),
    );
    let Err(err) = WaitAgentHandler::default().handle(invocation).await else {
        panic!("empty ids should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("agent ids must be non-empty".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_timeout_only_argument() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");

    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({"timeout_ms": 10_000})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "hello from worker".to_string(),
            /*trigger_turn*/ false,
        ))
        .await;

    let output = wait_task
        .await
        .expect("wait task should join")
        .expect("timeout-only args should be accepted in v2 mode");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_rejects_timeout_below_configured_min() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 50;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 50;
    set_turn_config(&mut turn, config);

    let Err(err) = WaitAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 1})),
        ))
        .await
    else {
        panic!("timeout below configured minimum should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("timeout_ms must be at least 50".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_explicit_timeout_at_configured_min() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 50;
    set_turn_config(&mut turn, config);

    let output = WaitAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 1})),
        ))
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_uses_configured_default_timeout() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 50;
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let early = timeout(
        Duration::from_millis(/*millis*/ 20),
        WaitAgentHandlerV2::default().handle(invocation(
            session.clone(),
            turn.clone(),
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await;
    assert!(
        early.is_err(),
        "wait_agent should not return before the configured default timeout"
    );

    let output = timeout(
        Duration::from_secs(/*secs*/ 1),
        WaitAgentHandlerV2::default().handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await
    .expect("configured default should be shorter than the test timeout")
    .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_allows_zero_configured_timeout() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 0;
    config.multi_agent_v2.max_wait_timeout_ms = 0;
    config.multi_agent_v2.default_wait_timeout_ms = 0;
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let output = timeout(
        Duration::from_secs(/*secs*/ 1),
        WaitAgentHandlerV2::default().handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await
    .expect("zero timeout should complete immediately")
    .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_rejects_timeout_above_configured_max() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 50;
    config.multi_agent_v2.default_wait_timeout_ms = 1;
    set_turn_config(&mut turn, config);

    let Err(err) = WaitAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 500})),
        ))
        .await
    else {
        panic!("timeout above configured maximum should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("timeout_ms must be at most 50".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_explicit_timeout_at_configured_max() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 1;
    config.multi_agent_v2.default_wait_timeout_ms = 1;
    set_turn_config(&mut turn, config);

    let output = WaitAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 1})),
        ))
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn wait_agent_returns_not_found_for_missing_agents() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let id_a = ThreadId::new();
    let id_b = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [id_a.to_string(), id_b.to_string()],
            "timeout_ms": 10_000
        })),
    );
    let output = WaitAgentHandler::default()
        .handle(invocation)
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            status: HashMap::from([
                (id_a.to_string(), AgentStatus::NotFound),
                (id_b.to_string(), AgentStatus::NotFound),
            ]),
            timed_out: false
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn wait_agent_times_out_when_status_is_not_final() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [agent_id.to_string()],
            "timeout_ms": MIN_WAIT_TIMEOUT_MS
        })),
    );
    let output = WaitAgentHandler::default()
        .handle(invocation)
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            status: HashMap::new(),
            timed_out: true
        }
    );
    assert_eq!(success, None);

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn wait_agent_clamps_short_timeouts_to_minimum() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [agent_id.to_string()],
            "timeout_ms": 10
        })),
    );

    let early = timeout(
        Duration::from_millis(50),
        WaitAgentHandler::default().handle(invocation),
    )
    .await;
    assert!(
        early.is_err(),
        "wait_agent should not return before the minimum timeout clamp"
    );

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn wait_agent_returns_final_status_without_timeout() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let mut status_rx = manager
        .agent_control()
        .subscribe_status(agent_id)
        .await
        .expect("subscribe should succeed");

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
    let _ = timeout(Duration::from_secs(1), status_rx.changed())
        .await
        .expect("shutdown status should arrive");

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [agent_id.to_string()],
            "timeout_ms": 10_000
        })),
    );
    let output = WaitAgentHandler::default()
        .handle(invocation)
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            status: HashMap::from([(agent_id.to_string(), AgentStatus::Shutdown)]),
            timed_out: false
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn codexx_wait_agent_status_resolves_task_name_and_returns_final_status() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = CodexxSpawnAgentHandler::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "finish eventually",
                "task_name": "status_target",
                "fork_turns": "none"
            })),
        ))
        .await
        .expect("codexx spawn_agent should succeed");
    let (content, _) = expect_text_output(spawn_output);
    let spawn_result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(spawn_result.task_name, "/root/status_target");
    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "status_target")
        .await
        .expect("status target should resolve");
    session
        .services
        .agent_control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");

    let output = CodexxWaitAgentStatusHandler::default()
        .handle(invocation(
            session,
            turn,
            "wait_agent_status",
            function_payload(json!({
                "targets": ["status_target"],
                "timeout_ms": 10_000
            })),
        ))
        .await
        .expect("codexx wait_agent_status should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent_status result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            status: HashMap::from([("/root/status_target".to_string(), AgentStatus::NotFound)]),
            timed_out: false
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_returns_summary_for_mailbox_activity() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "test_process")
        .await
        .expect("relative path should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");
    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({"timeout_ms": 10_000})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "completed".to_string(),
            /*trigger_turn*/ false,
        ))
        .await;

    let wait_output = wait_task
        .await
        .expect("wait task should join")
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(wait_output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_returns_for_already_queued_mail() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");

    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "already queued".to_string(),
            /*trigger_turn*/ false,
        ))
        .await;

    let output = timeout(
        Duration::from_millis(500),
        WaitAgentHandlerV2::default().handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({"timeout_ms": 10_000})),
        )),
    )
    .await
    .expect("already queued mail should complete wait_agent immediately")
    .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_wakes_on_any_mailbox_notification() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    for task_name in ["worker_a", "worker_b"] {
        SpawnAgentHandlerV2::default()
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "spawn_agent",
                function_payload(json!({
                    "message": format!("boot {task_name}"),
                    "task_name": task_name
                })),
            ))
            .await
            .expect("spawn worker");
    }
    let worker_b_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker_b")
        .await
        .expect("worker_b should resolve");
    let worker_b_path = session
        .services
        .agent_control
        .get_agent_metadata(worker_b_id)
        .expect("worker_b metadata")
        .agent_path
        .expect("worker_b path");

    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({"timeout_ms": 10_000})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_b_path,
            AgentPath::root(),
            Vec::new(),
            "from worker b".to_string(),
            /*trigger_turn*/ false,
        ))
        .await;

    let output = wait_task
        .await
        .expect("wait task should join")
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_does_not_return_completed_content() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");
    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({"timeout_ms": 10_000})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "sensitive child output".to_string(),
            /*trigger_turn*/ false,
        ))
        .await;

    let output = wait_task
        .await
        .expect("wait task should join")
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert!(!content.contains("sensitive child output"));
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_close_agent_accepts_task_name_target() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");

    let output = CloseAgentHandlerV2
        .handle(invocation(
            session,
            turn,
            "close_agent",
            function_payload(json!({"target": "worker"})),
        ))
        .await
        .expect("close_agent should succeed for v2 task names");
    let (content, success) = expect_text_output(output);
    let result: close_agent::CloseAgentResult =
        serde_json::from_str(&content).expect("close_agent result should be json");
    assert_ne!(result.previous_status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));
    assert_eq!(
        manager.agent_control().get_status(agent_id).await,
        AgentStatus::NotFound
    );
}

#[tokio::test]
async fn multi_agent_v2_close_agent_reaps_stale_task_name_target() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    let state_db = init_state_db(&config)
        .await
        .expect("sqlite state db should initialize");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db.clone()),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    set_turn_config(&mut turn, config.clone());

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let stale_thread = manager
        .remove_thread(&agent_id)
        .await
        .expect("worker thread should be loaded before removal");
    stale_thread
        .submit(Op::Shutdown {})
        .await
        .expect("removed worker thread should still accept shutdown");
    stale_thread.wait_until_terminated().await;

    let output = CloseAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "close_agent",
            function_payload(json!({"target": "worker"})),
        ))
        .await
        .expect("close_agent should reap stale v2 task names");
    let (content, success) = expect_text_output(output);
    let result: close_agent::CloseAgentResult =
        serde_json::from_str(&content).expect("close_agent result should be json");
    assert_eq!(result.previous_status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));

    let open_children = state_db
        .list_thread_spawn_children_with_status(
            root.thread_id,
            DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("open children should load");
    assert_eq!(open_children, Vec::<ThreadId>::new());
    let closed_children = state_db
        .list_thread_spawn_children_with_status(
            root.thread_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("closed children should load");
    assert_eq!(closed_children, vec![agent_id]);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo again",
                "task_name": "replacement"
            })),
        ))
        .await
        .expect("spawn_agent should succeed after stale close releases the slot");
    let replacement_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "replacement")
        .await
        .expect("replacement path should resolve");
    let _ = session
        .services
        .agent_control
        .shutdown_live_agent(replacement_id)
        .await
        .expect("replacement should shut down");
}

#[tokio::test]
async fn multi_agent_v2_close_agent_rejects_root_target_and_id() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let root_path_error = CloseAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "close_agent",
            function_payload(json!({"target": "/root"})),
        ))
        .await
        .err()
        .expect("close_agent should reject the root path");
    assert_eq!(
        root_path_error,
        FunctionCallError::RespondToModel("root is not a spawned agent".to_string())
    );

    let root_id_error = CloseAgentHandlerV2
        .handle(invocation(
            session,
            turn,
            "close_agent",
            function_payload(json!({"target": root.thread_id.to_string()})),
        ))
        .await
        .err()
        .expect("close_agent should reject the root thread id");
    assert_eq!(
        root_id_error,
        FunctionCallError::RespondToModel("root is not a spawned agent".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_close_agent_rejects_self_target_by_id() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path),
        agent_nickname: None,
        agent_role: None,
    });

    let err = CloseAgentHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "close_agent",
            function_payload(json!({"target": child_thread_id.to_string()})),
        ))
        .await
        .err()
        .expect("close_agent should reject self-target by id");
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "an agent cannot close itself; return your result and let the parent close you if needed"
                .to_string()
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_close_agent_rejects_self_target_by_task_name() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });

    let err = CloseAgentHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "close_agent",
            function_payload(json!({"target": child_path.to_string()})),
        ))
        .await
        .err()
        .expect("close_agent should reject self-target by task name");
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "an agent cannot close itself; return your result and let the parent close you if needed"
                .to_string()
        )
    );
}

#[tokio::test]
async fn close_agent_submits_shutdown_and_returns_previous_status() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .start_thread(config.clone())
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let status_before = manager.agent_control().get_status(agent_id).await;

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "close_agent",
        function_payload(json!({"target": agent_id.to_string()})),
    );
    let output = CloseAgentHandler
        .handle(invocation)
        .await
        .expect("close_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: close_agent::CloseAgentResult =
        serde_json::from_str(&content).expect("close_agent result should be json");
    assert_eq!(result.previous_status, status_before);
    assert_eq!(success, Some(true));

    let ops = manager.captured_ops();
    let submitted_shutdown = ops
        .iter()
        .any(|(id, op)| *id == agent_id && matches!(op, Op::Shutdown));
    assert_eq!(submitted_shutdown, true);

    let status_after = manager.agent_control().get_status(agent_id).await;
    assert_eq!(status_after, AgentStatus::NotFound);
}

#[tokio::test]
async fn tool_handlers_cascade_close_and_resume_and_keep_explicitly_closed_subtrees_closed() {
    let (_session, turn) = make_session_and_context().await;
    let mut config = turn.config.as_ref().clone();
    config.agent_max_depth = 3;
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    let state_db = init_state_db(&config).await;
    let manager = ThreadManager::new(
        &config,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        /*analytics_events_client*/ None,
        thread_store_from_config(&config, state_db.clone()),
        state_db.clone(),
        "11111111-1111-4111-8111-111111111111".to_string(),
        /*attestation_provider*/ None,
    );

    let parent = manager
        .start_thread(config.clone())
        .await
        .expect("parent thread should start");
    let parent_thread_id = parent.thread_id;
    let parent_session = parent.thread.codex.session.clone();

    let child_turn = parent_session.new_default_turn().await;
    let child_spawn_output = SpawnAgentHandler::default()
        .handle(invocation(
            parent_session.clone(),
            child_turn,
            "spawn_agent",
            function_payload(json!({"message": "hello child"})),
        ))
        .await
        .expect("child spawn should succeed");
    let (child_content, child_success) = expect_text_output(child_spawn_output);
    let child_result: serde_json::Value =
        serde_json::from_str(&child_content).expect("child spawn result should be json");
    let child_thread_id = parse_agent_id(
        child_result
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .expect("child spawn result should include agent_id"),
    );
    assert_eq!(child_success, Some(true));

    let child_thread = manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let child_session = child_thread.codex.session.clone();
    let grandchild_spawn_output = SpawnAgentHandler::default()
        .handle(invocation(
            child_session.clone(),
            child_session.new_default_turn().await,
            "spawn_agent",
            function_payload(json!({"message": "hello grandchild"})),
        ))
        .await
        .expect("grandchild spawn should succeed");
    let (grandchild_content, grandchild_success) = expect_text_output(grandchild_spawn_output);
    let grandchild_result: serde_json::Value =
        serde_json::from_str(&grandchild_content).expect("grandchild spawn result should be json");
    let grandchild_thread_id = parse_agent_id(
        grandchild_result
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .expect("grandchild spawn result should include agent_id"),
    );
    assert_eq!(grandchild_success, Some(true));

    let close_output = CloseAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "close_agent",
            function_payload(json!({"target": child_thread_id.to_string()})),
        ))
        .await
        .expect("close_agent should close the child subtree");
    let (close_content, close_success) = expect_text_output(close_output);
    let close_result: close_agent::CloseAgentResult =
        serde_json::from_str(&close_content).expect("close_agent result should be json");
    assert_ne!(close_result.previous_status, AgentStatus::NotFound);
    assert_eq!(close_success, Some(true));
    assert_eq!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let child_resume_output = ResumeAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({"id": child_thread_id.to_string()})),
        ))
        .await
        .expect("resume_agent should reopen the child subtree");
    let (child_resume_content, child_resume_success) = expect_text_output(child_resume_output);
    let child_resume_result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&child_resume_content).expect("resume result should be json");
    assert_ne!(child_resume_result.status, AgentStatus::NotFound);
    assert_eq!(child_resume_success, Some(true));
    assert_ne!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let close_again_output = CloseAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "close_agent",
            function_payload(json!({"target": child_thread_id.to_string()})),
        ))
        .await
        .expect("close_agent should be repeatable for the child subtree");
    let (close_again_content, close_again_success) = expect_text_output(close_again_output);
    let close_again_result: close_agent::CloseAgentResult =
        serde_json::from_str(&close_again_content)
            .expect("second close_agent result should be json");
    assert_ne!(close_again_result.previous_status, AgentStatus::NotFound);
    assert_eq!(close_again_success, Some(true));
    assert_eq!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let operator = manager
        .start_thread(config.clone())
        .await
        .expect("operator thread should start");
    let operator_session = operator.thread.codex.session.clone();
    let _ = manager
        .agent_control()
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
    assert_eq!(
        manager.agent_control().get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );

    let parent_resume_output = ResumeAgentHandler
        .handle(invocation(
            operator_session,
            operator.thread.codex.session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({"id": parent_thread_id.to_string()})),
        ))
        .await
        .expect("resume_agent should reopen the parent thread");
    let (parent_resume_content, parent_resume_success) = expect_text_output(parent_resume_output);
    let parent_resume_result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&parent_resume_content).expect("parent resume result should be json");
    assert_ne!(parent_resume_result.status, AgentStatus::NotFound);
    assert_eq!(parent_resume_success, Some(true));
    assert_ne!(
        manager.agent_control().get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let shutdown_report = manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(shutdown_report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(shutdown_report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn build_agent_spawn_config_uses_turn_context_values() {
    fn pick_allowed_sandbox_policy(
        permissions: &crate::config::Permissions,
        base: SandboxPolicy,
        cwd: &std::path::Path,
    ) -> SandboxPolicy {
        let candidates = [
            SandboxPolicy::new_read_only_policy(),
            SandboxPolicy::new_workspace_write_policy(),
            SandboxPolicy::DangerFullAccess,
        ];
        candidates
            .into_iter()
            .find(|candidate| {
                if *candidate == base {
                    return false;
                }
                permissions
                    .can_set_legacy_sandbox_policy(candidate, cwd)
                    .is_ok()
            })
            .unwrap_or(base)
    }

    let (_session, mut turn) = make_session_and_context().await;
    let base_instructions = BaseInstructions {
        text: "base".to_string(),
    };
    turn.developer_instructions = Some("dev".to_string());
    turn.compact_prompt = Some("compact".to_string());
    turn.shell_environment_policy = ShellEnvironmentPolicy {
        use_profile: true,
        ..ShellEnvironmentPolicy::default()
    };
    let temp_dir = tempfile::tempdir().expect("temp dir");
    #[allow(deprecated)]
    {
        turn.cwd = temp_dir.abs();
    }
    turn.codex_linux_sandbox_exe = Some(PathBuf::from("/bin/echo"));
    #[allow(deprecated)]
    let turn_cwd = turn.cwd.clone();
    let sandbox_policy = pick_allowed_sandbox_policy(
        &turn.config.permissions,
        turn.config.legacy_sandbox_policy(),
        turn_cwd.as_path(),
    );
    let file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&sandbox_policy, &turn_cwd);
    let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
    let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
        &file_system_sandbox_policy,
        network_sandbox_policy,
    );
    turn.permission_profile = permission_profile.clone();
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");

    let config = build_agent_spawn_config(&base_instructions, &turn).expect("spawn config");
    let mut expected = (*turn.config).clone();
    expected.base_instructions = Some(base_instructions.text);
    expected.model = Some(turn.model_info.slug.clone());
    expected.model_provider = turn.provider.info().clone();
    expected.model_reasoning_effort = turn.reasoning_effort;
    expected.model_reasoning_summary = Some(turn.reasoning_summary);
    expected.developer_instructions = turn.developer_instructions.clone();
    expected.compact_prompt = turn.compact_prompt.clone();
    expected.permissions.shell_environment_policy = turn.shell_environment_policy.clone();
    expected.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    #[allow(deprecated)]
    {
        expected.cwd = turn.cwd.clone();
    }
    expected
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");
    expected
        .permissions
        .set_permission_profile(permission_profile)
        .expect("permission profile set");
    assert_eq!(config, expected);
}

#[tokio::test]
async fn build_agent_spawn_config_preserves_base_user_instructions() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut base_config = (*turn.config).clone();
    base_config.user_instructions = Some("base-user".to_string());
    turn.user_instructions = Some("resolved-user".to_string());
    turn.config = Arc::new(base_config.clone());
    let base_instructions = BaseInstructions {
        text: "base".to_string(),
    };

    let config = build_agent_spawn_config(&base_instructions, &turn).expect("spawn config");

    assert_eq!(config.user_instructions, base_config.user_instructions);
}

#[tokio::test]
async fn build_agent_resume_config_clears_base_instructions() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut base_config = (*turn.config).clone();
    base_config.base_instructions = Some("caller-base".to_string());
    turn.config = Arc::new(base_config);
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");

    let config = build_agent_resume_config(&turn).expect("resume config");

    let mut expected = (*turn.config).clone();
    expected.base_instructions = None;
    expected.model = Some(turn.model_info.slug.clone());
    expected.model_provider = turn.provider.info().clone();
    expected.model_reasoning_effort = turn.reasoning_effort;
    expected.model_reasoning_summary = Some(turn.reasoning_summary);
    expected.developer_instructions = turn.developer_instructions.clone();
    expected.compact_prompt = turn.compact_prompt.clone();
    expected.permissions.shell_environment_policy = turn.shell_environment_policy.clone();
    expected.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    #[allow(deprecated)]
    {
        expected.cwd = turn.cwd.clone();
    }
    expected
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");
    expected
        .permissions
        .set_permission_profile(turn.permission_profile())
        .expect("permission profile set");
    assert_eq!(config, expected);
}
