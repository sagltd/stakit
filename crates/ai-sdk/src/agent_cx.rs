//! The context handed to middleware during a run.
use crate::{cancel::CancelToken, loop_event::Step, message::Message, usage::Usage};

/// Context handed to middleware hooks during a run. Borrows the agent's
/// run-state so middleware can read the app context and mutate the conversation.
pub struct AgentCx<'a, Ctx> {
    ctx: &'a Ctx,
    messages: &'a mut Vec<Message>,
    model: &'a mut String,
    system: &'a mut Option<String>,
    usage: &'a Usage,
    cost: Option<f64>,
    index: u32,
    last_step: Option<&'a Step>,
    cancel: &'a CancelToken,
}

impl<'a, Ctx> AgentCx<'a, Ctx> {
    /// Internal constructor used by the run loop (borrow-split of the agent).
    #[allow(clippy::too_many_arguments, dead_code)]
    pub(crate) const fn new(
        ctx: &'a Ctx,
        messages: &'a mut Vec<Message>,
        model: &'a mut String,
        system: &'a mut Option<String>,
        usage: &'a Usage,
        cost: Option<f64>,
        index: u32,
        last_step: Option<&'a Step>,
        cancel: &'a CancelToken,
    ) -> Self {
        Self {
            ctx,
            messages,
            model,
            system,
            usage,
            cost,
            index,
            last_step,
            cancel,
        }
    }

    /// The app context (db/user/session).
    pub const fn ctx(&self) -> &Ctx {
        self.ctx
    }

    /// The conversation (borrowed).
    pub fn messages(&self) -> &[Message] {
        self.messages
    }

    /// The conversation (mutable) — load / inject / compact.
    pub const fn messages_mut(&mut self) -> &mut Vec<Message> {
        self.messages
    }

    /// Accumulated usage.
    pub const fn usage(&self) -> &Usage {
        self.usage
    }

    /// Accumulated USD cost, if pricing is known.
    pub const fn cost(&self) -> Option<f64> {
        self.cost
    }

    /// Current step index.
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The last completed step (available in `on_step_done`).
    pub const fn step(&self) -> Option<&Step> {
        self.last_step
    }

    /// The active model id.
    pub fn model(&self) -> &str {
        self.model
    }

    /// Switch model+provider for the rest of the run.
    pub fn set_model(&mut self, id: impl Into<String>) {
        *self.model = id.into();
    }

    /// Switch system prompt for the rest of the run.
    pub fn set_system(&mut self, text: impl Into<String>) {
        *self.system = Some(text.into());
    }

    /// Cancellation token (for cooperative tool cancellation).
    pub const fn cancel_token(&self) -> &CancelToken {
        self.cancel
    }

    /// True if the run was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agentcx_exposes_ctx_messages_and_model() {
        let mut msgs = vec![Message::user("hi")];
        let mut model = String::from("gpt-5");
        let mut system: Option<String> = None;
        let usage = Usage::default();
        let cancel = CancelToken::new();
        let mut cx = AgentCx::new(
            &7u32,
            &mut msgs,
            &mut model,
            &mut system,
            &usage,
            None,
            0,
            None,
            &cancel,
        );
        assert_eq!(*cx.ctx(), 7);
        cx.messages_mut().push(Message::user("again"));
        assert_eq!(cx.messages().len(), 2);
        cx.set_model("claude-opus");
        assert_eq!(cx.model(), "claude-opus");
    }
}
