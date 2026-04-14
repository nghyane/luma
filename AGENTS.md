# Luma Rust — Agent Instructions

Prioritize correct, clear, and maintainable code. Every PR must pass `cargo fmt`, `cargo clippy -- -D warnings`, tests, and build.

## Architecture

- **Owner mutates.** Whoever owns the data holds `&mut`. Never bypass a facade to mutate a field directly.
- **One-way communication.** Use `&T` for read-only, return values for sync flow, `Event` enum over channels for async flow.
- **No shared mutable state.** No `static mut`. Avoid `Arc<Mutex<_>>`; use ownership or message passing instead.
- **Dependency flow is downward, never circular.** `app → agent → provider → tool`. No reverse dependencies.
- **Keep boundaries clear.** Data model, rendering, IO, and orchestration must not mix in the same place.

## Code

- **Public API must be self-explanatory.** Every `pub fn` needs a short doc comment describing its contract. Private fns only when logic is non-obvious.
- **Error handling.** Everything fallible returns `Result<T, E>`. No `.unwrap()` outside tests. Use `thiserror` for module errors, `anyhow` at app-level boundaries.
- **Add context at error boundaries.** Error messages must tell the reader what step failed and what to do next.
- **Naming.** Modules `snake_case`, types `PascalCase`, fns `snake_case` describing the action. Booleans use `is_*`, `has_*`, `should_*`.
- **No magic numbers in domain logic.** Extract to `const` when the value is not self-evident.
- **Keep code cohesive.** Structs: few fields, one responsibility. Functions: short, low nesting, one logical flow.
- **Types express invariants.** Use enums, newtypes, and pattern matching to encode valid states instead of loose booleans or comments.
- **Comments explain why, not what.** If a comment only restates the code, delete it and rename instead.

## Operations

- **Modules with logic must have tests.** Test behavior and regressions, not line counts. New public behavior, bugfixes, and important edge cases need at least one high-value test.
- **Tests live near the logic.** Prefer `#[cfg(test)] mod tests` in the same file.
- **No vague TODOs.** Use `todo!("specific description")`. Comments must have clear context.
- **No `unsafe`.** Only consider it with a clear proof that no safe equivalent exists.
- **Dependencies need justification.** Current allowlist: `tokio`, `reqwest`, `serde`, `serde_json`, `smallvec`, `tokio-util`, `thiserror`, `anyhow`, `crossterm`. New deps must be justified.
- **Clippy clean.** `cargo clippy -- -D warnings` must pass. No `#[allow(clippy::...)]` without a comment explaining the false positive.
- **Default formatting.** Use `rustfmt` defaults. No custom `rustfmt.toml`.

## Design defaults

- **No premature generics.** Only generalize when there are at least two concrete use cases, or the abstraction simplifies code now.
- **Owned at boundaries, borrowed on hot paths.** Public APIs take/return owned types; internal paths prefer borrows to avoid unnecessary clones.
- **Measure before optimizing.** Only add pre-allocation, `SmallVec`, caching, or complex optimizations when there is evidence of a bottleneck.
