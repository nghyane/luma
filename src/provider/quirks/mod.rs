//! Vendor-specific middleware for LLM providers.
//!
//! Each quirk is a pure helper (see submodules) that augments a request
//! body, set of headers, or system prompt for a specific vendor behavior.
//! Whether a quirk applies at all is decided by the [`QuirkSet`] the
//! gateway layer computes from `(GatewayId, AuthKind)` and stores on the
//! runtime — protocol code queries the set rather than branching on
//! `auth_kind` or hardcoding gateway names.
//!
//! The set is a bitflag `u32` wrapper: hand-rolled to keep the dependency
//! allowlist short (no `bitflags` crate).

pub mod adaptive_thinking;
pub mod cache_breakpoint;
pub mod claude_identity;
pub mod oauth_system_rewrite;

/// Selection of enabled vendor quirks. Constructed by the binding layer
/// from `(GatewayId, AuthKind)`; stored on the runtime and consumed by
/// protocol code via [`QuirkSet::contains`].
///
/// Adding a quirk:
///   1. Add a module next to the existing ones (pure helpers, no I/O).
///   2. Add a `const` flag here at the next free bit.
///   3. Wire it in the binding layer's `quirks_for` and the protocol
///      module that consumes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuirkSet(u32);

impl QuirkSet {
    /// Anthropic prompt caching breakpoint on the last mutable content
    /// block of the most recent message.
    pub const CACHE_BREAKPOINT: Self = Self(1 << 0);

    /// Anthropic adaptive / enabled-with-budget thinking config injected
    /// into the request body.
    pub const ADAPTIVE_THINKING: Self = Self(1 << 1);

    /// Claude Code OAuth system-block rewrite (billing attribution +
    /// identity preamble + mcp_noop fallback tool).
    pub const OAUTH_SYSTEM_REWRITE: Self = Self(1 << 2);

    /// Claude Code `anthropic-beta` header for OAuth traffic.
    pub const ANTHROPIC_BETAS: Self = Self(1 << 3);

    /// Claude Code identity headers: `x-app`, `User-Agent`,
    /// `X-Claude-Code-Session-Id`, `x-client-request-id`.
    pub const CLAUDE_IDENTITY: Self = Self(1 << 4);

    /// Empty set — no quirks enabled.
    pub const EMPTY: Self = Self(0);

    /// Const-friendly OR for use in `const` contexts (e.g. `static`
    /// gateway tables). Run-time composition uses the `BitOr` impl.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every flag in `other` is enabled in `self`.
    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl std::ops::BitOr for QuirkSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for QuirkSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_contains_nothing_but_itself() {
        let qs = QuirkSet::EMPTY;
        assert!(qs.contains(QuirkSet::EMPTY));
        assert!(!qs.contains(QuirkSet::CACHE_BREAKPOINT));
    }

    #[test]
    fn bitor_composes_flags() {
        let qs = QuirkSet::CACHE_BREAKPOINT | QuirkSet::ADAPTIVE_THINKING;
        assert!(qs.contains(QuirkSet::CACHE_BREAKPOINT));
        assert!(qs.contains(QuirkSet::ADAPTIVE_THINKING));
        assert!(!qs.contains(QuirkSet::OAUTH_SYSTEM_REWRITE));
    }

    #[test]
    fn contains_checks_whole_subset() {
        let qs = QuirkSet::CACHE_BREAKPOINT | QuirkSet::ADAPTIVE_THINKING;
        let subset = QuirkSet::CACHE_BREAKPOINT | QuirkSet::ADAPTIVE_THINKING;
        assert!(qs.contains(subset));
        let wider = subset | QuirkSet::OAUTH_SYSTEM_REWRITE;
        assert!(!qs.contains(wider));
    }

    #[test]
    fn bitor_assign_accumulates() {
        let mut qs = QuirkSet::EMPTY;
        qs |= QuirkSet::CACHE_BREAKPOINT;
        qs |= QuirkSet::ADAPTIVE_THINKING;
        assert!(qs.contains(QuirkSet::CACHE_BREAKPOINT));
        assert!(qs.contains(QuirkSet::ADAPTIVE_THINKING));
    }
}
