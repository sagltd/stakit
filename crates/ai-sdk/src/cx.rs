//! Tool execution context.
//!
//! [`ToolCx`] wraps the consumer-chosen context type `Ctx` together with the
//! run's [`CancelToken`]. `Ctx` is whatever the host needs — a database handle,
//! a router `Cx`, a websocket client — so the SDK never constrains it. This is
//! how client-side tools are wired without any dependency on a particular
//! transport.

use crate::cancel::CancelToken;

/// The context passed to every tool call.
#[derive(Debug, Clone)]
pub struct ToolCx<Ctx> {
    ctx: Ctx,
    cancel: CancelToken,
}

impl<Ctx> ToolCx<Ctx> {
    /// Builds a context wrapping `ctx` with a fresh cancel token.
    pub fn new(ctx: Ctx) -> Self {
        Self {
            ctx,
            cancel: CancelToken::new(),
        }
    }

    /// Builds a context wrapping `ctx` with an existing cancel token.
    #[must_use]
    pub const fn with_cancel(ctx: Ctx, cancel: CancelToken) -> Self {
        Self { ctx, cancel }
    }

    /// Borrows the consumer context.
    pub const fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// The run's cancel token.
    pub const fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Consumes the context, returning the inner value.
    pub fn into_inner(self) -> Ctx {
        self.ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cx_exposes_inner_context() {
        let cx = ToolCx::new(String::from("db"));
        assert_eq!(cx.ctx(), "db");
        assert!(!cx.is_cancelled());
        assert_eq!(cx.into_inner(), "db");
    }
}
