# Tool Usage

- Use `exec_command` for shell operations and `apply_patch` for file edits. Use `rg` or `cat` with line ranges to read files via `exec_command`.
- Use `web_search`, `web_fetch`, and `gh_*` tools for external lookups. Use `gh_*` only for remote repositories, not for files already present locally.
- ALWAYS follow tool call schemas exactly. Never use placeholders.
- Never refer to tool names when speaking to the user.
