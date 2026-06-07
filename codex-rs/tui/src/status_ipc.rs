use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::protocol::SubAgentSource;
use codex_utils_home_dir::find_codex_home;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::io::BufRead;
#[cfg(unix)]
use std::io::BufReader;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::AtomicI64;
#[cfg(unix)]
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;
use tokio::runtime::Handle;

pub const METHOD_STATUS_SUMMARY: &str = "status/summary";
pub const METHOD_THREAD_LOADED_LIST: &str = "thread/loaded/list";
pub const METHOD_THREAD_READ: &str = "thread/read";
pub const METHOD_AGENT_TREE: &str = "agent/tree";

const STATUS_DIR_NAME: &str = "tui-status";
#[cfg(unix)]
const IPC_TIMEOUT: Duration = Duration::from_millis(750);
#[cfg(unix)]
const REQUEST_ID_START: i64 = 10_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusIpcPaths {
    pub dir: PathBuf,
    pub metadata_path: PathBuf,
    pub socket_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StatusMetadata {
    pub pid: u32,
    pub cwd: PathBuf,
    pub root_thread_id: Option<String>,
    pub session_id: Option<String>,
    pub socket_path: PathBuf,
    pub started_at: i64,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StatusSummary {
    pub pid: u32,
    pub cwd: PathBuf,
    pub root_thread_id: Option<String>,
    pub session_id: Option<String>,
    pub alive: bool,
    pub loaded_thread_count: usize,
    pub active_thread_count: usize,
    pub started_at: i64,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLoadedListStatusResponse {
    pub thread_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadStatusParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStatusView {
    pub thread_id: String,
    pub session_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub status: String,
    pub active_flags: Vec<String>,
    pub cwd: Option<String>,
    pub path: Option<String>,
    pub agent_path: Option<String>,
    pub task_name: Option<String>,
    pub agent_type: Option<String>,
    pub agent_nickname: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTreeNode {
    pub thread_id: String,
    pub session_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub status: String,
    pub active_flags: Vec<String>,
    pub cwd: Option<String>,
    pub path: Option<String>,
    pub agent_path: Option<String>,
    pub task_name: Option<String>,
    pub agent_type: Option<String>,
    pub agent_nickname: Option<String>,
    pub summary: Option<String>,
    pub children: Vec<AgentTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTreeResponse {
    pub roots: Vec<AgentTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusIpcRequest {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusIpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StatusIpcResponse {
    pub fn ok<T: Serialize>(result: T) -> Self {
        match serde_json::to_value(result) {
            Ok(result) => Self {
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(err) => Self::error(format!("failed to serialize status response: {err}")),
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

pub fn default_codex_home() -> io::Result<PathBuf> {
    find_codex_home().map(|path| path.to_path_buf())
}

pub fn status_paths(codex_home: &Path, pid: u32) -> StatusIpcPaths {
    let dir = codex_home.join(STATUS_DIR_NAME);
    StatusIpcPaths {
        metadata_path: dir.join(format!("{pid}.json")),
        socket_path: dir.join(format!("{pid}.sock")),
        dir,
    }
}

pub fn read_metadata_file(path: &Path) -> io::Result<StatusMetadata> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid TUI status metadata {}: {err}", path.display()),
        )
    })
}

pub fn is_read_only_method(method: &str) -> bool {
    matches!(
        method,
        METHOD_STATUS_SUMMARY | METHOD_THREAD_LOADED_LIST | METHOD_THREAD_READ | METHOD_AGENT_TREE
    )
}

#[cfg(unix)]
pub fn ensure_status_dir(codex_home: &Path) -> io::Result<PathBuf> {
    let dir = codex_home.join(STATUS_DIR_NAME);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

#[cfg(not(unix))]
pub fn ensure_status_dir(codex_home: &Path) -> io::Result<PathBuf> {
    let dir = codex_home.join(STATUS_DIR_NAME);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub struct StatusIpcServerArgs {
    pub codex_home: PathBuf,
    pub cwd: PathBuf,
    pub root_thread_id: Option<String>,
    pub session_id: Option<String>,
    pub version: String,
    pub request_handle: AppServerRequestHandle,
}

#[cfg(unix)]
pub struct StatusIpcServer {
    shutdown: Arc<AtomicBool>,
    socket_path: PathBuf,
    metadata_path: PathBuf,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl StatusIpcServer {
    pub fn start(args: StatusIpcServerArgs) -> io::Result<Self> {
        let pid = std::process::id();
        let paths = status_paths(&args.codex_home, pid);
        ensure_status_dir(&args.codex_home)?;
        let _ = fs::remove_file(&paths.socket_path);
        let _ = fs::remove_file(&paths.metadata_path);

        let listener = UnixListener::bind(&paths.socket_path)?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(&paths.socket_path, fs::Permissions::from_mode(0o600))?;

        let metadata = StatusMetadata {
            pid,
            cwd: args.cwd,
            root_thread_id: args.root_thread_id,
            session_id: args.session_id,
            socket_path: paths.socket_path.clone(),
            started_at: chrono::Utc::now().timestamp(),
            version: args.version,
        };
        write_metadata_file(&paths.metadata_path, &metadata)?;

        let runtime = Handle::current();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let request_handle = args.request_handle;
        let metadata_for_thread = metadata.clone();
        let thread = thread::Builder::new()
            .name("codexx-status-ipc".to_string())
            .spawn(move || {
                serve_status_ipc(
                    listener,
                    thread_shutdown,
                    runtime,
                    request_handle,
                    metadata_for_thread,
                );
            })?;

        Ok(Self {
            shutdown,
            socket_path: paths.socket_path,
            metadata_path: paths.metadata_path,
            thread: Some(thread),
        })
    }
}

#[cfg(unix)]
impl Drop for StatusIpcServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
        let _ = fs::remove_file(&self.metadata_path);
        let _ = fs::remove_file(&self.socket_path);
        let _ = self.thread.take();
    }
}

#[cfg(not(unix))]
pub struct StatusIpcServer;

#[cfg(not(unix))]
impl StatusIpcServer {
    pub fn start(_args: StatusIpcServerArgs) -> io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(unix)]
fn write_metadata_file(path: &Path, metadata: &StatusMetadata) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(&mut file, metadata).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(unix)]
fn serve_status_ipc(
    listener: UnixListener,
    shutdown: Arc<AtomicBool>,
    runtime: Handle,
    request_handle: AppServerRequestHandle,
    metadata: StatusMetadata,
) {
    let next_request_id = AtomicI64::new(REQUEST_ID_START);
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(err) = handle_status_stream(
                    stream,
                    &runtime,
                    &request_handle,
                    &metadata,
                    &next_request_id,
                ) {
                    tracing::debug!("codexx status IPC request failed: {err}");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                tracing::warn!("codexx status IPC listener failed: {err}");
                break;
            }
        }
    }
}

#[cfg(unix)]
fn handle_status_stream(
    mut stream: UnixStream,
    runtime: &Handle,
    request_handle: &AppServerRequestHandle,
    metadata: &StatusMetadata,
    next_request_id: &AtomicI64,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;

    let mut request_line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut request_line)?;
    let response = match serde_json::from_str::<StatusIpcRequest>(&request_line) {
        Ok(request) => {
            dispatch_status_request(request, runtime, request_handle, metadata, next_request_id)
        }
        Err(err) => StatusIpcResponse::error(format!("invalid status IPC request: {err}")),
    };
    serde_json::to_writer(&mut stream, &response).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    Ok(())
}

#[cfg(unix)]
fn dispatch_status_request(
    request: StatusIpcRequest,
    runtime: &Handle,
    request_handle: &AppServerRequestHandle,
    metadata: &StatusMetadata,
    next_request_id: &AtomicI64,
) -> StatusIpcResponse {
    if !is_read_only_method(&request.method) {
        return StatusIpcResponse::error(format!(
            "method `{}` is not available on read-only status IPC",
            request.method
        ));
    }

    match request.method.as_str() {
        METHOD_STATUS_SUMMARY => status_summary(runtime, request_handle, metadata, next_request_id)
            .map(StatusIpcResponse::ok)
            .unwrap_or_else(StatusIpcResponse::error),
        METHOD_THREAD_LOADED_LIST => load_all_thread_ids(runtime, request_handle, next_request_id)
            .map(|thread_ids| StatusIpcResponse::ok(ThreadLoadedListStatusResponse { thread_ids }))
            .unwrap_or_else(StatusIpcResponse::error),
        METHOD_THREAD_READ => serde_json::from_value::<ThreadReadStatusParams>(request.params)
            .map_err(|err| format!("invalid thread/read params: {err}"))
            .and_then(|params| {
                read_thread(runtime, request_handle, next_request_id, params.thread_id)
                    .map(|thread| thread_status_view_from_thread(&thread))
            })
            .map(StatusIpcResponse::ok)
            .unwrap_or_else(StatusIpcResponse::error),
        METHOD_AGENT_TREE => agent_tree(runtime, request_handle, next_request_id)
            .map(StatusIpcResponse::ok)
            .unwrap_or_else(StatusIpcResponse::error),
        _ => StatusIpcResponse::error("unsupported status IPC method"),
    }
}

#[cfg(unix)]
fn status_summary(
    runtime: &Handle,
    request_handle: &AppServerRequestHandle,
    metadata: &StatusMetadata,
    next_request_id: &AtomicI64,
) -> Result<StatusSummary, String> {
    let ids = load_all_thread_ids(runtime, request_handle, next_request_id)?;
    let mut active_thread_count = 0;
    let mut root_thread_id = metadata.root_thread_id.clone();
    let mut session_id = metadata.session_id.clone();

    for thread_id in &ids {
        let Ok(thread) = read_thread(runtime, request_handle, next_request_id, thread_id.clone())
        else {
            continue;
        };
        if is_active_thread_status(&thread.status) {
            active_thread_count += 1;
        }
        if root_thread_id.is_none() && thread.parent_thread_id.is_none() {
            root_thread_id = Some(thread.id.clone());
        }
        if session_id.is_none() && root_thread_id.as_deref() == Some(thread.id.as_str()) {
            session_id = Some(thread.session_id.clone());
        }
    }

    Ok(StatusSummary {
        pid: metadata.pid,
        cwd: metadata.cwd.clone(),
        root_thread_id,
        session_id,
        alive: true,
        loaded_thread_count: ids.len(),
        active_thread_count,
        started_at: metadata.started_at,
        version: metadata.version.clone(),
    })
}

#[cfg(unix)]
fn agent_tree(
    runtime: &Handle,
    request_handle: &AppServerRequestHandle,
    next_request_id: &AtomicI64,
) -> Result<AgentTreeResponse, String> {
    let ids = load_all_thread_ids(runtime, request_handle, next_request_id)?;
    let mut threads = Vec::new();
    for thread_id in ids {
        let thread = read_thread(runtime, request_handle, next_request_id, thread_id)?;
        threads.push(thread_status_view_from_thread(&thread));
    }
    Ok(build_agent_tree_from_threads(threads))
}

#[cfg(unix)]
fn load_all_thread_ids(
    runtime: &Handle,
    request_handle: &AppServerRequestHandle,
    next_request_id: &AtomicI64,
) -> Result<Vec<String>, String> {
    runtime.block_on(async {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let response: ThreadLoadedListResponse = request_handle
                .request_typed(ClientRequest::ThreadLoadedList {
                    request_id: next_status_request_id(next_request_id),
                    params: ThreadLoadedListParams {
                        cursor: cursor.clone(),
                        limit: Some(100),
                    },
                })
                .await
                .map_err(|err| format!("thread/loaded/list failed: {err}"))?;
            ids.extend(response.data);
            match response.next_cursor {
                Some(next_cursor) => cursor = Some(next_cursor),
                None => break,
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    })
}

#[cfg(unix)]
fn read_thread(
    runtime: &Handle,
    request_handle: &AppServerRequestHandle,
    next_request_id: &AtomicI64,
    thread_id: String,
) -> Result<Thread, String> {
    runtime.block_on(async {
        let response: ThreadReadResponse = request_handle
            .request_typed(ClientRequest::ThreadRead {
                request_id: next_status_request_id(next_request_id),
                params: ThreadReadParams {
                    thread_id,
                    include_turns: false,
                },
            })
            .await
            .map_err(|err| format!("thread/read failed: {err}"))?;
        Ok(response.thread)
    })
}

#[cfg(unix)]
fn next_status_request_id(next_request_id: &AtomicI64) -> RequestId {
    RequestId::Integer(next_request_id.fetch_add(1, Ordering::SeqCst))
}

pub fn build_agent_tree_from_threads(mut threads: Vec<ThreadStatusView>) -> AgentTreeResponse {
    for thread in &mut threads {
        if thread.agent_path.is_none() && thread.parent_thread_id.is_none() {
            thread.agent_path = Some("/root".to_string());
            thread.task_name = Some("root".to_string());
        }
    }
    threads.sort_by(|left, right| {
        left.agent_path
            .cmp(&right.agent_path)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });

    let mut by_id = BTreeMap::new();
    let mut children_by_parent: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut child_ids = BTreeSet::new();
    for thread in threads {
        if let Some(parent_thread_id) = &thread.parent_thread_id {
            children_by_parent
                .entry(parent_thread_id.clone())
                .or_default()
                .push(thread.thread_id.clone());
            child_ids.insert(thread.thread_id.clone());
        }
        by_id.insert(thread.thread_id.clone(), thread);
    }

    let mut root_ids = by_id
        .values()
        .filter(|thread| thread.parent_thread_id.is_none())
        .map(|thread| thread.thread_id.clone())
        .collect::<Vec<_>>();
    if root_ids.is_empty() {
        root_ids = by_id
            .keys()
            .filter(|thread_id| !child_ids.contains(*thread_id))
            .cloned()
            .collect();
    }
    root_ids.sort_by(|left, right| compare_thread_ids_for_tree(left, right, &by_id));

    AgentTreeResponse {
        roots: root_ids
            .into_iter()
            .filter_map(|thread_id| build_agent_tree_node(&thread_id, &by_id, &children_by_parent))
            .collect(),
    }
}

fn build_agent_tree_node(
    thread_id: &str,
    by_id: &BTreeMap<String, ThreadStatusView>,
    children_by_parent: &BTreeMap<String, Vec<String>>,
) -> Option<AgentTreeNode> {
    let thread = by_id.get(thread_id)?;
    let mut child_ids = children_by_parent
        .get(thread_id)
        .cloned()
        .unwrap_or_default();
    child_ids.sort_by(|left, right| compare_thread_ids_for_tree(left, right, by_id));
    Some(AgentTreeNode {
        thread_id: thread.thread_id.clone(),
        session_id: thread.session_id.clone(),
        parent_thread_id: thread.parent_thread_id.clone(),
        status: thread.status.clone(),
        active_flags: thread.active_flags.clone(),
        cwd: thread.cwd.clone(),
        path: thread.path.clone(),
        agent_path: thread.agent_path.clone(),
        task_name: thread.task_name.clone(),
        agent_type: thread.agent_type.clone(),
        agent_nickname: thread.agent_nickname.clone(),
        summary: thread.summary.clone(),
        children: child_ids
            .into_iter()
            .filter_map(|child_id| build_agent_tree_node(&child_id, by_id, children_by_parent))
            .collect(),
    })
}

fn compare_thread_ids_for_tree(
    left: &String,
    right: &String,
    by_id: &BTreeMap<String, ThreadStatusView>,
) -> std::cmp::Ordering {
    let left_thread = by_id.get(left);
    let right_thread = by_id.get(right);
    left_thread
        .and_then(|thread| thread.agent_path.as_ref())
        .cmp(&right_thread.and_then(|thread| thread.agent_path.as_ref()))
        .then_with(|| left.cmp(right))
}

fn thread_status_view_from_thread(thread: &Thread) -> ThreadStatusView {
    let (status, active_flags) = thread_status_parts(&thread.status);
    let agent_path = agent_path_from_thread(thread).or_else(|| {
        if thread.parent_thread_id.is_none() {
            Some("/root".to_string())
        } else {
            None
        }
    });
    let task_name = agent_path.as_deref().map(task_name_from_agent_path);

    ThreadStatusView {
        thread_id: thread.id.clone(),
        session_id: Some(thread.session_id.clone()),
        parent_thread_id: thread.parent_thread_id.clone(),
        status,
        active_flags,
        cwd: Some(thread.cwd.to_string_lossy().to_string()),
        path: thread
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        agent_path,
        task_name,
        agent_type: thread.agent_role.clone(),
        agent_nickname: thread.agent_nickname.clone(),
        summary: thread_summary(thread),
    }
}

fn thread_status_parts(status: &ThreadStatus) -> (String, Vec<String>) {
    match status {
        ThreadStatus::NotLoaded => ("not_loaded".to_string(), Vec::new()),
        ThreadStatus::Idle => ("idle".to_string(), Vec::new()),
        ThreadStatus::SystemError => ("system_error".to_string(), Vec::new()),
        ThreadStatus::Active { active_flags } => (
            "active".to_string(),
            active_flags
                .iter()
                .map(|flag| match flag {
                    ThreadActiveFlag::WaitingOnApproval => "waiting_on_approval".to_string(),
                    ThreadActiveFlag::WaitingOnUserInput => "waiting_on_user_input".to_string(),
                })
                .collect(),
        ),
    }
}

fn is_active_thread_status(status: &ThreadStatus) -> bool {
    matches!(status, ThreadStatus::Active { .. })
}

fn agent_path_from_thread(thread: &Thread) -> Option<String> {
    match &thread.source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) => {
            agent_path.as_ref().map(|path| path.as_str().to_string())
        }
        _ => None,
    }
}

fn task_name_from_agent_path(agent_path: &str) -> String {
    if agent_path == "/root" {
        return "root".to_string();
    }
    agent_path
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(agent_path)
        .to_string()
}

fn thread_summary(thread: &Thread) -> Option<String> {
    thread
        .name
        .clone()
        .or_else(|| (!thread.preview.trim().is_empty()).then(|| thread.preview.clone()))
}

#[cfg(test)]
mod tests;
