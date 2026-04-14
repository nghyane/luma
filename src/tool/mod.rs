pub mod apply_patch;
pub mod bash;
mod bash_safety;
pub mod diff;
pub mod edit;
pub mod gh_file;
pub mod gh_ls;
pub mod gh_search;
pub mod glob;
pub mod grep;
pub mod multi_edit;
pub mod read;
mod shell;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use crate::core::registry::Registry;

/// Tool flavor — chosen by workflow mode first, then constrained by provider support.
///
/// - `Native`: dedicated file tools (Read/Write/Edit/MultiEdit/Glob/Grep)
///   plus shell. Used by Anthropic and OpenAI models.
/// - `Patch`: `exec_command` + `apply_patch`. Used by Codex-style models
///   that are trained on the patch protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStyle {
    Native,
    Patch,
}

impl ToolStyle {
    /// Lower-level compatibility mapping from provider source to tool style.
    pub fn for_source(source: &str) -> Self {
        match source {
            "codex" => Self::Patch,
            _ => Self::Native,
        }
    }

    /// Default tool style for a given agent mode and provider source.
    pub fn for_mode(mode: crate::config::models::AgentMode, source: &str) -> Self {
        match mode {
            crate::config::models::AgentMode::Rush | crate::config::models::AgentMode::Smart => {
                Self::Native
            }
            crate::config::models::AgentMode::Deep => Self::for_source(source),
        }
    }
}

/// Build the tool registry for a given provider tool style. Web and GitHub
/// tools are shared across styles; only the file/edit surface differs.
pub fn build_registry(style: ToolStyle, search: Option<web_search::SearchBackend>) -> Registry {
    let mut reg = Registry::new();
    match style {
        ToolStyle::Native => {
            reg.register(Box::new(read::ReadTool));
            reg.register(Box::new(write::WriteTool));
            reg.register(Box::new(edit::EditTool));
            reg.register(Box::new(multi_edit::MultiEditTool));
            reg.register(Box::new(bash::BashTool::claude()));
            reg.register(Box::new(glob::GlobTool));
            reg.register(Box::new(grep::GrepTool));
        }
        ToolStyle::Patch => {
            reg.register(Box::new(read::ReadTool));
            reg.register(Box::new(glob::GlobTool));
            reg.register(Box::new(grep::GrepTool));
            reg.register(Box::new(bash::BashTool::codex()));
            reg.register(Box::new(apply_patch::ApplyPatchTool));
        }
    }
    reg.register(Box::new(gh_file::GhFileTool));
    reg.register(Box::new(gh_ls::GhLsTool));
    reg.register(Box::new(gh_search::GhSearchTool));
    reg.add_server_capability("web_search");
    if let Some(backend) = search {
        reg.register(Box::new(web_search::WebSearchTool::new(backend)));
    }
    reg.register(Box::new(web_fetch::WebFetchTool));
    reg
}
