//! Middleware control-flow types.

/// Returned by conversation-phase middleware hooks.
#[derive(Debug, Clone)]
pub enum Flow {
    /// Keep running.
    Continue,
    /// Halt the run; the string becomes the final response text.
    Stop(String),
}

impl Flow {
    /// Stop with a final message.
    pub fn stop(msg: impl Into<String>) -> Self {
        Self::Stop(msg.into())
    }
}

/// Returned by `on_tool_approve`.
#[derive(Debug, Clone)]
pub enum Approval {
    /// Run the tool.
    Allow,
    /// Skip it; feed `message` to the model as the tool result; loop continues.
    Deny {
        /// Reason returned to the model.
        message: String,
    },
    /// Halt the whole agent; optional final text.
    Stop {
        /// Optional final response text.
        message: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_stop_carries_message() {
        assert!(matches!(Flow::stop("done"), Flow::Stop(ref m) if m == "done"));
    }

    #[test]
    fn approval_variants_exist() {
        let _ = (
            Approval::Allow,
            Approval::Deny {
                message: "no".into(),
            },
            Approval::Stop { message: None },
        );
    }
}
