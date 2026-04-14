---
name: cross-platform
description: Guidelines for writing platform-specific code in Luma. Use when adding or modifying code that behaves differently on Unix vs Windows, including shell commands, file paths, process spawning, or platform-specific tests.
---

# Cross-Platform Code in Luma

## Compile-time selection

Prefer compile-time platform selection over runtime branching:

```rust
// Preferred
#[cfg(unix)]
fn shell_command() -> &'static str { "sh" }

#[cfg(windows)]
fn shell_command() -> &'static str { "cmd" }

// Avoid
fn shell_command() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}
```

Use `#[cfg(unix)]`, `#[cfg(windows)]`, or module-level `#[cfg(...)]` attributes.

## Module layout

Platform-specific code lives in separate files:

```
src/tool/shell/
  mod.rs        ← public API, platform-agnostic
  unix.rs       ← #[cfg(unix)]
  windows.rs    ← #[cfg(windows)]
```

The parent module exposes a single unified API. Callers never import platform modules directly.

## Tests

Guard platform-specific tests at compile time:

```rust
#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn unix_specific_behavior() { ... }

    #[cfg(windows)]
    #[test]
    fn windows_specific_behavior() { ... }
}
```

Do not duplicate test logic when expectations can be shared across platforms.
