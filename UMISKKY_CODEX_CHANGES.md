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

### `codexx_multi_agent.resume_agent`

The custom resume tool accepts agent id, task name, or canonical task path. It resumes the
target child using the original child model and reasoning settings instead of falling back
to the parent session's current model or reasoning effort. When rollout turn-context data
is not available, codexx uses the last known child config snapshot as a fallback.

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
CARGO_BUILD_JOBS=3 cargo test -p codex-core codexx_ -- --nocapture
CARGO_BUILD_JOBS=3 cargo test -p codex-tui session_header_ -- --nocapture
```
