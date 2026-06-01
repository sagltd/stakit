//! Pluggable context loading.
//!
//! A [`ContextLoader`] produces conversation context (a system-prompt fragment
//! and/or seed messages) from any source — a file, a database, an HTTP service,
//! a RAG retriever — before a run. Multiple loaders compose: the agent runs them
//! all and merges the results into the initial system prompt and history. This
//! is the same "minimal trait + reference impl" pattern as `Provider`, `Tool`
//! and `SkillLoader`, so a host can wire context from anything.

use std::path::PathBuf;

use futures::future::BoxFuture;

use crate::cx::ToolCx;
use crate::error::AiError;
use crate::message::Message;

/// Context produced by a [`ContextLoader`].
#[derive(Debug, Clone, Default)]
pub struct LoadedContext {
    /// Text appended to the system prompt.
    pub system: Option<String>,
    /// Messages prepended to the conversation history.
    pub messages: Vec<Message>,
}

/// Loads conversation context from a source before a run.
pub trait ContextLoader<Ctx>: Send + Sync {
    /// Loads context, given the tool context (for DB handles, auth, etc.).
    fn load<'a>(&'a self, cx: &'a ToolCx<Ctx>) -> BoxFuture<'a, Result<LoadedContext, AiError>>;
}

/// A reference [`ContextLoader`] that reads a UTF-8 file into the system prompt.
#[derive(Debug, Clone)]
pub struct FsContextLoader {
    path: PathBuf,
}

impl FsContextLoader {
    /// Loads the file at `path` as a system-prompt fragment.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl<Ctx: Send + Sync + 'static> ContextLoader<Ctx> for FsContextLoader {
    fn load<'a>(&'a self, _cx: &'a ToolCx<Ctx>) -> BoxFuture<'a, Result<LoadedContext, AiError>> {
        Box::pin(async move {
            let text = tokio::fs::read_to_string(&self.path).await.map_err(|e| {
                AiError::Skill(format!("context file {}: {e}", self.path.display()))
            })?;
            Ok(LoadedContext {
                system: Some(text),
                messages: Vec::new(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticLoader(&'static str);

    impl<Ctx: Send + Sync + 'static> ContextLoader<Ctx> for StaticLoader {
        fn load<'a>(
            &'a self,
            _cx: &'a ToolCx<Ctx>,
        ) -> BoxFuture<'a, Result<LoadedContext, AiError>> {
            Box::pin(async move {
                Ok(LoadedContext {
                    system: Some(self.0.to_owned()),
                    messages: vec![Message::user_text("seed")],
                })
            })
        }
    }

    #[tokio::test]
    async fn loader_produces_system_and_messages() {
        let cx = ToolCx::new(());
        let loaded = ContextLoader::<()>::load(&StaticLoader("rules"), &cx)
            .await
            .expect("load");
        assert_eq!(loaded.system.as_deref(), Some("rules"));
        assert_eq!(loaded.messages, vec![Message::user_text("seed")]);
    }
}
