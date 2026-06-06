use super::*;
use crate::tools::handlers::multi_agents_spec::create_register_agent_config_tool_v2;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("register_agent_config")
    }

    fn spec(&self) -> ToolSpec {
        create_register_agent_config_tool_v2()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        handle_register_agent_config(invocation)
            .await
            .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
