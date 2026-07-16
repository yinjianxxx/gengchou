//! Pure, DPI-aware layout for the compact taskbar and floating surfaces.
//!
//! The caller supplies already-scaled metrics and text measurements. The
//! resulting scene therefore uses target-device pixels and must not be scaled
//! again by the GDI execution layer.

use crate::compact_view::{badge_window, Attention, CompactViewModel, ProviderView, Severity};
use crate::tray_icon::TrayIconKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadgeHit {
    pub(crate) kind: TrayIconKind,
    pub(crate) rect: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FontKey {
    Data12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TileSize {
    Chip16,
    Chip20,
    Chip28,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorKey {
    PillBg,
    PillBgWarn,
    PillText,
    PillAlertText,
    AuxText,
    CanvasWarnPrimary,
    CanvasWarnSecondary,
    NeutralText,
    GaugeTrack,
    GaugeAccent(TrayIconKind),
    GaugeWarn,
    Separator,
    HighContrastText,
    ErrorText,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DrawCmd {
    RoundRect {
        rect: Rect,
        color: ColorKey,
        radius: i32,
    },
    StrokeRoundRect {
        rect: Rect,
        color: ColorKey,
        radius: i32,
        width: i32,
    },
    GaugeFill {
        track: Rect,
        fraction: f64,
        color: ColorKey,
        radius: i32,
    },
    Text {
        rect: Rect,
        text: String,
        font: FontKey,
        color: ColorKey,
    },
    ProviderTile {
        rect: Rect,
        kind: TrayIconKind,
        size: TileSize,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Scene {
    pub(crate) cmds: Vec<DrawCmd>,
    pub(crate) badge_hits: Vec<BadgeHit>,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Metrics {
    pub(crate) taskbar_h: i32,
    pub(crate) floating_h: i32,
    pub(crate) pill_h: i32,
    pub(crate) pill_pad_x: i32,
    pub(crate) chip16: i32,
    pub(crate) chip_gap: i32,
    pub(crate) badge_gap: i32,
    pub(crate) badge_right_pad: i32,
    pub(crate) badge_text_gap: i32,
    pub(crate) border_w: i32,
    pub(crate) status_w: i32,
    pub(crate) status_gap: i32,
    pub(crate) chip20: i32,
    pub(crate) group_chip_gap: i32,
    pub(crate) label_min_w: i32,
    pub(crate) label_max_w: i32,
    pub(crate) label_gap: i32,
    pub(crate) separator_w: i32,
    pub(crate) row_text_h: i32,
    pub(crate) gauge_min_w: i32,
    pub(crate) gauge_h: i32,
    pub(crate) gauge_top_gap: i32,
    pub(crate) unit_gap: i32,
    pub(crate) sep_margin: i32,
    pub(crate) sep_h: i32,
    pub(crate) rows_left_pad: i32,
    pub(crate) rows_right_pad: i32,
}

impl Metrics {
    pub(crate) fn logical() -> Self {
        Self {
            taskbar_h: 46,
            floating_h: 52,
            pill_h: 32,
            pill_pad_x: 8,
            chip16: 16,
            chip_gap: 5,
            badge_gap: 6,
            badge_right_pad: 8,
            badge_text_gap: 3,
            border_w: 1,
            status_w: 8,
            status_gap: 2,
            chip20: 20,
            group_chip_gap: 5,
            label_min_w: 16,
            label_max_w: 32,
            label_gap: 2,
            separator_w: 8,
            row_text_h: 16,
            gauge_min_w: 44,
            gauge_h: 3,
            gauge_top_gap: 1,
            unit_gap: 4,
            sep_margin: 5,
            sep_h: 18,
            rows_left_pad: 0,
            rows_right_pad: 8,
        }
    }
}

pub(crate) type MeasureText<'a> = &'a dyn Fn(FontKey, &str) -> i32;

fn provider_abbrev(kind: TrayIconKind) -> &'static str {
    match kind {
        TrayIconKind::Claude => "CL",
        TrayIconKind::Codex => "CX",
        TrayIconKind::Antigravity => "AG",
    }
}

pub(crate) fn layout_badges(
    vm: &CompactViewModel,
    m: &Metrics,
    high_contrast: bool,
    measure: MeasureText,
) -> Scene {
    let mut cmds = Vec::new();
    let mut badge_hits = Vec::new();
    let mut x = 0;
    let pill_y = (m.taskbar_h - m.pill_h) / 2;
    for (index, provider) in vm.providers.iter().enumerate() {
        let selected = badge_window(provider);
        let value = selected
            .map(|window| window.percent_text.as_str())
            .or(provider.placeholder.as_deref())
            .unwrap_or("--");
        let label = selected.map(|window| window.label.as_str());
        let identity_w = if high_contrast {
            measure(FontKey::Data12, provider_abbrev(provider.kind))
        } else {
            m.chip16
        };
        let value_w = measure(FontKey::Data12, value);
        let label_w = label
            .map(|text| measure(FontKey::Data12, text) + m.label_gap)
            .unwrap_or(0);
        let context = match provider.attention {
            Attention::Warn => selected
                .filter(|window| !window.countdown.is_empty())
                .map(|window| window.countdown.as_str()),
            Attention::Error => Some("!"),
            Attention::Normal => None,
        };
        let context_w = context
            .map(|text| m.badge_text_gap + measure(FontKey::Data12, text))
            .unwrap_or(0);
        let pill_w = m.pill_pad_x * 2 + identity_w + m.chip_gap + label_w + value_w + context_w;
        let pill = Rect {
            x,
            y: pill_y,
            w: pill_w,
            h: m.pill_h,
        };
        badge_hits.push(BadgeHit {
            kind: provider.kind,
            rect: pill,
        });
        // Provider errors describe data freshness; quota severity still comes
        // from the selected window. A cached 51% should stay neutral, while a
        // cached 92% remains visibly near its limit and also carries `!`.
        let quota_warn = selected.is_some_and(|window| window.severity == Severity::Warn);
        cmds.push(DrawCmd::RoundRect {
            rect: pill,
            color: if quota_warn {
                ColorKey::PillBgWarn
            } else {
                ColorKey::PillBg
            },
            radius: m.pill_h / 2,
        });
        if high_contrast {
            cmds.push(DrawCmd::StrokeRoundRect {
                rect: pill,
                color: ColorKey::HighContrastText,
                radius: m.pill_h / 2,
                width: m.border_w,
            });
        }

        let identity_x = x + m.pill_pad_x;
        if high_contrast {
            cmds.push(DrawCmd::Text {
                rect: Rect {
                    x: identity_x,
                    y: pill_y,
                    w: identity_w,
                    h: m.pill_h,
                },
                text: provider_abbrev(provider.kind).to_string(),
                font: FontKey::Data12,
                color: if quota_warn {
                    ColorKey::PillAlertText
                } else {
                    ColorKey::HighContrastText
                },
            });
        } else {
            cmds.push(DrawCmd::ProviderTile {
                rect: Rect {
                    x: identity_x,
                    y: pill_y + (m.pill_h - m.chip16) / 2,
                    w: m.chip16,
                    h: m.chip16,
                },
                kind: provider.kind,
                size: TileSize::Chip16,
            });
        }
        let content_x = identity_x + identity_w + m.chip_gap;
        if let Some(label) = label {
            cmds.push(DrawCmd::Text {
                rect: Rect {
                    x: content_x,
                    y: pill_y,
                    w: label_w - m.label_gap,
                    h: m.pill_h,
                },
                text: label.to_string(),
                font: FontKey::Data12,
                color: if high_contrast {
                    if quota_warn {
                        ColorKey::PillAlertText
                    } else {
                        ColorKey::HighContrastText
                    }
                } else {
                    ColorKey::AuxText
                },
            });
        }
        let value_x = content_x + label_w;
        cmds.push(DrawCmd::Text {
            rect: Rect {
                x: value_x,
                y: pill_y,
                w: value_w,
                h: m.pill_h,
            },
            text: value.to_string(),
            font: FontKey::Data12,
            color: if quota_warn {
                ColorKey::PillAlertText
            } else if provider.placeholder.is_some() {
                ColorKey::AuxText
            } else {
                ColorKey::PillText
            },
        });
        if let Some(context) = context {
            cmds.push(DrawCmd::Text {
                rect: Rect {
                    x: value_x + value_w + m.badge_text_gap,
                    y: pill_y,
                    w: context_w - m.badge_text_gap,
                    h: m.pill_h,
                },
                text: context.to_string(),
                font: FontKey::Data12,
                color: if quota_warn {
                    ColorKey::PillAlertText
                } else if provider.attention == Attention::Error {
                    ColorKey::ErrorText
                } else {
                    ColorKey::PillAlertText
                },
            });
        }

        x += pill_w;
        if index + 1 < vm.providers.len() {
            x += m.badge_gap;
        }
    }
    Scene {
        cmds,
        badge_hits,
        width: x + m.badge_right_pad,
        height: m.taskbar_h,
    }
}

pub(crate) fn layout_provider_rows(
    vm: &CompactViewModel,
    m: &Metrics,
    high_contrast: bool,
    measure: MeasureText,
) -> Scene {
    let mut cmds = Vec::new();
    let mut x = m.rows_left_pad;
    let unit_h = m.row_text_h + m.gauge_top_gap + m.gauge_h;
    let row_measures = vm
        .providers
        .iter()
        .map(|provider| measure_provider_rows(provider, m, high_contrast, measure))
        .collect::<Vec<_>>();
    let pct_col_w = row_measures
        .iter()
        .map(|row_measure| row_measure.pct_col_w)
        .max()
        .unwrap_or_default();
    let countdown_col_w = row_measures
        .iter()
        .map(|row_measure| row_measure.countdown_col_w)
        .max()
        .unwrap_or_default();
    let payload_w = pct_col_w
        + if countdown_col_w > 0 {
            m.separator_w + countdown_col_w
        } else {
            0
        };
    let gauge_w = payload_w.max(m.gauge_min_w);
    for ((index, provider), row_measure) in vm.providers.iter().enumerate().zip(&row_measures) {
        let chip_x = x;
        let identity_w = row_measure.identity_w;
        if high_contrast {
            cmds.push(DrawCmd::Text {
                rect: Rect {
                    x: chip_x,
                    y: 0,
                    w: identity_w,
                    h: m.floating_h,
                },
                text: provider_abbrev(provider.kind).to_string(),
                font: FontKey::Data12,
                color: ColorKey::HighContrastText,
            });
        } else {
            cmds.push(DrawCmd::ProviderTile {
                rect: Rect {
                    x: chip_x,
                    y: (m.floating_h - m.chip20) / 2,
                    w: m.chip20,
                    h: m.chip20,
                },
                kind: provider.kind,
                size: TileSize::Chip20,
            });
        }
        let identity_right = chip_x + identity_w;
        let status_space = if provider.attention == Attention::Error {
            let status_x = identity_right + m.status_gap;
            cmds.push(DrawCmd::Text {
                rect: Rect {
                    x: status_x,
                    y: 0,
                    w: m.status_w,
                    h: m.floating_h,
                },
                text: "!".to_string(),
                font: FontKey::Data12,
                color: ColorKey::ErrorText,
            });
            m.status_gap + m.status_w
        } else {
            0
        };
        let content_x = identity_right + status_space + m.group_chip_gap;

        let content_w = if let Some(placeholder) = &provider.placeholder {
            let placeholder_w = measure(FontKey::Data12, placeholder);
            cmds.push(DrawCmd::Text {
                rect: Rect {
                    x: content_x,
                    y: 0,
                    w: placeholder_w,
                    h: m.floating_h,
                },
                text: placeholder.clone(),
                font: FontKey::Data12,
                color: ColorKey::AuxText,
            });
            placeholder_w
        } else {
            let label_w = row_measure.label_w;
            let pct_col_x = content_x + label_w + m.label_gap;
            let content_w = label_w + m.label_gap + gauge_w;
            let units = provider.windows.len() as i32;
            let block_h = units * unit_h + (units - 1).max(0) * m.unit_gap;
            let mut y = (m.floating_h - block_h) / 2;
            for window in &provider.windows {
                let warn = window.severity == Severity::Warn;
                cmds.push(DrawCmd::Text {
                    rect: Rect {
                        x: content_x,
                        y,
                        w: label_w,
                        h: m.row_text_h,
                    },
                    text: window.label.clone(),
                    font: FontKey::Data12,
                    color: ColorKey::AuxText,
                });
                let pct_w = measure(FontKey::Data12, &window.percent_text);
                let pct_x = pct_col_x;
                cmds.push(DrawCmd::Text {
                    rect: Rect {
                        x: pct_x,
                        y,
                        w: pct_w,
                        h: m.row_text_h,
                    },
                    text: window.percent_text.clone(),
                    font: FontKey::Data12,
                    color: if warn {
                        ColorKey::CanvasWarnPrimary
                    } else {
                        ColorKey::NeutralText
                    },
                });
                if !window.countdown.is_empty() {
                    let countdown = window
                        .countdown
                        .strip_prefix('\u{00b7}')
                        .unwrap_or(&window.countdown);
                    let countdown_w = measure(FontKey::Data12, countdown);
                    let countdown_x = pct_col_x + gauge_w - countdown_w;
                    let separator_w = measure(FontKey::Data12, "\u{00b7}");
                    let percent_right = pct_x + pct_w;
                    let visual_gap = (countdown_x - percent_right).max(separator_w);
                    let separator_x = percent_right + (visual_gap - separator_w) / 2;
                    cmds.push(DrawCmd::Text {
                        rect: Rect {
                            x: separator_x,
                            y,
                            w: separator_w,
                            h: m.row_text_h,
                        },
                        text: "\u{00b7}".to_string(),
                        font: FontKey::Data12,
                        color: if warn {
                            ColorKey::CanvasWarnSecondary
                        } else {
                            ColorKey::AuxText
                        },
                    });
                    cmds.push(DrawCmd::Text {
                        rect: Rect {
                            x: countdown_x,
                            y,
                            w: countdown_w,
                            h: m.row_text_h,
                        },
                        text: countdown.to_string(),
                        font: FontKey::Data12,
                        color: if warn {
                            ColorKey::CanvasWarnSecondary
                        } else {
                            ColorKey::AuxText
                        },
                    });
                }

                // The gauge shares the percentage column's left anchor and the
                // countdown column's right anchor. Every row therefore keeps
                // the same start, end, and length regardless of digit count.
                let track = Rect {
                    x: pct_x,
                    y: y + m.row_text_h + m.gauge_top_gap,
                    w: gauge_w,
                    h: m.gauge_h,
                };
                cmds.push(DrawCmd::RoundRect {
                    rect: track,
                    color: ColorKey::GaugeTrack,
                    radius: m.gauge_h / 2,
                });
                let fraction = window.percent.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
                if fraction > 0.0 {
                    cmds.push(DrawCmd::GaugeFill {
                        track,
                        fraction,
                        color: if warn {
                            ColorKey::GaugeWarn
                        } else {
                            ColorKey::GaugeAccent(provider.kind)
                        },
                        radius: m.gauge_h / 2,
                    });
                }
                y += unit_h + m.unit_gap;
            }
            content_w
        };

        x = content_x + content_w;
        if index + 1 < vm.providers.len() {
            let separator_x = x + m.sep_margin;
            cmds.push(DrawCmd::RoundRect {
                rect: Rect {
                    x: separator_x,
                    y: (m.floating_h - m.sep_h) / 2,
                    w: 1,
                    h: m.sep_h,
                },
                color: ColorKey::Separator,
                radius: 0,
            });
            x = separator_x + 1 + m.sep_margin;
        }
    }
    Scene {
        cmds,
        badge_hits: Vec::new(),
        width: x + m.rows_right_pad,
        height: m.floating_h,
    }
}

fn measured_label_width(provider: &ProviderView, m: &Metrics, measure: MeasureText) -> i32 {
    provider
        .windows
        .iter()
        .map(|window| measure(FontKey::Data12, &window.label))
        .max()
        .unwrap_or(m.label_min_w)
        .clamp(m.label_min_w, m.label_max_w)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProviderRowsMeasure {
    identity_w: i32,
    label_w: i32,
    pct_col_w: i32,
    countdown_col_w: i32,
}

fn measure_provider_rows(
    provider: &ProviderView,
    m: &Metrics,
    high_contrast: bool,
    measure: MeasureText,
) -> ProviderRowsMeasure {
    let identity_w = if high_contrast {
        measure(FontKey::Data12, provider_abbrev(provider.kind))
    } else {
        m.chip20
    };
    let label_w = measured_label_width(provider, m, measure);
    let pct_col_w = provider
        .windows
        .iter()
        .map(|window| measure(FontKey::Data12, &window.percent_text))
        .max()
        .unwrap_or_default();
    let countdown_col_w = provider
        .windows
        .iter()
        .filter(|window| !window.countdown.is_empty())
        .map(|window| {
            let countdown = window
                .countdown
                .strip_prefix('\u{00b7}')
                .unwrap_or(&window.countdown);
            measure(FontKey::Data12, countdown)
        })
        .max()
        .unwrap_or_default();
    ProviderRowsMeasure {
        identity_w,
        label_w,
        pct_col_w,
        countdown_col_w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact_view::{Attention, ProviderView, Severity, WindowView};

    fn fake_measure(font: FontKey, text: &str) -> i32 {
        let per_char = match font {
            FontKey::Data12 => 6,
        };
        per_char * text.chars().count() as i32
    }

    fn window(label: &str, pct: f64, countdown: &str, severity: Severity) -> WindowView {
        WindowView {
            label: label.to_string(),
            percent: Some(pct),
            display_percent: pct.round() as i32,
            percent_text: format!("{pct:.0}%"),
            countdown: countdown.to_string(),
            duration_seconds: None,
            severity,
        }
    }

    fn provider(
        kind: TrayIconKind,
        windows: Vec<WindowView>,
        attention: Attention,
    ) -> ProviderView {
        ProviderView {
            kind,
            badge: None,
            windows,
            placeholder: None,
            attention,
        }
    }

    #[test]
    fn badge_width_tracks_one_line_attention_context() {
        let m = Metrics::logical();
        let model = |attention, severity| CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![window("7d", 92.0, "\u{00b7}4d", severity)],
                attention,
            )],
        };
        let normal = layout_badges(
            &model(Attention::Normal, Severity::Normal),
            &m,
            false,
            &fake_measure,
        );
        let warn = layout_badges(
            &model(Attention::Warn, Severity::Warn),
            &m,
            false,
            &fake_measure,
        );
        let error = layout_badges(
            &model(Attention::Error, Severity::Normal),
            &m,
            false,
            &fake_measure,
        );
        assert_eq!(
            warn.width - normal.width,
            m.badge_text_gap + fake_measure(FontKey::Data12, "\u{00b7}4d")
        );
        assert_eq!(
            error.width - normal.width,
            m.badge_text_gap + fake_measure(FontKey::Data12, "!")
        );
        let pill_y = (m.taskbar_h - m.pill_h) / 2;
        for rect in warn.cmds.iter().filter_map(|cmd| match cmd {
            DrawCmd::Text { rect, .. } => Some(*rect),
            _ => None,
        }) {
            assert_eq!(rect.y, pill_y);
            assert_eq!(rect.h, m.pill_h);
        }
    }

    #[test]
    fn warning_badge_shows_window_label_and_countdown() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![window("7d", 92.0, "\u{00b7}4d", Severity::Warn)],
                Attention::Warn,
            )],
        };
        let scene = layout_badges(&model, &m, false, &fake_measure);
        let texts = scene
            .cmds
            .iter()
            .filter_map(|cmd| match cmd {
                DrawCmd::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(texts.contains(&"92%"));
        assert!(texts.contains(&"\u{00b7}4d"));
        assert!(texts.contains(&"7d"));
        assert_eq!(scene.badge_hits.len(), 1);
        assert_eq!(scene.badge_hits[0].kind, TrayIconKind::Claude);
    }

    #[test]
    fn error_badge_stays_neutral_and_floating_marker_follows_identity() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Codex,
                vec![window("7d", 51.0, "\u{00b7}6d", Severity::Normal)],
                Attention::Error,
            )],
        };
        let badge = layout_badges(&model, &m, false, &fake_measure);
        assert!(badge.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::RoundRect {
                color: ColorKey::PillBg,
                ..
            }
        )));
        assert!(badge.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::Text { text, color: ColorKey::PillText, .. } if text == "51%"
        )));
        assert!(badge.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::Text { text, color: ColorKey::ErrorText, .. } if text == "!"
        )));
        assert!(!badge.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::RoundRect {
                color: ColorKey::PillBgWarn,
                ..
            }
        )));

        let floating = layout_provider_rows(&model, &m, false, &fake_measure);
        let chip = floating.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::ProviderTile { rect, .. } => Some(*rect),
            _ => None,
        });
        let marker = floating.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::Text { rect, text, .. } if text == "!" => Some(*rect),
            _ => None,
        });
        let label = floating.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::Text { rect, text, .. } if text == "7d" => Some(*rect),
            _ => None,
        });
        let chip = chip.unwrap();
        let marker = marker.unwrap();
        let label = label.unwrap();
        assert!(chip.x + chip.w <= marker.x);
        assert!(marker.x + marker.w <= label.x);
    }

    #[test]
    fn quota_warning_keeps_alert_style_when_provider_also_has_an_error() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![window("7d", 92.0, "\u{00b7}4d", Severity::Warn)],
                Attention::Error,
            )],
        };
        let badge = layout_badges(&model, &m, true, &fake_measure);
        assert!(badge.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::RoundRect {
                color: ColorKey::PillBgWarn,
                ..
            }
        )));
        for text in ["CL", "92%", "!"] {
            assert!(badge.cmds.iter().any(|cmd| matches!(
                cmd,
                DrawCmd::Text { text: actual, color: ColorKey::PillAlertText, .. }
                    if actual == text
            )));
        }
    }

    #[test]
    fn high_contrast_warning_uses_text_roles_for_each_background() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![window("7d", 92.0, "\u{00b7}4d", Severity::Warn)],
                Attention::Warn,
            )],
        };
        let badge = layout_badges(&model, &m, true, &fake_measure);
        for text in ["CL", "7d", "92%", "\u{00b7}4d"] {
            assert!(badge.cmds.iter().any(|cmd| matches!(
                cmd,
                DrawCmd::Text { text: actual, color: ColorKey::PillAlertText, .. }
                    if actual == text
            )));
        }

        let floating = layout_provider_rows(&model, &m, true, &fake_measure);
        assert!(floating.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::Text { text, color: ColorKey::CanvasWarnPrimary, .. }
                if text == "92%"
        )));
        for expected in ["\u{00b7}", "4d"] {
            assert!(floating.cmds.iter().any(|cmd| matches!(
                cmd,
                DrawCmd::Text { text, color: ColorKey::CanvasWarnSecondary, .. }
                    if text == expected
            )));
        }
    }

    #[test]
    fn high_contrast_badges_have_a_real_outline() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Antigravity,
                vec![window("7d", 1.0, "\u{00b7}2d", Severity::Normal)],
                Attention::Normal,
            )],
        };
        let scene = layout_badges(&model, &m, true, &fake_measure);
        assert!(scene
            .cmds
            .iter()
            .any(|cmd| matches!(cmd, DrawCmd::StrokeRoundRect { .. })));
        assert!(!scene
            .cmds
            .iter()
            .any(|cmd| matches!(cmd, DrawCmd::ProviderTile { .. })));
    }

    #[test]
    fn floating_layout_measures_long_labels_and_leaves_vertical_padding() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![
                    window("30m", 64.0, "\u{00b7}3h", Severity::Normal),
                    window("365d", 92.0, "\u{00b7}4d", Severity::Warn),
                ],
                Attention::Warn,
            )],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        assert_eq!(scene.height, 52);
        let label_rect = scene.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::Text { rect, text, .. } if text == "365d" => Some(*rect),
            _ => None,
        });
        assert_eq!(label_rect.unwrap().w, 24);
        let first_text_y = scene.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::Text { rect, text, .. } if text == "30m" => Some(rect.y),
            _ => None,
        });
        assert!(first_text_y.unwrap() > 0);
    }

    #[test]
    fn floating_gauge_underlines_the_numeric_payload_not_the_label() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![window("5h", 64.0, "\u{00b7}3h", Severity::Normal)],
                Attention::Normal,
            )],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        let label = scene.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::Text { rect, text, .. } if text == "5h" => Some(*rect),
            _ => None,
        });
        let track = scene.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::RoundRect {
                rect,
                color: ColorKey::GaugeTrack,
                ..
            } => Some(*rect),
            _ => None,
        });
        let label = label.unwrap();
        let track = track.unwrap();
        assert_eq!(track.x, label.x + label.w + m.label_gap);
        assert!(track.y >= label.y + label.h);
    }

    #[test]
    fn floating_centers_separator_between_actual_text_and_right_aligns_countdown() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![
                    window("5h", 8.0, "\u{00b7}2h", Severity::Normal),
                    window("7d", 78.0, "\u{00b7}59m", Severity::Normal),
                ],
                Attention::Normal,
            )],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        let text_rect = |target: &str| {
            scene.cmds.iter().find_map(|cmd| match cmd {
                DrawCmd::Text { rect, text, .. } if text == target => Some(*rect),
                _ => None,
            })
        };
        let short_pct = text_rect("8%").unwrap();
        let long_pct = text_rect("78%").unwrap();
        let first_countdown = text_rect("2h").unwrap();
        let second_countdown = text_rect("59m").unwrap();
        let separators = scene
            .cmds
            .iter()
            .filter_map(|cmd| match cmd {
                DrawCmd::Text { rect, text, .. } if text == "\u{00b7}" => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(short_pct.x, long_pct.x);
        assert_eq!(
            first_countdown.x + first_countdown.w,
            second_countdown.x + second_countdown.w
        );
        assert_eq!(separators.len(), 2);
        for (percent, separator, countdown) in [
            (short_pct, separators[0], first_countdown),
            (long_pct, separators[1], second_countdown),
        ] {
            let text_gap_midpoint_twice = percent.x + percent.w + countdown.x;
            let separator_midpoint_twice = 2 * separator.x + separator.w;
            assert!((separator_midpoint_twice - text_gap_midpoint_twice).abs() <= 1);
        }
    }

    #[test]
    fn floating_gauge_keeps_one_start_end_and_length_for_mixed_digit_counts() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Claude,
                vec![
                    window("5h", 0.0, "", Severity::Normal),
                    window("7d", 29.0, "\u{00b7}5d", Severity::Normal),
                ],
                Attention::Normal,
            )],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        let text_rect = |target: &str| {
            scene.cmds.iter().find_map(|cmd| match cmd {
                DrawCmd::Text { rect, text, .. } if text == target => Some(*rect),
                _ => None,
            })
        };
        let tracks = scene
            .cmds
            .iter()
            .filter_map(|cmd| match cmd {
                DrawCmd::RoundRect {
                    rect,
                    color: ColorKey::GaugeTrack,
                    ..
                } => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>();

        let zero = text_rect("0%").unwrap();
        let twenty_nine = text_rect("29%").unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].x, zero.x);
        assert_eq!(tracks[1].x, twenty_nine.x);
        assert_eq!(tracks[0].x, tracks[1].x);
        assert_eq!(tracks[0].w, tracks[1].w);
        assert_eq!(tracks[0].x + tracks[0].w, tracks[1].x + tracks[1].w);
    }

    #[test]
    fn floating_gauges_share_one_scale_across_providers() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![
                provider(
                    TrayIconKind::Claude,
                    vec![window("5h", 100.0, "\u{00b7}59m", Severity::Warn)],
                    Attention::Warn,
                ),
                provider(
                    TrayIconKind::Codex,
                    vec![window("7d", 51.0, "\u{00b7}6d", Severity::Normal)],
                    Attention::Normal,
                ),
            ],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        let tracks = scene
            .cmds
            .iter()
            .filter_map(|cmd| match cmd {
                DrawCmd::RoundRect {
                    rect,
                    color: ColorKey::GaugeTrack,
                    ..
                } => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].w, tracks[1].w);
        assert_eq!(tracks[0].w, 50);
        assert!(tracks[0].w >= m.gauge_min_w);
    }

    #[test]
    fn floating_short_group_does_not_reserve_the_label_width_twice() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Antigravity,
                vec![window("5h", 0.0, "", Severity::Normal)],
                Attention::Normal,
            )],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        let track = scene.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::RoundRect {
                rect,
                color: ColorKey::GaugeTrack,
                ..
            } => Some(*rect),
            _ => None,
        });
        let track = track.unwrap();
        assert_eq!(scene.width, track.x + track.w + m.rows_right_pad);
    }

    #[test]
    fn floating_placeholder_does_not_reserve_a_hidden_gauge() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![ProviderView {
                kind: TrayIconKind::Codex,
                badge: None,
                windows: Vec::new(),
                placeholder: Some("--".to_string()),
                attention: Attention::Normal,
            }],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        let placeholder = scene.cmds.iter().find_map(|cmd| match cmd {
            DrawCmd::Text { rect, text, .. } if text == "--" => Some(*rect),
            _ => None,
        });
        let placeholder = placeholder.unwrap();
        assert_eq!(placeholder.w, fake_measure(FontKey::Data12, "--"));
        assert_eq!(
            scene.width,
            placeholder.x + placeholder.w + m.rows_right_pad
        );
        assert!(!scene
            .cmds
            .iter()
            .any(|cmd| matches!(cmd, DrawCmd::GaugeFill { .. })));
    }

    #[test]
    fn zero_percent_draws_track_without_fill() {
        let m = Metrics::logical();
        let model = CompactViewModel {
            providers: vec![provider(
                TrayIconKind::Antigravity,
                vec![window("5h", 0.0, "", Severity::Normal)],
                Attention::Normal,
            )],
        };
        let scene = layout_provider_rows(&model, &m, false, &fake_measure);
        assert!(scene.cmds.iter().any(|cmd| matches!(
            cmd,
            DrawCmd::RoundRect {
                color: ColorKey::GaugeTrack,
                ..
            }
        )));
        assert!(!scene
            .cmds
            .iter()
            .any(|cmd| matches!(cmd, DrawCmd::GaugeFill { .. })));
    }
}
