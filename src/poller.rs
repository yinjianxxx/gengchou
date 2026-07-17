use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::models::{
    AppUsageData, ProviderStatus, UsageData, UsageWindow, FIVE_HOURS_SECONDS, ONE_DAY_SECONDS,
    ONE_WEEK_SECONDS,
};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_USER_AGENT: &str = "claude-code/2.1.85";
const CLAUDE_USAGE_NORMAL_POLL_MS: u64 = 180_000;
const CLAUDE_USAGE_FAST_POLL_MS: u64 = 120_000;
const CLAUDE_USAGE_FAST_EXTRA: u32 = 2;
const CLAUDE_RATE_LIMIT_MIN_RETRY_MS: u32 = 300_000;
const CLAUDE_RATE_LIMIT_MAX_RETRY_MS: u32 = 3_600_000;
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const CODEX_REQUEST_TIMEOUT_SECS: u64 = 10;
const CODEX_RETRY_DELAY_MS: u64 = 1_000;
const ANTIGRAVITY_REQUEST_TIMEOUT_SECS: u64 = 10;
const AUTH_REJECTION_RECHECK_SECS: u64 = 15 * 60;
const CODEX_KEYRING_SERVICE: &str = "Codex Auth";
const ANTIGRAVITY_CREDENTIAL_TARGET: &str = "gemini:antigravity";
const ANTIGRAVITY_USER_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
const ANTIGRAVITY_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RateLimited(Option<u32>),
    RequestFailed,
}

#[derive(Debug)]
pub struct PollFailure {
    pub error: PollError,
    pub data: Box<AppUsageData>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource,
    AllSources,
    Codex,
    Antigravity,
    AllProviders,
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}
#[derive(Clone)]
struct CachedClaudeUsage {
    token_hash: u64,
    fetched_at: SystemTime,
    data: UsageData,
    fast_polls_remaining: u32,
}

#[derive(Clone, Copy)]
struct ClaudeRateLimit {
    token_hash: u64,
    until: SystemTime,
}

#[derive(Default)]
struct ClaudePollState {
    cached: Option<CachedClaudeUsage>,
    rate_limit: Option<ClaudeRateLimit>,
}

static CLAUDE_POLL_STATE: OnceLock<Mutex<ClaudePollState>> = OnceLock::new();

#[derive(Clone, Copy)]
struct AuthRejectionBackoff {
    token_hash: u64,
    retry_at: Instant,
}

static CODEX_AUTH_REJECTION: OnceLock<Mutex<Option<AuthRejectionBackoff>>> = OnceLock::new();
static ANTIGRAVITY_AUTH_REJECTION: OnceLock<Mutex<Option<AuthRejectionBackoff>>> = OnceLock::new();

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<u64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct AntigravityAuthFile {
    token: AntigravityTokenData,
}

#[derive(Deserialize)]
struct AntigravityTokenData {
    access_token: String,
}

#[derive(Deserialize)]
struct AntigravityLoadResponse {
    #[serde(rename = "cloudaicompanionProject")]
    project: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityModelsResponse {
    models: HashMap<String, AntigravityModelInfo>,
}

#[derive(Deserialize)]
struct AntigravityModelInfo {
    #[serde(rename = "quotaInfo")]
    quota_info: Option<AntigravityQuotaInfo>,
}

#[derive(Deserialize)]
struct AntigravityQuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityUserQuotaResponse {
    #[serde(default)]
    buckets: Vec<AntigravityUserQuotaBucket>,
}

#[derive(Deserialize)]
struct AntigravityUserQuotaBucket {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
    disabled: Option<bool>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryResponse {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
    #[serde(rename = "quotaSummary")]
    quota_summary: Option<AntigravityQuotaSummaryEnvelope>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryEnvelope {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryGroup {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
    buckets: Option<Vec<AntigravityQuotaSummaryBucket>>,
}

#[derive(Clone, Deserialize)]
struct AntigravityQuotaSummaryBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    window: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredFree(buffer: *mut c_void);
}

pub fn poll(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    force_claude_refresh: bool,
) -> Result<AppUsageData, PollFailure> {
    poll_with(
        show_claude_code,
        show_codex,
        show_antigravity,
        || poll_claude_code(force_claude_refresh),
        poll_codex,
        poll_antigravity,
    )
}

fn poll_with(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    poll_claude_code: impl FnOnce() -> Result<UsageData, PollError> + Send,
    poll_codex: impl FnOnce() -> Result<UsageData, PollError> + Send,
    poll_antigravity: impl FnOnce() -> Result<UsageData, PollError> + Send,
) -> Result<AppUsageData, PollFailure> {
    // Fetch the enabled providers concurrently: results reach the UI only
    // once the whole pass finishes, so a slow endpoint would otherwise hold
    // back every other provider's fresh numbers for its full duration.
    let (claude_code, codex, antigravity) = std::thread::scope(|scope| {
        let claude_code = show_claude_code.then(|| scope.spawn(poll_claude_code));
        let codex = show_codex.then(|| scope.spawn(poll_codex));
        let antigravity = show_antigravity.then(|| scope.spawn(poll_antigravity));
        (
            claude_code.map(|handle| handle.join().unwrap_or(Err(PollError::RequestFailed))),
            codex.map(|handle| handle.join().unwrap_or(Err(PollError::RequestFailed))),
            antigravity.map(|handle| handle.join().unwrap_or(Err(PollError::RequestFailed))),
        )
    });

    let mut data = AppUsageData::default();
    let mut errors = Vec::new();
    let active_provider_count = show_claude_code as u8 + show_codex as u8 + show_antigravity as u8;

    if let Some(result) = claude_code {
        match result {
            Ok(claude_code) => data.claude_code = Some(claude_code),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Claude Code usage poll failed: {error:?}"));
                }
                data.claude_code_error = Some(provider_status(error));
                record_poll_error(&mut data, error);
                errors.push(error);
            }
        }
    }

    if let Some(result) = codex {
        match result {
            Ok(codex) => data.codex = Some(codex),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Codex usage poll failed: {error:?}"));
                }
                data.codex_error = Some(provider_status(error));
                record_poll_error(&mut data, error);
                errors.push(error);
            }
        }
    }

    if let Some(result) = antigravity {
        match result {
            Ok(antigravity) => data.antigravity = Some(antigravity),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!("Antigravity usage poll failed: {error:?}"));
                }
                data.antigravity_error = Some(provider_status(error));
                record_poll_error(&mut data, error);
                errors.push(error);
            }
        }
    }

    if data.claude_code.is_none() && data.codex.is_none() && data.antigravity.is_none() {
        Err(PollFailure {
            error: aggregate_poll_errors(&errors),
            data: Box::new(data),
        })
    } else {
        Ok(data)
    }
}

fn aggregate_poll_errors(errors: &[PollError]) -> PollError {
    let Some(&first) = errors.first() else {
        return PollError::RequestFailed;
    };
    if errors.len() == 1 {
        return first;
    }

    let all_require_user_action = errors.iter().all(|error| {
        matches!(
            error,
            PollError::AuthRequired | PollError::NoCredentials | PollError::TokenExpired
        )
    });
    if all_require_user_action {
        return first;
    }

    let retry_after_ms = errors
        .iter()
        .filter_map(|error| match error {
            PollError::RateLimited(value) => *value,
            _ => None,
        })
        .max();
    if errors
        .iter()
        .any(|error| matches!(error, PollError::RateLimited(_)))
    {
        PollError::RateLimited(retry_after_ms)
    } else {
        PollError::RequestFailed
    }
}

/// Collapse a poll error to display granularity (see models::ProviderStatus).
pub fn provider_status(error: PollError) -> ProviderStatus {
    match error {
        PollError::AuthRequired | PollError::NoCredentials | PollError::TokenExpired => {
            ProviderStatus::AuthRequired
        }
        PollError::RateLimited(_) => ProviderStatus::RateLimited,
        PollError::RequestFailed => ProviderStatus::RequestFailed,
    }
}

fn record_poll_error(data: &mut AppUsageData, error: PollError) {
    if let PollError::RateLimited(retry_after_ms) = error {
        data.rate_limited = true;
        if let Some(retry_after_ms) = retry_after_ms {
            data.rate_limit_retry_after_ms = Some(
                data.rate_limit_retry_after_ms
                    .map_or(retry_after_ms, |current| current.max(retry_after_ms)),
            );
        }
    }
}

fn claude_poll_state() -> &'static Mutex<ClaudePollState> {
    CLAUDE_POLL_STATE.get_or_init(|| Mutex::new(ClaudePollState::default()))
}

fn token_hash(token: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

fn claude_poll_interval_ms(cached: &CachedClaudeUsage) -> u64 {
    if cached.fast_polls_remaining > 0 {
        CLAUDE_USAGE_FAST_POLL_MS
    } else {
        CLAUDE_USAGE_NORMAL_POLL_MS
    }
}

fn claude_cache_is_fresh(
    cached: &CachedClaudeUsage,
    token_hash: u64,
    force_refresh: bool,
    now: SystemTime,
) -> bool {
    if force_refresh || cached.token_hash != token_hash {
        return false;
    }
    let Ok(age) = now.duration_since(cached.fetched_at) else {
        return false;
    };
    // A snapshot taken before a window's reset goes stale the moment that
    // reset passes: the server reports the refilled window while the cache
    // would keep showing it exhausted for up to a full cadence. This cannot
    // loop against a lagging server: a confirming fetch whose reply still
    // carries the old reset time re-caches with fetched_at past that reset.
    let reset_elapsed = cached.data.windows.iter().any(
        |window| matches!(window.resets_at, Some(reset) if reset > cached.fetched_at && now >= reset),
    );
    if reset_elapsed {
        return false;
    }
    age < Duration::from_millis(claude_poll_interval_ms(cached))
}

/// Padding past the exact cooldown deadline so the aligned tick's fetch
/// cannot land a few milliseconds early and be served from the cache.
const CLAUDE_ALIGN_MARGIN_MS: u64 = 250;
/// Floor for an aligned tick so an overdue deadline never arms a zero-delay
/// timer loop.
const CLAUDE_ALIGN_MIN_DELAY_MS: u64 = 1_000;

/// Delay until the next poll tick should fire so the Claude fetch lands right
/// at its cache-cooldown deadline (180s/120s after the previous fetch).
/// Without this the deadline falls between fixed ticks and the observed
/// cadence stretches by up to one user poll interval. Returns None when the
/// fixed schedule should own the timer: no cached data yet, a rate-limit
/// backoff pending, or a user cadence at least as coarse as the cooldown
/// (every tick fetches then, so there is nothing to align).
pub fn claude_aligned_poll_delay_ms(poll_interval_ms: u32) -> Option<u32> {
    let state = claude_poll_state().lock().ok()?;
    if state.rate_limit.is_some() {
        return None;
    }
    let cached = state.cached.as_ref()?;
    let age = SystemTime::now().duration_since(cached.fetched_at).ok()?;
    aligned_poll_delay_ms(poll_interval_ms, claude_poll_interval_ms(cached), age)
}

fn aligned_poll_delay_ms(poll_interval_ms: u32, cadence_ms: u64, age: Duration) -> Option<u32> {
    if u64::from(poll_interval_ms) >= cadence_ms {
        return None;
    }
    let age_ms = age.as_millis().min(u128::from(u64::MAX)) as u64;
    let due_ms = cadence_ms
        .saturating_sub(age_ms)
        .saturating_add(CLAUDE_ALIGN_MARGIN_MS);
    Some(due_ms.clamp(CLAUDE_ALIGN_MIN_DELAY_MS, u64::from(poll_interval_ms)) as u32)
}

fn cached_claude_usage(token_hash: u64, force_refresh: bool) -> Option<UsageData> {
    let state = claude_poll_state().lock().ok()?;
    let cached = state.cached.as_ref()?;
    let now = SystemTime::now();
    if !claude_cache_is_fresh(cached, token_hash, force_refresh, now) {
        if force_refresh && cached.token_hash == token_hash {
            diagnose::log("Claude usage manual refresh bypassed the normal cache cooldown");
        }
        return None;
    }
    let age = now.duration_since(cached.fetched_at).ok()?;
    let interval_ms = claude_poll_interval_ms(cached);
    diagnose::log(format!(
        "Claude usage poll skipped; using cached usage data age={}s cadence={}s",
        age.as_secs(),
        interval_ms / 1000
    ));
    Some(cached.data.clone())
}

fn claude_usage_increased(previous: &UsageData, current: &UsageData) -> bool {
    current.windows.iter().any(|current_window| {
        previous.windows.iter().any(|previous_window| {
            previous_window.duration_seconds == current_window.duration_seconds
                && previous_window.source_label == current_window.source_label
                && current_window.percentage > previous_window.percentage
        })
    })
}

fn next_claude_fast_polls(
    cached: Option<&CachedClaudeUsage>,
    token_hash: u64,
    data: &UsageData,
) -> u32 {
    cached
        .filter(|cached| cached.token_hash == token_hash)
        .map_or(0, |cached| {
            if claude_usage_increased(&cached.data, data) {
                CLAUDE_USAGE_FAST_EXTRA + 1
            } else {
                cached.fast_polls_remaining.saturating_sub(1)
            }
        })
}

fn store_cached_claude_usage(token_hash: u64, data: &UsageData) {
    if let Ok(mut state) = claude_poll_state().lock() {
        let fast_polls_remaining = next_claude_fast_polls(state.cached.as_ref(), token_hash, data);
        let interval_ms = if fast_polls_remaining > 0 {
            CLAUDE_USAGE_FAST_POLL_MS
        } else {
            CLAUDE_USAGE_NORMAL_POLL_MS
        };
        diagnose::log(format!(
            "Claude usage poll succeeded; next cadence={}s fast_polls_remaining={fast_polls_remaining}",
            interval_ms / 1000
        ));
        state.cached = Some(CachedClaudeUsage {
            token_hash,
            fetched_at: SystemTime::now(),
            data: data.clone(),
            fast_polls_remaining,
        });
        state.rate_limit = None;
    }
}

fn claude_rate_limit_delay_ms(retry_after_ms: Option<u32>) -> u32 {
    retry_after_ms
        .unwrap_or(CLAUDE_RATE_LIMIT_MIN_RETRY_MS)
        .clamp(
            CLAUDE_RATE_LIMIT_MIN_RETRY_MS,
            CLAUDE_RATE_LIMIT_MAX_RETRY_MS,
        )
}

fn store_claude_rate_limit(token_hash: u64, retry_after_ms: Option<u32>) -> u32 {
    let delay_ms = claude_rate_limit_delay_ms(retry_after_ms);
    if let Ok(mut state) = claude_poll_state().lock() {
        state.rate_limit = Some(ClaudeRateLimit {
            token_hash,
            until: SystemTime::now()
                .checked_add(Duration::from_millis(delay_ms as u64))
                .unwrap_or_else(SystemTime::now),
        });
    }
    delay_ms
}

fn claude_rate_limit_remaining_ms(token_hash: u64) -> Option<u32> {
    let mut state = claude_poll_state().lock().ok()?;
    let rate_limit = state.rate_limit?;
    if rate_limit.token_hash != token_hash {
        state.rate_limit = None;
        return None;
    }
    match rate_limit.until.duration_since(SystemTime::now()) {
        Ok(remaining) if !remaining.is_zero() => {
            Some(remaining.as_millis().clamp(1, u32::MAX as u128) as u32)
        }
        _ => {
            state.rate_limit = None;
            None
        }
    }
}

fn poll_claude_code(force_refresh: bool) -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;
    let token_hash = token_hash(&creds.access_token);
    if let Some(remaining_ms) = claude_rate_limit_remaining_ms(token_hash) {
        diagnose::log(format!(
            "Claude usage poll skipped; rate-limit backoff remaining={}s",
            remaining_ms.div_ceil(1000)
        ));
        return Err(PollError::RateLimited(Some(remaining_ms)));
    }
    if let Some(cached) = cached_claude_usage(token_hash, force_refresh) {
        return Ok(cached);
    }

    match fetch_usage_with_fallback(&creds.access_token) {
        Ok(data) => {
            store_cached_claude_usage(token_hash, &data);
            Ok(data)
        }
        Err(PollError::RateLimited(retry_after_ms)) => {
            let delay_ms = store_claude_rate_limit(token_hash, retry_after_ms);
            Err(PollError::RateLimited(Some(delay_ms)))
        }
        Err(error) => Err(error),
    }
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let token_hash = token_hash(&creds.access_token);
    if auth_rejection_is_backed_off(&CODEX_AUTH_REJECTION, token_hash) {
        diagnose::log("Codex usage poll skipped; rejected credentials have not changed");
        return Err(PollError::AuthRequired);
    }

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => {
            clear_auth_rejection(&CODEX_AUTH_REJECTION);
            Ok(data)
        }
        Err(PollError::AuthRequired) => {
            record_auth_rejection(&CODEX_AUTH_REJECTION, token_hash);
            diagnose::log(
                "Codex usage endpoint returned auth required; automatic CLI refresh is disabled because it would require running a model-capable Codex command.",
            );
            Err(PollError::AuthRequired)
        }
        Err(error) => Err(error),
    }
}
fn poll_antigravity() -> Result<UsageData, PollError> {
    let creds = match read_antigravity_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Antigravity usage poll failed: no Antigravity credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let token_hash = token_hash(&creds.access_token);
    if auth_rejection_is_backed_off(&ANTIGRAVITY_AUTH_REJECTION, token_hash) {
        diagnose::log("Antigravity usage poll skipped; rejected credentials have not changed");
        return Err(PollError::AuthRequired);
    }

    match fetch_antigravity_usage(&creds.access_token) {
        Ok(data) => {
            clear_auth_rejection(&ANTIGRAVITY_AUTH_REJECTION);
            Ok(data)
        }
        Err(PollError::AuthRequired) => {
            record_auth_rejection(&ANTIGRAVITY_AUTH_REJECTION, token_hash);
            Err(PollError::AuthRequired)
        }
        Err(error) => Err(error),
    }
}

fn auth_rejection_is_backed_off(
    state: &OnceLock<Mutex<Option<AuthRejectionBackoff>>>,
    token_hash: u64,
) -> bool {
    let Some(state) = state.get() else {
        return false;
    };
    let Ok(mut rejection) = state.lock() else {
        return false;
    };
    match *rejection {
        Some(value) if value.token_hash == token_hash && Instant::now() < value.retry_at => true,
        Some(_) => {
            *rejection = None;
            false
        }
        None => false,
    }
}

fn record_auth_rejection(state: &OnceLock<Mutex<Option<AuthRejectionBackoff>>>, token_hash: u64) {
    let state = state.get_or_init(|| Mutex::new(None));
    if let Ok(mut rejection) = state.lock() {
        *rejection = Some(AuthRejectionBackoff {
            token_hash,
            retry_at: Instant::now() + Duration::from_secs(AUTH_REJECTION_RECHECK_SECS),
        });
    }
}

fn clear_auth_rejection(state: &OnceLock<Mutex<Option<AuthRejectionBackoff>>>) {
    if let Some(state) = state.get() {
        if let Ok(mut rejection) = state.lock() {
            *rejection = None;
        }
    }
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    loop {
        if !is_token_expired(creds.expires_at) {
            return Ok(creds);
        }

        let source = creds.source.clone();
        diagnose::log(format!(
            "Claude credentials from {source:?} are expired; automatic CLI refresh is disabled because it would require running a model-capable Claude command."
        ));

        match read_next_credentials_after(&source) {
            Some(next) => creds = next,
            None => return Err(PollError::TokenExpired),
        }
    }
}
/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    build_agent_with_timeout(Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
}

fn build_agent_with_timeout(timeout: Duration) -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(timeout)
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    let mut snapshot = match mode {
        CredentialWatchMode::ActiveSource => claude_credential_watch_snapshot(true),
        CredentialWatchMode::AllSources => claude_credential_watch_snapshot(false),
        CredentialWatchMode::Codex => vec![codex_credential_watch_signature()],
        CredentialWatchMode::Antigravity => vec![antigravity_credential_watch_signature()],
        CredentialWatchMode::AllProviders => {
            let mut snapshot = claude_credential_watch_snapshot(false);
            snapshot.push(codex_credential_watch_signature());
            snapshot.push(antigravity_credential_watch_signature());
            snapshot
        }
    };
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

fn claude_credential_watch_snapshot(active_only: bool) -> CredentialWatchSnapshot {
    let sources = if active_only {
        read_first_credentials()
            .map(|creds| vec![creds.source])
            .unwrap_or_else(all_known_credential_sources)
    } else {
        all_known_credential_sources()
    };
    sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect()
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn windows_credential_source() -> Option<CredentialSource> {
    let home = dirs::home_dir()?;
    Some(CredentialSource::Windows(
        home.join(".claude").join(".credentials.json"),
    ))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::read(path) {
        Ok(content) => content_watch_signature(&key, &content),
        Err(_) => format!("{key}|missing"),
    }
}

fn codex_credential_watch_signature() -> String {
    let Some(codex_home) = codex_home() else {
        return "win:codex-auth|missing".to_string();
    };
    let auth_path = codex_home.join("auth.json");
    let file_signature = windows_credential_watch_signature(&auth_path);
    let keyring_signature = codex_direct_keyring_target(&codex_home)
        .and_then(|target| {
            read_windows_generic_credential_quiet(&target).map(|content| {
                content_watch_signature(&format!("wincred:{target}"), content.as_bytes())
            })
        })
        .unwrap_or_else(|| "wincred:codex-auth|missing".to_string());
    format!("{file_signature};{keyring_signature}")
}

fn content_watch_signature(key: &str, content: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{key}|present|{}|{}", content.len(), hasher.finish())
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("test -f ~/.claude/.credentials.json && cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    let state = if output.status.success() {
        let key = format!("wsl:{distro}");
        return Some(content_watch_signature(&key, &output.stdout));
    } else {
        "missing".to_string()
    };

    Some(format!("wsl:{distro}|{state}"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    match try_usage_endpoint(token)? {
        Some(data) => {
            if data.windows.iter().any(|window| window.resets_at.is_none()) {
                diagnose::log(
                    "usage endpoint omitted one or more reset timers; keeping usage data and refusing Messages API fallback because it sends a model request.",
                );
            }
            Ok(data)
        }
        None => {
            diagnose::log(
                "usage endpoint unavailable; refusing Messages API fallback because it sends a model request.",
            );
            Err(PollError::RequestFailed)
        }
    }
}
fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", CLAUDE_USER_AGENT)
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            match http_status_poll_error("Claude usage endpoint", code, &response) {
                PollError::AuthRequired => return Err(PollError::AuthRequired),
                rate_limited @ PollError::RateLimited(_) => return Err(rate_limited),
                _ => {
                    diagnose::log(
                        "refusing Messages API fallback because it sends a model request",
                    );
                    return Ok(None);
                }
            }
        }
        Err(error) => {
            diagnose::log_error("Claude usage endpoint request failed", error);
            return Ok(None);
        }
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Claude usage endpoint response", error);
            return Ok(None);
        }
    };
    let mut windows = Vec::new();

    if let Some(bucket) = &response.five_hour {
        windows.push(UsageWindow::new(
            bucket.utilization,
            parse_iso8601(bucket.resets_at.as_deref()),
            Some(FIVE_HOURS_SECONDS),
        ));
    }

    if let Some(bucket) = &response.seven_day {
        windows.push(UsageWindow::new(
            bucket.utilization,
            parse_iso8601(bucket.resets_at.as_deref()),
            Some(ONE_WEEK_SECONDS),
        ));
    }

    Ok(Some(UsageData::from_windows(windows)))
}

fn classify_http_status(code: u16, retry_after_ms: Option<u32>) -> PollError {
    match code {
        401 | 403 => PollError::AuthRequired,
        429 => PollError::RateLimited(retry_after_ms),
        _ => PollError::RequestFailed,
    }
}

fn http_status_poll_error(endpoint: &str, code: u16, response: &ureq::Response) -> PollError {
    let retry_after_ms = (code == 429).then(|| retry_after_ms(response)).flatten();
    let error = classify_http_status(code, retry_after_ms);
    match error {
        PollError::AuthRequired => diagnose::log(format!(
            "{endpoint} returned auth error status {code}; re-login required"
        )),
        PollError::RateLimited(retry_after_ms) => diagnose::log(format!(
            "{endpoint} returned rate limit status 429; retry_after_ms={}",
            retry_after_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )),
        _ => diagnose::log(format!("{endpoint} returned HTTP status {code}")),
    }
    error
}

fn retry_after_ms(response: &ureq::Response) -> Option<u32> {
    retry_after_value_ms(response.header("Retry-After")?, SystemTime::now())
}

fn retry_after_value_ms(value: &str, now: SystemTime) -> Option<u32> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.saturating_mul(1000).min(u32::MAX as u64) as u32);
    }

    let retry_unix = parse_retry_after_http_date(value)?;
    let now_unix = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(
        retry_unix
            .saturating_sub(now_unix)
            .saturating_mul(1000)
            .min(u32::MAX as u64) as u32,
    )
}

fn parse_retry_after_http_date(value: &str) -> Option<u64> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || parts[5] != "GMT" || !parts[0].ends_with(',') {
        return None;
    }
    let day = parts[1].parse::<u64>().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts[3].parse::<u64>().ok()?;
    parse_datetime_to_unix(&format!("{year:04}-{month:02}-{day:02}T{}", parts[4])).ok()
}

enum CodexAttemptError {
    Retryable(PollError),
    Final(PollError),
}

impl CodexAttemptError {
    fn poll_error(self) -> PollError {
        match self {
            Self::Retryable(error) | Self::Final(error) => error,
        }
    }
}

fn codex_http_status_is_retryable(code: u16) -> bool {
    matches!(code, 502..=504)
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent_with_timeout(Duration::from_secs(CODEX_REQUEST_TIMEOUT_SECS))?;
    fetch_codex_usage_with_retry(
        || fetch_codex_usage_once(&agent, token, account_id),
        || std::thread::sleep(Duration::from_millis(CODEX_RETRY_DELAY_MS)),
    )
}

fn fetch_codex_usage_with_retry(
    mut fetch_once: impl FnMut() -> Result<UsageData, CodexAttemptError>,
    mut retry_wait: impl FnMut(),
) -> Result<UsageData, PollError> {
    let started = Instant::now();
    match fetch_once() {
        Ok(data) => Ok(data),
        Err(CodexAttemptError::Final(error)) => Err(error),
        Err(CodexAttemptError::Retryable(_)) => {
            diagnose::log(format!(
                "Codex usage endpoint transient failure after {}ms; retrying once in {}ms",
                started.elapsed().as_millis(),
                CODEX_RETRY_DELAY_MS
            ));
            retry_wait();
            match fetch_once() {
                Ok(data) => {
                    diagnose::log(format!(
                        "Codex usage endpoint recovered on retry; total_elapsed_ms={}",
                        started.elapsed().as_millis()
                    ));
                    Ok(data)
                }
                Err(error) => {
                    let error = error.poll_error();
                    diagnose::log(format!(
                        "Codex usage endpoint retry failed; total_elapsed_ms={} error={error:?}",
                        started.elapsed().as_millis()
                    ));
                    Err(error)
                }
            }
        }
    }
}

fn fetch_codex_usage_once(
    agent: &ureq::Agent,
    token: &str,
    account_id: Option<&str>,
) -> Result<UsageData, CodexAttemptError> {
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            let error = http_status_poll_error("Codex usage endpoint", code, &response);
            return Err(if codex_http_status_is_retryable(code) {
                CodexAttemptError::Retryable(error)
            } else {
                CodexAttemptError::Final(error)
            });
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(CodexAttemptError::Retryable(PollError::RequestFailed));
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(CodexAttemptError::Final(PollError::RequestFailed));
        }
    };

    codex_usage_from_response(response).ok_or_else(|| {
        diagnose::log("Codex usage response did not contain a usable quota window");
        CodexAttemptError::Final(PollError::RequestFailed)
    })
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut windows = Vec::new();

    if let Some(window) = details.primary_window.flatten() {
        windows.extend(codex_usage_window(&window, "Primary"));
    }

    if let Some(window) = details.secondary_window.flatten() {
        windows.extend(codex_usage_window(&window, "Secondary"));
    }

    Some(UsageData::from_windows(windows))
}

fn codex_usage_window(window: &CodexRateLimitWindow, fallback_label: &str) -> Option<UsageWindow> {
    let duration_seconds = window.limit_window_seconds.filter(|seconds| *seconds > 0);
    Some(
        UsageWindow::new(
            window.used_percent?,
            unix_to_system_time(window.reset_at),
            duration_seconds,
        )
        .with_source_label(
            duration_seconds
                .is_none()
                .then(|| fallback_label.to_string()),
        ),
    )
}

fn antigravity_credential_watch_signature() -> String {
    let Some(content) = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET) else {
        return format!("{ANTIGRAVITY_CREDENTIAL_TARGET}|missing");
    };
    content_watch_signature(ANTIGRAVITY_CREDENTIAL_TARGET, content.as_bytes())
}

fn fetch_antigravity_usage(token: &str) -> Result<UsageData, PollError> {
    let mut errors = Vec::new();

    for base_url in ANTIGRAVITY_ENDPOINTS {
        match fetch_antigravity_usage_from_endpoint(base_url, token) {
            Ok(data) => return Ok(data),
            Err(error) => errors.push(error),
        }
    }

    Err(aggregate_poll_errors(&errors))
}

fn fetch_antigravity_usage_from_endpoint(
    base_url: &str,
    token: &str,
) -> Result<UsageData, PollError> {
    let agent = build_agent_with_timeout(Duration::from_secs(ANTIGRAVITY_REQUEST_TIMEOUT_SECS))?;
    let project = fetch_antigravity_project(&agent, base_url, token)?;
    if let Some(project) = project.as_deref() {
        let per_model = match fetch_antigravity_user_quota(&agent, token, project) {
            Ok(data) if !data.is_empty() => Some(data),
            Ok(_) => None,
            Err(error) => {
                diagnose::log(format!(
                    "Antigravity retrieveUserQuota unavailable; continuing with weekly summary: {error:?}"
                ));
                None
            }
        };
        let summary = match fetch_antigravity_quota_summary(&agent, base_url, token, project) {
            Ok(data) if !data.is_empty() => Some(data),
            Ok(_) => None,
            Err(error) => {
                diagnose::log(format!(
                    "Antigravity retrieveUserQuotaSummary unavailable; continuing with per-model quota: {error:?}"
                ));
                None
            }
        };

        if let Some(data) = merge_antigravity_usage_sources(per_model, summary) {
            return Ok(data);
        }
    }

    let window = fetch_antigravity_model_quota(&agent, base_url, token, project.as_deref())?;
    Ok(UsageData::from_windows(vec![window]))
}

fn fetch_antigravity_user_quota(
    agent: &ureq::Agent,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(ANTIGRAVITY_USER_QUOTA_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity retrieveUserQuota",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity retrieveUserQuota request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityUserQuotaResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuota response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    Ok(antigravity_usage_from_user_quota(response).unwrap_or_default())
}

fn fetch_antigravity_project(
    agent: &ureq::Agent,
    base_url: &str,
    token: &str,
) -> Result<Option<String>, PollError> {
    let body = serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:loadCodeAssist"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity loadCodeAssist",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity loadCodeAssist request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityLoadResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Antigravity loadCodeAssist response", error);
            return Err(PollError::RequestFailed);
        }
    };

    Ok(response.project.filter(|project| !project.is_empty()))
}

fn fetch_antigravity_model_quota(
    agent: &ureq::Agent,
    base_url: &str,
    token: &str,
    project: Option<&str>,
) -> Result<UsageWindow, PollError> {
    let body = match project {
        Some(project) => serde_json::json!({ "project": project }),
        None => serde_json::json!({}),
    };

    let resp = match agent
        .post(&format!("{base_url}/v1internal:fetchAvailableModels"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity fetchAvailableModels",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity fetchAvailableModels request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityModelsResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity fetchAvailableModels response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    best_antigravity_section(response.models.into_iter().filter_map(|(model, info)| {
        let quota = info.quota_info?;
        if !is_antigravity_display_model(&model) {
            return None;
        }
        antigravity_section_from_quota(quota)
    }))
    .ok_or(PollError::RequestFailed)
}

fn fetch_antigravity_quota_summary(
    agent: &ureq::Agent,
    base_url: &str,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:retrieveUserQuotaSummary"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity retrieveUserQuotaSummary",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity retrieveUserQuotaSummary request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: AntigravityQuotaSummaryResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuotaSummary response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    antigravity_usage_from_summary(response).ok_or(PollError::RequestFailed)
}

fn antigravity_section_from_quota(quota: AntigravityQuotaInfo) -> Option<UsageWindow> {
    let remaining = quota.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageWindow::new(
        (1.0 - remaining) * 100.0,
        parse_iso8601(quota.reset_time.as_deref()),
        None,
    ))
}

fn antigravity_usage_from_user_quota(response: AntigravityUserQuotaResponse) -> Option<UsageData> {
    let window = best_antigravity_section(response.buckets.into_iter().filter_map(|bucket| {
        if bucket.disabled.unwrap_or(false) {
            return None;
        }
        let model = bucket.model_id?.trim().to_ascii_lowercase();
        let model = model.strip_prefix("models/").unwrap_or(&model);
        if !is_antigravity_display_model(model) {
            return None;
        }
        let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
        Some(UsageWindow::new(
            (1.0 - remaining) * 100.0,
            parse_iso8601(bucket.reset_time.as_deref()),
            Some(FIVE_HOURS_SECONDS),
        ))
    }))?;

    Some(UsageData::from_windows(vec![window]))
}

fn merge_antigravity_usage_sources(
    per_model: Option<UsageData>,
    summary: Option<UsageData>,
) -> Option<UsageData> {
    let mut windows = Vec::new();
    for usage in [per_model, summary].into_iter().flatten() {
        for window in usage.windows {
            upsert_usage_window(&mut windows, window);
        }
    }
    (!windows.is_empty()).then(|| UsageData::from_windows(windows))
}

fn antigravity_section_from_summary_bucket(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<UsageWindow> {
    let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
    let duration_seconds = antigravity_summary_bucket_duration_seconds(bucket);
    let source_label = duration_seconds.is_none().then(|| {
        bucket
            .window
            .clone()
            .or_else(|| bucket.display_name.clone())
            .unwrap_or_default()
    });
    Some(
        UsageWindow::new(
            (1.0 - remaining) * 100.0,
            parse_iso8601(bucket.reset_time.as_deref()),
            duration_seconds,
        )
        .with_source_label(source_label),
    )
}

fn antigravity_summary_bucket_duration_seconds(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<u64> {
    if let Some(seconds) = usage_window_duration_seconds(bucket.window.as_deref()) {
        return Some(seconds);
    }

    let text = format!(
        "{} {}",
        bucket.bucket_id.as_deref().unwrap_or_default(),
        bucket.display_name.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let words = text
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();

    if words
        .iter()
        .any(|word| matches!(*word, "weekly" | "week" | "7d" | "1w"))
    {
        return Some(ONE_WEEK_SECONDS);
    }
    if words.iter().any(|word| *word == "5h")
        || words
            .windows(2)
            .any(|pair| pair == ["five", "hour"] || pair == ["5", "hour"])
    {
        return Some(FIVE_HOURS_SECONDS);
    }

    None
}

fn antigravity_usage_from_summary(response: AntigravityQuotaSummaryResponse) -> Option<UsageData> {
    let mut fallback = None;

    let groups = response
        .groups
        .or_else(|| response.quota_summary.and_then(|summary| summary.groups))
        .unwrap_or_default();
    for group in groups {
        let is_gemini = is_antigravity_gemini_summary_group(&group);
        let usage = antigravity_usage_from_summary_group(group);

        if is_gemini && usage.is_some() {
            return usage;
        }

        if fallback.is_none() {
            fallback = usage;
        }
    }

    fallback
}

fn antigravity_usage_from_summary_group(group: AntigravityQuotaSummaryGroup) -> Option<UsageData> {
    let mut windows = Vec::new();

    for bucket in group.buckets.unwrap_or_default() {
        let Some(window) = antigravity_section_from_summary_bucket(&bucket) else {
            continue;
        };
        upsert_usage_window(&mut windows, window);
    }

    (!windows.is_empty()).then(|| UsageData::from_windows(windows))
}

fn upsert_usage_window(windows: &mut Vec<UsageWindow>, candidate: UsageWindow) {
    let same_window = |window: &&mut UsageWindow| {
        window.duration_seconds == candidate.duration_seconds
            && window.source_label.as_deref() == candidate.source_label.as_deref()
    };
    if let Some(existing) = windows.iter_mut().find(same_window) {
        if candidate.percentage > existing.percentage {
            *existing = candidate;
        }
    } else {
        windows.push(candidate);
    }
}

fn usage_window_duration_seconds(label: Option<&str>) -> Option<u64> {
    let label = label?.trim().to_ascii_lowercase();
    match label.as_str() {
        "5h" => Some(FIVE_HOURS_SECONDS),
        "daily" | "1d" | "24h" => Some(ONE_DAY_SECONDS),
        "weekly" | "7d" | "1w" => Some(ONE_WEEK_SECONDS),
        "monthly" | "30d" => Some(30 * ONE_DAY_SECONDS),
        "annual" | "yearly" | "365d" => Some(365 * ONE_DAY_SECONDS),
        _ => {
            let (number, multiplier) = if let Some(value) = label.strip_suffix('h') {
                (value, 60 * 60)
            } else if let Some(value) = label.strip_suffix('d') {
                (value, ONE_DAY_SECONDS)
            } else if let Some(value) = label.strip_suffix('w') {
                (value, ONE_WEEK_SECONDS)
            } else {
                return None;
            };
            number.parse::<u64>().ok()?.checked_mul(multiplier)
        }
    }
}

fn is_antigravity_gemini_summary_group(group: &AntigravityQuotaSummaryGroup) -> bool {
    group
        .display_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
        || group
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains("gemini"))
        || group.buckets.as_ref().is_some_and(|buckets| {
            buckets.iter().any(|bucket| {
                bucket
                    .bucket_id
                    .as_deref()
                    .is_some_and(|id| id.to_ascii_lowercase().starts_with("gemini-"))
                    || bucket
                        .display_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
            })
        })
}

fn best_antigravity_section<I>(sections: I) -> Option<UsageWindow>
where
    I: IntoIterator<Item = UsageWindow>,
{
    sections.into_iter().max_by(|a, b| {
        a.percentage
            .partial_cmp(&b.percentage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.resets_at.cmp(&b.resets_at))
    })
}

fn is_antigravity_display_model(model: &str) -> bool {
    model.starts_with("gemini")
        || model.starts_with("claude")
        || model.starts_with("gpt")
        || model.starts_with("image")
        || model.starts_with("imagen")
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn read_windows_credentials() -> Option<Credentials> {
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn codex_home() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home);
    }

    Some(dirs::home_dir()?.join(".codex"))
}

fn codex_direct_keyring_target_from_path(path: &str) -> Option<String> {
    let digest = crate::updater::sha256_hex(path.as_bytes()).ok()?;
    let short = digest.get(..16).unwrap_or(&digest);
    // keyring-rs' Windows backend uses "{user}.{service}" as the generic
    // credential target by default. Codex passes the computed cli key as the
    // user and "Codex Auth" as the service.
    Some(format!("cli|{short}.{CODEX_KEYRING_SERVICE}"))
}

fn codex_direct_keyring_target(codex_home: &Path) -> Option<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    codex_direct_keyring_target_from_path(&canonical.to_string_lossy())
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let codex_home = codex_home()?;
    let auth_path = codex_home.join("auth.json");
    let auth = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|content| serde_json::from_str::<CodexAuthFile>(&content).ok())
        .or_else(|| {
            let target = codex_direct_keyring_target(&codex_home)?;
            let content = read_windows_generic_credential_quiet(&target)?;
            diagnose::log("loaded Codex credentials from Windows Credential Manager");
            serde_json::from_str::<CodexAuthFile>(&content).ok()
        });

    if auth.is_none() {
        diagnose::log(format!(
            "no readable Codex Desktop/CLI credentials found at {} or in the direct Windows keyring",
            auth_path.display()
        ));
    }
    auth?
        .tokens
        .filter(|tokens| !tokens.access_token.is_empty())
}

fn read_antigravity_credentials() -> Option<AntigravityTokenData> {
    let content = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET)?;
    let auth: AntigravityAuthFile = serde_json::from_str(&content).ok()?;
    if auth.token.access_token.is_empty() {
        None
    } else {
        Some(auth.token)
    }
}

fn read_windows_generic_credential(target: &str) -> Option<String> {
    let result = read_windows_generic_credential_quiet(target);
    if result.is_none() {
        diagnose::log(format!(
            "unable to read Windows generic credential target {target}"
        ));
    }
    result
}

fn read_windows_generic_credential_quiet(target: &str) -> Option<String> {
    const CRED_TYPE_GENERIC: u32 = 1;

    let mut target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut credential: *mut CredentialW = std::ptr::null_mut();

    let ok = unsafe {
        CredReadW(
            target_wide.as_mut_ptr(),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
    };

    if ok == 0 || credential.is_null() {
        return None;
    }

    let result = unsafe {
        let cred = &*credential;
        if cred.credential_blob_size == 0 || cred.credential_blob.is_null() {
            CredFree(credential as *mut c_void);
            return None;
        }
        let bytes =
            std::slice::from_raw_parts(cred.credential_blob, cred.credential_blob_size as usize);
        let text = String::from_utf8(bytes.to_vec()).ok();
        CredFree(credential as *mut c_void);
        text
    };

    result
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
}

/// Installed distros change about as often as Windows itself, but every
/// credential read and watch snapshot re-ran `wsl.exe -l -q`. That spawn
/// costs the full 5s timeout whenever WSL is absent or broken (a common
/// setup: it fails with REGDB_E_CLASSNOTREG), and the enumeration happens on
/// the UI thread during the auth-error watch. Cache it, including the
/// failure, so a stalled WSL cannot be paid for on every tick.
const WSL_DISTRO_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

struct WslDistroCache {
    fetched_at: Instant,
    distros: Vec<String>,
}

fn wsl_cache_is_fresh(entry: &WslDistroCache, now: Instant) -> bool {
    now.duration_since(entry.fetched_at) < WSL_DISTRO_CACHE_TTL
}

static WSL_DISTRO_CACHE: OnceLock<Mutex<Option<WslDistroCache>>> = OnceLock::new();

fn list_wsl_distros() -> Vec<String> {
    let cache = WSL_DISTRO_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(cached) = cache.lock() {
        if let Some(entry) = cached.as_ref() {
            if wsl_cache_is_fresh(entry, Instant::now()) {
                return entry.distros.clone();
            }
        }
    }

    let distros = enumerate_wsl_distros();
    if let Ok(mut cached) = cache.lock() {
        *cached = Some(WslDistroCache {
            fetched_at: Instant::now(),
            distros: distros.clone(),
        });
    }
    distros
}

fn enumerate_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
/// The APIs return formats like "2026-03-05T08:00:00.321598+00:00" and
/// "2026-06-13T22:08:54Z"; non-zero UTC offsets are converted, not dropped.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?.trim();
    let t_pos = s.find('T')?;
    let time_tail = &s[t_pos + 1..];

    // Split off the timezone suffix: 'Z', or a '+HH:MM' / '-HH:MM' offset.
    let (datetime_part, offset_secs) = if let Some(z_rel) = time_tail.find(['Z', 'z']) {
        (&s[..t_pos + 1 + z_rel], 0)
    } else if let Some(sign_rel) = time_tail.find(['+', '-']) {
        let sign_pos = t_pos + 1 + sign_rel;
        (&s[..sign_pos], parse_utc_offset_secs(&s[sign_pos..])?)
    } else {
        (s, 0)
    };

    let local_secs = parse_datetime_to_unix(datetime_part).ok()?;
    // "08:00 at +02:00" is 06:00 UTC: subtract the offset.
    let utc_secs = (local_secs as i64).checked_sub(offset_secs)?;
    if utc_secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(utc_secs as u64))
}

/// Parse "+HH:MM", "-HHMM", or "+HH" into signed seconds east of UTC.
fn parse_utc_offset_secs(s: &str) -> Option<i64> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if rest.len() == 4 => (&rest[..2], &rest[2..]),
        None if rest.len() == 2 => (rest, "0"),
        None => return None,
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Minimal datetime parser - avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;

    // Bounds before arithmetic: month indexes month_days, day-1 must not
    // underflow, and a huge year would spin the per-year loop below. The
    // input is a provider API response, so a malformed value must fail the
    // parse rather than panic or wrap into a bogus timestamp.
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(());
    }

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;
    if hour > 23 || min > 59 || sec > 59 {
        return Err(());
    }

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X%·Yh" style text for compact surfaces.
/// These units deliberately stay English (d/h/m/s/now) in every UI
/// language: they are terse, universally recognizable, and keep the taskbar
/// and floating surfaces compact.
#[cfg(test)]
fn format_line(section: &UsageWindow) -> String {
    let pct = format!("{:.0}%", section.percentage);
    let cd = format_countdown(section.resets_at);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct}\u{00b7}{cd}")
    }
}

pub(crate) fn format_countdown(resets_at: Option<SystemTime>) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return "now".to_string(),
    };

    format_countdown_from_secs(remaining.as_secs())
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64) -> String {
    if total_secs == 0 {
        return "now".to_string();
    }
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}d")
    } else if total_hours >= 1 {
        format!("{total_hours}h")
    } else if total_mins >= 1 {
        format!("{total_mins}m")
    } else {
        format!("{total_secs}s")
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400
    } else if total_hours >= 1 {
        total_hours * 3600
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if any reported window has reached "now".
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    data.windows
        .iter()
        .any(|window| matches!(window.resets_at, Some(t) if now.duration_since(t).is_ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_countdown_always_uses_english_units() {
        assert_eq!(format_countdown_from_secs(2 * 86_400), "2d");
        assert_eq!(format_countdown_from_secs(3 * 3_600), "3h");
        assert_eq!(format_countdown_from_secs(42 * 60), "42m");
        assert_eq!(format_countdown_from_secs(17), "17s");
        assert_eq!(format_countdown_from_secs(0), "now");
    }

    #[test]
    fn compact_line_uses_now_for_elapsed_reset() {
        let usage = UsageWindow::new(
            85.0,
            SystemTime::now().checked_sub(Duration::from_secs(1)),
            Some(FIVE_HOURS_SECONDS),
        );

        assert_eq!(format_line(&usage), "85%·now");
    }

    #[test]
    fn every_provider_status_uses_the_same_rate_limit_classification() {
        assert_eq!(
            classify_http_status(429, Some(120_000)),
            PollError::RateLimited(Some(120_000))
        );
        assert_eq!(classify_http_status(401, None), PollError::AuthRequired);
        assert_eq!(classify_http_status(403, None), PollError::AuthRequired);
        assert_eq!(classify_http_status(500, None), PollError::RequestFailed);
    }

    #[test]
    fn codex_retries_only_transient_gateway_statuses() {
        assert!(codex_http_status_is_retryable(502));
        assert!(codex_http_status_is_retryable(503));
        assert!(codex_http_status_is_retryable(504));
        assert!(!codex_http_status_is_retryable(401));
        assert!(!codex_http_status_is_retryable(403));
        assert!(!codex_http_status_is_retryable(429));
        assert!(!codex_http_status_is_retryable(500));
    }

    #[test]
    fn codex_transient_failure_retries_once_and_recovers() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let data = fetch_codex_usage_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(CodexAttemptError::Retryable(PollError::RequestFailed))
                } else {
                    Ok(usage_with_percent(42.0))
                }
            },
            || waits.set(waits.get() + 1),
        )
        .expect("second Codex attempt should recover");

        assert_eq!(attempts.get(), 2);
        assert_eq!(waits.get(), 1);
        assert_eq!(data.windows[0].percentage, 42.0);
    }

    #[test]
    fn codex_final_failure_is_not_retried() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let error = fetch_codex_usage_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(CodexAttemptError::Final(PollError::AuthRequired))
            },
            || waits.set(waits.get() + 1),
        )
        .expect_err("auth failures must not be retried");

        assert_eq!(error, PollError::AuthRequired);
        assert_eq!(attempts.get(), 1);
        assert_eq!(waits.get(), 0);
    }

    #[test]
    fn retry_after_accepts_seconds_and_caps_large_values() {
        assert_eq!(retry_after_value_ms("120", UNIX_EPOCH), Some(120_000));
        assert_eq!(
            retry_after_value_ms("999999999999", UNIX_EPOCH),
            Some(u32::MAX)
        );
    }

    #[test]
    fn retry_after_accepts_http_date_and_rejects_invalid_values() {
        let retry_unix = parse_retry_after_http_date("Mon, 13 Jul 2026 12:00:00 GMT")
            .expect("valid IMF-fixdate should parse");
        let now = UNIX_EPOCH + Duration::from_secs(retry_unix - 120);

        assert_eq!(
            retry_after_value_ms("Mon, 13 Jul 2026 12:00:00 GMT", now),
            Some(120_000)
        );
        assert_eq!(retry_after_value_ms("not-a-date", now), None);
    }

    #[test]
    fn credential_content_fingerprint_detects_same_length_replacement() {
        let before = content_watch_signature("credential", b"token-a");
        let after = content_watch_signature("credential", b"token-b");

        assert_ne!(before, after);
        assert!(before.contains("|present|7|"));
    }

    #[test]
    fn codex_direct_keyring_target_matches_official_windows_mapping() {
        assert_eq!(
            codex_direct_keyring_target_from_path("abc").as_deref(),
            Some("cli|ba7816bf8f01cfea.Codex Auth")
        );
    }

    #[test]
    fn missing_windows_credential_has_stable_signature() {
        let path = std::env::temp_dir().join(format!(
            "aium-missing-credential-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));

        assert!(windows_credential_watch_signature(&path).ends_with("|missing"));
    }

    fn usage_with_percent(percentage: f64) -> UsageData {
        UsageData::from_windows(vec![UsageWindow::new(
            percentage,
            None,
            Some(FIVE_HOURS_SECONDS),
        )])
    }

    fn cached_claude_usage_for_test(
        percentage: f64,
        fetched_at: SystemTime,
        fast_polls_remaining: u32,
    ) -> CachedClaudeUsage {
        CachedClaudeUsage {
            token_hash: 7,
            fetched_at,
            data: usage_with_percent(percentage),
            fast_polls_remaining,
        }
    }

    #[test]
    fn claude_cache_uses_completion_based_normal_and_fast_deadlines() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let normal_fresh = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(179)).unwrap(),
            0,
        );
        let normal_due = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(180)).unwrap(),
            0,
        );
        let fast_fresh = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(119)).unwrap(),
            1,
        );
        let fast_due = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(120)).unwrap(),
            1,
        );

        assert!(claude_cache_is_fresh(&normal_fresh, 7, false, now));
        assert!(!claude_cache_is_fresh(&normal_due, 7, false, now));
        assert!(claude_cache_is_fresh(&fast_fresh, 7, false, now));
        assert!(!claude_cache_is_fresh(&fast_due, 7, false, now));
        assert!(!claude_cache_is_fresh(&normal_fresh, 7, true, now));
    }

    fn cached_usage_with_reset(fetched_at: SystemTime, resets_at: SystemTime) -> CachedClaudeUsage {
        CachedClaudeUsage {
            token_hash: 7,
            fetched_at,
            data: UsageData::from_windows(vec![UsageWindow::new(
                90.0,
                Some(resets_at),
                Some(FIVE_HOURS_SECONDS),
            )]),
            fast_polls_remaining: 0,
        }
    }

    #[test]
    fn claude_cache_goes_stale_once_a_cached_window_reset_passes() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let fetched_at = now.checked_sub(Duration::from_secs(60)).unwrap();

        // Snapshot taken before the reset, reset has passed: stale even
        // though the cadence deadline is still 120s away.
        let past_reset = cached_usage_with_reset(
            fetched_at,
            now.checked_sub(Duration::from_secs(10)).unwrap(),
        );
        assert!(!claude_cache_is_fresh(&past_reset, 7, false, now));

        // Reset still ahead: the normal cadence owns freshness.
        let upcoming_reset = cached_usage_with_reset(
            fetched_at,
            now.checked_add(Duration::from_secs(600)).unwrap(),
        );
        assert!(claude_cache_is_fresh(&upcoming_reset, 7, false, now));

        // Server still reporting a reset older than the snapshot (lagging
        // propagation): no re-fetch loop, the cadence owns freshness again.
        let lagging_reset = cached_usage_with_reset(
            fetched_at,
            fetched_at.checked_sub(Duration::from_secs(10)).unwrap(),
        );
        assert!(claude_cache_is_fresh(&lagging_reset, 7, false, now));
    }

    #[test]
    fn aligned_poll_delay_pulls_the_tick_to_the_cooldown_deadline() {
        // Deadline between fixed ticks: align to it (plus the fetch margin).
        assert_eq!(
            aligned_poll_delay_ms(60_000, 180_000, Duration::from_secs(130)),
            Some(50_250)
        );
        // Deadline beyond the next fixed tick: keep the fixed cadence.
        assert_eq!(
            aligned_poll_delay_ms(60_000, 180_000, Duration::from_secs(30)),
            Some(60_000)
        );
        // Deadline already passed: fire after the minimum delay, not 0ms.
        assert_eq!(
            aligned_poll_delay_ms(60_000, 180_000, Duration::from_secs(200)),
            Some(1_000)
        );
        // User cadence at least as coarse as the cooldown: nothing to align,
        // every fixed tick fetches anyway.
        assert_eq!(
            aligned_poll_delay_ms(300_000, 180_000, Duration::from_secs(30)),
            None
        );
        assert_eq!(
            aligned_poll_delay_ms(120_000, 120_000, Duration::from_secs(30)),
            None
        );
    }

    #[test]
    fn claude_usage_growth_arms_three_fast_follow_up_polls() {
        let previous = cached_claude_usage_for_test(10.0, SystemTime::now(), 0);
        let increased = usage_with_percent(11.0);
        assert_eq!(
            next_claude_fast_polls(Some(&previous), 7, &increased),
            CLAUDE_USAGE_FAST_EXTRA + 1
        );

        let fast = cached_claude_usage_for_test(11.0, SystemTime::now(), 3);
        assert_eq!(
            next_claude_fast_polls(Some(&fast), 7, &usage_with_percent(11.0)),
            2
        );
        assert_eq!(
            next_claude_fast_polls(Some(&fast), 8, &usage_with_percent(12.0)),
            0
        );
    }

    #[test]
    fn claude_rate_limit_delay_keeps_manual_refresh_inside_backoff() {
        assert_eq!(
            claude_rate_limit_delay_ms(None),
            CLAUDE_RATE_LIMIT_MIN_RETRY_MS
        );
        assert_eq!(
            claude_rate_limit_delay_ms(Some(1_000)),
            CLAUDE_RATE_LIMIT_MIN_RETRY_MS
        );
        assert_eq!(
            claude_rate_limit_delay_ms(Some(u32::MAX)),
            CLAUDE_RATE_LIMIT_MAX_RETRY_MS
        );
    }

    #[test]
    fn enabled_providers_are_polled_concurrently() {
        use std::sync::{Arc, Condvar, Mutex};

        let rendezvous = Arc::new((Mutex::new(0_u8), Condvar::new()));
        let make_poll = |percentage| {
            let rendezvous = Arc::clone(&rendezvous);
            move || {
                let (lock, ready) = &*rendezvous;
                let mut started = lock.lock().expect("provider rendezvous lock should work");
                *started += 1;
                ready.notify_all();
                let (started, wait) = ready
                    .wait_timeout_while(started, Duration::from_secs(5), |started| *started < 2)
                    .expect("provider rendezvous wait should work");
                if wait.timed_out() {
                    return Err(PollError::RequestFailed);
                }
                drop(started);
                Ok(usage_with_percent(percentage))
            }
        };

        let data = poll_with(true, true, false, make_poll(11.0), make_poll(22.0), || {
            unreachable!("antigravity is disabled")
        })
        .expect("both concurrent providers should succeed");

        assert_eq!(data.claude_code.unwrap().windows[0].percentage, 11.0);
        assert_eq!(data.codex.unwrap().windows[0].percentage, 22.0);
    }

    #[test]
    fn claude_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            || Err(PollError::AuthRequired),
            || Ok(usage_with_percent(42.0)),
            || unreachable!("antigravity is disabled"),
        )
        .expect("codex data should keep the poll successful");

        assert!(data.claude_code.is_none());
        assert_eq!(data.claude_code_error, Some(ProviderStatus::AuthRequired));
        assert!(data.codex_error.is_none());
        assert_eq!(data.codex.unwrap().windows[0].percentage, 42.0);
    }

    #[test]
    fn codex_failure_does_not_block_claude_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            || Ok(usage_with_percent(64.0)),
            || Err(PollError::RequestFailed),
            || unreachable!("antigravity is disabled"),
        )
        .expect("claude data should keep the poll successful");

        assert_eq!(data.claude_code.unwrap().windows[0].percentage, 64.0);
        assert!(data.codex.is_none());
    }

    #[test]
    fn rate_limit_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            || Err(PollError::RateLimited(Some(120_000))),
            || Ok(usage_with_percent(42.0)),
            || unreachable!("antigravity is disabled"),
        )
        .expect("codex data should keep the poll successful");

        assert!(data.claude_code.is_none());
        assert_eq!(data.claude_code_error, Some(ProviderStatus::RateLimited));
        assert_eq!(data.codex.unwrap().windows[0].percentage, 42.0);
        assert!(data.rate_limited);
        assert_eq!(data.rate_limit_retry_after_ms, Some(120_000));
    }
    #[test]
    fn mixed_all_provider_failure_does_not_claim_every_provider_needs_login() {
        let failure = poll_with(
            true,
            true,
            true,
            || Err(PollError::AuthRequired),
            || Err(PollError::RequestFailed),
            || Err(PollError::NoCredentials),
        )
        .expect_err("all-provider failure should return an error");

        assert_eq!(failure.error, PollError::RequestFailed);
        assert_eq!(
            failure.data.claude_code_error,
            Some(ProviderStatus::AuthRequired)
        );
        assert_eq!(
            failure.data.codex_error,
            Some(ProviderStatus::RequestFailed)
        );
        assert_eq!(
            failure.data.antigravity_error,
            Some(ProviderStatus::AuthRequired)
        );
    }

    #[test]
    fn all_provider_auth_failures_still_require_login() {
        let failure = poll_with(
            true,
            true,
            true,
            || Err(PollError::AuthRequired),
            || Err(PollError::NoCredentials),
            || Err(PollError::TokenExpired),
        )
        .expect_err("all-provider authentication failure should return an error");

        assert!(matches!(
            failure.error,
            PollError::AuthRequired | PollError::NoCredentials | PollError::TokenExpired
        ));
    }

    #[test]
    fn alternative_endpoint_auth_error_needs_consensus_before_login_is_required() {
        assert_eq!(
            aggregate_poll_errors(&[
                PollError::AuthRequired,
                PollError::RequestFailed,
                PollError::RequestFailed,
            ]),
            PollError::RequestFailed
        );
        assert_eq!(
            aggregate_poll_errors(&[
                PollError::AuthRequired,
                PollError::AuthRequired,
                PollError::AuthRequired,
            ]),
            PollError::AuthRequired
        );
        assert_eq!(
            aggregate_poll_errors(&[
                PollError::AuthRequired,
                PollError::RateLimited(Some(120_000)),
                PollError::RequestFailed,
            ]),
            PollError::RateLimited(Some(120_000))
        );
    }

    #[test]
    fn rejected_credential_backoff_clears_for_a_new_token_or_success() {
        let state = OnceLock::new();

        assert!(!auth_rejection_is_backed_off(&state, 11));
        record_auth_rejection(&state, 11);
        assert!(auth_rejection_is_backed_off(&state, 11));
        assert!(!auth_rejection_is_backed_off(&state, 22));

        record_auth_rejection(&state, 22);
        clear_auth_rejection(&state);
        assert!(!auth_rejection_is_backed_off(&state, 22));
    }

    #[test]
    fn antigravity_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            false,
            true,
            true,
            || unreachable!("claude code is disabled"),
            || Ok(usage_with_percent(42.0)),
            || Err(PollError::NoCredentials),
        )
        .expect("codex data should keep the poll successful");

        assert!(data.antigravity.is_none());
        assert_eq!(data.codex.unwrap().windows[0].percentage, 42.0);
    }

    #[test]
    fn codex_weekly_only_window_is_not_treated_as_five_hour_usage() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 1,
                        "limit_window_seconds": 604800,
                        "reset_at": 1783872000
                    },
                    "secondary_window": null
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].percentage, 1.0);
        assert_eq!(usage.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    #[test]
    fn codex_windows_are_ordered_by_duration_instead_of_api_position() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 12,
                        "limit_window_seconds": 604800,
                        "reset_at": 1783872000
                    },
                    "secondary_window": {
                        "used_percent": 34,
                        "limit_window_seconds": 18000,
                        "reset_at": 1783353600
                    }
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| window.duration_seconds)
                .collect::<Vec<_>>(),
            vec![Some(FIVE_HOURS_SECONDS), Some(ONE_WEEK_SECONDS)]
        );
        assert_eq!(usage.windows[0].percentage, 34.0);
        assert_eq!(usage.windows[1].percentage, 12.0);
    }

    #[test]
    fn codex_unknown_durations_keep_distinct_fallback_labels() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 3,
                        "limit_window_seconds": 0,
                        "reset_at": 1783872000
                    },
                    "secondary_window": {
                        "used_percent": 4,
                        "reset_at": 1783958400
                    }
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| window.source_label.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("Primary"), Some("Secondary")]
        );
    }

    #[test]
    fn antigravity_summary_prefers_gemini_group() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "groups": [
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {
                                "bucketId": "3p-weekly",
                                "window": "weekly",
                                "resetTime": "2026-06-20T18:32:02Z",
                                "remainingFraction": 1
                            },
                            {
                                "bucketId": "3p-5h",
                                "window": "5h",
                                "resetTime": "2026-06-13T23:32:02Z",
                                "remainingFraction": 1
                            }
                        ]
                    },
                    {
                        "displayName": "Gemini Models",
                        "description": "Models within this group: Gemini Flash, Gemini Pro",
                        "buckets": [
                            {
                                "bucketId": "gemini-weekly",
                                "displayName": "Weekly Limit",
                                "window": "weekly",
                                "resetTime": "2026-06-20T17:08:54Z",
                                "remainingFraction": 0.99304295
                            },
                            {
                                "bucketId": "gemini-5h",
                                "displayName": "Five Hour Limit",
                                "window": "5h",
                                "resetTime": "2026-06-13T22:08:54Z",
                                "remainingFraction": 0.9582575
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("summary response should deserialize");

        let usage =
            antigravity_usage_from_summary(response).expect("Gemini quota should be selected");

        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].duration_seconds, Some(FIVE_HOURS_SECONDS));
        assert_eq!(usage.windows[1].duration_seconds, Some(ONE_WEEK_SECONDS));
        assert!((usage.windows[0].percentage - 4.17425).abs() < 0.000001);
        assert!((usage.windows[1].percentage - 0.695705).abs() < 0.000001);
        assert!(usage
            .windows
            .iter()
            .all(|window| window.resets_at.is_some()));
    }

    #[test]
    fn antigravity_user_quota_collapses_models_into_the_most_used_five_hour_window() {
        let response: AntigravityUserQuotaResponse = serde_json::from_str(
            r#"{
                "buckets": [
                    {
                        "modelId": "models/gemini-3.5-flash-high",
                        "remainingFraction": 0.8,
                        "resetTime": "2026-06-13T22:08:54Z"
                    },
                    {
                        "modelId": "claude-opus",
                        "remainingFraction": 0.25,
                        "resetTime": "2026-06-13T22:18:54Z"
                    },
                    {
                        "modelId": "tab-completion",
                        "remainingFraction": 0.0,
                        "resetTime": "2026-06-13T22:18:54Z"
                    },
                    {
                        "modelId": "gpt-disabled",
                        "remainingFraction": 0.0,
                        "resetTime": "2026-06-13T22:18:54Z",
                        "disabled": true
                    }
                ]
            }"#,
        )
        .expect("user quota response should deserialize");

        let usage = antigravity_usage_from_user_quota(response)
            .expect("a supported per-model quota should be selected");

        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].duration_seconds, Some(FIVE_HOURS_SECONDS));
        assert!((usage.windows[0].percentage - 75.0).abs() < f64::EPSILON);
        assert!(usage.windows[0].resets_at.is_some());
    }

    #[test]
    fn antigravity_sources_merge_five_hour_and_weekly_windows() {
        let per_model =
            UsageData::from_windows(vec![UsageWindow::new(35.0, None, Some(FIVE_HOURS_SECONDS))]);
        let summary =
            UsageData::from_windows(vec![UsageWindow::new(12.0, None, Some(ONE_WEEK_SECONDS))]);

        let merged = merge_antigravity_usage_sources(Some(per_model), Some(summary))
            .expect("both quota sources should merge");

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].duration_seconds, Some(FIVE_HOURS_SECONDS));
        assert_eq!(merged.windows[1].duration_seconds, Some(ONE_WEEK_SECONDS));
        assert_eq!(merged.windows[0].percentage, 35.0);
        assert_eq!(merged.windows[1].percentage, 12.0);
    }

    #[test]
    fn antigravity_sources_keep_weekly_only_free_plan_without_inventing_five_hour() {
        let summary =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(ONE_WEEK_SECONDS))]);

        let merged = merge_antigravity_usage_sources(None, Some(summary))
            .expect("weekly-only usage should remain usable");

        assert_eq!(merged.windows.len(), 1);
        assert_eq!(merged.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    #[test]
    fn antigravity_sources_dedupe_legacy_summary_five_hour_bucket() {
        let per_model =
            UsageData::from_windows(vec![UsageWindow::new(40.0, None, Some(FIVE_HOURS_SECONDS))]);
        let summary = UsageData::from_windows(vec![
            UsageWindow::new(20.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(8.0, None, Some(ONE_WEEK_SECONDS)),
        ]);

        let merged = merge_antigravity_usage_sources(Some(per_model), Some(summary))
            .expect("legacy duplicate windows should merge");

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].percentage, 40.0);
        assert_eq!(merged.windows[1].percentage, 8.0);
    }

    #[test]
    fn antigravity_summary_accepts_nested_quota_summary_envelope() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "quotaSummary": {
                    "groups": [
                        {
                            "displayName": "Gemini Models",
                            "buckets": [
                                {
                                    "bucketId": "gemini-weekly",
                                    "displayName": "Weekly Quota",
                                    "remainingFraction": 0.6,
                                    "resetTime": "2026-06-20T17:08:54Z"
                                }
                            ]
                        }
                    ]
                }
            }"#,
        )
        .expect("nested summary response should deserialize");

        let usage = antigravity_usage_from_summary(response)
            .expect("nested weekly quota should be selected");

        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
        assert!((usage.windows[0].percentage - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn antigravity_summary_infers_five_hour_window_without_explicit_window_field() {
        let bucket = AntigravityQuotaSummaryBucket {
            bucket_id: Some("gemini-5h".to_string()),
            display_name: Some("Five Hour Quota".to_string()),
            window: None,
            remaining_fraction: Some(0.5),
            reset_time: None,
        };

        assert_eq!(
            antigravity_summary_bucket_duration_seconds(&bucket),
            Some(FIVE_HOURS_SECONDS)
        );
    }

    #[test]
    fn provider_window_labels_accept_known_and_numeric_durations() {
        assert_eq!(
            usage_window_duration_seconds(Some("daily")),
            Some(ONE_DAY_SECONDS)
        );
        assert_eq!(
            usage_window_duration_seconds(Some("12h")),
            Some(12 * 60 * 60)
        );
        assert_eq!(
            usage_window_duration_seconds(Some("2w")),
            Some(2 * ONE_WEEK_SECONDS)
        );
        assert_eq!(usage_window_duration_seconds(Some("rolling")), None);
    }

    #[test]
    fn iso8601_accepts_the_provider_formats() {
        // 2026-01-01T00:00:00Z is 1767225600 seconds after the epoch.
        let expected = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert_eq!(parse_iso8601(Some("2026-01-01T00:00:00Z")), Some(expected));
        assert_eq!(
            parse_iso8601(Some("2026-01-01T00:00:00.321598+00:00")),
            Some(expected)
        );
        assert_eq!(
            parse_iso8601(Some("1970-01-01T00:00:00Z")),
            Some(UNIX_EPOCH)
        );
    }

    #[test]
    fn iso8601_converts_utc_offsets_instead_of_dropping_them() {
        let utc = parse_iso8601(Some("2026-03-05T06:00:00Z"));
        assert!(utc.is_some());
        assert_eq!(parse_iso8601(Some("2026-03-05T08:00:00+02:00")), utc);
        assert_eq!(parse_iso8601(Some("2026-03-05T01:00:00-05:00")), utc);
        assert_eq!(parse_iso8601(Some("2026-03-05T08:00:00+0200")), utc);
    }

    #[test]
    fn wsl_distro_cache_serves_within_ttl_and_expires_after() {
        // Enumerating WSL costs a process spawn - and a full 5s timeout when
        // WSL is absent or broken - so a fresh entry must be reused rather
        // than re-probed on every credential read.
        let now = Instant::now();
        let entry = WslDistroCache {
            fetched_at: now,
            distros: vec!["Ubuntu".to_string()],
        };
        assert!(wsl_cache_is_fresh(&entry, now));
        assert!(wsl_cache_is_fresh(
            &entry,
            now + WSL_DISTRO_CACHE_TTL - Duration::from_secs(1)
        ));
        assert!(!wsl_cache_is_fresh(&entry, now + WSL_DISTRO_CACHE_TTL));
        assert!(!wsl_cache_is_fresh(
            &entry,
            now + WSL_DISTRO_CACHE_TTL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn iso8601_rejects_malformed_input_without_panicking() {
        for input in [
            "2026-99-05T00:00:00Z",      // month out of range
            "2026-00-05T00:00:00Z",      // month zero
            "2026-03-00T00:00:00Z",      // day zero
            "2026-03-05T99:00:00Z",      // hour out of range
            "1969-12-31T23:59:59Z",      // before the epoch
            "2026-03-05T08:00:00+99:00", // bogus offset
            "not a timestamp",
            "",
        ] {
            assert_eq!(parse_iso8601(Some(input)), None, "input: {input}");
        }
        assert_eq!(parse_iso8601(None), None);
    }
}
