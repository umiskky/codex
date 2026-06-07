use anyhow::Context;
use clap::Args;
use codex_tui::status_ipc::AgentTreeNode;
use codex_tui::status_ipc::AgentTreeResponse;
use codex_tui::status_ipc::METHOD_AGENT_TREE;
use codex_tui::status_ipc::METHOD_STATUS_SUMMARY;
use codex_tui::status_ipc::StatusIpcRequest;
use codex_tui::status_ipc::StatusIpcResponse;
use codex_tui::status_ipc::StatusMetadata;
use codex_tui::status_ipc::StatusSummary;
use codex_tui::status_ipc::default_codex_home;
use codex_tui::status_ipc::read_metadata_file;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const STATUS_IPC_TIMEOUT: Duration = Duration::from_millis(750);
const NO_LIVE_TUI_MESSAGE: &str = "No live codexx TUI sessions found.";

#[derive(Debug, Args, Clone)]
pub struct StatusCommand {
    /// Print machine-readable JSON instead of the default table.
    #[arg(long)]
    pub json: bool,
}

pub async fn run_status_command(command: StatusCommand) -> anyhow::Result<()> {
    let codex_home = default_codex_home().context("failed to resolve CODEX_HOME")?;
    let mut stdout = std::io::stdout();
    if command.json {
        run_status_command_with_codex_home_json(&codex_home, &mut stdout).await
    } else {
        run_status_command_with_codex_home(&codex_home, &mut stdout).await
    }
}

pub async fn run_status_command_with_codex_home(
    codex_home: &Path,
    writer: &mut dyn Write,
) -> anyhow::Result<()> {
    let sessions = live_status_sessions(codex_home)?;
    write_status_table(writer, &sessions)
}

pub async fn run_status_command_with_codex_home_json(
    codex_home: &Path,
    writer: &mut dyn Write,
) -> anyhow::Result<()> {
    let sessions = live_status_sessions(codex_home)?;
    write_status_json(writer, &sessions)
}

fn write_status_table(
    writer: &mut dyn Write,
    sessions: &[LiveStatusSession],
) -> anyhow::Result<()> {
    if sessions.is_empty() {
        writeln!(writer, "{NO_LIVE_TUI_MESSAGE}")?;
        return Ok(());
    }

    writeln!(
        writer,
        "{:<8}  {:<16}  {:<16}  {:<36}  {:<24}  {:<16}  {:<12}  {:<14}  SUMMARY",
        "TUI_PID",
        "TUI_WORK",
        "THREAD_WORK",
        "THREAD_ID",
        "AGENT_PATH",
        "TASK_NAME",
        "AGENT_TYPE",
        "NICKNAME",
    )?;
    for session in sessions {
        let mut rows = Vec::new();
        for root in &session.tree.roots {
            collect_rows(session.summary.pid, root, &mut rows);
        }
        let session_work = session_work_state(&session.summary, &rows);
        if rows.is_empty() {
            writeln!(
                writer,
                "{:<8}  {:<16}  {:<16}  {:<36}  {:<24}  {:<16}  {:<12}  {:<14}  {}",
                session.summary.pid,
                session_work,
                "-",
                session.summary.root_thread_id.as_deref().unwrap_or("-"),
                "/root",
                "root",
                "-",
                "-",
                ""
            )?;
            continue;
        }
        for row in rows {
            let thread_work = thread_work_state(&row.thread_state, &row.active_flags);
            writeln!(
                writer,
                "{:<8}  {:<16}  {:<16}  {:<36}  {:<24}  {:<16}  {:<12}  {:<14}  {}",
                row.pid,
                session_work,
                thread_work,
                row.thread_id,
                truncate(row.agent_path.as_deref().unwrap_or("-"), 24),
                truncate(row.task_name.as_deref().unwrap_or("-"), 16),
                truncate(row.agent_type.as_deref().unwrap_or("-"), 12),
                truncate(row.nickname.as_deref().unwrap_or("-"), 14),
                row.summary.unwrap_or_default(),
            )?;
        }
    }
    Ok(())
}

fn write_status_json(writer: &mut dyn Write, sessions: &[LiveStatusSession]) -> anyhow::Result<()> {
    let output = StatusJsonOutput {
        sessions: sessions
            .iter()
            .map(|session| {
                let mut rows = Vec::new();
                for root in &session.tree.roots {
                    collect_rows(session.summary.pid, root, &mut rows);
                }
                let tui_work = session_work_state(&session.summary, &rows);
                StatusJsonSession {
                    pid: session.summary.pid,
                    tui_state: "alive".to_string(),
                    tui_work,
                    summary: session.summary.clone(),
                    threads: rows.iter().map(StatusJsonThread::from_row).collect(),
                    agent_tree: session.tree.clone(),
                }
            })
            .collect(),
    };
    serde_json::to_writer_pretty(&mut *writer, &output)?;
    writeln!(writer)?;
    Ok(())
}

struct LiveStatusSession {
    summary: StatusSummary,
    tree: AgentTreeResponse,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusJsonOutput {
    sessions: Vec<StatusJsonSession>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusJsonSession {
    pid: u32,
    tui_state: String,
    tui_work: String,
    summary: StatusSummary,
    threads: Vec<StatusJsonThread>,
    agent_tree: AgentTreeResponse,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusJsonThread {
    pid: u32,
    thread_work: String,
    thread_state: String,
    active_flags: Vec<String>,
    thread_id: String,
    agent_path: Option<String>,
    task_name: Option<String>,
    agent_type: Option<String>,
    nickname: Option<String>,
    summary: Option<String>,
}

impl StatusJsonThread {
    fn from_row(row: &StatusRow) -> Self {
        Self {
            pid: row.pid,
            thread_work: thread_work_state(&row.thread_state, &row.active_flags),
            thread_state: row.thread_state.clone(),
            active_flags: row.active_flags.clone(),
            thread_id: row.thread_id.clone(),
            agent_path: row.agent_path.clone(),
            task_name: row.task_name.clone(),
            agent_type: row.agent_type.clone(),
            nickname: row.nickname.clone(),
            summary: row.summary.clone(),
        }
    }
}

fn live_status_sessions(codex_home: &Path) -> anyhow::Result<Vec<LiveStatusSession>> {
    let status_dir = codex_home.join("tui-status");
    let entries = match fs::read_dir(&status_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context("failed to read TUI status directory"),
    };

    let mut sessions = Vec::new();
    let mut metadata_paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    metadata_paths.sort();

    for metadata_path in metadata_paths {
        let metadata = match read_metadata_file(&metadata_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                let _ = fs::remove_file(&metadata_path);
                continue;
            }
        };
        if !pid_exists(metadata.pid) {
            cleanup_stale_metadata(&metadata_path, &metadata);
            continue;
        }

        let summary = match request_status_ipc::<StatusSummary>(
            &metadata.socket_path,
            METHOD_STATUS_SUMMARY,
            Value::Null,
        ) {
            Ok(summary) => summary,
            Err(_) => continue,
        };
        let tree = match request_status_ipc::<AgentTreeResponse>(
            &metadata.socket_path,
            METHOD_AGENT_TREE,
            Value::Null,
        ) {
            Ok(tree) => tree,
            Err(_) => continue,
        };
        sessions.push(LiveStatusSession { summary, tree });
    }

    sessions.sort_by_key(|session| session.summary.pid);
    Ok(sessions)
}

#[cfg(unix)]
fn request_status_ipc<T: DeserializeOwned>(
    socket_path: &Path,
    method: &str,
    params: Value,
) -> anyhow::Result<T> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    stream.set_read_timeout(Some(STATUS_IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(STATUS_IPC_TIMEOUT))?;

    let request = StatusIpcRequest {
        method: method.to_string(),
        params,
    };
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("status IPC response timed out or closed")?;
    let response: StatusIpcResponse = serde_json::from_str(&line)?;
    if !response.ok {
        anyhow::bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "status IPC request failed".to_string())
        );
    }
    let value = response
        .result
        .context("status IPC response omitted result")?;
    serde_json::from_value(value).context("failed to decode status IPC response")
}

#[cfg(not(unix))]
fn request_status_ipc<T: DeserializeOwned>(
    _socket_path: &Path,
    _method: &str,
    _params: Value,
) -> anyhow::Result<T> {
    anyhow::bail!("codexx status IPC is only supported on Unix platforms")
}

fn cleanup_stale_metadata(metadata_path: &Path, metadata: &StatusMetadata) {
    let _ = fs::remove_file(metadata_path);
    let _ = fs::remove_file(&metadata.socket_path);
}

#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_exists(_pid: u32) -> bool {
    false
}

#[derive(Debug)]
struct StatusRow {
    pid: u32,
    thread_state: String,
    active_flags: Vec<String>,
    thread_id: String,
    agent_path: Option<String>,
    task_name: Option<String>,
    agent_type: Option<String>,
    nickname: Option<String>,
    summary: Option<String>,
}

fn collect_rows(pid: u32, node: &AgentTreeNode, rows: &mut Vec<StatusRow>) {
    rows.push(StatusRow {
        pid,
        thread_state: node.status.clone(),
        active_flags: node.active_flags.clone(),
        thread_id: node.thread_id.clone(),
        agent_path: node.agent_path.clone(),
        task_name: node.task_name.clone(),
        agent_type: node.agent_type.clone(),
        nickname: node.agent_nickname.clone(),
        summary: node.summary.clone(),
    });
    for child in &node.children {
        collect_rows(pid, child, rows);
    }
}

fn session_work_state(summary: &StatusSummary, rows: &[StatusRow]) -> String {
    let row_states = rows
        .iter()
        .map(|row| thread_work_state(&row.thread_state, &row.active_flags))
        .collect::<Vec<_>>();
    if row_states.iter().any(|state| state == "working") {
        return "working".to_string();
    }
    if row_states.iter().any(|state| state == "waiting_approval") {
        return "waiting_approval".to_string();
    }
    if row_states.iter().any(|state| state == "waiting_input") {
        return "waiting_input".to_string();
    }
    if row_states.iter().any(|state| state == "waiting") {
        return "waiting".to_string();
    }
    if summary.active_thread_count > 0 {
        return "working".to_string();
    }
    "idle".to_string()
}

fn thread_work_state(thread_state: &str, active_flags: &[String]) -> String {
    if thread_state != "active" {
        return thread_state.to_string();
    }
    if active_flags
        .iter()
        .any(|flag| flag == "waiting_on_approval")
    {
        return "waiting_approval".to_string();
    }
    if active_flags
        .iter()
        .any(|flag| flag == "waiting_on_user_input")
    {
        return "waiting_input".to_string();
    }
    if !active_flags.is_empty() {
        return "waiting".to_string();
    }
    "working".to_string()
}

fn truncate(value: &str, width: usize) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if output.chars().count() + 3 >= width {
            output.push_str("...");
            return output;
        }
        output.push(ch);
    }
    output
}

#[cfg(all(test, unix))]
mod tests;
