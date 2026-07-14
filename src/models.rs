use std::time::SystemTime;

pub const FIVE_HOURS_SECONDS: u64 = 5 * 60 * 60;
pub const ONE_DAY_SECONDS: u64 = 24 * 60 * 60;
pub const ONE_WEEK_SECONDS: u64 = 7 * ONE_DAY_SECONDS;

#[derive(Clone, Debug)]
pub struct UsageWindow {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
    /// Length of the provider's rolling quota window when the API exposes it.
    pub duration_seconds: Option<u64>,
    /// Compact provider-supplied label for windows whose duration is unknown.
    pub source_label: Option<String>,
}

impl UsageWindow {
    pub fn new(
        percentage: f64,
        resets_at: Option<SystemTime>,
        duration_seconds: Option<u64>,
    ) -> Self {
        Self {
            percentage,
            resets_at,
            duration_seconds,
            source_label: None,
        }
    }

    pub fn with_source_label(mut self, label: Option<String>) -> Self {
        self.source_label = label.filter(|label| !label.trim().is_empty());
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    /// Provider quota windows ordered from shortest to longest. Windows whose
    /// duration is unknown retain provider order after all known durations.
    pub windows: Vec<UsageWindow>,
}

impl UsageData {
    pub fn from_windows(mut windows: Vec<UsageWindow>) -> Self {
        for window in &mut windows {
            window.duration_seconds = window.duration_seconds.filter(|seconds| *seconds > 0);
        }
        windows.retain(|window| window.percentage.is_finite());
        windows.sort_by(
            |left, right| match (left.duration_seconds, right.duration_seconds) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        );
        Self { windows }
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
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
    pub claude_code_updated_unix: Option<u64>,
    pub codex_updated_unix: Option<u64>,
    pub antigravity_updated_unix: Option<u64>,
    pub claude_code_error: Option<ProviderStatus>,
    pub codex_error: Option<ProviderStatus>,
    pub antigravity_error: Option<ProviderStatus>,
    pub rate_limited: bool,
    pub rate_limit_retry_after_ms: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_windows_sort_known_durations_and_reject_invalid_values() {
        let data = UsageData::from_windows(vec![
            UsageWindow::new(3.0, None, Some(ONE_WEEK_SECONDS)),
            UsageWindow::new(f64::NAN, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(1.0, None, None),
            UsageWindow::new(2.0, None, Some(FIVE_HOURS_SECONDS)),
        ]);

        assert_eq!(data.windows.len(), 3);
        assert_eq!(
            data.windows
                .iter()
                .map(|window| window.duration_seconds)
                .collect::<Vec<_>>(),
            vec![Some(FIVE_HOURS_SECONDS), Some(ONE_WEEK_SECONDS), None]
        );
    }

    #[test]
    fn zero_duration_is_treated_as_unknown() {
        let data = UsageData::from_windows(vec![UsageWindow::new(1.0, None, Some(0))]);
        assert_eq!(data.windows[0].duration_seconds, None);
    }
}
