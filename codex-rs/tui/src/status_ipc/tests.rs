use super::*;
use std::path::PathBuf;

#[test]
fn status_ipc_paths_are_under_private_tui_status_dir() {
    let codex_home = PathBuf::from("/tmp/codex-home");
    let paths = status_paths(&codex_home, 12345);

    assert_eq!(paths.dir, codex_home.join("tui-status"));
    assert_eq!(
        paths.metadata_path,
        codex_home.join("tui-status/12345.json")
    );
    assert_eq!(paths.socket_path, codex_home.join("tui-status/12345.sock"));
}

#[test]
fn status_ipc_only_allows_read_methods() {
    assert!(is_read_only_method(METHOD_STATUS_SUMMARY));
    assert!(is_read_only_method(METHOD_THREAD_LOADED_LIST));
    assert!(is_read_only_method(METHOD_THREAD_READ));
    assert!(is_read_only_method(METHOD_AGENT_TREE));

    assert!(!is_read_only_method("spawn_agent"));
    assert!(!is_read_only_method("thread/start"));
    assert!(!is_read_only_method("agent/close"));
}

#[test]
fn status_ipc_agent_tree_includes_child_nodes() {
    let root = ThreadStatusView {
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
    };
    let child = ThreadStatusView {
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
    };

    let tree = build_agent_tree_from_threads(vec![child, root]);

    assert_eq!(tree.roots.len(), 1);
    assert_eq!(tree.roots[0].thread_id, "root-thread");
    assert_eq!(tree.roots[0].children.len(), 1);
    assert_eq!(tree.roots[0].children[0].thread_id, "child-thread");
    assert_eq!(
        tree.roots[0].children[0].agent_path.as_deref(),
        Some("/root/worker")
    );
}
