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
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

const STATUS_IPC_TIMEOUT: Duration = Duration::from_millis(750);
const NO_LIVE_TUI_MESSAGE: &str = "No live codexx TUI sessions found.";
const COLUMN_SEPARATOR: &str = "  ";
const PID_WIDTH: usize = 8;
const TUI_WORK_WIDTH: usize = 16;
const THREAD_WORK_WIDTH: usize = 16;
const THREAD_ID_WIDTH: usize = 36;
const AGENT_PATH_WIDTH: usize = 24;
const TASK_NAME_WIDTH: usize = 16;
const AGENT_TYPE_WIDTH: usize = 12;
const NICKNAME_WIDTH: usize = 14;
const SUMMARY_WIDTH: usize = 32;
const SUMMARY_MAX_LINES: usize = 2;
#[cfg(test)]
const STATUS_TABLE_WIDTH: usize = PID_WIDTH
    + TUI_WORK_WIDTH
    + THREAD_WORK_WIDTH
    + THREAD_ID_WIDTH
    + AGENT_PATH_WIDTH
    + TASK_NAME_WIDTH
    + AGENT_TYPE_WIDTH
    + NICKNAME_WIDTH
    + SUMMARY_WIDTH
    + (8 * COLUMN_SEPARATOR.len());

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

    write_status_table_row(
        writer,
        StatusTableRow {
            pid: "TUI_PID".to_string(),
            tui_work: "TUI_WORK".to_string(),
            thread_work: "THREAD_WORK".to_string(),
            thread_id: "THREAD_ID".to_string(),
            agent_path: "AGENT_PATH".to_string(),
            task_name: "TASK_NAME".to_string(),
            agent_type: "AGENT_TYPE".to_string(),
            nickname: "NICKNAME".to_string(),
            summary: "SUMMARY".to_string(),
        },
        false,
    )?;
    for session in sessions {
        let mut rows = Vec::new();
        for root in &session.tree.roots {
            collect_rows(session.summary.pid, root, &mut rows);
        }
        let session_work = session_work_state(&session.summary, &rows);
        if rows.is_empty() {
            write_status_table_row(
                writer,
                StatusTableRow {
                    pid: session.summary.pid.to_string(),
                    tui_work: session_work,
                    thread_work: "-".to_string(),
                    thread_id: session
                        .summary
                        .root_thread_id
                        .clone()
                        .unwrap_or_else(|| "-".to_string()),
                    agent_path: "/root".to_string(),
                    task_name: "root".to_string(),
                    agent_type: "-".to_string(),
                    nickname: "-".to_string(),
                    summary: String::new(),
                },
                true,
            )?;
            continue;
        }
        for row in rows {
            let thread_work = thread_work_state(&row.thread_state, &row.active_flags);
            write_status_table_row(
                writer,
                StatusTableRow {
                    pid: row.pid.to_string(),
                    tui_work: session_work.clone(),
                    thread_work,
                    thread_id: row.thread_id,
                    agent_path: row.agent_path.unwrap_or_else(|| "-".to_string()),
                    task_name: row.task_name.unwrap_or_else(|| "-".to_string()),
                    agent_type: row.agent_type.unwrap_or_else(|| "-".to_string()),
                    nickname: row.nickname.unwrap_or_else(|| "-".to_string()),
                    summary: row.summary.unwrap_or_default(),
                },
                true,
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

struct StatusTableRow {
    pid: String,
    tui_work: String,
    thread_work: String,
    thread_id: String,
    agent_path: String,
    task_name: String,
    agent_type: String,
    nickname: String,
    summary: String,
}

fn write_status_table_row(
    writer: &mut dyn Write,
    row: StatusTableRow,
    wrap_summary: bool,
) -> anyhow::Result<()> {
    let summary_lines = if wrap_summary {
        wrap_summary_lines(&row.summary)
    } else {
        vec![row.summary]
    };
    let fixed_cells = [
        fit_cell(&row.pid, PID_WIDTH),
        fit_cell(&row.tui_work, TUI_WORK_WIDTH),
        fit_cell(&row.thread_work, THREAD_WORK_WIDTH),
        fit_cell(&row.thread_id, THREAD_ID_WIDTH),
        fit_cell(&row.agent_path, AGENT_PATH_WIDTH),
        fit_cell(&row.task_name, TASK_NAME_WIDTH),
        fit_cell(&row.agent_type, AGENT_TYPE_WIDTH),
        fit_cell(&row.nickname, NICKNAME_WIDTH),
    ];
    let blank_cells = [
        " ".repeat(PID_WIDTH),
        " ".repeat(TUI_WORK_WIDTH),
        " ".repeat(THREAD_WORK_WIDTH),
        " ".repeat(THREAD_ID_WIDTH),
        " ".repeat(AGENT_PATH_WIDTH),
        " ".repeat(TASK_NAME_WIDTH),
        " ".repeat(AGENT_TYPE_WIDTH),
        " ".repeat(NICKNAME_WIDTH),
    ];

    for (index, summary_line) in summary_lines.iter().enumerate() {
        let cells = if index == 0 {
            &fixed_cells
        } else {
            &blank_cells
        };
        writeln!(
            writer,
            "{}{}{}",
            cells.join(COLUMN_SEPARATOR),
            COLUMN_SEPARATOR,
            truncate_with_ellipsis(summary_line, SUMMARY_WIDTH),
        )?;
    }

    Ok(())
}

fn wrap_summary_lines(summary: &str) -> Vec<String> {
    let normalized = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    let mut truncated = false;

    for ch in normalized.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > SUMMARY_WIDTH && !current.is_empty() {
            lines.push(current.trim_end().to_string());
            current.clear();
            current_width = 0;
            if lines.len() == SUMMARY_MAX_LINES {
                truncated = true;
                break;
            }
            if ch == ' ' {
                continue;
            }
        }
        current.push(ch);
        current_width += ch_width;
    }

    if !truncated && !current.is_empty() {
        lines.push(current.trim_end().to_string());
    }

    if lines.len() > SUMMARY_MAX_LINES {
        lines.truncate(SUMMARY_MAX_LINES);
        truncated = true;
    }

    if truncated && let Some(last) = lines.last_mut() {
        *last = append_ellipsis(last.trim_end(), SUMMARY_WIDTH);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn fit_cell(value: &str, width: usize) -> String {
    let truncated = truncate_with_ellipsis(value, width);
    pad_to_width(&truncated, width)
}

fn pad_to_width(value: &str, width: usize) -> String {
    let mut output = value.to_string();
    let output_width = display_width(&output);
    if output_width < width {
        output.push_str(&" ".repeat(width - output_width));
    }
    output
}

fn truncate_with_ellipsis(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }

    let mut output = truncate_to_width(value, width - 3);
    while output.ends_with(' ') {
        output.pop();
    }
    output.push_str("...");
    output
}

fn append_ellipsis(value: &str, width: usize) -> String {
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut output = truncate_to_width(value, width - 3);
    while output.ends_with(' ') {
        output.pop();
    }
    output.push_str("...");
    output
}

fn truncate_to_width(value: &str, width: usize) -> String {
    let mut output = String::new();
    let mut output_width = 0;
    for ch in value.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if output_width + ch_width > width {
            break;
        }
        output.push(ch);
        output_width += ch_width;
    }
    output
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

#[cfg(all(test, unix))]
mod tests;
