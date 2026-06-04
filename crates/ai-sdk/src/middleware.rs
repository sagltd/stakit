//! The single agent extension trait.
use crate::{
    agent_cx::AgentCx,
    control::{Approval, Flow},
    error::AgentError,
    loop_event::PendingToolCall,
};

/// Conversation load/save, tool approval, stop, and model/system switching.
///
/// Implement this to hook the agent loop: load history in `on_start`, persist in
/// `on_step_done`, gate tools in `on_tool_approve`, switch model/system or stop in
/// `on_step`. All methods default to no-ops.
#[async_trait::async_trait]
pub trait AgentMiddleware<Ctx>: Send + Sync + 'static {
    /// Before the first model call. Load the conversation / inject guidance.
    async fn on_start(&self, _cx: &mut AgentCx<'_, Ctx>) -> Result<Flow, AgentError> {
        Ok(Flow::Continue)
    }

    /// Before each model call. Switch model/system, check budget, compact, drain queued input.
    async fn on_step(&self, _cx: &mut AgentCx<'_, Ctx>) -> Result<Flow, AgentError> {
        Ok(Flow::Continue)
    }

    /// After each step resolves. Persist / observe.
    async fn on_step_done(&self, _cx: &mut AgentCx<'_, Ctx>) -> Result<Flow, AgentError> {
        Ok(Flow::Continue)
    }

    /// Gate every tool call.
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, Ctx>,
        _call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        Ok(Approval::Allow)
    }

    /// After the loop ends (any reason). Persist final / cleanup.
    async fn on_finish(&self, _cx: &AgentCx<'_, Ctx>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cancel::CancelToken, message::Message, usage::Usage};

    struct Empty;

    #[async_trait::async_trait]
    impl AgentMiddleware<()> for Empty {}

    #[tokio::test]
    async fn default_hooks_are_noops() {
        let m = Empty;
        let mut msgs: Vec<Message> = vec![];
        let mut model = "m".to_string();
        let mut system: Option<String> = None;
        let u = Usage::default();
        let cancel = CancelToken::new();
        let mut cx = AgentCx::new(
            &(),
            &mut msgs,
            &mut model,
            &mut system,
            &u,
            None,
            0,
            None,
            &cancel,
        );
        assert!(matches!(m.on_start(&mut cx).await.unwrap(), Flow::Continue));
        assert!(matches!(m.on_step(&mut cx).await.unwrap(), Flow::Continue));
        assert!(matches!(
            m.on_step_done(&mut cx).await.unwrap(),
            Flow::Continue
        ));
        let approve = m
            .on_tool_approve(
                &cx,
                &PendingToolCall {
                    id: "1".into(),
                    name: "t".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        assert!(matches!(approve, Approval::Allow));
        m.on_finish(&cx).await.unwrap();
    }
}
