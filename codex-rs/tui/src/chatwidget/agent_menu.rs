use super::*;

impl ChatWidget {
    pub(crate) fn show_agent_new_params_prompt(
        &mut self,
        parent_thread_id: ThreadId,
        agent_type: Option<String>,
    ) {
        let tx = self.app_event_tx.clone();
        let context_label = agent_type
            .as_deref()
            .map(|agent_type| format!("agent_type: {agent_type}"))
            .unwrap_or_else(|| "agent_type: default".to_string());
        let view = CustomPromptView::new(
            "New subagent".to_string(),
            "task_name -- initial message".to_string(),
            /*initial_text*/ String::new(),
            Some(context_label),
            Box::new(move |prompt: String| {
                tx.send(AppEvent::SpawnAgentFromPrompt {
                    parent_thread_id,
                    agent_type: agent_type.clone(),
                    prompt,
                });
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
    }
}
