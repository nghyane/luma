# Tool Usage

- Prefer dedicated file tools (`Read`, `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`) over `Bash` for file operations.
- NEVER use `sed` in Bash for file editing — it silently corrupts files. Use `Edit`, `MultiEdit`, or `Write` instead.
- Use `Read`, `Grep`, and `Glob` as the default path for local repository inspection.
- Do not use GitHub or web tools for files that already exist in the current workspace.
- When auditing current repo behavior, do not jump to prior art or remote examples before checking the local implementation.
- Use `WebSearch`, `WebFetch`, and `Gh*` tools for external lookups. Use `Gh*` only for remote repositories, not for files already present locally.
- ALWAYS follow tool call schemas exactly. Never use placeholders.
- Never refer to tool names when speaking to the user.
