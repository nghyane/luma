use crate::core::tool::Tool;
use crate::core::types::ToolSchema;
/// Tool registry — client tools (we execute) and server capabilities.
use std::collections::HashMap;

/// Stores registered tools and declared server capabilities.
pub struct Registry {
    tools: HashMap<String, Box<dyn Tool>>,
    /// Server capability names (e.g. "web_search").
    /// Provider maps these to its own schema format.
    server_capabilities: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            server_capabilities: Vec::new(),
        }
    }

    /// Register a client tool (we execute).
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Declare a server capability by name.
    /// Provider will map this to its native schema format.
    pub fn add_server_capability(&mut self, name: &str) {
        if !self.server_capabilities.contains(&name.to_owned()) {
            self.server_capabilities.push(name.to_owned());
        }
    }

    /// Server capability names.
    pub fn server_capabilities(&self) -> &[String] {
        &self.server_capabilities
    }

    /// Check if a server capability is declared.
    #[cfg(test)]
    pub fn has_capability(&self, name: &str) -> bool {
        self.server_capabilities.iter().any(|c| c == name)
    }

    /// Look up a client tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Client tool schemas.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool::Tool;
    use crate::core::types::ToolSchema;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    struct FakeTool;

    impl Tool for FakeTool {
        fn name(&self) -> &str {
            "Read"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "Read".into(),
                description: "read a file".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                streamable_arg: None,
            }
        }
        fn execute(
            &self,
            _args: serde_json::Value,
            _output_tx: mpsc::Sender<String>,
            _cancel: CancellationToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + '_>>
        {
            Box::pin(async { Ok("done".into()) })
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = Registry::new();
        assert!(reg.is_empty());
        reg.register(Box::new(FakeTool));
        assert_eq!(reg.len(), 1);
        assert!(reg.get("Read").is_some());
    }

    #[test]
    fn server_capabilities() {
        let mut reg = Registry::new();
        reg.add_server_capability("web_search");
        assert!(reg.has_capability("web_search"));
        assert!(!reg.has_capability("code_interpreter"));
        assert_eq!(reg.server_capabilities(), &["web_search"]);
    }

    #[test]
    fn no_duplicate_capabilities() {
        let mut reg = Registry::new();
        reg.add_server_capability("web_search");
        reg.add_server_capability("web_search");
        assert_eq!(reg.server_capabilities().len(), 1);
    }
}
