//! Shared view model for the taskbar widget and floating monitor.
//!
//! This module contains product semantics only. It deliberately has no HWND,
//! HDC, DPI, or drawing dependencies so compact-surface state can be tested
//! without constructing Windows UI objects.

use crate::localization::Strings;
use crate::models::{AppUsageData, ProviderStatus, UsageData, UsageWindow};
use crate::poller;
use crate::tray_icon::TrayIconKind;

pub(crate) const WARN_THRESHOLD_PERCENT: i32 = 90;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Normal,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attention {
    Normal,
    Warn,
    Degraded,
    Error,
}

#[derive(Clone, Debug)]
pub(crate) struct WindowView {
    pub(crate) label: String,
    pub(crate) percent: Option<f64>,
    pub(crate) display_percent: i32,
    pub(crate) percent_text: String,
    pub(crate) countdown: String,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) severity: Severity,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderView {
    pub(crate) kind: TrayIconKind,
    /// Surface-specific taskbar payload. Kept separate from `windows`, whose
    /// top-two selection is optimized for the floating monitor.
    pub(crate) badge: Option<WindowView>,
    pub(crate) windows: Vec<WindowView>,
    pub(crate) placeholder: Option<String>,
    pub(crate) attention: Attention,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CompactViewModel {
    pub(crate) providers: Vec<ProviderView>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    data: Option<&AppUsageData>,
    strings: Strings,
    order: &[TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) -> CompactViewModel {
    let providers = order
        .iter()
        .filter_map(|kind| match kind {
            TrayIconKind::Claude if show_claude_code => Some(provider_view(
                *kind,
                data.and_then(|data| data.claude_code.as_ref()),
                data.and_then(|data| data.claude_code_error),
                strings,
            )),
            TrayIconKind::Codex if show_codex => Some(provider_view(
                *kind,
                data.and_then(|data| data.codex.as_ref()),
                data.and_then(|data| data.codex_error),
                strings,
            )),
            TrayIconKind::Antigravity if show_antigravity => Some(provider_view(
                *kind,
                data.and_then(|data| data.antigravity.as_ref()),
                data.and_then(|data| data.antigravity_error),
                strings,
            )),
            _ => None,
        })
        .collect();
    CompactViewModel { providers }
}

pub(crate) fn placeholder_model(
    text: &str,
    order: &[TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) -> CompactViewModel {
    let providers = order
        .iter()
        .filter_map(|kind| {
            let visible = match kind {
                TrayIconKind::Claude => show_claude_code,
                TrayIconKind::Codex => show_codex,
                TrayIconKind::Antigravity => show_antigravity,
            };
            visible.then(|| ProviderView {
                kind: *kind,
                badge: None,
                windows: Vec::new(),
                placeholder: Some(text.to_string()),
                attention: Attention::Normal,
            })
        })
        .collect();
    CompactViewModel { providers }
}

pub(crate) fn worst_window(provider: &ProviderView) -> Option<&WindowView> {
    provider.windows.iter().reduce(|best, candidate| {
        if candidate.display_percent > best.display_percent
            || (candidate.display_percent == best.display_percent
                && duration_rank(candidate.duration_seconds) < duration_rank(best.duration_seconds))
        {
            candidate
        } else {
            best
        }
    })
}

/// Window shown by the taskbar badge.
///
/// Keep the short-window value stable during normal operation. A different
/// window only takes over after it reaches the warning threshold, so weekly
/// exhaustion is never hidden even though sub-threshold long-window drift is
/// intentionally left to the tooltip and detail surfaces.
pub(crate) fn badge_window(provider: &ProviderView) -> Option<&WindowView> {
    if provider.badge.is_some() {
        return provider.badge.as_ref();
    }
    if let Some(warned) = worst_window(provider).filter(|window| window.severity == Severity::Warn)
    {
        return Some(warned);
    }

    provider
        .windows
        .iter()
        .find(|window| {
            window
                .duration_seconds
                .is_some_and(|seconds| approximately(seconds, 5 * 60 * 60))
        })
        .or_else(|| {
            provider
                .windows
                .iter()
                .min_by_key(|window| duration_rank(window.duration_seconds))
        })
}

fn duration_rank(duration_seconds: Option<u64>) -> u64 {
    duration_seconds.unwrap_or(u64::MAX)
}

fn provider_view(
    kind: TrayIconKind,
    usage: Option<&UsageData>,
    error: Option<ProviderStatus>,
    strings: Strings,
) -> ProviderView {
    let badge = usage
        .filter(|usage| !usage.is_empty())
        .and_then(|usage| badge_usage_window(usage, strings));
    let windows = usage
        .filter(|usage| !usage.is_empty())
        .map(|usage| {
            selected_usage_windows(usage)
                .into_iter()
                .map(|window| window_view(window, strings))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let placeholder = windows.is_empty().then(|| "--".to_string());
    let attention = match error {
        Some(ProviderStatus::RateLimited | ProviderStatus::RequestFailed) => Attention::Degraded,
        Some(ProviderStatus::AuthRequired) => Attention::Error,
        None if windows
            .iter()
            .any(|window| window.severity == Severity::Warn) =>
        {
            Attention::Warn
        }
        None => Attention::Normal,
    };
    ProviderView {
        kind,
        badge,
        windows,
        placeholder,
        attention,
    }
}

fn badge_usage_window(usage: &UsageData, strings: Strings) -> Option<WindowView> {
    let worst = usage.windows.iter().reduce(|best, candidate| {
        let best_percent = display_percent(best.percentage);
        let candidate_percent = display_percent(candidate.percentage);
        if candidate_percent > best_percent
            || (candidate_percent == best_percent
                && duration_rank(candidate.duration_seconds) < duration_rank(best.duration_seconds))
        {
            candidate
        } else {
            best
        }
    });
    if let Some(warned) = worst.filter(|window| {
        display_percent(window.percentage.clamp(0.0, 100.0)) >= WARN_THRESHOLD_PERCENT
    }) {
        return Some(window_view(warned, strings));
    }

    usage
        .windows
        .iter()
        .find(|window| {
            window
                .duration_seconds
                .is_some_and(|seconds| approximately(seconds, 5 * 60 * 60))
        })
        .or_else(|| {
            usage
                .windows
                .iter()
                .min_by_key(|window| duration_rank(window.duration_seconds))
        })
        .map(|window| window_view(window, strings))
}

fn window_view(window: &UsageWindow, strings: Strings) -> WindowView {
    let percent = window.percentage.clamp(0.0, 100.0);
    let shown = display_percent(percent);
    let countdown = if shown == 0 {
        String::new()
    } else {
        let countdown = poller::format_countdown(window.resets_at);
        if countdown.is_empty() {
            String::new()
        } else {
            format!("\u{00b7}{countdown}")
        }
    };
    WindowView {
        label: compact_usage_window_label(window, strings),
        percent: Some(percent),
        display_percent: shown,
        percent_text: format!("{shown}%"),
        countdown,
        duration_seconds: window.duration_seconds,
        severity: if shown >= WARN_THRESHOLD_PERCENT {
            Severity::Warn
        } else {
            Severity::Normal
        },
    }
}

pub(crate) fn display_percent(percent: f64) -> i32 {
    if percent.is_finite() {
        percent.clamp(0.0, 100.0).round() as i32
    } else {
        0
    }
}

pub(crate) fn approximately(actual: u64, expected: u64) -> bool {
    actual >= expected.saturating_mul(95) / 100 && actual <= expected.saturating_mul(105) / 100
}

pub(crate) fn compact_usage_window_label(window: &UsageWindow, strings: Strings) -> String {
    if let Some(seconds) = window.duration_seconds.filter(|seconds| *seconds > 0) {
        if approximately(seconds, 5 * 60 * 60) {
            return "5h".to_string();
        }
        if approximately(seconds, 7 * 24 * 60 * 60) {
            return "7d".to_string();
        }
        if seconds % (24 * 60 * 60) == 0 {
            return format!("{}d", seconds / (24 * 60 * 60));
        }
        if seconds % (60 * 60) == 0 {
            return format!("{}h", seconds / (60 * 60));
        }
        if seconds % 60 == 0 {
            return format!("{}m", seconds / 60);
        }
        return format!("{seconds}s");
    }

    window
        .source_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| label.chars().take(8).collect())
        .unwrap_or_else(|| strings.quota_window.to_string())
}

pub(crate) fn selected_usage_windows(usage: &UsageData) -> Vec<&UsageWindow> {
    let mut selected: Vec<&UsageWindow> = usage.windows.iter().collect();
    if selected.len() > 2 {
        selected.sort_by(|left, right| {
            right
                .percentage
                .partial_cmp(&left.percentage)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        selected.truncate(2);
        selected.sort_by_key(|window| window.duration_seconds.unwrap_or(u64::MAX));
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localization::LanguageId;
    use crate::models::{
        AppUsageData, UsageData, UsageWindow, FIVE_HOURS_SECONDS, ONE_WEEK_SECONDS,
    };
    use std::time::{Duration, SystemTime};

    const ORDER: [TrayIconKind; 3] = [
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
    ];

    fn usage(windows: Vec<UsageWindow>) -> UsageData {
        UsageData::from_windows(windows)
    }

    fn data_with_claude(claude: UsageData) -> AppUsageData {
        AppUsageData {
            claude_code: Some(claude),
            ..Default::default()
        }
    }

    #[test]
    fn warning_and_zero_countdown_rules_are_shared() {
        let strings = LanguageId::English.strings();
        let resets = SystemTime::now().checked_add(Duration::from_secs(4 * 86_400));
        let data = data_with_claude(usage(vec![
            UsageWindow::new(0.0, resets, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(92.0, resets, Some(ONE_WEEK_SECONDS)),
        ]));
        let vm = build(Some(&data), strings, &ORDER, true, false, false);
        assert_eq!(vm.providers[0].attention, Attention::Warn);
        assert!(vm.providers[0].windows[0].countdown.is_empty());
        assert_eq!(vm.providers[0].windows[1].severity, Severity::Warn);
    }

    #[test]
    fn provider_errors_remain_visible_with_and_without_cached_usage() {
        let strings = LanguageId::English.strings();
        let cached = AppUsageData {
            codex: Some(usage(vec![UsageWindow::new(
                51.0,
                None,
                Some(ONE_WEEK_SECONDS),
            )])),
            codex_error: Some(ProviderStatus::RateLimited),
            ..Default::default()
        };
        let vm = build(Some(&cached), strings, &ORDER, false, true, false);
        assert_eq!(vm.providers[0].attention, Attention::Degraded);
        assert_eq!(vm.providers[0].windows[0].percent_text, "51%");

        let transient = AppUsageData {
            codex_error: Some(ProviderStatus::RequestFailed),
            ..Default::default()
        };
        let vm = build(Some(&transient), strings, &ORDER, false, true, false);
        assert_eq!(vm.providers[0].attention, Attention::Degraded);
        assert_eq!(vm.providers[0].placeholder.as_deref(), Some("--"));

        let unavailable = AppUsageData {
            codex_error: Some(ProviderStatus::AuthRequired),
            ..Default::default()
        };
        let vm = build(Some(&unavailable), strings, &ORDER, false, true, false);
        assert_eq!(vm.providers[0].attention, Attention::Error);
        assert_eq!(vm.providers[0].placeholder.as_deref(), Some("--"));
    }

    #[test]
    fn worst_window_ties_use_duration_not_input_order() {
        let strings = LanguageId::English.strings();
        let provider = ProviderView {
            kind: TrayIconKind::Claude,
            badge: None,
            windows: vec![
                window_view(
                    &UsageWindow::new(50.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
                window_view(
                    &UsageWindow::new(50.0, None, Some(FIVE_HOURS_SECONDS)),
                    strings,
                ),
            ],
            placeholder: None,
            attention: Attention::Normal,
        };
        assert_eq!(worst_window(&provider).unwrap().label, "5h");
    }

    #[test]
    fn badge_window_pins_five_hour_usage_until_another_window_warns() {
        let strings = LanguageId::English.strings();
        let provider = ProviderView {
            kind: TrayIconKind::Claude,
            badge: None,
            windows: vec![
                window_view(
                    &UsageWindow::new(53.0, None, Some(FIVE_HOURS_SECONDS)),
                    strings,
                ),
                window_view(
                    &UsageWindow::new(85.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
            ],
            placeholder: None,
            attention: Attention::Normal,
        };
        assert_eq!(badge_window(&provider).unwrap().label, "5h");

        let warned = ProviderView {
            windows: vec![
                window_view(
                    &UsageWindow::new(53.0, None, Some(FIVE_HOURS_SECONDS)),
                    strings,
                ),
                window_view(
                    &UsageWindow::new(92.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
            ],
            attention: Attention::Warn,
            ..provider
        };
        let selected = badge_window(&warned).unwrap();
        assert_eq!(selected.label, "7d");
        assert_eq!(selected.display_percent, 92);
    }

    #[test]
    fn badge_window_uses_shortest_available_window_when_five_hour_is_absent() {
        let strings = LanguageId::English.strings();
        let provider = ProviderView {
            kind: TrayIconKind::Codex,
            badge: None,
            windows: vec![
                window_view(
                    &UsageWindow::new(20.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
                window_view(&UsageWindow::new(40.0, None, Some(24 * 60 * 60)), strings),
            ],
            placeholder: None,
            attention: Attention::Normal,
        };
        assert_eq!(badge_window(&provider).unwrap().label, "1d");
    }

    #[test]
    fn badge_keeps_five_hour_window_even_when_floating_top_two_omit_it() {
        let strings = LanguageId::English.strings();
        let data = data_with_claude(usage(vec![
            UsageWindow::new(10.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(70.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(80.0, None, Some(ONE_WEEK_SECONDS)),
        ]));
        let vm = build(Some(&data), strings, &ORDER, true, false, false);
        let provider = &vm.providers[0];

        assert_eq!(provider.windows.len(), 2);
        assert!(provider.windows.iter().all(|window| window.label != "5h"));
        assert_eq!(badge_window(provider).unwrap().label, "5h");
    }

    #[test]
    fn labels_and_provider_order_are_language_independent() {
        let strings = LanguageId::Korean.strings();
        let data = AppUsageData {
            claude_code: Some(usage(vec![UsageWindow::new(10.0, None, Some(30 * 60))])),
            antigravity: Some(usage(vec![UsageWindow::new(
                1.0,
                None,
                Some(365 * 24 * 60 * 60),
            )])),
            ..Default::default()
        };
        let vm = build(Some(&data), strings, &ORDER, true, false, true);
        assert_eq!(vm.providers[0].kind, TrayIconKind::Claude);
        assert_eq!(vm.providers[0].windows[0].label, "30m");
        assert_eq!(vm.providers[1].windows[0].label, "365d");
    }

    #[test]
    fn placeholder_model_respects_visibility() {
        let vm = placeholder_model("--", &ORDER, true, false, true);
        assert_eq!(vm.providers.len(), 2);
        assert!(vm
            .providers
            .iter()
            .all(|provider| provider.placeholder.as_deref() == Some("--")));
    }
}
