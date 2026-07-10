use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
}

/// Why a single provider's poll failed while others may have succeeded.
/// Coarser than poller::PollError on purpose: this is display granularity
/// for the detail popup's per-provider status badges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderStatus {
    AuthRequired,
    RateLimited,
    RequestFailed,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
    pub antigravity: Option<UsageData>,
    pub claude_code_error: Option<ProviderStatus>,
    pub codex_error: Option<ProviderStatus>,
    pub antigravity_error: Option<ProviderStatus>,
    pub rate_limited: bool,
    pub rate_limit_retry_after_ms: Option<u32>,
}
