//! Session, resume, fork, and subagent selection lifecycle for the TUI app.
//!
//! This module owns the high-level transitions between app-server threads: starting fresh sessions,
//! resuming/forking saved sessions, replacing ChatWidget instances, and maintaining the agent picker
//! cache used for multi-agent navigation.

use super::*;

const AGENT_STATUS_WIDTH: usize = 7;
const AGENT_TASK_WIDTH: usize = 18;
const AGENT_PATH_WIDTH: usize = 30;
const AGENT_THREAD_WIDTH: usize = 8;
const AGENT_TYPE_WIDTH: usize = 12;
const AGENT_NICKNAME_WIDTH: usize = 14;
const AGENT_SUMMARY_WIDTH: usize = 36;

#[derive(Debug, Clone)]
struct AgentPickerRow {
    thread_id: ThreadId,
    status: String,
    task_name: String,
    agent_path: Option<String>,
    agent_type: String,
    nickname: String,
    summary: String,
}

impl App {
    pub(super) async fn open_agent_picker(&mut self, app_server: &mut AppServerSession) {
        let mut thread_ids = self.agent_navigation.tracked_thread_ids();
        for thread_id in self.thread_event_channels.keys().copied() {
            if !thread_ids.contains(&thread_id) {
                thread_ids.push(thread_id);
            }
        }
        for thread_id in thread_ids {
            if self.side_threads.contains_key(&thread_id) {
                continue;
            }
            if !self
                .refresh_agent_picker_thread_liveness(app_server, thread_id)
                .await
            {
                continue;
            }
        }

        let has_non_primary_agent_thread = self
            .agent_navigation
            .has_non_primary_thread(self.primary_thread_id);
        if !self.config.features.enabled(Feature::Collab) && !has_non_primary_agent_thread {
            self.chat_widget.open_multi_agent_enable_prompt();
            return;
        }

        if self.agent_navigation.is_empty() {
            self.chat_widget
                .add_info_message("No agents available yet.".to_string(), /*hint*/ None);
            return;
        }

        let mut initial_selected_idx = None;
        let mut rows = Vec::new();
        let ordered_threads = self
            .agent_navigation
            .ordered_threads()
            .into_iter()
            .map(|(thread_id, entry)| (thread_id, entry.clone()))
            .collect::<Vec<_>>();
        for (thread_id, entry) in ordered_threads {
            let thread = app_server
                .thread_read(thread_id, /*include_turns*/ false)
                .await
                .ok();
            rows.push(agent_picker_row_from_active_thread(
                thread_id,
                &entry,
                thread.as_ref(),
                self.primary_thread_id,
                self.active_thread_id,
            ));
        }
        rows.sort_by(compare_agent_picker_rows);
        let items: Vec<SelectionItem> = rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| {
                if self.active_thread_id == Some(row.thread_id) {
                    initial_selected_idx = Some(idx);
                }
                let id = row.thread_id;
                let search_value = agent_picker_row_search_value(&row);
                SelectionItem {
                    name: format_agent_table_row(&row),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::SelectAgentThread(id));
                    })],
                    dismiss_on_select: true,
                    search_value: Some(search_value),
                    ..Default::default()
                }
            })
            .collect();

        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Subagents".to_string()),
            subtitle: Some(AgentNavigationState::picker_subtitle()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx,
            is_searchable: true,
            col_width_mode: ColumnWidthMode::AutoAllRows,
            row_display: SelectionRowDisplay::SingleLine,
            header: Box::new(agent_table_header_line()),
            ..Default::default()
        });
    }

    pub(super) async fn open_agent_resume_picker(&mut self, app_server: &mut AppServerSession) {
        let threads = match self.recoverable_agent_threads(app_server).await {
            Ok(threads) => threads,
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to list recoverable agents: {err}"));
                return;
            }
        };

        if threads.is_empty() {
            self.chat_widget.add_info_message(
                "No recoverable agents found.".to_string(),
                Some(
                    "Use /agent to switch among agents already loaded in this session.".to_string(),
                ),
            );
            return;
        }

        let mut rows = threads
            .iter()
            .filter_map(|thread| agent_picker_row_from_recoverable_thread(thread))
            .collect::<Vec<_>>();
        rows.sort_by(compare_agent_picker_rows);
        let items = rows
            .into_iter()
            .map(|row| {
                let id = row.thread_id;
                let search_value = agent_picker_row_search_value(&row);
                SelectionItem {
                    name: format_agent_table_row(&row),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAgentResumeScopePicker(id));
                    })],
                    dismiss_on_select: true,
                    search_value: Some(search_value),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>();

        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Subagents".to_string()),
            subtitle: Some("Recoverable historical agents not loaded in this session.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            col_width_mode: ColumnWidthMode::AutoAllRows,
            row_display: SelectionRowDisplay::SingleLine,
            header: Box::new(agent_table_header_line()),
            ..Default::default()
        });
    }

    pub(super) fn open_agent_resume_scope_picker(&mut self, thread_id: ThreadId) {
        let items = vec![
            SelectionItem {
                name: "Resume selected agent".to_string(),
                description: Some("Only attach the selected historical subagent.".to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::ResumeAgentThread {
                        thread_id,
                        scope: AgentResumeScope::SelfOnly,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Resume selected agent and descendants".to_string(),
                description: Some(
                    "Attach the selected subagent plus all recoverable child subagents."
                        .to_string(),
                ),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::ResumeAgentThread {
                        thread_id,
                        scope: AgentResumeScope::AllDescendants,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Resume Subagent".to_string()),
            subtitle: Some(thread_id.to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx: Some(0),
            col_width_mode: ColumnWidthMode::AutoAllRows,
            ..Default::default()
        });
    }

    pub(super) async fn open_agent_new_picker(&mut self, app_server: &mut AppServerSession) {
        let Some(parent_thread_id) = self.active_thread_id.or(self.primary_thread_id) else {
            self.chat_widget.add_error_message(
                "/agent new is unavailable before the session starts.".to_string(),
            );
            return;
        };

        let mut roles = match app_server.list_agent_roles(parent_thread_id).await {
            Ok(roles) => roles,
            Err(err) => {
                tracing::warn!("failed to list runtime agent roles: {err}");
                self.agent_roles_from_startup_config()
            }
        };
        roles.sort_by(|left, right| left.agent_type.cmp(&right.agent_type));

        let mut items = Vec::with_capacity(roles.len() + 1);
        items.push(SelectionItem {
            name: "default".to_string(),
            description: Some("Spawn without an agent_type override".to_string()),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenAgentNewParamsPrompt {
                    parent_thread_id,
                    agent_type: None,
                });
            })],
            dismiss_on_select: true,
            search_value: Some("default".to_string()),
            ..Default::default()
        });

        items.extend(roles.into_iter().map(|role| {
            let agent_type = role.agent_type;
            let description = role.description;
            let nicknames = role
                .nickname_candidates
                .as_ref()
                .map(|values| values.join(", "))
                .filter(|value| !value.is_empty());
            let row_description = match (description.as_deref(), nicknames.as_deref()) {
                (Some(description), Some(nicknames)) => {
                    Some(format!("{description} nicknames={nicknames}"))
                }
                (Some(description), None) => Some(description.to_string()),
                (None, Some(nicknames)) => Some(format!("nicknames={nicknames}")),
                (None, None) => None,
            };
            let selected_agent_type = agent_type.clone();
            SelectionItem {
                name: agent_type.clone(),
                description: row_description.clone(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAgentNewParamsPrompt {
                        parent_thread_id,
                        agent_type: Some(selected_agent_type.clone()),
                    });
                })],
                dismiss_on_select: true,
                search_value: Some(format!(
                    "{} {}",
                    agent_type,
                    row_description.unwrap_or_default()
                )),
                ..Default::default()
            }
        }));

        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("New Subagent".to_string()),
            subtitle: Some(
                "Choose an agent_type, then enter task_name -- initial message.".to_string(),
            ),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            col_width_mode: ColumnWidthMode::AutoAllRows,
            ..Default::default()
        });
    }

    fn agent_roles_from_startup_config(&self) -> Vec<AgentRoleSummary> {
        self.config
            .agent_roles
            .iter()
            .map(|(agent_type, role)| AgentRoleSummary {
                agent_type: agent_type.clone(),
                description: role.description.clone(),
                nickname_candidates: role.nickname_candidates.clone(),
            })
            .collect()
    }

    pub(super) async fn recoverable_agent_threads(
        &mut self,
        app_server: &mut AppServerSession,
    ) -> Result<Vec<Thread>> {
        let Some(primary_thread_id) = self.primary_thread_id else {
            return Ok(Vec::new());
        };

        let loaded_ids = app_server
            .thread_loaded_list(ThreadLoadedListParams {
                cursor: None,
                limit: None,
            })
            .await
            .map(|response| {
                response
                    .data
                    .into_iter()
                    .filter_map(|thread_id| ThreadId::from_string(&thread_id).ok())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();

        let mut all_threads = Vec::new();
        let mut cursor = None;
        loop {
            let response = app_server
                .thread_list(ThreadListParams {
                    cursor,
                    limit: Some(100),
                    sort_key: Some(ThreadSortKey::CreatedAt),
                    sort_direction: Some(SortDirection::Asc),
                    model_providers: None,
                    source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                    archived: Some(false),
                    cwd: None,
                    use_state_db_only: true,
                    search_term: None,
                })
                .await?;
            all_threads.extend(response.data);
            cursor = response.next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        let thread_by_id = all_threads
            .iter()
            .filter_map(|thread| {
                ThreadId::from_string(&thread.id)
                    .ok()
                    .map(|thread_id| (thread_id, thread.clone()))
            })
            .collect::<HashMap<_, _>>();
        let mut recoverable = all_threads
            .into_iter()
            .filter(|thread| {
                let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                    return false;
                };
                !loaded_ids.contains(&thread_id)
                    && !self.thread_event_channels.contains_key(&thread_id)
                    && !self
                        .agent_navigation
                        .tracked_thread_ids()
                        .contains(&thread_id)
                    && thread_is_descendant_of_primary(thread, primary_thread_id, &thread_by_id)
            })
            .collect::<Vec<_>>();
        recoverable.sort_by(|left, right| {
            agent_path_from_thread(left)
                .cmp(&agent_path_from_thread(right))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(recoverable)
    }

    pub(super) async fn resume_agent_thread_from_history(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        self.resume_agent_thread_into_session(app_server, thread_id)
            .await?;
        self.select_agent_thread_and_discard_side(tui, app_server, thread_id)
            .await
    }

    pub(super) async fn resume_agent_thread_from_history_with_scope(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        scope: AgentResumeScope,
    ) -> Result<()> {
        match scope {
            AgentResumeScope::SelfOnly => {
                self.resume_agent_thread_from_history(tui, app_server, thread_id)
                    .await
            }
            AgentResumeScope::AllDescendants => {
                self.resume_agent_thread_and_descendants_from_history(tui, app_server, thread_id)
                    .await
            }
        }
    }

    async fn resume_agent_thread_and_descendants_from_history(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        let threads = self.recoverable_agent_threads(app_server).await?;
        let thread_by_id = threads
            .iter()
            .filter_map(|thread| {
                ThreadId::from_string(&thread.id)
                    .ok()
                    .map(|id| (id, thread.clone()))
            })
            .collect::<HashMap<_, _>>();
        let mut target_agent_path = threads
            .iter()
            .find(|thread| ThreadId::from_string(&thread.id).ok() == Some(thread_id))
            .and_then(agent_path_from_thread);
        if target_agent_path.is_none() {
            target_agent_path = app_server
                .thread_read(thread_id, /*include_turns*/ false)
                .await
                .ok()
                .and_then(|thread| agent_path_from_thread(&thread));
        }
        let descendant_ids = threads
            .iter()
            .filter_map(|thread| {
                let id = ThreadId::from_string(&thread.id).ok()?;
                (id != thread_id
                    && thread_is_descendant_of_agent(
                        thread,
                        thread_id,
                        target_agent_path.as_deref(),
                        &thread_by_id,
                    ))
                .then_some(id)
            })
            .collect::<Vec<_>>();

        self.resume_agent_thread_into_session(app_server, thread_id)
            .await?;
        let mut resumed_descendants = 0usize;
        for descendant_id in &descendant_ids {
            match self
                .resume_agent_thread_into_session(app_server, *descendant_id)
                .await
            {
                Ok(()) => resumed_descendants += 1,
                Err(err) => {
                    tracing::warn!(
                        thread_id = %descendant_id,
                        "failed to resume recoverable descendant agent: {err}"
                    );
                }
            }
        }

        if !descendant_ids.is_empty() {
            self.chat_widget.add_info_message(
                format!(
                    "Resumed selected agent and {resumed_descendants} of {} descendants.",
                    descendant_ids.len()
                ),
                /*hint*/ None,
            );
        }
        self.select_agent_thread_and_discard_side(tui, app_server, thread_id)
            .await
    }

    pub(super) async fn resume_all_recoverable_agent_threads(
        &mut self,
        app_server: &mut AppServerSession,
    ) -> Result<()> {
        let threads = self.recoverable_agent_threads(app_server).await?;
        let total = threads.len();
        let mut resumed = 0usize;
        for thread in threads {
            let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                continue;
            };
            match self
                .resume_agent_thread_into_session(app_server, thread_id)
                .await
            {
                Ok(()) => resumed += 1,
                Err(err) => {
                    tracing::warn!(thread_id = %thread_id, "failed to resume recoverable agent: {err}");
                }
            }
        }

        self.chat_widget.add_info_message(
            format!("Resumed {resumed} of {total} recoverable agents."),
            /*hint*/ None,
        );
        Ok(())
    }

    pub(super) async fn spawn_agent_from_prompt(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        parent_thread_id: ThreadId,
        agent_type: Option<String>,
        prompt: String,
    ) {
        let (task_name, message) = match parse_agent_new_prompt(&prompt) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.chat_widget.add_error_message(err);
                self.chat_widget
                    .show_agent_new_params_prompt(parent_thread_id, agent_type);
                return;
            }
        };
        match app_server
            .spawn_agent(parent_thread_id, agent_type, task_name.clone(), message)
            .await
        {
            Ok(thread_id) => {
                if let Err(err) = self
                    .resume_agent_thread_from_history(tui, app_server, thread_id)
                    .await
                {
                    self.chat_widget.add_error_message(format!(
                        "Spawned agent {thread_id}, but failed to attach it: {err}"
                    ));
                    return;
                }
                self.chat_widget
                    .add_info_message(format!("Spawned subagent {task_name}."), /*hint*/ None);
            }
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to spawn subagent: {err}"));
            }
        }
    }

    async fn resume_agent_thread_into_session(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        let started = app_server
            .resume_thread_preserving_stored_config(&self.config, thread_id)
            .await?;
        let metadata = app_server
            .thread_read(thread_id, /*include_turns*/ false)
            .await
            .ok();

        let channel = self.ensure_thread_channel(thread_id);
        {
            let mut store = channel.store.lock().await;
            store.set_session(started.session, started.turns);
        }
        self.upsert_agent_picker_thread(
            thread_id,
            metadata
                .as_ref()
                .and_then(|thread| thread.agent_nickname.clone()),
            metadata
                .as_ref()
                .and_then(|thread| thread.agent_role.clone()),
            /*is_closed*/ false,
        );
        Ok(())
    }

    pub(super) fn is_terminal_thread_read_error(err: &color_eyre::Report) -> bool {
        err.chain()
            .any(|cause| cause.to_string().contains("thread not loaded:"))
    }

    pub(super) fn closed_state_for_thread_read_error(
        err: &color_eyre::Report,
        existing_is_closed: Option<bool>,
    ) -> bool {
        Self::is_terminal_thread_read_error(err) || existing_is_closed.unwrap_or(false)
    }

    pub(super) fn can_fallback_from_include_turns_error(err: &color_eyre::Report) -> bool {
        err.chain().any(|cause| {
            let message = cause.to_string();
            message.contains("includeTurns is unavailable before first user message")
                || message.contains("ephemeral threads do not support includeTurns")
        })
    }

    /// Updates cached picker metadata and then mirrors any visible-label change into the footer.
    ///
    /// These two writes stay paired so the picker rows and contextual footer continue to describe
    /// the same displayed thread after nickname or role updates.
    pub(super) fn upsert_agent_picker_thread(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        is_closed: bool,
    ) {
        self.chat_widget.set_collab_agent_metadata(
            thread_id,
            agent_nickname.clone(),
            agent_role.clone(),
        );
        self.agent_navigation
            .upsert(thread_id, agent_nickname, agent_role, is_closed);
        self.sync_active_agent_label();
    }

    /// Marks a cached picker thread closed and recomputes the contextual footer label.
    ///
    /// Closing a thread is not the same as removing it: users can still inspect finished agent
    /// transcripts, and the stable next/previous traversal order should not collapse around them.
    pub(super) fn mark_agent_picker_thread_closed(&mut self, thread_id: ThreadId) {
        self.agent_navigation.mark_closed(thread_id);
        self.sync_active_agent_label();
    }

    pub(super) async fn refresh_agent_picker_thread_liveness(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> bool {
        let existing_entry = self.agent_navigation.get(&thread_id).cloned();
        let has_replay_channel = self.thread_event_channels.contains_key(&thread_id);
        match app_server
            .thread_read(thread_id, /*include_turns*/ false)
            .await
        {
            Ok(thread) => {
                self.upsert_agent_picker_thread(
                    thread_id,
                    thread.agent_nickname.or_else(|| {
                        existing_entry
                            .as_ref()
                            .and_then(|entry| entry.agent_nickname.clone())
                    }),
                    thread.agent_role.or_else(|| {
                        existing_entry
                            .as_ref()
                            .and_then(|entry| entry.agent_role.clone())
                    }),
                    matches!(
                        thread.status,
                        codex_app_server_protocol::ThreadStatus::NotLoaded
                    ),
                );
                true
            }
            Err(err) => {
                if Self::is_terminal_thread_read_error(&err) && !has_replay_channel {
                    self.agent_navigation.remove(thread_id);
                    return false;
                }
                let is_closed = Self::closed_state_for_thread_read_error(
                    &err,
                    existing_entry.as_ref().map(|entry| entry.is_closed),
                );
                if let Some(entry) = existing_entry {
                    self.upsert_agent_picker_thread(
                        thread_id,
                        entry.agent_nickname,
                        entry.agent_role,
                        is_closed,
                    );
                } else {
                    self.upsert_agent_picker_thread(
                        thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                        is_closed,
                    );
                }
                true
            }
        }
    }

    /// Materializes a live thread into local replay state when the picker knows about it but the
    /// TUI has not cached a local event channel yet.
    ///
    /// Resume-time backfill intentionally avoids creating empty placeholder channels, because those
    /// placeholders make stale `/agent` entries open blank transcripts. When a user later selects a
    /// still-live discovered thread, attach it on demand with a real resumed snapshot.
    pub(super) async fn attach_live_thread_for_selection(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<bool> {
        if self.thread_event_channels.contains_key(&thread_id) {
            return Ok(true);
        }

        let (session, turns, live_attached) = match app_server
            .resume_thread_preserving_stored_config(&self.config, thread_id)
            .await
        {
            Ok(started) => (started.session, started.turns, true),
            Err(resume_err) => {
                tracing::warn!(
                    thread_id = %thread_id,
                    error = %resume_err,
                    "failed to resume live thread for selection; falling back to thread/read"
                );
                let (thread, turns) = match app_server
                    .thread_read(thread_id, /*include_turns*/ true)
                    .await
                {
                    Ok(thread) => {
                        let turns = thread.turns.clone();
                        (thread, turns)
                    }
                    Err(err) if Self::can_fallback_from_include_turns_error(&err) => {
                        let thread = app_server
                            .thread_read(thread_id, /*include_turns*/ false)
                            .await?;
                        (thread, Vec::new())
                    }
                    Err(err) => return Err(err),
                };
                if turns.is_empty() {
                    // A `thread/read` fallback without turns would create a blank local replay
                    // channel with no live listener attached, which blocks later real re-attach.
                    return Err(color_eyre::eyre::eyre!(
                        "Agent thread {thread_id} is not yet available for replay or live attach."
                    ));
                }
                let mut session = self.session_state_for_thread_read(thread_id, &thread).await;
                // `thread/read` can seed replay state, but it does not attach the app-server
                // listener that `thread/resume` establishes, so treat this path as replay-only.
                session.model.clear();
                (session, turns, false)
            }
        };
        let channel = self.ensure_thread_channel(thread_id);
        let mut store = channel.store.lock().await;
        store.set_session(session, turns);
        Ok(live_attached)
    }

    /// Replaces the chat widget and re-seeds the new widget's collab metadata from the navigation
    /// cache.
    ///
    /// Thread switches reconstruct the `ChatWidget`, which loses the `collab_agent_metadata` map.
    /// This helper copies every known nickname/role from `AgentNavigationState` into the
    /// replacement widget so that replayed collab items render agent names immediately.
    pub(super) fn replace_chat_widget(&mut self, mut chat_widget: ChatWidget) {
        // Transfer the last-written terminal title to the replacement widget
        // so it knows what OSC title is currently displayed. Without this, the
        // new widget would redundantly clear and rewrite the same title, causing
        // a visible flicker in some terminals.
        let previous_terminal_title = self.chat_widget.last_terminal_title.take();
        if chat_widget.last_terminal_title.is_none() {
            chat_widget.last_terminal_title = previous_terminal_title;
        }
        chat_widget.remote_connection = self.chat_widget.remote_connection.clone();
        for (thread_id, entry) in self.agent_navigation.ordered_threads() {
            chat_widget.set_collab_agent_metadata(
                thread_id,
                entry.agent_nickname.clone(),
                entry.agent_role.clone(),
            );
        }
        self.chat_widget = chat_widget;
        self.sync_active_agent_label();
    }

    pub(super) async fn select_agent_thread(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Result<()> {
        if self.active_thread_id == Some(thread_id) {
            return Ok(());
        }

        if !self
            .refresh_agent_picker_thread_liveness(app_server, thread_id)
            .await
        {
            self.chat_widget
                .add_error_message(format!("Agent thread {thread_id} is no longer available."));
            return Ok(());
        }

        let mut is_replay_only = self
            .agent_navigation
            .get(&thread_id)
            .is_some_and(|entry| entry.is_closed);
        let mut attached_replay_only = false;
        if self.should_attach_live_thread_for_selection(thread_id) {
            match self
                .attach_live_thread_for_selection(app_server, thread_id)
                .await
            {
                Ok(live_attached) => {
                    attached_replay_only = !live_attached;
                    if attached_replay_only {
                        is_replay_only = true;
                    }
                }
                Err(err) => {
                    self.chat_widget.add_error_message(format!(
                        "Failed to attach to agent thread {thread_id}: {err}"
                    ));
                    return Ok(());
                }
            }
        } else if !self.thread_event_channels.contains_key(&thread_id) && is_replay_only {
            self.chat_widget
                .add_error_message(format!("Agent thread {thread_id} is no longer available."));
            return Ok(());
        }

        let previous_thread_id = self.active_thread_id;
        self.store_active_thread_receiver().await;
        self.active_thread_id = None;
        let Some((receiver, mut snapshot)) = self.activate_thread_for_replay(thread_id).await
        else {
            self.chat_widget
                .add_error_message(format!("Agent thread {thread_id} is already active."));
            if let Some(previous_thread_id) = previous_thread_id {
                self.activate_thread_channel(previous_thread_id).await;
            }
            return Ok(());
        };

        self.refresh_snapshot_session_if_needed(
            app_server,
            thread_id,
            is_replay_only,
            &mut snapshot,
        )
        .await;

        self.active_thread_id = Some(thread_id);
        self.active_thread_rx = Some(receiver);

        let init = self.chatwidget_init_for_forked_or_resumed_thread(
            tui,
            self.config.clone(),
            /*initial_user_message*/ None,
        );
        self.replace_chat_widget(ChatWidget::new_with_app_event(init));

        self.reset_for_thread_switch(tui)?;
        self.replay_thread_snapshot(snapshot, !is_replay_only);
        if is_replay_only {
            let message = if attached_replay_only {
                format!(
                    "Agent thread {thread_id} could not be resumed live. Replaying saved transcript."
                )
            } else {
                format!("Agent thread {thread_id} is closed. Replaying saved transcript.")
            };
            self.chat_widget.add_info_message(message, /*hint*/ None);
        }
        self.drain_active_thread_events(tui).await?;
        self.refresh_pending_thread_approvals().await;

        Ok(())
    }

    pub(super) fn should_attach_live_thread_for_selection(&self, thread_id: ThreadId) -> bool {
        !self.thread_event_channels.contains_key(&thread_id)
            && self
                .agent_navigation
                .get(&thread_id)
                .is_none_or(|entry| !entry.is_closed)
    }

    pub(super) fn reset_for_thread_switch(&mut self, tui: &mut tui::Tui) -> Result<()> {
        self.reset_transcript_state_after_clear();
        tui.clear_pending_history_lines();
        Self::clear_terminal_for_thread_switch(&mut tui.terminal)?;
        Ok(())
    }

    pub(super) fn clear_terminal_for_thread_switch<B>(
        terminal: &mut crate::custom_terminal::Terminal<B>,
    ) -> Result<()>
    where
        B: Backend + Write,
    {
        terminal.clear_scrollback_and_visible_screen_ansi()?;
        let mut area = terminal.viewport_area;
        if area.y > 0 {
            area.y = 0;
            terminal.set_viewport_area(area);
        }
        Ok(())
    }

    pub(super) fn reset_thread_event_state(&mut self) {
        self.abort_all_thread_event_listeners();
        self.thread_event_channels.clear();
        self.agent_navigation.clear();
        self.side_threads.clear();
        self.active_thread_id = None;
        self.active_thread_rx = None;
        self.primary_thread_id = None;
        self.last_subagent_backfill_attempt = None;
        self.primary_session_configured = None;
        self.pending_primary_events.clear();
        self.pending_app_server_requests.clear();
        self.pending_startup_thread_start = false;
        self.chat_widget.set_pending_thread_approvals(Vec::new());
        self.sync_active_agent_label();
    }

    pub(super) async fn handle_startup_thread_started(
        &mut self,
        app_server: &mut AppServerSession,
        result: Result<AppServerStartedThread, String>,
    ) -> Result<()> {
        if !self.pending_startup_thread_start {
            if let Ok(started) = result {
                let thread_id = started.session.thread_id;
                if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                    tracing::warn!(
                        thread_id = %thread_id,
                        "failed to unsubscribe stale startup thread: {err}"
                    );
                }
                self.discard_thread_local_state(thread_id).await;
            }
            return Ok(());
        }

        self.pending_startup_thread_start = false;
        self.chat_widget
            .set_queue_submissions_until_session_configured(/*queue*/ false);
        match result {
            Ok(started) => {
                self.enqueue_primary_thread_session(started.session, started.turns)
                    .await?;
                self.chat_widget.maybe_send_next_queued_input();
            }
            Err(err) => {
                return Err(color_eyre::eyre::eyre!(
                    "Failed to start a fresh session through the app server: {err}"
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn start_fresh_session_with_summary_hint(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        session_start_source: Option<ThreadStartSource>,
        initial_user_message: Option<crate::chatwidget::UserMessage>,
    ) {
        // Start a fresh in-memory session while preserving resumability via persisted rollout
        // history. If an initial message is provided, `enqueue_primary_thread_session` suppresses it
        // until the new session is configured and any replayed turns have been rendered.
        self.refresh_in_memory_config_from_disk_best_effort("starting a new thread")
            .await;
        let model = self.chat_widget.current_model().to_string();
        let config = self.fresh_session_config();
        let summary = session_summary(
            self.chat_widget.token_usage(),
            self.chat_widget.thread_id(),
            self.chat_widget.thread_name(),
            self.chat_widget.rollout_path().as_deref(),
        );
        self.shutdown_current_thread(app_server).await;
        let tracked_thread_ids: Vec<ThreadId> =
            self.thread_event_channels.keys().copied().collect();
        for thread_id in tracked_thread_ids {
            if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
                tracing::warn!("failed to unsubscribe tracked thread {thread_id}: {err}");
            }
        }
        self.config = config.clone();
        match app_server
            .start_thread_with_session_start_source(&config, session_start_source)
            .await
        {
            Ok(started) => {
                if let Err(err) = self
                    .replace_chat_widget_with_app_server_thread(
                        tui,
                        app_server,
                        started,
                        initial_user_message,
                    )
                    .await
                {
                    self.chat_widget.add_error_message(format!(
                        "Failed to attach to fresh app-server thread: {err}"
                    ));
                } else if let Some(summary) = summary {
                    let mut lines: Vec<Line<'static>> = Vec::new();
                    if let Some(usage_line) = summary.usage_line {
                        lines.push(usage_line.into());
                    }
                    if let Some(command) = summary.resume_hint {
                        let spans = vec!["To continue this session, run ".into(), command.cyan()];
                        lines.push(spans.into());
                    }
                    self.chat_widget.add_plain_history_lines(lines);
                }
            }
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "Failed to start a fresh session through the app server: {err}"
                ));
                self.config.model = Some(model);
            }
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) async fn replace_chat_widget_with_app_server_thread(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        started: AppServerStartedThread,
        initial_user_message: Option<crate::chatwidget::UserMessage>,
    ) -> Result<()> {
        // Initial messages are for freshly attached primary threads only. Thread switches and
        // resume/fork flows pass `None` so they cannot replay old history and then auto-submit a new
        // user turn by accident.
        self.reset_thread_event_state();
        let init = self.chatwidget_init_for_forked_or_resumed_thread(
            tui,
            self.config.clone(),
            initial_user_message,
        );
        self.replace_chat_widget(ChatWidget::new_with_app_event(init));
        self.enqueue_primary_thread_session(started.session, started.turns)
            .await?;
        self.backfill_loaded_subagent_threads(app_server).await;
        Ok(())
    }

    /// Fetches all loaded threads from the app server and registers descendants of the primary
    /// thread in the navigation cache and chat widget metadata.
    ///
    /// Called after `replace_chat_widget_with_app_server_thread` during resume, fork, and new
    /// thread creation so that the `/agent` picker and keyboard navigation are pre-populated even
    /// if the TUI did not witness the original spawn events.
    ///
    /// The loaded-thread list is fetched in full (no pagination) and the spawn tree is walked
    /// by `find_loaded_subagent_threads_for_primary`. Each discovered subagent is registered via
    /// `upsert_agent_picker_thread`, which writes to both `AgentNavigationState` and the
    /// `ChatWidget` metadata map.
    pub(super) async fn backfill_loaded_subagent_threads(
        &mut self,
        app_server: &mut AppServerSession,
    ) -> bool {
        let Some(primary_thread_id) = self.primary_thread_id else {
            return false;
        };

        let loaded_thread_ids = match app_server
            .thread_loaded_list(ThreadLoadedListParams {
                cursor: None,
                limit: None,
            })
            .await
        {
            Ok(response) => response.data,
            Err(err) => {
                tracing::warn!(%err, "failed to list loaded threads for subagent backfill");
                return false;
            }
        };

        let mut threads = Vec::new();
        let mut had_read_error = false;
        for thread_id in loaded_thread_ids {
            let Ok(thread_id) = ThreadId::from_string(&thread_id) else {
                tracing::warn!("ignoring loaded thread with invalid id during subagent backfill");
                continue;
            };

            if thread_id == primary_thread_id {
                continue;
            }

            match app_server
                .thread_read(thread_id, /*include_turns*/ false)
                .await
            {
                Ok(thread) => threads.push(thread),
                Err(err) => {
                    had_read_error = true;
                    tracing::warn!(thread_id = %thread_id, %err, "failed to read loaded thread");
                }
            }
        }

        for thread in find_loaded_subagent_threads_for_primary(threads, primary_thread_id) {
            self.upsert_agent_picker_thread(
                thread.thread_id,
                thread.agent_nickname,
                thread.agent_role,
                /*is_closed*/ false,
            );
        }

        !had_read_error
    }

    /// Returns the adjacent thread id for keyboard navigation, backfilling from the server if the
    /// local cache has no neighbor.
    ///
    /// Tries the fast path first: ask `AgentNavigationState` directly. If it returns `None` (no
    /// adjacent entry exists, typically because the cache was never populated with remote
    /// subagents), performs a full `backfill_loaded_subagent_threads` and retries. This ensures the
    /// first next/previous keypress in a resumed remote session discovers subagents on demand
    /// without requiring the user to wait for a proactive fetch.
    pub(super) async fn adjacent_thread_id_with_backfill(
        &mut self,
        app_server: &mut AppServerSession,
        direction: AgentNavigationDirection,
    ) -> Option<ThreadId> {
        let current_thread = self.current_displayed_thread_id();
        if let Some(thread_id) = self
            .agent_navigation
            .adjacent_thread_id(current_thread, direction)
        {
            return Some(thread_id);
        }

        let primary_thread_id = self.primary_thread_id?;
        if self.last_subagent_backfill_attempt == Some(primary_thread_id) {
            return None;
        }

        if self.backfill_loaded_subagent_threads(app_server).await {
            self.last_subagent_backfill_attempt = Some(primary_thread_id);
        }
        self.agent_navigation
            .adjacent_thread_id(self.current_displayed_thread_id(), direction)
    }

    pub(super) fn fresh_session_config(&self) -> Config {
        let mut config = self.config.clone();
        config.service_tier = self.chat_widget.configured_service_tier();
        config
    }
    pub(super) async fn resume_target_session(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        target_session: SessionTarget,
    ) -> Result<AppRunControl> {
        if self.ignore_same_thread_resume(&target_session) {
            tui.frame_requester().schedule_frame();
            return Ok(AppRunControl::Continue);
        }

        let current_cwd = self.config.cwd.to_path_buf();
        let resume_cwd = if self.app_server_target.uses_remote_workspace() {
            current_cwd.clone()
        } else {
            match crate::session_resume::resolve_cwd_for_resume_or_fork(
                tui,
                self.state_db.as_deref(),
                &current_cwd,
                target_session.thread_id,
                target_session.path.as_deref(),
                CwdPromptAction::Resume,
                /*allow_prompt*/ true,
            )
            .await?
            {
                crate::session_resume::ResolveCwdOutcome::Continue(Some(cwd)) => cwd,
                crate::session_resume::ResolveCwdOutcome::Continue(None) => current_cwd.clone(),
                crate::session_resume::ResolveCwdOutcome::Exit => {
                    return Ok(AppRunControl::Exit(ExitReason::UserRequested));
                }
            }
        };

        let mut resume_config = match self
            .rebuild_config_for_resume_or_fallback(&current_cwd, resume_cwd)
            .await
        {
            Ok(cfg) => cfg,
            Err(err) => {
                self.chat_widget.add_error_message(format!(
                    "Failed to rebuild configuration for resume: {err}"
                ));
                return Ok(AppRunControl::Continue);
            }
        };
        self.apply_runtime_policy_overrides(&mut resume_config);

        let summary = session_summary(
            self.chat_widget.token_usage(),
            self.chat_widget.thread_id(),
            self.chat_widget.thread_name(),
            self.chat_widget.rollout_path().as_deref(),
        );
        match app_server
            .resume_thread(resume_config.clone(), target_session.thread_id)
            .await
        {
            Ok(resumed) => {
                let resumed_thread_id = resumed.session.thread_id;
                self.shutdown_current_thread(app_server).await;
                self.config = resume_config;
                tui.set_notification_settings(
                    self.config.tui_notifications.method,
                    self.config.tui_notifications.condition,
                );
                self.file_search
                    .update_search_dir(self.config.cwd.to_path_buf());
                match self
                    .replace_chat_widget_with_app_server_thread(
                        tui, app_server, resumed, /*initial_user_message*/ None,
                    )
                    .await
                {
                    Ok(()) => {
                        if let Some(summary) = summary {
                            let mut lines: Vec<Line<'static>> = Vec::new();
                            if let Some(usage_line) = summary.usage_line {
                                lines.push(usage_line.into());
                            }
                            if let Some(command) = summary.resume_hint {
                                let spans =
                                    vec!["To continue this session, run ".into(), command.cyan()];
                                lines.push(spans.into());
                            }
                            self.chat_widget.add_plain_history_lines(lines);
                        }
                        self.maybe_prompt_resume_paused_goal_after_resume(
                            app_server,
                            resumed_thread_id,
                        )
                        .await;
                    }
                    Err(err) => {
                        self.chat_widget.add_error_message(format!(
                            "Failed to attach to resumed app-server thread: {err}"
                        ));
                    }
                }
            }
            Err(err) => {
                let path_display = target_session.display_label();
                self.chat_widget.add_error_message(format!(
                    "Failed to resume session from {path_display}: {err}"
                ));
            }
        }

        Ok(AppRunControl::Continue)
    }
}

fn agent_table_header_line() -> Line<'static> {
    Line::from(format!("  {}", format_agent_table_header())).dim()
}

fn format_agent_table_header() -> String {
    format_agent_table_cells(
        "STATUS",
        "TASK",
        "AGENT PATH",
        "THREAD",
        "TYPE",
        "NICKNAME",
        "SUMMARY",
    )
}

fn format_agent_table_row(row: &AgentPickerRow) -> String {
    format_agent_table_cells(
        &row.status,
        &row.task_name,
        row.agent_path.as_deref().unwrap_or("-"),
        &short_thread_id(row.thread_id),
        &row.agent_type,
        &row.nickname,
        &row.summary,
    )
}

fn format_agent_table_cells(
    status: &str,
    task: &str,
    agent_path: &str,
    thread_id: &str,
    agent_type: &str,
    nickname: &str,
    summary: &str,
) -> String {
    [
        fixed_width_cell(status, AGENT_STATUS_WIDTH),
        fixed_width_cell(task, AGENT_TASK_WIDTH),
        fixed_width_cell(agent_path, AGENT_PATH_WIDTH),
        fixed_width_cell(thread_id, AGENT_THREAD_WIDTH),
        fixed_width_cell(agent_type, AGENT_TYPE_WIDTH),
        fixed_width_cell(nickname, AGENT_NICKNAME_WIDTH),
        fixed_width_cell(summary, AGENT_SUMMARY_WIDTH),
    ]
    .join("  ")
}

fn fixed_width_cell(value: &str, width: usize) -> String {
    let normalized = value.replace(['\n', '\r'], " ");
    let trimmed = normalized.trim();
    let value = if trimmed.is_empty() { "-" } else { trimmed };
    let mut chars = value.chars();
    let mut cell = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width >= 3 {
        cell = cell
            .chars()
            .take(width.saturating_sub(3))
            .collect::<String>();
        cell.push_str("...");
    }
    format!("{cell:<width$}")
}

fn short_thread_id(thread_id: ThreadId) -> String {
    thread_id.to_string().chars().take(8).collect()
}

fn agent_picker_row_from_active_thread(
    thread_id: ThreadId,
    entry: &AgentPickerThreadEntry,
    thread: Option<&Thread>,
    primary_thread_id: Option<ThreadId>,
    active_thread_id: Option<ThreadId>,
) -> AgentPickerRow {
    let is_primary = primary_thread_id == Some(thread_id);
    let agent_path = thread
        .and_then(agent_path_from_thread)
        .or_else(|| is_primary.then(|| "/root".to_string()));
    let task_name = task_name_from_agent_path(agent_path.as_deref())
        .unwrap_or_else(|| if is_primary { "root" } else { "-" }.to_string());
    let nickname = thread
        .and_then(|thread| non_empty_string(thread.agent_nickname.as_deref()))
        .or_else(|| non_empty_string(entry.agent_nickname.as_deref()))
        .unwrap_or_else(|| {
            if is_primary {
                "Main".to_string()
            } else {
                "-".to_string()
            }
        });
    let agent_type = thread
        .and_then(|thread| non_empty_string(thread.agent_role.as_deref()))
        .or_else(|| non_empty_string(entry.agent_role.as_deref()))
        .unwrap_or_else(|| {
            if is_primary {
                "default".to_string()
            } else {
                "-".to_string()
            }
        });
    let summary = thread_summary(thread).unwrap_or_else(|| {
        if is_primary {
            "Main session".to_string()
        } else {
            "-".to_string()
        }
    });
    let status = if active_thread_id == Some(thread_id) {
        "current"
    } else if entry.is_closed {
        "closed"
    } else {
        thread
            .map(|thread| thread_status_label(&thread.status))
            .unwrap_or("open")
    }
    .to_string();

    AgentPickerRow {
        thread_id,
        status,
        task_name,
        agent_path,
        agent_type,
        nickname,
        summary,
    }
}

fn agent_picker_row_from_recoverable_thread(thread: &Thread) -> Option<AgentPickerRow> {
    let thread_id = ThreadId::from_string(&thread.id).ok()?;
    let agent_path = agent_path_from_thread(thread);
    Some(AgentPickerRow {
        thread_id,
        status: thread_status_label(&thread.status).to_string(),
        task_name: task_name_from_agent_path(agent_path.as_deref()).unwrap_or_else(|| "-".into()),
        agent_path,
        agent_type: non_empty_string(thread.agent_role.as_deref()).unwrap_or_else(|| "-".into()),
        nickname: non_empty_string(thread.agent_nickname.as_deref()).unwrap_or_else(|| "-".into()),
        summary: thread_summary(Some(thread)).unwrap_or_else(|| "-".into()),
    })
}

fn compare_agent_picker_rows(left: &AgentPickerRow, right: &AgentPickerRow) -> std::cmp::Ordering {
    agent_row_sort_key(left).cmp(&agent_row_sort_key(right))
}

fn agent_row_sort_key(row: &AgentPickerRow) -> (bool, String, String) {
    (
        row.agent_path.is_none(),
        row.agent_path.clone().unwrap_or_default(),
        row.thread_id.to_string(),
    )
}

fn agent_picker_row_search_value(row: &AgentPickerRow) -> String {
    [
        row.status.clone(),
        row.task_name.clone(),
        row.agent_path.clone().unwrap_or_default(),
        row.thread_id.to_string(),
        row.agent_type.clone(),
        row.nickname.clone(),
        row.summary.clone(),
    ]
    .join(" ")
}

fn thread_status_label(status: &codex_app_server_protocol::ThreadStatus) -> &'static str {
    match status {
        codex_app_server_protocol::ThreadStatus::NotLoaded => "closed",
        codex_app_server_protocol::ThreadStatus::Idle => "open",
        codex_app_server_protocol::ThreadStatus::SystemError => "error",
        codex_app_server_protocol::ThreadStatus::Active { .. } => "active",
    }
}

fn thread_summary(thread: Option<&Thread>) -> Option<String> {
    let thread = thread?;
    non_empty_string(thread.name.as_deref())
        .or_else(|| non_empty_string(Some(thread.preview.as_str())))
}

fn task_name_from_agent_path(agent_path: Option<&str>) -> Option<String> {
    let agent_path = agent_path?;
    let task_name = agent_path.rsplit('/').next()?;
    if task_name.is_empty() {
        return None;
    }
    if task_name == "root" {
        return (agent_path == "/root").then(|| "root".to_string());
    }
    Some(task_name.to_string())
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn agent_path_from_thread(thread: &Thread) -> Option<String> {
    match &thread.source {
        codex_app_server_protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                agent_path: Some(agent_path),
                ..
            },
        ) => Some(agent_path.to_string()),
        _ => None,
    }
}

fn parse_agent_new_prompt(input: &str) -> std::result::Result<(String, String), String> {
    let Some((task_name, message)) = input.trim().split_once("--") else {
        return Err("Use task_name -- initial message.".to_string());
    };
    let task_name = task_name.trim();
    let message = message.trim();
    if task_name.is_empty() {
        return Err("Agent task_name must not be empty.".to_string());
    }
    if message.is_empty() {
        return Err("Agent initial message must not be empty.".to_string());
    }
    Ok((task_name.to_string(), message.to_string()))
}

fn parent_thread_id_from_thread(thread: &Thread) -> Option<ThreadId> {
    thread
        .parent_thread_id
        .as_deref()
        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
        .or_else(|| match &thread.source {
            codex_app_server_protocol::SessionSource::SubAgent(
                codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                },
            ) => Some(*parent_thread_id),
            _ => None,
        })
}

fn thread_is_descendant_of_primary(
    thread: &Thread,
    primary_thread_id: ThreadId,
    thread_by_id: &HashMap<ThreadId, Thread>,
) -> bool {
    let mut seen = HashSet::new();
    let mut parent_thread_id = parent_thread_id_from_thread(thread);
    while let Some(parent_id) = parent_thread_id {
        if parent_id == primary_thread_id {
            return true;
        }
        if !seen.insert(parent_id) {
            return false;
        }
        parent_thread_id = thread_by_id
            .get(&parent_id)
            .and_then(parent_thread_id_from_thread);
    }
    false
}

fn thread_is_descendant_of_agent(
    thread: &Thread,
    ancestor_thread_id: ThreadId,
    ancestor_agent_path: Option<&str>,
    thread_by_id: &HashMap<ThreadId, Thread>,
) -> bool {
    let mut seen = HashSet::new();
    let mut parent_thread_id = parent_thread_id_from_thread(thread);
    while let Some(parent_id) = parent_thread_id {
        if parent_id == ancestor_thread_id {
            return true;
        }
        if !seen.insert(parent_id) {
            break;
        }
        parent_thread_id = thread_by_id
            .get(&parent_id)
            .and_then(parent_thread_id_from_thread);
    }

    match (agent_path_from_thread(thread), ancestor_agent_path) {
        (Some(agent_path), Some(ancestor_agent_path)) => {
            agent_path_is_descendant_of(&agent_path, ancestor_agent_path)
        }
        _ => false,
    }
}

fn agent_path_is_descendant_of(agent_path: &str, ancestor_agent_path: &str) -> bool {
    if agent_path == ancestor_agent_path {
        return false;
    }
    let ancestor_prefix = format!("{}/", ancestor_agent_path.trim_end_matches('/'));
    agent_path.starts_with(&ancestor_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_thread_read_error_detection_matches_not_loaded_errors() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: thread not loaded: thr_123"
        );

        assert!(App::is_terminal_thread_read_error(&err));
    }

    #[test]
    fn terminal_thread_read_error_detection_ignores_transient_failures() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read transport error: broken pipe"
        );

        assert!(!App::is_terminal_thread_read_error(&err));
    }

    #[test]
    fn closed_state_for_thread_read_error_preserves_live_state_without_cache_on_transient_error() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read transport error: broken pipe"
        );

        assert!(!App::closed_state_for_thread_read_error(
            &err, /*existing_is_closed*/ None
        ));
    }

    #[test]
    fn closed_state_for_thread_read_error_marks_terminal_uncached_threads_closed() {
        let err = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: thread not loaded: thr_123"
        );

        assert!(App::closed_state_for_thread_read_error(
            &err, /*existing_is_closed*/ None
        ));
    }

    #[test]
    fn include_turns_fallback_detection_handles_unmaterialized_and_ephemeral_threads() {
        let unmaterialized = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: thread thr_123 is not materialized yet; includeTurns is unavailable before first user message"
        );
        let ephemeral = color_eyre::eyre::eyre!(
            "thread/read failed during TUI session lookup: thread/read failed: ephemeral threads do not support includeTurns"
        );

        assert!(App::can_fallback_from_include_turns_error(&unmaterialized));
        assert!(App::can_fallback_from_include_turns_error(&ephemeral));
    }

    #[test]
    fn parse_agent_new_prompt_requires_task_and_message() {
        assert_eq!(
            parse_agent_new_prompt("research -- inspect the repo").unwrap(),
            ("research".to_string(), "inspect the repo".to_string())
        );
        assert!(parse_agent_new_prompt("research").is_err());
        assert!(parse_agent_new_prompt(" -- inspect").is_err());
        assert!(parse_agent_new_prompt("research -- ").is_err());
    }

    #[test]
    fn codexx_agent_picker_table_row_renders_separate_columns() {
        let row = AgentPickerRow {
            thread_id: ThreadId::from_string("00000000-0000-0000-0000-000000000123")
                .expect("valid thread id"),
            status: "open".to_string(),
            task_name: "inspect".to_string(),
            agent_path: Some("/root/inspect".to_string()),
            agent_type: "reviewer".to_string(),
            nickname: "Ada".to_string(),
            summary: "Review the implementation".to_string(),
        };

        let header = format_agent_table_header();
        let rendered = format_agent_table_row(&row);

        assert!(header.contains("AGENT PATH"));
        assert!(rendered.contains("/root/inspect"));
        assert!(rendered.contains("reviewer"));
        assert!(rendered.contains("Ada"));
        assert!(rendered.contains("00000000"));
        assert!(!rendered.contains("task="));
        assert!(!rendered.contains("path="));
    }

    #[test]
    fn codexx_agent_picker_rows_sort_by_agent_path_with_missing_paths_last() {
        fn row(id_suffix: &str, agent_path: Option<&str>) -> AgentPickerRow {
            AgentPickerRow {
                thread_id: ThreadId::from_string(&format!(
                    "00000000-0000-0000-0000-000000000{id_suffix}"
                ))
                .expect("valid thread id"),
                status: "open".to_string(),
                task_name: "-".to_string(),
                agent_path: agent_path.map(ToOwned::to_owned),
                agent_type: "-".to_string(),
                nickname: "-".to_string(),
                summary: "-".to_string(),
            }
        }

        let mut rows = vec![
            row("003", Some("/root/b")),
            row("004", None),
            row("002", Some("/root/a")),
            row("001", Some("/root")),
        ];

        rows.sort_by(compare_agent_picker_rows);

        let sorted_paths = rows
            .iter()
            .map(|row| row.agent_path.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            sorted_paths,
            vec![Some("/root"), Some("/root/a"), Some("/root/b"), None]
        );
    }

    #[test]
    fn codexx_agent_picker_task_name_from_agent_path_uses_last_path_component() {
        assert_eq!(
            task_name_from_agent_path(Some("/root")),
            Some("root".to_string())
        );
        assert_eq!(
            task_name_from_agent_path(Some("/root/research")),
            Some("research".to_string())
        );
        assert_eq!(
            task_name_from_agent_path(Some("/root/research/deep")),
            Some("deep".to_string())
        );
        assert_eq!(task_name_from_agent_path(Some("/")), None);
        assert_eq!(task_name_from_agent_path(None), None);
    }

    #[test]
    fn codexx_agent_picker_path_descendant_requires_strict_child_path() {
        assert!(agent_path_is_descendant_of(
            "/root/research/deep",
            "/root/research"
        ));
        assert!(!agent_path_is_descendant_of(
            "/root/research",
            "/root/research"
        ));
        assert!(!agent_path_is_descendant_of(
            "/root/researcher",
            "/root/research"
        ));
    }
}
