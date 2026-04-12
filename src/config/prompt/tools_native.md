# Tool Usage

- Use dedicated tools (`Read`, `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`) over `Bash` for file operations. Each tool description says when to use it.
- Use `WebSearch`, `WebFetch`, and `Gh*` tools for external lookups and repo inspection.
- Maximize parallel tool calls for independent operations. Serialize only when one depends on another.
- ALWAYS follow tool call schemas exactly. Provide all required parameters. Never use placeholders.
- NEVER refer to tool names when speaking to the user. Say what you're doing in natural language.
