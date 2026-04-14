//! Auth — account pool for multi-account, multi-provider OAuth.
//!
//! Every credential that luma holds lives in one place: `~/.config/luma/auth.json`,
//! a flat pool of accounts across providers (Anthropic + Codex/OpenAI). Callers
//! ask the pool for a credential for a given provider and the pool returns the
//! first healthy account — skipping any that are on rate-limit cooldown or that
//! need a fresh login. When a caller receives a 429 from the backend it reports
//! the cooldown back via `mark_rate_limited`, and the next `resolve` call routes
//! around that account automatically.
//!
//! First-run bootstrap imports whatever it can from the upstream CLIs
//! (`~/.claude/.credentials.json` / macOS keychain for Claude Code,
//! `~/.codex/auth.json` for Codex). After that the pool owns the token
//! lifecycle; refresh is done with our own `refresh_token` against the provider's
//! OAuth endpoint. If refresh fails (upstream CLI rotated the token, revoked,
//! etc.), we attempt one auto-recovery pass by re-reading the local source and
//! retrying; if that also fails, the account is flagged `needs_relogin` and
//! skipped until the user runs `/login` again.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

mod codex_identity;
mod pkce;
mod policy;
pub(crate) use codex_identity::{CODEX_ORIGINATOR, codex_user_agent, resolve_installation_id};
pub use pkce::login;

// --- provider-specific OAuth config ---

const CLAUDE_OAUTH_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Login scopes for Claude.ai subscribers (Max / Pro / Team). We do not
/// support the Console API-key-creation OAuth lane, so we only request the
/// Claude.ai scope set here.
const CLAUDE_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];
/// Refresh scopes — Claude.ai subscriber set only.
const CLAUDE_REFRESH_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

const OPENAI_OAUTH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Refresh the `access_token` this many seconds before its expiry.
const EXPIRY_GRACE_SECS: u64 = 300;

/// On-disk store format. Bumped when the schema breaks compatibility.
const POOL_STORE_VERSION: u32 = 2;

// =============================================================================
// public types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVendor {
    Anthropic,
    OpenAI,
    OpenCodeGo,
    Kiro,
}

impl AuthVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthVendor::Anthropic => "anthropic",
            AuthVendor::OpenAI => "openai",
            AuthVendor::OpenCodeGo => "opencode-go",
            AuthVendor::Kiro => "kiro",
        }
    }

    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "codex" => Some(Self::OpenAI),
            "opencode-go" => Some(Self::OpenCodeGo),
            "kiro" => Some(Self::Kiro),
            _ => None,
        }
    }
}

/// Resolved credential ready to hand to a provider client. Carries the
/// account `label` so callers can mark rate-limits / usage back onto the
/// correct pool entry after a request.
#[derive(Debug, Clone)]
pub struct Credential {
    pub token: String,
    pub is_oauth: bool,
    pub account_id: Option<String>,
    pub label: String,
    pub profile_arn: Option<String>,
    /// Stable key for the new AuthService. Populated when account_id or email
    /// is available; None for anonymous entries that haven't been migrated yet.
    pub account_key: Option<crate::auth::domain::AccountKey>,
}

#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub requests_remaining: Option<u64>,
    pub requests_limit: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub reset_at: Option<u64>,
    pub updated_at: u64,
}

// =============================================================================
// on-disk types
// =============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct PoolStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

impl Default for PoolStore {
    fn default() -> Self {
        Self {
            version: POOL_STORE_VERSION,
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AccountEntry {
    label: String,
    provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_arn: Option<String>,
    #[serde(default = "default_true")]
    is_oauth: bool,
    /// Unix seconds when the `access_token` expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<String>>,
    /// When set and `> now`, the account is rate-limited and skipped by `resolve`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cooldown_until: Option<u64>,
    #[serde(default, skip_serializing_if = "UsageRec::is_empty")]
    usage: UsageRec,
    #[serde(default)]
    needs_relogin: bool,
    #[serde(default)]
    disabled: bool,
}

fn default_true() -> bool {
    true
}

fn should_use_claude_ai_auth(scopes: Option<&[String]>) -> bool {
    scopes.unwrap_or(&[]).iter().any(|s| s == "user:inference")
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct UsageRec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requests_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requests_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reset_at: Option<u64>,
    #[serde(default)]
    updated_at: u64,
}

impl UsageRec {
    fn is_empty(&self) -> bool {
        self.requests_remaining.is_none()
            && self.requests_limit.is_none()
            && self.tokens_remaining.is_none()
            && self.tokens_limit.is_none()
            && self.reset_at.is_none()
            && self.updated_at == 0
    }
}

// =============================================================================
// pool state (in-memory, lazily loaded, persisted on mutation)
// =============================================================================

static POOL: OnceLock<Mutex<PoolStore>> = OnceLock::new();

fn pool() -> MutexGuard<'static, PoolStore> {
    POOL.get_or_init(|| Mutex::new(load_pool_from_disk()))
        .lock()
        .expect("pool mutex poisoned")
}

/// Run a closure against the mutable pool state and persist the result.
fn with_pool_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut PoolStore) -> R,
{
    let mut guard = pool();
    let out = f(&mut guard);
    save_pool_locked(&guard);
    out
}

// =============================================================================
// public API
// =============================================================================

/// Resolve a credential for `provider` from the pool. Bootstraps from the
/// local upstream CLIs on first run and transparently refreshes expired
/// tokens. If the current candidate's refresh fails, the account is flagged
/// and the next healthy candidate is tried in the same call.
pub async fn resolve(provider: AuthVendor) -> Result<Credential> {
    if matches!(provider, AuthVendor::Anthropic | AuthVendor::OpenAI | AuthVendor::Kiro | AuthVendor::OpenCodeGo) {
        return crate::auth::service::AuthService::new(
            crate::auth::repo::FileAuthRepository::with_default_path(),
        )
        .resolve_credential(provider.into())
        .await
        .map_err(anyhow::Error::from);
    }
    resolve_inner(provider, false).await
}

/// Force-refresh the cached credential for `provider`. Bypasses the
/// "still valid" cache check and runs through `try_refresh`. Use after a
/// 401 response when the local TTL says the token should be fine — the
/// server said this token is bad, so don't trust local state.
///
/// Returns an error immediately for credentials that have no refresh
/// token (raw API keys); there is nothing to refresh and re-reading the
/// same dead key would just loop.
pub async fn force_refresh(provider: AuthVendor) -> Result<Credential> {
    if matches!(provider, AuthVendor::Anthropic | AuthVendor::OpenAI | AuthVendor::Kiro | AuthVendor::OpenCodeGo) {
        let service = crate::auth::service::AuthService::new(
            crate::auth::repo::FileAuthRepository::with_default_path(),
        );
        let cred = service
            .resolve_credential(provider.into())
            .await
            .map_err(anyhow::Error::from)?;
        let key = cred
            .account_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("resolved credential missing account key"))?
            .clone();
        return service
            .refresh_credential(&key)
            .await
            .map_err(anyhow::Error::from);
    }
    let entry = pick_candidate(provider)
        .ok_or_else(|| anyhow::anyhow!("no {} account in pool", provider.as_str()))?;
    if !entry.is_oauth || entry.refresh_token.is_none() {
        anyhow::bail!(
            "{} credential cannot be refreshed (api key) — re-add the key with `luma login`",
            provider.as_str()
        );
    }
    resolve_inner(provider, true).await
}

/// Mark an account as rate-limited. The pool will skip it until
/// `retry_after_secs` have elapsed. Called from the provider layer when a
/// 429 response is received.
pub fn mark_rate_limited(label: &str, retry_after_secs: u64) {
    let until = now_unix().saturating_add(retry_after_secs.max(1));
    with_pool_mut(|p| {
        if let Some(a) = p.accounts.iter_mut().find(|a| a.label == label) {
            a.cooldown_until = Some(until);
        }
    });
}

/// Mark an account as requiring a fresh login.
pub fn mark_needs_relogin(label: &str) {
    with_pool_mut(|p| set_needs_relogin(p, label));
}

/// Upsert a raw API key into the pool.
///
/// Used by `luma login` for gateways that do not speak OAuth (today:
/// OpenCode Go). The entry is stored with `is_oauth: false`, no refresh
/// token, and no expiry — `resolve` fast-paths this shape straight into
/// a `Credential`.
///
/// The label is derived from `vendor` + a short key fingerprint so
/// multiple keys on the same vendor stay distinguishable in the
/// `/accounts` listing.
pub fn upsert_api_key(vendor: AuthVendor, token: &str) -> String {
    let label = derive_api_key_label(vendor, token);
    let entry = AccountEntry {
        label: label.clone(),
        provider: vendor.as_str().to_owned(),
        email: None,
        access_token: token.to_owned(),
        refresh_token: None,
        account_id: None,
        profile_arn: None,
        is_oauth: false,
        expires_at: None,
        scopes: None,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    };
    with_pool_mut(|p| upsert_by_label(p, entry));
    label
}

fn derive_api_key_label(vendor: AuthVendor, token: &str) -> String {
    // Short suffix is just for human disambiguation in /accounts — not
    // security-critical. Strip any leading `sk-` to keep the suffix
    // meaningful for keys that all share a common prefix.
    let core = token.strip_prefix("sk-").unwrap_or(token);
    let suffix: String = core.chars().take(6).collect();
    format!("{}:key:{}", vendor.as_str(), suffix)
}

/// Record the latest usage snapshot for an account, for display on the
/// `/accounts` screen. Called from the provider layer after a successful
/// response where rate-limit headers were parsed.
pub fn record_usage(label: &str, usage: UsageSnapshot) {
    with_pool_mut(|p| {
        if let Some(a) = p.accounts.iter_mut().find(|a| a.label == label) {
            a.usage = UsageRec {
                requests_remaining: usage.requests_remaining,
                requests_limit: usage.requests_limit,
                tokens_remaining: usage.tokens_remaining,
                tokens_limit: usage.tokens_limit,
                reset_at: usage.reset_at,
                updated_at: if usage.updated_at == 0 {
                    now_unix()
                } else {
                    usage.updated_at
                },
            };
        }
    });
}

// =============================================================================
// resolve flow
// =============================================================================

async fn resolve_inner(provider: AuthVendor, force: bool) -> Result<Credential> {
    // Bootstrap the pool if it has no account for this provider.
    ensure_bootstrapped(provider).await;
    crate::dbg_log!(
        "auth resolve start provider={} force={}",
        provider.as_str(),
        force
    );

    // Walk candidates until one works. Each iteration may mark an account
    // as unhealthy and try the next.
    loop {
        let Some(entry) = pick_candidate(provider) else {
            crate::dbg_log!("auth resolve no candidate provider={}", provider.as_str());
            let have_any = pool()
                .accounts
                .iter()
                .any(|a| a.provider == provider.as_str());
            if have_any {
                anyhow::bail!(
                    "all {} accounts are unavailable (rate-limited or need re-login). \
                     Run `/login` or wait for cooldown.",
                    provider.as_str()
                );
            }
            anyhow::bail!(
                "no {} account in pool. Run `/login` to add one.",
                provider.as_str()
            );
        };

        crate::dbg_log!(
            "auth resolve candidate provider={} label={} oauth={} expires_at={:?} has_refresh={} force={}",
            provider.as_str(),
            entry.label,
            entry.is_oauth,
            entry.expires_at,
            entry.refresh_token.is_some(),
            force
        );

        // Fast path: cached token still valid and not forced.
        // Raw API keys (non-oauth) also short-circuit here — no expiry,
        // no refresh, `force` has no effect.
        if !entry.is_oauth || (!force && !is_expired(entry.expires_at)) {
            crate::dbg_log!(
                "auth resolve fast-path provider={} label={}",
                provider.as_str(),
                entry.label
            );
            return Ok(credential_from(&entry));
        }

        if entry.refresh_token.is_none() {
            // Nothing to refresh with. Try one last auto-recovery from the
            // local source for this account's provider, otherwise retire it.
            crate::dbg_log!(
                "auth resolve missing refresh_token provider={} label={} attempting auto-recover",
                provider.as_str(),
                entry.label
            );
            if attempt_auto_recover(provider, &entry.label).await {
                continue;
            }
            crate::dbg_log!(
                "auth resolve mark needs_relogin provider={} label={} reason=no_refresh_token",
                provider.as_str(),
                entry.label
            );
            with_pool_mut(|p| set_needs_relogin(p, &entry.label));
            continue;
        }

        match try_refresh(&entry, provider).await {
            Ok(refreshed) => {
                crate::dbg_log!(
                    "auth resolve refresh ok provider={} label={} new_expires_at={:?}",
                    provider.as_str(),
                    refreshed.label,
                    refreshed.expires_at
                );
                with_pool_mut(|p| upsert_by_label(p, refreshed.clone()));
                return Ok(credential_from(&refreshed));
            }
            Err(err) => {
                crate::dbg_log!(
                    "auth: refresh failed for {} ({}): {}",
                    entry.label,
                    provider.as_str(),
                    err
                );
                crate::dbg_log!(
                    "auth resolve attempting auto-recover provider={} label={}",
                    provider.as_str(),
                    entry.label
                );
                if attempt_auto_recover(provider, &entry.label).await {
                    continue;
                }
                crate::dbg_log!(
                    "auth resolve mark needs_relogin provider={} label={} reason=refresh_failed",
                    provider.as_str(),
                    entry.label
                );
                with_pool_mut(|p| set_needs_relogin(p, &entry.label));
                continue;
            }
        }
    }
}

/// Pick the first healthy account for `provider`. "Healthy" means not
/// flagged `needs_relogin` and not currently on cooldown.
fn pick_candidate(provider: AuthVendor) -> Option<AccountEntry> {
    let now = now_unix();
    let pool = pool();
    pool.accounts
        .iter()
        .filter(|a| {
            a.provider == provider.as_str()
                && !a.needs_relogin
                && !a.disabled
                && a.cooldown_until.is_none_or(|t| t <= now)
        })
        .max_by_key(|a| candidate_rank(a))
        .cloned()
}

fn candidate_rank(a: &AccountEntry) -> (u8, u8, u8, u8, u64) {
    (
        u8::from(a.is_oauth),
        u8::from(a.email.is_some()),
        u8::from(a.account_id.is_some()),
        u8::from(a.refresh_token.is_some()),
        a.expires_at.unwrap_or(0),
    )
}

fn credential_from(e: &AccountEntry) -> Credential {
    use crate::auth::domain::AccountKey;
    let vendor = AuthVendor::from_str(&e.provider)
        .map(crate::auth::domain::AuthVendor::from);
    let account_key = vendor.and_then(|v| {
        if let Some(id) = e.account_id.as_deref().filter(|s| !s.is_empty()) {
            Some(AccountKey::account_id(v, id))
        } else if let Some(em) = e.email.as_deref().filter(|s| !s.is_empty()) {
            Some(AccountKey::email(v, em))
        } else {
            Some(AccountKey::anonymous(v, e.label.clone()))
        }
    });
    Credential {
        token: e.access_token.clone(),
        is_oauth: e.is_oauth,
        account_id: e.account_id.clone(),
        label: e.label.clone(),
        profile_arn: e.profile_arn.clone(),
        account_key,
    }
}

fn set_needs_relogin(pool: &mut PoolStore, label: &str) {
    if let Some(a) = pool.accounts.iter_mut().find(|a| a.label == label) {
        a.needs_relogin = true;
    }
}

fn identity_key(entry: &AccountEntry) -> Option<String> {
    if let Some(account_id) = entry.account_id.as_ref().filter(|s| !s.is_empty()) {
        return Some(format!("{}:account:{}", entry.provider, account_id));
    }
    if let Some(email) = entry.email.as_ref().filter(|s| !s.is_empty()) {
        return Some(format!(
            "{}:email:{}",
            entry.provider,
            email.to_ascii_lowercase()
        ));
    }
    None
}

fn merge_account(existing: &AccountEntry, mut incoming: AccountEntry) -> AccountEntry {
    if incoming.email.is_none() {
        incoming.email = existing.email.clone();
    }
    if incoming.account_id.is_none() {
        incoming.account_id = existing.account_id.clone();
    }
    if incoming.cooldown_until.is_none() {
        incoming.cooldown_until = existing.cooldown_until;
    }
    if incoming.usage.is_empty() {
        incoming.usage = existing.usage.clone();
    }
    incoming.needs_relogin = false;
    incoming.disabled = existing.disabled;

    // Provider string in pool entries is always one of the known providers
    // (set by parse_*_json or PKCE login). A bad value here is an invariant
    // violation, not a fallback case.
    let provider =
        AuthVendor::from_str(&incoming.provider).expect("pool entry has invalid provider string");
    let current_label = derive_label(provider, incoming.email.as_deref());
    if !current_label.ends_with("-1") {
        incoming.label = current_label;
    } else if !existing.label.ends_with("-1") {
        incoming.label = existing.label.clone();
    }
    incoming
}

/// Insert or replace an entry by label, preserving cooldown/usage from the
/// existing entry unless the new one carries fresh values.
fn upsert_by_label(pool: &mut PoolStore, mut entry: AccountEntry) {
    if let Some(key) = identity_key(&entry)
        && let Some(existing_idx) = pool
            .accounts
            .iter()
            .position(|a| identity_key(a).as_deref() == Some(key.as_str()))
    {
        let merged = merge_account(&pool.accounts[existing_idx], entry);
        pool.accounts[existing_idx] = merged;
        return;
    }

    if let Some(existing) = pool.accounts.iter_mut().find(|a| a.label == entry.label) {
        // Preserve usage + cooldown across refreshes (refresh response
        // doesn't carry rate-limit info).
        if entry.usage.is_empty() {
            entry.usage = existing.usage.clone();
        }
        if entry.cooldown_until.is_none() {
            entry.cooldown_until = existing.cooldown_until;
        }
        if entry.email.is_none() {
            entry.email = existing.email.clone();
        }
        entry.needs_relogin = false;
        *existing = entry;
    } else {
        pool.accounts.push(entry);
    }
}

// =============================================================================
// bootstrap + auto-recovery
// =============================================================================

/// If the pool has no account for this provider, try to seed one from the
/// upstream CLI's local credential store.
async fn ensure_bootstrapped(provider: AuthVendor) {
    if let Some(mut seed) = load_local(provider) {
        // For Kiro, fetch real email via Cognito + SigV4
        // Also replace "via google/github" placeholder with real email
        let needs_real_email = provider == AuthVendor::Kiro
            && seed
                .email
                .as_deref()
                .map(|e| e.starts_with("via ") || e.is_empty())
                .unwrap_or(true);
        if needs_real_email {
            let profile_arn = seed.profile_arn.clone().unwrap_or_default();
            if let Some(email) = fetch_kiro_email_via_api(&seed.access_token, &profile_arn).await {
                seed.label = derive_label(provider, Some(&email));
                seed.email = Some(email);
            }
        }
        with_pool_mut(|p| upsert_by_label(p, seed));
    }
}

/// Try to recover a failed refresh by re-reading the local source. If local
/// has a materially different entry (different `refresh_token` or still-valid
/// `access_token`), save it into the pool and return `true` so the caller
/// retries. Returns `false` if no recovery is possible.
async fn attempt_auto_recover(provider: AuthVendor, label: &str) -> bool {
    let Some(fresh) = load_local(provider) else {
        crate::dbg_log!(
            "auth auto-recover no local credential provider={} label={}",
            provider.as_str(),
            label
        );
        return false;
    };
    let current = pool().accounts.iter().find(|a| a.label == label).cloned();

    // If the local source returned a different label (e.g. keychain now has
    // an email-derived label "nghia@gmail" but the failing entry is
    // "anthropic-1"), remove the stale anonymous entry and upsert the fresh
    // one — the fresh token is what matters, not preserving the old label.
    if fresh.label != label {
        crate::dbg_log!(
            "auth auto-recover replacing label provider={} old_label={} new_label={}",
            provider.as_str(),
            label,
            fresh.label
        );
        with_pool_mut(|p| {
            p.accounts.retain(|a| a.label != label);
            upsert_by_label(p, fresh);
        });
        return true;
    }

    // Same label: only recover if the token actually changed.
    if let Some(cur) = current.as_ref() {
        let same_refresh = cur.refresh_token == fresh.refresh_token && cur.refresh_token.is_some();
        let fresh_still_valid = !is_expired(fresh.expires_at);
        if same_refresh && !fresh_still_valid {
            crate::dbg_log!(
                "auth auto-recover rejected provider={} label={} same_refresh=true fresh_still_valid=false",
                provider.as_str(),
                label
            );
            return false;
        }
    }

    crate::dbg_log!(
        "auth auto-recover accepted provider={} label={} fresh_label={}",
        provider.as_str(),
        label,
        fresh.label
    );
    with_pool_mut(|p| upsert_by_label(p, fresh));
    true
}

// =============================================================================
// local source readers (first-run import + auto-recovery)
// =============================================================================

fn load_local(provider: AuthVendor) -> Option<AccountEntry> {
    match provider {
        AuthVendor::Anthropic => load_claude_local(),
        AuthVendor::OpenAI => load_codex_local(),
        // OpenCode Go has no upstream CLI to bootstrap from; keys are
        // pasted interactively via `luma login`.
        AuthVendor::OpenCodeGo => None,
        AuthVendor::Kiro => load_kiro_local(),
    }
}

fn load_claude_local() -> Option<AccountEntry> {
    // Read the OAuth tokens from keychain (macOS) or the credentials file,
    // then enrich the entry with the user identity (email + accountUuid)
    // that Claude Code stores separately in ~/.claude.json. The access_token
    // for Claude is an opaque bearer (not a JWT), so JWT-claim extraction
    // doesn't work — we must read the identity from the global config.
    let mut entry = {
        #[cfg(target_os = "macos")]
        {
            load_claude_keychain().or_else(load_claude_credentials_file)
        }
        #[cfg(not(target_os = "macos"))]
        {
            load_claude_credentials_file()
        }
    }?;
    if let Some(profile) = load_claude_profile() {
        if entry.email.is_none() {
            entry.email = profile.email;
        }
        if entry.account_id.is_none() {
            entry.account_id = profile.account_uuid;
        }
        // Regenerate label now that we have identity.
        entry.label = derive_label(AuthVendor::Anthropic, entry.email.as_deref());
    }
    Some(entry)
}

fn load_claude_credentials_file() -> Option<AccountEntry> {
    let cred_file = home_dir().join(".claude").join(".credentials.json");
    let raw = fs::read_to_string(&cred_file).ok()?;
    parse_claude_json(&raw)
}

/// Identity profile pulled from Claude Code's ~/.claude.json.
#[derive(Debug, Default)]
struct ClaudeProfile {
    email: Option<String>,
    account_uuid: Option<String>,
}

/// Read the `oauthAccount` block from `~/.claude.json`, the Claude Code
/// global config. This is where Claude Code stores the current signed-in
/// user's email + account UUID (the opaque bearer token in the keychain
/// doesn't carry those claims).
fn load_claude_profile() -> Option<ClaudeProfile> {
    let path = home_dir().join(".claude.json");
    let raw = fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let oa = v.get("oauthAccount")?;
    Some(ClaudeProfile {
        email: oa
            .get("emailAddress")
            .or_else(|| oa.get("email"))
            .and_then(|v| v.as_str())
            .map(std::borrow::ToOwned::to_owned),
        account_uuid: oa
            .get("accountUuid")
            .or_else(|| oa.get("account_uuid"))
            .and_then(|v| v.as_str())
            .map(std::borrow::ToOwned::to_owned),
    })
}

#[cfg(target_os = "macos")]
fn load_claude_keychain() -> Option<AccountEntry> {
    use std::process::Command;
    for svc in list_keychain_services() {
        let output = Command::new("security")
            .args(["find-generic-password", "-s", &svc, "-w"])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if let Some(entry) = parse_claude_json(&raw) {
            return Some(entry);
        }
    }
    None
}

#[cfg(target_os = "macos")]
/// Return the list of Claude Code keychain service names to probe.
///
/// Uses `security find-generic-password` to query each known service
/// instead of `dump-keychain` (which dumps everything and produces
/// hundreds of stale matches). We probe the primary service first, then
/// one secondary slot. Claude Code only ever uses these two names in
/// practice.
fn list_keychain_services() -> Vec<String> {
    use std::process::Command;

    let candidates = ["Claude Code-credentials".to_owned()];

    // Also discover secondary accounts (Claude Code uses hex-suffixed names
    // for additional accounts). We use find-generic-password with -a "" to
    // list all accounts under the service — but since that's fragile, we
    // instead try the primary and look for hex-suffixed variants via a
    // targeted find rather than a full dump.
    let mut found: Vec<String> = Vec::new();
    for svc in &candidates {
        let ok = Command::new("security")
            .args(["find-generic-password", "-s", svc, "-w"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            found.push(svc.clone());
        }
    }

    // Secondary accounts: Claude Code appends a hex suffix like
    // "Claude Code-credentials-abc123ef". Use find-generic-password -D
    // to enumerate — the least-invasive approach on macOS.
    let secondary_output = Command::new("security")
        .args(["find-generic-password", "-D", "application password", "-g"])
        .output();
    if let Ok(out) = secondary_output {
        let text = String::from_utf8_lossy(&out.stderr).to_string()
            + &String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(pos) = line.find("\"svce\"")
                && let Some(start) = line[pos..].find('"').and_then(|i| line[pos..].get(i + 1..))
                && let Some(end) = start.find('"')
            {
                let svc = &start[..end];
                if svc.starts_with("Claude Code-credentials-") && !found.contains(&svc.to_owned()) {
                    found.push(svc.to_owned());
                }
            }
        }
    }

    if found.is_empty() {
        found.push("Claude Code-credentials".into());
    }
    found
}

fn parse_claude_json(raw: &str) -> Option<AccountEntry> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let oauth = v.get("claudeAiOauth").unwrap_or(&v);
    let access_token = oauth.get("accessToken")?.as_str()?.to_owned();
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned);
    let scopes = oauth.get("scopes").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|s| s.as_str().map(std::borrow::ToOwned::to_owned))
            .collect()
    });
    // `expiresAt` can be a number (ms) or a string. Normalize to Unix seconds.
    let expires_at = oauth.get("expiresAt").and_then(parse_expires_field);
    // Claude access_tokens are opaque bearers, so email + accountUuid are
    // normally loaded from `~/.claude.json` in `load_claude_local`. Try a
    // JWT extract here as a best-effort fallback (e.g. future token format
    // changes, or test fixtures that use real JWTs).
    let email = extract_email_from_jwt(&access_token);
    let mut entry = AccountEntry {
        label: String::new(),
        provider: "anthropic".into(),
        email,
        access_token,
        refresh_token,
        account_id: None,
        profile_arn: None,
        is_oauth: true,
        expires_at,
        scopes,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    };
    entry.label = derive_label(AuthVendor::Anthropic, entry.email.as_deref());
    Some(entry)
}

fn load_codex_local() -> Option<AccountEntry> {
    #[cfg(target_os = "macos")]
    if let Some(entry) = load_codex_keychain() {
        return Some(entry);
    }

    let auth_file = home_dir().join(".codex").join("auth.json");
    let raw = fs::read_to_string(&auth_file).ok()?;
    parse_codex_json(&raw)
}

#[cfg(target_os = "macos")]
fn load_codex_keychain() -> Option<AccountEntry> {
    let codex_home = home_dir().join(".codex");
    let store_key = compute_codex_store_key(&codex_home)?;
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "codex-auth",
            "-a",
            &store_key,
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    parse_codex_json(&raw)
}

#[cfg(target_os = "macos")]
fn compute_codex_store_key(codex_home: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = hex.get(..16).unwrap_or(&hex);
    Some(format!("cli|{truncated}"))
}

fn parse_codex_json(raw: &str) -> Option<AccountEntry> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let tokens = v.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?.to_owned();
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned);

    // access_token is a JWT; extract exp and account id directly so we don't
    // depend on `last_refresh` in the file (which is just an upstream hint).
    let access_claims = decode_jwt_payload(&access_token);
    let id_claims = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .and_then(decode_jwt_payload);
    let account_id = id_claims
        .as_ref()
        .and_then(extract_account_id_from_claims)
        .or_else(|| {
            access_claims
                .as_ref()
                .and_then(extract_account_id_from_claims)
        });
    let expires_at = access_claims
        .as_ref()
        .and_then(|c| c.get("exp").and_then(serde_json::Value::as_u64));
    let email = id_claims
        .as_ref()
        .and_then(|c| {
            c.get("email")
                .and_then(|v| v.as_str())
                .map(std::borrow::ToOwned::to_owned)
        })
        .or_else(|| {
            access_claims.as_ref().and_then(|c| {
                c.get("email")
                    .and_then(|v| v.as_str())
                    .map(std::borrow::ToOwned::to_owned)
            })
        });

    let label = derive_label(AuthVendor::OpenAI, email.as_deref());
    Some(AccountEntry {
        label,
        provider: "openai".into(),
        email,
        access_token,
        refresh_token,
        account_id,
        profile_arn: None,
        is_oauth: true,
        expires_at,
        scopes: None,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    })
}

fn load_kiro_local() -> Option<AccountEntry> {
    #[cfg(target_os = "macos")]
    {
        load_kiro_keychain()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn load_kiro_keychain() -> Option<AccountEntry> {
    use std::process::Command;
    let output = Command::new("security")
        .args(["find-generic-password", "-s", "kirocli:social:token", "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let access_token = v.get("access_token")?.as_str()?.to_owned();
    let refresh_token = v
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let profile_arn = v
        .get("profile_arn")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let expires_at = v.get("expires_at").and_then(|v| v.as_str()).and_then(|s| {
        // ISO 8601 → unix timestamp via parse_expires_field
        parse_expires_field(&serde_json::Value::String(s.to_owned()))
    });
    let provider_name = v
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("google");
    let email_hint = Some(format!("via {provider_name}"));
    let label = derive_label(AuthVendor::Kiro, email_hint.as_deref());
    Some(AccountEntry {
        label,
        provider: AuthVendor::Kiro.as_str().to_owned(),
        email: email_hint,
        access_token,
        refresh_token,
        account_id: None,
        profile_arn,
        is_oauth: true,
        expires_at,
        scopes: None,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    })
}

// =============================================================================
// refresh
// =============================================================================

async fn try_refresh(
    entry: &AccountEntry,
    provider: AuthVendor,
) -> std::result::Result<AccountEntry, String> {
    crate::dbg_log!(
        "auth refresh start provider={} label={} expires_at={:?}",
        provider.as_str(),
        entry.label,
        entry.expires_at
    );
    let refresh_token = entry
        .refresh_token
        .as_ref()
        .ok_or_else(|| "no refresh_token on entry".to_owned())?;
    let client = reqwest::Client::new();

    let refresh = provider.build_refresh_request(refresh_token, entry.scopes.as_deref());

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        client
            .post(refresh.url)
            .header("Content-Type", refresh.content_type)
            .header("Accept", "application/json")
            .body(refresh.body)
            .send(),
    )
    .await
    .map_err(|_| "refresh timeout".to_owned())?
    .map_err(|e| format!("network error: {e}"))?;

    let status = res.status();
    let text = res.text().await.map_err(|e| format!("read body: {e}"))?;
    crate::dbg_log!(
        "auth refresh response provider={} label={} status={} body={}",
        provider.as_str(),
        entry.label,
        status,
        text.chars().take(200).collect::<String>()
    );
    if !status.is_success() {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("HTTP {status}: {snippet}"));
    }

    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad json response: {e}"))?;
    let new_access = json
        .get("access_token")
        .or_else(|| json.get("accessToken"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing access_token in response".to_owned())?
        .to_owned();
    let new_refresh = json
        .get("refresh_token")
        .or_else(|| json.get("refreshToken"))
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned)
        .or_else(|| entry.refresh_token.clone());
    let expires_at = json
        .get("expires_in")
        .or_else(|| json.get("expiresIn"))
        .and_then(serde_json::Value::as_u64)
        .map(|secs| now_unix().saturating_add(secs))
        .or_else(|| {
            // Some providers omit expires_in; fall back to JWT `exp` claim.
            decode_jwt_payload(&new_access)
                .as_ref()
                .and_then(|c| c.get("exp").and_then(serde_json::Value::as_u64))
        });
    let scopes = json
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split_whitespace()
                .map(std::borrow::ToOwned::to_owned)
                .collect()
        })
        .or_else(|| entry.scopes.clone());
    // Kiro returns profileArn in refresh response
    let profile_arn = json
        .get("profileArn")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .or_else(|| entry.profile_arn.clone());

    Ok(AccountEntry {
        label: entry.label.clone(),
        provider: provider.as_str().to_owned(),
        email: entry.email.clone(),
        access_token: new_access,
        refresh_token: new_refresh,
        account_id: entry.account_id.clone(),
        profile_arn,
        is_oauth: true,
        expires_at,
        scopes,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    })
}

// =============================================================================
// on-disk load/save with migration
// =============================================================================

fn pool_path() -> PathBuf {
    home_dir().join(".config").join("luma").join("auth.json")
}

fn home_dir() -> PathBuf {
    super::home_dir()
}

fn load_pool_from_disk() -> PoolStore {
    let Ok(raw) = fs::read_to_string(pool_path()) else {
        return PoolStore::default();
    };
    // Try new format first.
    if let Ok(mut pool) = serde_json::from_str::<PoolStore>(&raw)
        && (pool.version >= 2 || !pool.accounts.is_empty())
    {
        dedup_accounts(&mut pool.accounts);
        // Drop entries that are expired with no refresh_token — they can
        // never be recovered and would only cause noisy refresh failures.
        pool.accounts
            .retain(|a| !is_expired(a.expires_at) || a.refresh_token.is_some());
        // Persist the deduped state so next load is already clean.
        save_pool_locked(&pool);
        return pool;
    }
    // Migrate old format (`ManagedStore { credentials: [...] }`).
    if let Ok(legacy) = serde_json::from_str::<LegacyStore>(&raw) {
        let mut accounts: Vec<AccountEntry> = legacy
            .credentials
            .into_iter()
            .filter_map(legacy_to_account)
            .collect();
        dedup_accounts(&mut accounts);
        return PoolStore {
            version: POOL_STORE_VERSION,
            accounts,
        };
    }
    PoolStore::default()
}

/// Deduplicate accounts by `access_token`, keeping the entry with the most
/// data (email preferred over blank). Called on load so stale keychain
/// re-imports don't accumulate hundreds of identical entries.
fn dedup_accounts(accounts: &mut Vec<AccountEntry>) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut keep = vec![true; accounts.len()];
    for (i, a) in accounts.iter().enumerate() {
        let key = format!(
            "{}:{}",
            a.provider,
            &a.access_token[..a.access_token.len().min(40)]
        );
        match seen.entry(key) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(i);
            }
            std::collections::hash_map::Entry::Occupied(e) => {
                let prev = *e.get();
                // Keep whichever has email; if tie, keep first.
                if accounts[prev].email.is_none() && accounts[i].email.is_some() {
                    keep[prev] = false;
                    *e.into_mut() = i;
                } else {
                    keep[i] = false;
                }
            }
        }
    }
    let mut iter = keep.iter();
    accounts.retain(|_| *iter.next().unwrap_or(&true));

    // Drop anonymous provider placeholders once we have a richer account for
    // the same provider. This prevents legacy labels like `anthropic-1` from
    // winning candidate selection over accounts that carry real identity.
    let provider_has_identity: std::collections::HashSet<String> = accounts
        .iter()
        .filter(|a| a.email.is_some() || a.account_id.is_some())
        .map(|a| a.provider.clone())
        .collect();
    accounts.retain(|a| {
        let anonymous_legacy = a.email.is_none()
            && a.account_id.is_none()
            && a.label
                .strip_prefix(a.provider.as_str())
                .is_some_and(|rest| rest.starts_with('-'));
        !(anonymous_legacy && provider_has_identity.contains(&a.provider))
    });
}

fn save_pool_locked(pool: &PoolStore) {
    let path = pool_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut out = PoolStore {
        version: POOL_STORE_VERSION,
        accounts: pool.accounts.clone(),
    };
    // Stable ordering for deterministic diffs.
    out.accounts.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.label.cmp(&b.label))
    });
    if let Ok(json) = serde_json::to_string_pretty(&out) {
        let _ = fs::write(&path, json);
    }
}

// --- legacy store migration ---

#[derive(Debug, Deserialize)]
struct LegacyStore {
    #[serde(default)]
    credentials: Vec<LegacyEntry>,
}

#[derive(Debug, Deserialize)]
struct LegacyEntry {
    provider: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default = "default_true")]
    is_oauth: bool,
    #[serde(default)]
    expires_at: Option<serde_json::Value>,
}

fn legacy_to_account(le: LegacyEntry) -> Option<AccountEntry> {
    let provider = AuthVendor::from_str(&le.provider)?;
    let email = extract_email_from_jwt(&le.access_token);
    let label = derive_label(provider, email.as_deref());
    let expires_at = le.expires_at.as_ref().and_then(parse_expires_field);
    Some(AccountEntry {
        label,
        provider: provider.as_str().to_owned(),
        email,
        access_token: le.access_token,
        refresh_token: le.refresh_token,
        account_id: le.account_id,
        profile_arn: None,
        is_oauth: le.is_oauth,
        expires_at,
        scopes: None,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    })
}

// =============================================================================
// label derivation
// =============================================================================

/// Auto-derive a short account label from the email address. Falls back to
/// `{provider}-{N}` when no email is available.
fn derive_label(provider: AuthVendor, email: Option<&str>) -> String {
    if let Some(email) = email
        && let Some((local, domain)) = email.split_once('@')
    {
        let short_domain = domain.split('.').next().unwrap_or(domain);
        return format!("{local}@{short_domain}");
    }
    // Fallback: find next unused index for this provider.
    let pool = POOL.get();
    let existing = pool
        .and_then(|m| m.lock().ok().map(|g| g.accounts.clone()))
        .unwrap_or_default();
    let taken: std::collections::HashSet<String> =
        existing.iter().map(|a| a.label.clone()).collect();
    let prefix = provider.as_str();
    for i in 1..=1000 {
        let candidate = format!("{prefix}-{i}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("{prefix}-unknown")
}

fn extract_email_from_jwt(token: &str) -> Option<String> {
    let claims = decode_jwt_payload(token)?;
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned)
}

// =============================================================================
// small helpers
// =============================================================================

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `expires_at` on disk / in upstream files can be a number (ms or secs) or
/// a string. Normalize to Unix seconds.
fn parse_expires_field(v: &serde_json::Value) -> Option<u64> {
    let raw = if let Some(n) = v.as_u64() {
        n
    } else if let Some(s) = v.as_str() {
        s.parse::<u64>().ok()?
    } else {
        return None;
    };
    Some(if raw > 4_102_444_800 { raw / 1000 } else { raw })
}

fn is_expired(expires_at: Option<u64>) -> bool {
    let Some(ts) = expires_at else {
        return false;
    };
    now_unix() >= ts.saturating_sub(EXPIRY_GRACE_SECS)
}

fn extract_account_id_from_claims(payload: &serde_json::Value) -> Option<String> {
    let auth = payload.get("https://api.openai.com/auth")?;
    auth.get("chatgpt_account_id")
        .or_else(|| auth.get("account_id"))
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned)
}

/// Decode the payload of an unverified JWT. Claims are used only as hints
/// (expiration, account id, email) — the token itself is validated by the
/// remote API on every request.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let padded = match parts[1].len() % 4 {
        2 => format!("{}==", parts[1]),
        3 => format!("{}=", parts[1]),
        _ => parts[1].to_owned(),
    };
    let decoded = padded.replace('-', "+").replace('_', "/");
    let bytes = base64_decode(&decoded)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            let val = TABLE.iter().position(|&c| c == b)? as u32;
            n |= val << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// =============================================================================
// background refresher
// =============================================================================

// =============================================================================
// tests
// =============================================================================

pub(super) async fn fetch_kiro_email_via_api(
    access_token: &str,
    profile_arn: &str,
) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let body = serde_json::json!({"profileArn": profile_arn, "isEmailRequired": true}).to_string();
    let resp = client
        .post("https://codewhisperer.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", "AmazonCodeWhispererService.GetUsageLimits")
        .body(body)
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("userInfo")?
        .get("email")?
        .as_str()
        .map(|s| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct an unsigned JWT whose payload is the given JSON.
    fn make_jwt(payload: &serde_json::Value) -> String {
        fn b64url(bytes: &[u8]) -> String {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(bytes)
        }
        let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let body = b64url(serde_json::to_string(payload).unwrap().as_bytes());
        let sig = b64url(b"dummy");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn decode_jwt_payload_extracts_claims() {
        let jwt = make_jwt(&serde_json::json!({
            "sub": "user-123",
            "exp": 1_700_000_000u64,
            "email": "me@example.com",
        }));
        let claims = decode_jwt_payload(&jwt).expect("payload decodes");
        assert_eq!(claims["sub"], "user-123");
        assert_eq!(claims["exp"], 1_700_000_000u64);
        assert_eq!(claims["email"], "me@example.com");
    }

    #[test]
    fn decode_jwt_payload_rejects_non_jwt() {
        assert!(decode_jwt_payload("not.a.jwt").is_none());
        assert!(decode_jwt_payload("single-segment").is_none());
    }

    #[test]
    fn extract_email_from_jwt_works() {
        let jwt = make_jwt(&serde_json::json!({ "email": "nghia@gmail.com" }));
        assert_eq!(extract_email_from_jwt(&jwt), Some("nghia@gmail.com".into()));
    }

    #[test]
    fn extract_account_id_from_id_token_claims() {
        let jwt = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-abc" }
        }));
        let claims = decode_jwt_payload(&jwt).unwrap();
        assert_eq!(
            extract_account_id_from_claims(&claims),
            Some("acc-abc".into())
        );
    }

    #[test]
    fn is_expired_none_is_not_expired() {
        assert!(!is_expired(None));
    }

    #[test]
    fn is_expired_past_timestamp() {
        assert!(is_expired(Some(1)));
    }

    #[test]
    fn is_expired_future_timestamp() {
        // Year 2099 in seconds.
        assert!(!is_expired(Some(4_070_908_800)));
    }

    #[test]
    fn is_expired_grace_window() {
        // 10 seconds in the future counts as expired (grace is 300s).
        let soon = now_unix() + 10;
        assert!(is_expired(Some(soon)));
    }

    #[test]
    fn parse_expires_field_handles_number_and_string() {
        assert_eq!(
            parse_expires_field(&serde_json::json!(1_700_000_000u64)),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_expires_field(&serde_json::json!("1700000000")),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn parse_expires_field_normalizes_milliseconds() {
        // Year 2020 in ms.
        assert_eq!(
            parse_expires_field(&serde_json::json!(1_577_836_800_000u64)),
            Some(1_577_836_800)
        );
    }

    #[test]
    fn derive_label_from_email() {
        assert_eq!(
            derive_label(AuthVendor::Anthropic, Some("nghia@gmail.com")),
            "nghia@gmail"
        );
        assert_eq!(
            derive_label(AuthVendor::OpenAI, Some("work@company.co.uk")),
            "work@company"
        );
    }

    #[test]
    fn parse_claude_json_extracts_fields() {
        let jwt = make_jwt(&serde_json::json!({
            "email": "me@example.com",
            "exp": 1_800_000_000u64,
        }));
        let payload = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": jwt,
                "refreshToken": "refresh-xyz",
                "expiresAt": 1_800_000_000_000u64,
                "scopes": ["user:profile", "user:inference"],
            }
        });
        let entry = parse_claude_json(&payload.to_string()).expect("parses");
        assert_eq!(entry.provider, "anthropic");
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh-xyz"));
        assert_eq!(entry.expires_at, Some(1_800_000_000));
        assert_eq!(entry.email.as_deref(), Some("me@example.com"));
        assert_eq!(entry.label, "me@example");
        assert_eq!(entry.scopes.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn parse_codex_json_extracts_oauth_fields() {
        let access = make_jwt(&serde_json::json!({
            "email": "coder@example.com",
            "exp": 1_900_000_000u64,
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_from_access" }
        }));
        let id_token = make_jwt(&serde_json::json!({
            "email": "coder@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_from_id" }
        }));

        let payload = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access,
                "refresh_token": "refresh-openai",
                "id_token": id_token
            }
        });

        let entry = parse_codex_json(&payload.to_string()).expect("parses codex oauth json");
        assert_eq!(entry.provider, "openai");
        assert_eq!(entry.email.as_deref(), Some("coder@example.com"));
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh-openai"));
        assert_eq!(entry.account_id.as_deref(), Some("acc_from_id"));
        assert_eq!(entry.expires_at, Some(1_900_000_000));
        assert_eq!(entry.label, "coder@example");
        assert!(entry.is_oauth);
    }

    #[test]
    fn parse_codex_json_falls_back_to_access_claims_without_id_token() {
        let access = make_jwt(&serde_json::json!({
            "email": "fallback@example.com",
            "exp": 1_901_000_000u64,
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_fallback" }
        }));
        let payload = serde_json::json!({
            "tokens": {
                "access_token": access,
                "refresh_token": "refresh-openai"
            }
        });

        let entry = parse_codex_json(&payload.to_string()).expect("parses codex oauth json");
        assert_eq!(entry.email.as_deref(), Some("fallback@example.com"));
        assert_eq!(entry.account_id.as_deref(), Some("acc_fallback"));
        assert_eq!(entry.expires_at, Some(1_901_000_000));
    }

    #[test]
    fn upsert_preserves_usage_and_cooldown() {
        let mut pool = PoolStore::default();
        pool.accounts.push(AccountEntry {
            label: "a@b".into(),
            provider: "anthropic".into(),
            email: Some("a@b.com".into()),
            access_token: "old".into(),
            refresh_token: Some("r1".into()),
            account_id: None,
            profile_arn: None,
            is_oauth: true,
            expires_at: Some(100),
            scopes: None,
            cooldown_until: Some(9999),
            usage: UsageRec {
                requests_remaining: Some(42),
                ..Default::default()
            },
            needs_relogin: true,
            disabled: false,
        });

        let refreshed = AccountEntry {
            label: "a@b".into(),
            provider: "anthropic".into(),
            email: None, // should be preserved from existing
            access_token: "new".into(),
            refresh_token: Some("r2".into()),
            account_id: None,
            profile_arn: None,
            is_oauth: true,
            expires_at: Some(2_000),
            scopes: None,
            cooldown_until: None,       // should be preserved from existing
            usage: UsageRec::default(), // should be preserved from existing
            needs_relogin: true,        // upsert clears this
            disabled: false,
        };
        upsert_by_label(&mut pool, refreshed);

        let a = &pool.accounts[0];
        assert_eq!(a.access_token, "new");
        assert_eq!(a.refresh_token.as_deref(), Some("r2"));
        assert_eq!(a.email.as_deref(), Some("a@b.com"));
        assert_eq!(a.cooldown_until, Some(9999));
        assert_eq!(a.usage.requests_remaining, Some(42));
        assert!(!a.needs_relogin);
    }

    #[test]
    fn upsert_by_identity_merges_anonymous_placeholder() {
        let mut pool = PoolStore::default();
        pool.accounts.push(AccountEntry {
            label: "anthropic-1".into(),
            provider: "anthropic".into(),
            email: None,
            access_token: "old-token".into(),
            refresh_token: Some("same-refresh".into()),
            account_id: Some("acc-123".into()),
            profile_arn: None,
            is_oauth: true,
            expires_at: Some(100),
            scopes: None,
            cooldown_until: Some(999),
            usage: UsageRec {
                requests_remaining: Some(7),
                ..Default::default()
            },
            needs_relogin: false,
            disabled: false,
        });

        upsert_by_label(
            &mut pool,
            AccountEntry {
                label: "anthropic-1".into(),
                provider: "anthropic".into(),
                email: Some("real@example.com".into()),
                access_token: "new-token".into(),
                refresh_token: Some("same-refresh".into()),
                account_id: Some("acc-123".into()),
                profile_arn: None,
                is_oauth: true,
                expires_at: Some(200),
                scopes: None,
                cooldown_until: None,
                usage: UsageRec::default(),
                needs_relogin: false,
                disabled: false,
            },
        );

        assert_eq!(pool.accounts.len(), 1);
        let a = &pool.accounts[0];
        assert_eq!(a.label, "real@example");
        assert_eq!(a.email.as_deref(), Some("real@example.com"));
        assert_eq!(a.account_id.as_deref(), Some("acc-123"));
        assert_eq!(a.cooldown_until, Some(999));
        assert_eq!(a.usage.requests_remaining, Some(7));
    }

    #[test]
    fn dedup_drops_anthropic_placeholder_when_real_identity_exists() {
        let mut accounts = vec![
            AccountEntry {
                label: "anthropic-1".into(),
                provider: "anthropic".into(),
                email: None,
                access_token: "tok-a".into(),
                refresh_token: Some("rt-a".into()),
                account_id: None,
                profile_arn: None,
                is_oauth: true,
                expires_at: Some(100),
                scopes: None,
                cooldown_until: None,
                usage: UsageRec::default(),
                needs_relogin: false,
                disabled: false,
            },
            AccountEntry {
                label: "real@example".into(),
                provider: "anthropic".into(),
                email: Some("real@example.com".into()),
                access_token: "tok-b".into(),
                refresh_token: Some("rt-b".into()),
                account_id: Some("acc-123".into()),
                profile_arn: None,
                is_oauth: true,
                expires_at: Some(200),
                scopes: None,
                cooldown_until: None,
                usage: UsageRec::default(),
                needs_relogin: false,
                disabled: false,
            },
        ];

        dedup_accounts(&mut accounts);

        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "real@example");
    }

    #[test]
    fn legacy_store_migration() {
        let jwt = make_jwt(&serde_json::json!({ "email": "legacy@test.com" }));
        let legacy = serde_json::json!({
            "credentials": [
                {
                    "provider": "anthropic",
                    "access_token": jwt,
                    "refresh_token": "r",
                    "is_oauth": true,
                    "expires_at": "1700000000"
                }
            ]
        });
        let parsed: LegacyStore = serde_json::from_str(&legacy.to_string()).unwrap();
        let account = legacy_to_account(parsed.credentials.into_iter().next().unwrap()).unwrap();
        assert_eq!(account.provider, "anthropic");
        assert_eq!(account.label, "legacy@test");
        assert_eq!(account.expires_at, Some(1_700_000_000));
    }

    #[test]
    fn candidate_rank_prefers_oauth_over_api_key_for_openai() {
        let oauth = AccountEntry {
            label: "oauth@example".into(),
            provider: "openai".into(),
            email: Some("oauth@example.com".into()),
            access_token: "oauth-token".into(),
            refresh_token: Some("refresh-token".into()),
            account_id: Some("account-123".into()),
            profile_arn: None,
            is_oauth: true,
            expires_at: Some(100),
            scopes: None,
            cooldown_until: None,
            usage: UsageRec::default(),
            needs_relogin: false,
            disabled: false,
        };
        let api_key = AccountEntry {
            label: "openai:key:abcdef".into(),
            provider: "openai".into(),
            email: None,
            access_token: "sk-local".into(),
            refresh_token: None,
            account_id: None,
            profile_arn: None,
            is_oauth: false,
            expires_at: None,
            scopes: None,
            cooldown_until: None,
            usage: UsageRec::default(),
            needs_relogin: false,
            disabled: false,
        };

        assert!(candidate_rank(&oauth) > candidate_rank(&api_key));
    }
}
