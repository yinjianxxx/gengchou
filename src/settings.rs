use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS};

use crate::diagnose;
use crate::tray_icon::TrayIconKind;

pub const APP_DIR_NAME: &str = "Gengchou";

pub const POLL_1_MIN: u32 = 60_000;
pub const POLL_2_MIN: u32 = 120_000;
pub const POLL_5_MIN: u32 = 300_000;
pub const POLL_10_MIN: u32 = 600_000;
pub const POLL_15_MIN: u32 = 900_000;
pub const POLL_30_MIN: u32 = 1_800_000;
const SUPPORTED_POLL_INTERVALS: [u32; 6] = [
    POLL_1_MIN,
    POLL_2_MIN,
    POLL_5_MIN,
    POLL_10_MIN,
    POLL_15_MIN,
    POLL_30_MIN,
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SettingsFile {
    #[serde(default)]
    pub tray_offset: i32,
    #[serde(default)]
    pub taskbar_index: usize,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    pub widget_visible: bool,
    #[serde(default)]
    pub floating_visible: bool,
    #[serde(default = "default_detailed_tray_icons")]
    pub detailed_tray_icons: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_y: Option<i32>,
    #[serde(default = "default_show_claude_code")]
    pub show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    pub show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    pub show_antigravity: bool,
    #[serde(default = "default_provider_order")]
    pub provider_order: Vec<TrayIconKind>,
    #[serde(default)]
    pub notify_session_reset: bool,
    #[serde(default)]
    pub notify_weekly_reset: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            taskbar_index: 0,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            widget_visible: true,
            floating_visible: false,
            detailed_tray_icons: true,
            floating_x: None,
            floating_y: None,
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            provider_order: default_provider_order(),
            notify_session_reset: false,
            notify_weekly_reset: false,
        }
    }
}

pub fn default_provider_order() -> Vec<TrayIconKind> {
    vec![
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
    ]
}

fn default_poll_interval() -> u32 {
    POLL_5_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_detailed_tray_icons() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    true
}

fn default_show_codex() -> bool {
    false
}

fn default_show_antigravity() -> bool {
    false
}

fn normalize_provider_order(configured: &[TrayIconKind]) -> Vec<TrayIconKind> {
    let mut normalized = Vec::with_capacity(3);
    for kind in configured.iter().chain(default_provider_order().iter()) {
        if !normalized.contains(kind) {
            normalized.push(*kind);
        }
    }
    normalized
}

pub(crate) fn normalize(settings: &mut SettingsFile) -> Vec<&'static str> {
    let mut repaired = Vec::new();
    if settings.tray_offset < 0 {
        settings.tray_offset = 0;
        repaired.push("tray_offset");
    }
    if !SUPPORTED_POLL_INTERVALS.contains(&settings.poll_interval_ms) {
        settings.poll_interval_ms = default_poll_interval();
        repaired.push("poll_interval_ms");
    }
    if !settings.show_claude_code && !settings.show_codex && !settings.show_antigravity {
        settings.show_claude_code = true;
        repaired.push("enabled_providers");
    }
    let provider_order = normalize_provider_order(&settings.provider_order);
    if provider_order != settings.provider_order {
        settings.provider_order = provider_order;
        repaired.push("provider_order");
    }
    if settings
        .language
        .as_deref()
        .is_some_and(|language| language.trim().is_empty())
    {
        settings.language = None;
        repaired.push("language");
    }
    repaired
}

fn settings_path_for(app_dir_name: &str) -> PathBuf {
    app_data_dir(app_dir_name).join("settings.json")
}

fn app_data_dir(app_dir_name: &str) -> PathBuf {
    let appdata = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .expect("APPDATA must be available before settings are loaded");
    PathBuf::from(appdata).join(app_dir_name)
}

pub fn app_data_file(name: &str) -> PathBuf {
    app_data_dir(APP_DIR_NAME).join(name)
}

fn read_settings_content() -> Option<String> {
    let current_path = settings_path_for(APP_DIR_NAME);
    match std::fs::read_to_string(&current_path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            diagnose::log_error(
                &format!("settings read failed path={}", current_path.display()),
                error,
            );
            None
        }
    }
}

pub(crate) fn load() -> SettingsFile {
    let Some(content) = read_settings_content() else {
        return SettingsFile::default();
    };
    let mut settings: SettingsFile = match serde_json::from_str(&content) {
        Ok(settings) => settings,
        Err(error) => {
            diagnose::log(format!(
                "settings parse failed; using defaults without overwriting the file: {error}"
            ));
            return SettingsFile::default();
        }
    };
    let repaired = normalize(&mut settings);
    if !repaired.is_empty() {
        diagnose::log(format!("settings normalized fields={}", repaired.join(",")));
    }
    if !repaired.is_empty() {
        if let Err(error) = save(&settings) {
            diagnose::log_error("settings normalization save failed", error);
        }
    }
    settings
}

pub(crate) fn normalized_json(content: &str) -> Result<(SettingsFile, String), String> {
    let mut settings: SettingsFile =
        serde_json::from_str(content).map_err(|error| format!("invalid settings JSON: {error}"))?;
    normalize(&mut settings);
    let json = serde_json::to_string_pretty(&settings)
        .map_err(|error| format!("unable to serialize normalized settings: {error}"))?;
    Ok((settings, json))
}

pub(crate) fn write_file_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    if let Err(error) = std::fs::write(&tmp, contents) {
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {}
            Err(cleanup_error) => diagnose::log_error(
                &format!("atomic write cleanup failed path={}", tmp.display()),
                cleanup_error,
            ),
        }
        return Err(error);
    }
    let replace_result = if path.exists() {
        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let tmp_wide: Vec<u16> = tmp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            ReplaceFileW(
                PCWSTR::from_raw(path_wide.as_ptr()),
                PCWSTR::from_raw(tmp_wide.as_ptr()),
                PCWSTR::null(),
                REPLACE_FILE_FLAGS(0),
                None,
                None,
            )
        }
        .map_err(io::Error::other)
    } else {
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = replace_result {
        if let Err(cleanup_error) = std::fs::remove_file(&tmp) {
            if cleanup_error.kind() != io::ErrorKind::NotFound {
                diagnose::log_error(
                    &format!("atomic write cleanup failed path={}", tmp.display()),
                    cleanup_error,
                );
            }
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) fn save(settings: &SettingsFile) -> io::Result<()> {
    let json = serde_json::to_string_pretty(settings).map_err(io::Error::other)?;
    let path = settings_path_for(APP_DIR_NAME);
    write_file_atomic(&path, &json)
        .map_err(|error| io::Error::new(error.kind(), format!("path={}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_order_is_deduplicated_and_completed() {
        assert_eq!(
            normalize_provider_order(&[TrayIconKind::Codex, TrayIconKind::Codex]),
            vec![
                TrayIconKind::Codex,
                TrayIconKind::Claude,
                TrayIconKind::Antigravity,
            ]
        );
    }

    #[test]
    fn unsafe_or_unsupported_values_are_repaired() {
        let mut settings = SettingsFile {
            tray_offset: -1,
            poll_interval_ms: 0,
            language: Some("  ".to_string()),
            show_claude_code: false,
            show_codex: false,
            show_antigravity: false,
            provider_order: vec![TrayIconKind::Antigravity, TrayIconKind::Antigravity],
            ..SettingsFile::default()
        };

        let repaired = normalize(&mut settings);

        assert_eq!(settings.tray_offset, 0);
        assert_eq!(settings.poll_interval_ms, POLL_5_MIN);
        assert_eq!(settings.language, None);
        assert!(settings.show_claude_code);
        assert_eq!(
            settings.provider_order,
            vec![
                TrayIconKind::Antigravity,
                TrayIconKind::Claude,
                TrayIconKind::Codex,
            ]
        );
        assert_eq!(repaired.len(), 5);
    }

    #[test]
    fn all_refresh_menu_intervals_are_preserved() {
        for poll_interval_ms in SUPPORTED_POLL_INTERVALS {
            let mut settings = SettingsFile {
                poll_interval_ms,
                ..SettingsFile::default()
            };

            assert!(!normalize(&mut settings).contains(&"poll_interval_ms"));
            assert_eq!(settings.poll_interval_ms, poll_interval_ms);
        }
    }

    #[test]
    fn removed_one_hour_interval_migrates_to_five_minute_default() {
        let mut settings = SettingsFile {
            poll_interval_ms: 60 * 60 * 1_000,
            ..SettingsFile::default()
        };

        let repaired = normalize(&mut settings);

        assert_eq!(settings.poll_interval_ms, POLL_5_MIN);
        assert_eq!(repaired, vec!["poll_interval_ms"]);
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(serde_json::from_str::<SettingsFile>("{not-json").is_err());
    }

    #[test]
    fn older_settings_default_floating_monitor_to_hidden() {
        let settings: SettingsFile = serde_json::from_str(
            r#"{
                "widget_visible": true,
                "show_claude_code": true,
                "provider_order": ["claude", "codex", "antigravity"]
            }"#,
        )
        .expect("older settings should remain readable");

        assert!(!settings.floating_visible);
        assert!(settings.detailed_tray_icons);
        assert_eq!(settings.floating_x, None);
        assert_eq!(settings.floating_y, None);
    }

    #[test]
    fn atomic_write_creates_parent_and_replaces_existing_file() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gengchou-settings-test-{}-{nonce}",
            std::process::id(),
        ));
        let path = root.join("nested").join("settings.json");

        write_file_atomic(&path, "first").expect("initial atomic write");
        write_file_atomic(&path, "second").expect("replacement atomic write");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn atomic_write_reports_an_unwritable_parent() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gengchou-settings-error-test-{}-{nonce}",
            std::process::id(),
        ));
        std::fs::write(&root, "not a directory").unwrap();

        let error = write_file_atomic(&root.join("settings.json"), "{}")
            .expect_err("a file cannot be used as the parent directory");

        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_file(root);
    }
}
