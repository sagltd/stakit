//! Pluggable skill loading with progressive disclosure.
//!
//! Skills follow three-level progressive disclosure: only a [`SkillManifest`]
//! (`name` + `description`) enters the context up front; the full
//! [`SkillContent`] body is fetched on demand (via the built-in `load_skill`
//! tool); referenced files load only when needed. A [`SkillLoader`] sources
//! manifests and bodies from anywhere — the filesystem ([`FsSkillLoader`]), a
//! database, a server — so large skill libraries never bloat the prompt.

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;

use crate::cx::ToolCx;
use crate::error::AiError;

/// A skill's metadata (level 1 — cheap, no body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillManifest {
    /// Unique skill name (kebab-case, matches the directory name).
    pub name: String,
    /// One-line description used to decide when the skill applies.
    pub description: String,
    /// Tools the skill is allowed to use (space-separated in `SKILL.md`).
    pub allowed_tools: Vec<String>,
}

/// A skill's full body (level 2 — fetched on demand).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillContent {
    /// The Markdown instructions (everything after the frontmatter).
    pub body: String,
    /// Relative paths of bundled reference files (level 3).
    pub references: Vec<String>,
}

/// Loads skills from a source, separating cheap manifest listing from on-demand
/// body fetching.
pub trait SkillLoader<Ctx>: Send + Sync {
    /// Lists every skill's manifest (must be cheap — no bodies).
    fn list<'a>(
        &'a self,
        cx: &'a ToolCx<Ctx>,
    ) -> BoxFuture<'a, Result<Vec<SkillManifest>, AiError>>;

    /// Loads one skill's full body by name.
    fn load<'a>(
        &'a self,
        name: &'a str,
        cx: &'a ToolCx<Ctx>,
    ) -> BoxFuture<'a, Result<SkillContent, AiError>>;
}

/// A reference [`SkillLoader`] over a directory of `<root>/<name>/SKILL.md`.
#[derive(Debug, Clone)]
pub struct FsSkillLoader {
    root: PathBuf,
}

impl FsSkillLoader {
    /// Loads skills from `<root>/*/SKILL.md` (e.g. `.agents/skills`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl<Ctx: Send + Sync + 'static> SkillLoader<Ctx> for FsSkillLoader {
    fn list<'a>(
        &'a self,
        _cx: &'a ToolCx<Ctx>,
    ) -> BoxFuture<'a, Result<Vec<SkillManifest>, AiError>> {
        Box::pin(async move {
            let mut manifests = Vec::new();
            let mut dir = tokio::fs::read_dir(&self.root)
                .await
                .map_err(|e| AiError::Skill(format!("skills dir {}: {e}", self.root.display())))?;
            while let Some(entry) = dir
                .next_entry()
                .await
                .map_err(|e| AiError::Skill(e.to_string()))?
            {
                let skill_md = entry.path().join("SKILL.md");
                if let Ok(text) = tokio::fs::read_to_string(&skill_md).await {
                    if let Some(front) = parse_frontmatter(&text.replace('\r', "")) {
                        manifests.push(front.into_manifest());
                    }
                }
            }
            manifests.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(manifests)
        })
    }

    fn load<'a>(
        &'a self,
        name: &'a str,
        _cx: &'a ToolCx<Ctx>,
    ) -> BoxFuture<'a, Result<SkillContent, AiError>> {
        Box::pin(async move {
            let dir = self.root.join(name);
            let text = tokio::fs::read_to_string(dir.join("SKILL.md"))
                .await
                .map_err(|e| AiError::Skill(format!("skill {name}: {e}")))?
                .replace('\r', "");
            let body = strip_frontmatter(&text).trim().to_owned();
            let references = list_references(&dir).await;
            Ok(SkillContent { body, references })
        })
    }
}

async fn list_references(dir: &Path) -> Vec<String> {
    let mut refs = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(dir.join("references")).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                refs.push(format!("references/{name}"));
            }
        }
    }
    refs.sort();
    refs
}

/// Parsed YAML frontmatter fields we care about.
#[derive(Default)]
struct Frontmatter {
    name: String,
    description: String,
    allowed_tools: Vec<String>,
}

impl Frontmatter {
    fn into_manifest(self) -> SkillManifest {
        SkillManifest {
            name: self.name,
            description: self.description,
            allowed_tools: self.allowed_tools,
        }
    }
}

/// Returns the body text after the `---`-fenced frontmatter (or the whole text).
fn strip_frontmatter(text: &str) -> &str {
    let trimmed = text.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else {
        return text;
    };
    after
        .find("\n---")
        .and_then(|end| {
            after[end + 4..]
                .find('\n')
                .map(|nl| &after[end + 4 + nl + 1..])
        })
        .unwrap_or(text)
}

/// Minimal YAML-frontmatter parser for the fields skills use: scalar `key:
/// value`, quoted values, folded/literal blocks (`>` / `|`), and a nested
/// `metadata:` map. Avoids a heavyweight YAML dependency.
fn parse_frontmatter(text: &str) -> Option<Frontmatter> {
    let trimmed = text.trim_start();
    let after = trimmed.strip_prefix("---\n")?;
    let end = after.find("\n---")?;
    let block = &after[..end];

    let mut front = Frontmatter::default();
    let lines: Vec<&str> = block.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.starts_with(char::is_whitespace) {
            continue; // blank or nested (handled by block collection below)
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let resolved = if value == ">" || value == "|" {
            let folded = value == ">";
            let mut collected = Vec::new();
            while i < lines.len() && lines[i].starts_with(char::is_whitespace) {
                collected.push(lines[i].trim());
                i += 1;
            }
            collected.join(if folded { " " } else { "\n" })
        } else {
            value.trim_matches(['"', '\'']).to_owned()
        };
        match key {
            "name" => front.name = resolved,
            "description" => front.description = resolved,
            "allowed-tools" => {
                front.allowed_tools = resolved.split_whitespace().map(ToOwned::to_owned).collect();
            }
            _ => {}
        }
    }
    if front.name.is_empty() {
        return None;
    }
    Some(front)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalar_and_folded_and_allowed_tools() {
        let text = "---\n\
name: my-skill\n\
description: >\n  A folded\n  description here\n\
allowed-tools: Read Write Bash(cargo:*)\n\
---\n\
# Body\nhello\n";
        let front = parse_frontmatter(text).expect("frontmatter");
        assert_eq!(front.name, "my-skill");
        assert_eq!(front.description, "A folded description here");
        assert_eq!(front.allowed_tools, vec!["Read", "Write", "Bash(cargo:*)"]);
        assert!(strip_frontmatter(text).contains("# Body"));
    }

    #[test]
    fn missing_name_is_rejected() {
        assert!(parse_frontmatter("---\ndescription: x\n---\nbody").is_none());
    }

    #[tokio::test]
    async fn fs_loader_lists_repo_skills() {
        // The repo ships `.agents/skills/*/SKILL.md`.
        let loader =
            FsSkillLoader::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.agents/skills"));
        let cx = ToolCx::new(());
        let manifests = SkillLoader::<()>::list(&loader, &cx).await.expect("list");
        assert!(
            manifests.iter().any(|m| m.name == "rust-best-practices"),
            "expected rust-best-practices skill, got {manifests:?}"
        );
        let content = SkillLoader::<()>::load(&loader, "rust-best-practices", &cx)
            .await
            .expect("load");
        assert!(content.body.contains("Rust"), "body: {}", content.body);
    }
}
