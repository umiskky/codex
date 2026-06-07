use super::*;
use codex_tui::status_ipc::AgentTreeNode;
use codex_tui::status_ipc::AgentTreeResponse;
use codex_tui::status_ipc::StatusIpcResponse;
use codex_tui::status_ipc::StatusMetadata;
use codex_tui::status_ipc::StatusSummary;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread;

#[tokio::test]
async fn status_command_no_live_sessions_prints_friendly_message() {
    let codex_home = tempfile::tempdir().expect("create temp codex home");
    let mut output = Vec::new();

    run_status_command_with_codex_home(codex_home.path(), &mut output)
        .await
        .expect("status command succeeds without live sessions");

    let output = String::from_utf8(output).expect("valid utf8");
    assert!(output.contains("No live codexx TUI sessions found."));
}

#[tokio::test]
async fn status_command_removes_stale_metadata_for_dead_pid() {
    let codex_home = tempfile::tempdir().expect("create temp codex home");
    let status_dir = codex_home.path().join("tui-status");
    fs::create_dir_all(&status_dir).expect("create status dir");
    let metadata_path = status_dir.join("99999999.json");
    let socket_path = status_dir.join("99999999.sock");
    let metadata = StatusMetadata {
        pid: 99_999_999,
        cwd: "/workspace".into(),
        root_thread_id: None,
        session_id: None,
        socket_path: socket_path.clone(),
        started_at: 1,
        version: "0.137.0-codexx-test".to_string(),
    };
    fs::write(
        &metadata_path,
        serde_json::to_vec(&metadata).expect("serialize metadata"),
    )
    .expect("write metadata");

    let mut output = Vec::new();
    run_status_command_with_codex_home(codex_home.path(), &mut output)
        .await
        .expect("status command succeeds with stale metadata");

    assert!(!metadata_path.exists());
}

#[tokio::test]
async fn status_command_formats_live_ipc_session_rows() {
    let codex_home = tempfile::tempdir().expect("create temp codex home");
    let status_dir = codex_home.path().join("tui-status");
    fs::create_dir_all(&status_dir).expect("create status dir");
    let pid = std::process::id();
    let metadata_path = status_dir.join(format!("{pid}.json"));
    let socket_path = status_dir.join(format!("{pid}.sock"));
    let metadata = StatusMetadata {
        pid,
        cwd: "/workspace".into(),
        root_thread_id: Some("root-thread".to_string()),
        session_id: Some("session-1".to_string()),
        socket_path: socket_path.clone(),
        started_at: 1,
        version: "0.137.0-codexx-test".to_string(),
    };
    fs::write(
        &metadata_path,
        serde_json::to_vec(&metadata).expect("serialize metadata"),
    )
    .expect("write metadata");

    let listener = UnixListener::bind(&socket_path).expect("bind mock status socket");
    let server = thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.expect("accept status client");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone stream"))
                .read_line(&mut request)
                .expect("read request");
            let method = serde_json::from_str::<serde_json::Value>(&request)
                .expect("parse request")
                .get("method")
                .and_then(|value| value.as_str())
                .expect("request method")
                .to_string();
            let response = match method.as_str() {
                "status/summary" => StatusIpcResponse::ok(StatusSummary {
                    pid,
                    cwd: "/workspace".into(),
                    root_thread_id: Some("root-thread".to_string()),
                    session_id: Some("session-1".to_string()),
                    alive: true,
                    loaded_thread_count: 3,
                    active_thread_count: 2,
                    started_at: 1,
                    version: "0.137.0-codexx-test".to_string(),
                }),
                "agent/tree" => StatusIpcResponse::ok(AgentTreeResponse {
                    roots: vec![AgentTreeNode {
                        thread_id: "root-thread".to_string(),
                        session_id: Some("session-1".to_string()),
                        parent_thread_id: None,
                        status: "idle".to_string(),
                        active_flags: Vec::new(),
                        cwd: Some("/workspace".to_string()),
                        path: None,
                        agent_path: Some("/root".to_string()),
                        task_name: Some("root".to_string()),
                        agent_type: Some("default".to_string()),
                        agent_nickname: Some("Main".to_string()),
                        summary: Some("main preview".to_string()),
                        children: vec![
                            AgentTreeNode {
                                thread_id: "child-thread".to_string(),
                                session_id: Some("session-1".to_string()),
                                parent_thread_id: Some("root-thread".to_string()),
                                status: "active".to_string(),
                                active_flags: vec!["waiting_on_user_input".to_string()],
                                cwd: Some("/workspace".to_string()),
                                path: None,
                                agent_path: Some("/root/worker".to_string()),
                                task_name: Some("worker".to_string()),
                                agent_type: Some("worker".to_string()),
                                agent_nickname: Some("worker_a".to_string()),
                                summary: Some("child preview".to_string()),
                                children: Vec::new(),
                            },
                            AgentTreeNode {
                                thread_id: "working-thread".to_string(),
                                session_id: Some("session-1".to_string()),
                                parent_thread_id: Some("root-thread".to_string()),
                                status: "active".to_string(),
                                active_flags: Vec::new(),
                                cwd: Some("/workspace".to_string()),
                                path: None,
                                agent_path: Some("/root/coder".to_string()),
                                task_name: Some("coder".to_string()),
                                agent_type: Some("coder".to_string()),
                                agent_nickname: Some("coder_a".to_string()),
                                summary: Some("actively working".to_string()),
                                children: Vec::new(),
                            },
                        ],
                    }],
                }),
                other => StatusIpcResponse::error(format!("unexpected method {other}")),
            };
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&response).expect("serialize response")
            )
            .expect("write response");
        }
    });

    let mut output = Vec::new();
    run_status_command_with_codex_home(codex_home.path(), &mut output)
        .await
        .expect("status command succeeds with live mock IPC");

    server.join().expect("mock server exits");
    let output = String::from_utf8(output).expect("valid utf8");
    assert!(output.contains("TUI_PID"));
    assert!(output.contains("TUI_WORK"));
    assert!(output.contains("THREAD_WORK"));
    assert!(output.contains(&pid.to_string()));
    assert!(output.contains("root-thread"));
    assert!(output.contains("child-thread"));
    assert!(output.contains("working-thread"));
    assert!(output.contains("working"));
    assert!(output.contains("waiting_input"));
    assert!(output.contains("/root/worker"));
}

#[tokio::test]
async fn status_command_json_outputs_sessions_threads_and_work_state() {
    let codex_home = tempfile::tempdir().expect("create temp codex home");
    let pid = write_mock_metadata_and_start_status_server(codex_home.path());

    let mut output = Vec::new();
    run_status_command_with_codex_home_json(codex_home.path(), &mut output)
        .await
        .expect("json status command succeeds with live mock IPC");

    let output: serde_json::Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(output["sessions"][0]["pid"], pid);
    assert_eq!(output["sessions"][0]["tuiState"], "alive");
    assert_eq!(output["sessions"][0]["tuiWork"], "working");
    assert_eq!(output["sessions"][0]["summary"]["loadedThreadCount"], 3);
    let threads = output["sessions"][0]["threads"]
        .as_array()
        .expect("threads array");
    assert!(threads.iter().any(|thread| {
        thread["threadId"] == "working-thread" && thread["threadWork"] == "working"
    }));
    assert!(threads.iter().any(|thread| {
        thread["threadId"] == "child-thread" && thread["threadWork"] == "waiting_input"
    }));
    assert_eq!(
        output["sessions"][0]["agentTree"]["roots"][0]["children"][1]["threadId"],
        "working-thread"
    );
}

#[tokio::test]
async fn status_command_wraps_summary_inside_column_boundary() {
    let codex_home = tempfile::tempdir().expect("create temp codex home");
    write_mock_metadata_and_start_status_server(codex_home.path());

    let mut output = Vec::new();
    run_status_command_with_codex_home(codex_home.path(), &mut output)
        .await
        .expect("status command succeeds with live mock IPC");

    let output = String::from_utf8(output).expect("valid utf8");
    for line in output.lines() {
        assert!(
            display_width(line) <= STATUS_TABLE_WIDTH,
            "line exceeds table boundary: {line}"
        );
    }
    assert!(output.contains("..."));
}

fn write_mock_metadata_and_start_status_server(codex_home: &Path) -> u32 {
    let status_dir = codex_home.join("tui-status");
    fs::create_dir_all(&status_dir).expect("create status dir");
    let pid = std::process::id();
    let metadata_path = status_dir.join(format!("{pid}.json"));
    let socket_path = status_dir.join(format!("{pid}.sock"));
    let metadata = StatusMetadata {
        pid,
        cwd: "/workspace".into(),
        root_thread_id: Some("root-thread".to_string()),
        session_id: Some("session-1".to_string()),
        socket_path: socket_path.clone(),
        started_at: 1,
        version: "0.137.0-codexx-test".to_string(),
    };
    fs::write(
        &metadata_path,
        serde_json::to_vec(&metadata).expect("serialize metadata"),
    )
    .expect("write metadata");

    let listener = UnixListener::bind(&socket_path).expect("bind mock status socket");
    thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.expect("accept status client");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone stream"))
                .read_line(&mut request)
                .expect("read request");
            let method = serde_json::from_str::<serde_json::Value>(&request)
                .expect("parse request")
                .get("method")
                .and_then(|value| value.as_str())
                .expect("request method")
                .to_string();
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&mock_status_response(pid, method.as_str()))
                    .expect("serialize response")
            )
            .expect("write response");
        }
    });

    pid
}

fn mock_status_response(pid: u32, method: &str) -> StatusIpcResponse {
    match method {
        "status/summary" => StatusIpcResponse::ok(StatusSummary {
            pid,
            cwd: "/workspace".into(),
            root_thread_id: Some("root-thread".to_string()),
            session_id: Some("session-1".to_string()),
            alive: true,
            loaded_thread_count: 3,
            active_thread_count: 2,
            started_at: 1,
            version: "0.137.0-codexx-test".to_string(),
        }),
        "agent/tree" => StatusIpcResponse::ok(AgentTreeResponse {
            roots: vec![AgentTreeNode {
                thread_id: "root-thread".to_string(),
                session_id: Some("session-1".to_string()),
                parent_thread_id: None,
                status: "idle".to_string(),
                active_flags: Vec::new(),
                cwd: Some("/workspace".to_string()),
                path: None,
                agent_path: Some("/root".to_string()),
                task_name: Some("root".to_string()),
                agent_type: Some("default".to_string()),
                agent_nickname: Some("Main".to_string()),
                summary: Some("main preview".to_string()),
                children: vec![
                    AgentTreeNode {
                        thread_id: "child-thread".to_string(),
                        session_id: Some("session-1".to_string()),
                        parent_thread_id: Some("root-thread".to_string()),
                        status: "active".to_string(),
                        active_flags: vec!["waiting_on_user_input".to_string()],
                        cwd: Some("/workspace".to_string()),
                        path: None,
                        agent_path: Some("/root/worker".to_string()),
                        task_name: Some("worker".to_string()),
                        agent_type: Some("worker".to_string()),
                        agent_nickname: Some("worker_a".to_string()),
                        summary: Some("child preview".to_string()),
                        children: Vec::new(),
                    },
                    AgentTreeNode {
                        thread_id: "working-thread".to_string(),
                        session_id: Some("session-1".to_string()),
                        parent_thread_id: Some("root-thread".to_string()),
                        status: "active".to_string(),
                        active_flags: Vec::new(),
                        cwd: Some("/workspace".to_string()),
                        path: None,
                        agent_path: Some("/root/coder".to_string()),
                        task_name: Some("coder".to_string()),
                        agent_type: Some("coder".to_string()),
                        agent_nickname: Some("coder_a".to_string()),
                        summary: Some(
                            "This is a deliberately long summary that should wrap inside the \
                             fixed summary table column instead of making the status table grow \
                             sideways beyond the summary boundary."
                                .to_string(),
                        ),
                        children: Vec::new(),
                    },
                ],
            }],
        }),
        other => StatusIpcResponse::error(format!("unexpected method {other}")),
    }
}
