//! Host-supplied skills (name + description; body loaded on demand).
//!
//! Skills follow progressive disclosure: only a [`Skill`] (`id`, `name`,
//! `description`) enters the context up front; the full [`SkillContent`] body is
//! fetched on demand via the built-in `load_skill` tool. A [`SkillLoader`]
//! sources skills from anywhere — a database, the filesystem, a server — so the
//! SDK never decides where a host's skills live.
use crate::error::AgentError;

/// A skill manifest entry (no body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Stable identifier used to load the body (`load_skill(id)`).
    pub id: String,
    /// Human-readable name shown in the system prompt.
    pub name: String,
    /// One-line description used to decide when the skill applies.
    pub description: String,
}

/// A loaded skill body.
#[derive(Debug, Clone)]
pub struct SkillContent {
    /// The Markdown instructions.
    pub body: String,
    /// Relative paths of bundled reference files (loaded on demand by the host).
    pub references: Vec<String>,
}

/// Source of skills — db, fs, anywhere.
#[async_trait::async_trait]
pub trait SkillLoader<Ctx>: Send + Sync + 'static {
    /// All available skills (name + description only).
    async fn list(&self, ctx: &Ctx) -> Result<Vec<Skill>, AgentError>;
    /// Fetch one skill's body by id.
    async fn load(&self, ctx: &Ctx, id: &str) -> Result<SkillContent, AgentError>;
}
