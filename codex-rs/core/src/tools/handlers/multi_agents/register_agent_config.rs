use super::*;
use crate::tools::handlers::multi_agents_spec::create_register_agent_config_tool_v1;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "register_agent_config")
    }

    fn spec(&self) -> ToolSpec {
        create_register_agent_config_tool_v1()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "register_agent_config register agent config config.toml agent_type agents reload role",
            self.spec(),
        )
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
