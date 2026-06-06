# Umiskky Codex Changes

This branch is based on Codex `0.137.0` and carries local changes for the `codexx`
binary. The intent is to keep the official `codex` install separate while testing custom
behavior.

## Installed Binary

Build and install the custom CLI as:

```bash
CARGO_BUILD_JOBS=3 cargo rustc -p codex-cli --bin codex --release --locked -- -C lto=off -C codegen-units=16
sudo install -m 0755 target/release/codex /usr/local/bin/codexx
```

`lto=off` is used to reduce link time and memory pressure. It does not change the feature
surface; it mainly affects binary size and optimization.

## Multi-Agent Changes

Official `multi_agent_v1` and the official v2 top-level tools remain compatibility code.
They are not the default codexx tool surface, and codexx-specific enhancements are not
mounted onto those official namespaces.

The codexx default tool surface is the `codexx_multi_agent` namespace:

- `codexx_multi_agent.register_agent`
- `codexx_multi_agent.spawn_agent`
- `codexx_multi_agent.resume_agent`
- `codexx_multi_agent.send_message`
- `codexx_multi_agent.followup_task`
- `codexx_multi_agent.wait_agent`
- `codexx_multi_agent.wait_agent_status`
- `codexx_multi_agent.close_agent`
- `codexx_multi_agent.list_agents`

### `codexx_multi_agent.spawn_agent`

The custom spawn tool follows the task-path-based v2 interface and supports `task_name`,
canonical task paths, `agent_type`, `model`, `reasoning_effort`, `service_tier`, and
`fork_turns`.

`agent_type` is resolved from registered agent role definitions and applied to the child
agent config, including role-specific model and reasoning settings.

For codexx, the child role/config wins over the parent session's current runtime
permissions and scope. If a role TOML sets `approval_policy`, `sandbox_mode`,
`permissions`, `default_permissions`, `shell_environment_policy`, `cwd`, or workspace
roots, those child values are kept unless the role omits them. Official v1/v2 spawn
handlers keep their compatibility behavior.

### `codexx_multi_agent.resume_agent`

The custom resume tool accepts agent id, task name, or canonical task path. It also accepts
`thread_id`, `task_name`, and `agent_path` as explicit target fields. If no target is
provided, it returns the same historical agent table as `list_agents` with
`selection_required = true`, so callers can choose a concrete agent and call resume again.

`resume_scope` controls how much of the previous agent tree is restored:

- `"self"` restores only the requested agent and is the codexx default.
- `"subtree"` restores the requested agent plus stored open descendants.
- `"all"` restores all historical descendants under the target path. This is intended for
  restoring every previous child under `/root`.

Codexx resume restores the child thread's original runtime config instead of falling back
to the parent session. This includes model, reasoning effort, approval policy, permission
profile, cwd, and workspace roots from rollout turn context. When a live child config
snapshot is available, codexx also restores `shell_environment_policy`; rollout turn
context does not currently persist that field. When rollout turn-context data is not
available, codexx uses the last known child config snapshot as a fallback. Restored
descendants keep their canonical agent paths.

### `codexx_multi_agent.list_agents`

The custom list tool reads the state DB, not just live in-memory agents. With no filters it
lists historical sub-agents under the current root session and returns both structured rows
and an ASCII table.

Supported filters:

- `thread_id`
- `task_name`
- `agent_path`
- `query`
- `limit`

Rows include task name, canonical agent path, thread id, agent type, nickname, live status,
and a short summary. Spawned agents write their initial task message into thread metadata,
so historical lists have a useful summary even before the child produces a later turn.

## TUI `/agent` Changes

Codexx keeps the regular `/agent` picker focused on agents already loaded in the current
TUI session. It no longer mixes in every historical subagent by default.

Additional commands:

- `/agent resume` lists recoverable historical subagents that are not loaded in the current
  TUI session. Rows include task name, canonical path, thread id prefix, role/nickname, and
  a short summary.
- `/agent resume --all` restores all recoverable historical subagents under the current root
  session.
- `/agent new` opens an interactive flow. First choose a registered `agent_type`, then enter
  `task_name -- initial message`.

The TUI asks the app-server for runtime agent roles from the active parent thread, so roles
registered after startup through `codexx_multi_agent.register_agent` are available without
restarting. The supporting app-server methods are internal codexx plumbing:

- `agent/list_roles`
- `agent/spawn`

When the TUI attaches to spawned or resumed child threads, it now requests resume without
passing parent permission/model overrides and derives the visible permission state from the
app-server response. This keeps child permissions aligned with the child thread config.
The permissions popup also shows a `Custom permissions (current)` row when a child config
does not match one of the built-in presets.

Session exit resume hints use the running binary name. When the installed command is
`codexx`, the hint is shown as `codexx resume ...`.

### Runtime Agent Registration

The runtime registration tool is `codexx_multi_agent.register_agent`.

It accepts one unified parameter:

```json
{
  "agent_config_paths": [
    "/absolute/path/to/reviewer.toml",
    "/absolute/path/to/agents_dir"
  ]
}
```

Entries may be:

- an agent role TOML file
- a directory containing agent role TOML files
- a mixed list of files and directories

Absolute paths are recommended. Relative paths are accepted and resolved against the
current turn working directory.

The old runtime interface that loaded a Codex `config.toml` with an `[agents]` table is
not kept for this tool. Startup config loading still supports upstream Codex config
behavior; only the runtime registration tool changed.

`register_agent` returns all currently registered `agent_type` names:

```json
{
  "agent_types": ["researcher", "reviewer"]
}
```

### Agent Identity and Override Rules

An agent role's type is determined by the role TOML:

- `name = "..."` wins when present and non-empty.
- Otherwise the file stem is used, for example `reviewer.toml` registers `reviewer`.

`description` is required for runtime registration. Other Codex config fields can be
placed directly in the same role TOML and are applied when spawning that `agent_type`.

Minimal role file:

```toml
description = "Reviewer role"
developer_instructions = "Review carefully."
model = "gpt-5.4"
model_reasoning_effort = "minimal"
```

If multiple paths register the same agent type, later entries in `agent_config_paths`
override earlier entries. Registered roles are stored in the active session config, so
future `spawn_agent` calls can use them without restarting Codex.

## TUI Session Banner

The codexx TUI embeds a custom ASCII banner in:

```text
codex-rs/tui/src/history_cell/wechat_ascii_banner.txt
```

It is included at compile time with `include_str!`; the CLI does not read `/tmp` or any
runtime image/banner file. The banner is displayed in the session header when the available
inner width is at least 100 columns. Narrow terminals keep the original header-only layout
so model, directory, and permissions lines remain readable.

## Tests

Focused verification commands used for this branch:

```bash
cargo check -p codex-core
CARGO_BUILD_JOBS=3 cargo check -p codex-app-server
CARGO_BUILD_JOBS=3 cargo check -p codex-tui
RUST_MIN_STACK=8388608 CARGO_BUILD_JOBS=3 cargo nextest run -p codex-core codexx_
CARGO_BUILD_JOBS=3 cargo test -p codex-tui parse_agent_new_prompt_requires_task_and_message
CARGO_BUILD_JOBS=3 cargo test -p codex-tui permissions_selection_shows_custom_current_when_no_builtin_preset_matches
CARGO_BUILD_JOBS=3 cargo test -p codex-tui embedded_thread_response_uses_response_sandbox_profile
CARGO_BUILD_JOBS=3 cargo test -p codex-utils-cli resume_hint_can_use_codexx_binary_name
```

The `codexx_` filter currently covers runtime agent registration, spawn permission
override, historical list/filter output, resume model/reasoning/runtime config restore,
resume self/subtree/all behavior, final-status waiting, and the default codexx tool surface.
