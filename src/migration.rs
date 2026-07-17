use std::collections::HashSet;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

use crate::settings::{self, SettingsFile};
use crate::updater;

const CURRENT_DIR_NAME: &str = "Gengchou";
const OWNED_RETIRED_DIR_NAME: &str = "AIUsageMonitor";
const SETTINGS_SOURCE_DIR_NAMES: [&str; 2] = ["AIUsageMonitor", "ClaudeCodexUsageMonitor"];
const OWNED_RETIRED_RUN_VALUE_NAME: &str = "AIUsageMonitor";
const CURRENT_RUN_VALUE_NAME: &str = "Gengchou";
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STATE_FILE_NAME: &str = "migration-v2.2.4.json";
const CACHE_FILE_NAME: &str = "usage-cache.json";
const CACHE_MAX_AGE_SECS: u64 = 48 * 60 * 60;
const FILE_ATTRIBUTE_REPARSE_POINT_VALUE: u32 = 0x0000_0400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationStage {
    Prepared,
    ReadySeen,
    Complete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MigrationState {
    schema_version: u32,
    stage: MigrationStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_settings_sha256: Option<String>,
    current_settings_sha256: String,
    #[serde(default)]
    settings_pending: bool,
}

#[derive(Clone, Debug)]
struct Roots {
    appdata: PathBuf,
    local_appdata: PathBuf,
}

#[derive(Debug)]
struct SettingsPreparation {
    source_hash: Option<String>,
    current_hash: String,
    source_changed: bool,
}

pub(crate) struct PreparationReport {
    pub messages: Vec<String>,
    pub updates_allowed: bool,
}

pub(crate) fn prepare_for_ui(update_ready_requested: bool) -> Result<PreparationReport, String> {
    let roots = Roots::from_env()?;
    validate_migration_directories(&roots)?;
    let current_exe = std::env::current_exe()
        .map_err(|error| format!("Unable to locate the running Gengchou executable: {error}"))?;
    let canonical_executable_name = current_exe_has_canonical_name(&current_exe);
    refuse_executable_inside_legacy_directory(&roots, &current_exe)?;

    let state_path = state_path(&roots);
    let previous_state = read_state(&state_path)?;
    let settings = prepare_settings(&roots, previous_state.as_ref(), &state_path)?;
    let mut messages = vec![format!(
        "migration settings verified current_sha256={}",
        settings.current_hash
    )];
    messages.extend(prepare_cache(&roots, previous_state.is_some())?);
    migrate_startup_value(&current_exe)?;

    let legacy_reappeared = previous_state
        .as_ref()
        .is_some_and(|state| state.stage == MigrationStage::Complete)
        && owned_retired_artifacts_exist(&roots)?;
    let mut stage = previous_state
        .as_ref()
        .map(|state| state.stage)
        .unwrap_or(MigrationStage::Prepared);
    if settings.source_changed || legacy_reappeared {
        stage = MigrationStage::Prepared;
        messages.push("migration source changed; healthy-launch confirmation reset".to_string());
    }

    let mut next_state = MigrationState {
        schema_version: 1,
        stage,
        source_settings_sha256: settings.source_hash,
        current_settings_sha256: settings.current_hash,
        settings_pending: false,
    };

    // A helper may still be executing from the old updates directory during
    // an updater launch. Only a later launch without either ready-marker env
    // may remove the old files.
    if next_state.stage == MigrationStage::ReadySeen
        && !update_ready_requested
        && canonical_executable_name
    {
        match cleanup_legacy_artifacts(&roots, &current_exe) {
            Ok(()) => {
                next_state.stage = MigrationStage::Complete;
                messages.push("migration cleanup completed".to_string());
            }
            Err(error) => {
                messages.push(format!(
                    "migration cleanup pending; monitoring remains available and updates stay disabled: {error}"
                ));
            }
        }
    } else if next_state.stage == MigrationStage::ReadySeen
        && !update_ready_requested
        && !canonical_executable_name
    {
        messages.push(
            "migration cleanup pending; rename the running file to gengchou.exe and restart; monitoring remains available and updates stay disabled"
                .to_string(),
        );
    }

    write_state(&state_path, &next_state)?;
    messages.push(format!("migration stage={:?}", next_state.stage));
    Ok(PreparationReport {
        messages,
        updates_allowed: next_state.stage == MigrationStage::Complete && canonical_executable_name,
    })
}

pub(crate) fn mark_ready_seen() -> Result<(), String> {
    let roots = Roots::from_env()?;
    let path = state_path(&roots);
    let mut state = read_state(&path)?
        .ok_or_else(|| "Migration state is missing at the UI readiness milestone.".to_string())?;
    if state.stage == MigrationStage::Prepared {
        state.stage = MigrationStage::ReadySeen;
        write_state(&path, &state)?;
    }
    Ok(())
}

pub(crate) fn show_blocking_error(message: &str) {
    let title: Vec<u16> = "Gengchou migration blocked"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let body: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR::from_raw(body.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

impl Roots {
    fn from_env() -> Result<Self, String> {
        let appdata = required_absolute_env_path("APPDATA")?;
        let local_appdata = required_absolute_env_path("LOCALAPPDATA")?;
        Ok(Self {
            appdata,
            local_appdata,
        })
    }
}

fn required_absolute_env_path(name: &str) -> Result<PathBuf, String> {
    let value = std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is unavailable; migration cannot choose a safe path."))?;
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(format!(
            "{name} is not a clean absolute path; migration was refused: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn state_path(roots: &Roots) -> PathBuf {
    roots.appdata.join(CURRENT_DIR_NAME).join(STATE_FILE_NAME)
}

fn read_state(path: &Path) -> Result<Option<MigrationState>, String> {
    let Some(content) = read_regular_text(path, "migration state")? else {
        return Ok(None);
    };
    let state: MigrationState = serde_json::from_str(&content).map_err(|error| {
        format!(
            "Migration state at {} is invalid and was not overwritten: {error}",
            path.display()
        )
    })?;
    if state.schema_version != 1
        || !is_sha256_hex(&state.current_settings_sha256)
        || state
            .source_settings_sha256
            .as_ref()
            .is_some_and(|hash| !is_sha256_hex(hash))
    {
        return Err(format!(
            "Migration state at {} has an unsupported schema or invalid hash.",
            path.display()
        ));
    }
    Ok(Some(state))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn write_state(path: &Path, state: &MigrationState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state)
        .map_err(|error| format!("Unable to serialize migration state: {error}"))?;
    settings::write_file_atomic(path, &json).map_err(|error| {
        format!(
            "Unable to persist migration state at {}: {error}",
            path.display()
        )
    })
}

fn prepare_settings(
    roots: &Roots,
    previous: Option<&MigrationState>,
    state_path: &Path,
) -> Result<SettingsPreparation, String> {
    let current_path = roots.appdata.join(CURRENT_DIR_NAME).join("settings.json");
    let current = match read_regular_text(&current_path, "current settings")? {
        Some(content) => {
            let (_, json) = settings::normalized_json(&content).map_err(|error| {
                format!(
                    "Current settings at {} are invalid; migration was stopped: {error}",
                    current_path.display()
                )
            })?;
            let hash = updater::sha256_hex(json.as_bytes())?;
            Some((json, hash))
        }
        None => None,
    };

    let trusted_current = previous.is_some() && current.is_some();
    let direct_dir = roots.appdata.join(SETTINGS_SOURCE_DIR_NAMES[0]);
    let direct_readable = match validate_directory_if_exists(&direct_dir) {
        Ok(()) => true,
        Err(_) if trusted_current => false,
        Err(error) => return Err(error),
    };
    let direct_path = direct_dir.join("settings.json");
    let direct = if direct_readable {
        read_regular_text(&direct_path, "legacy settings")?
    } else {
        None
    };
    let direct_present = direct.is_some();
    let legacy_source = if let Some(content) = direct {
        Some((direct_path, content))
    } else if previous.is_none() || current.is_none() {
        // The upstream directory was the fallback used by v2.2.3 only when
        // its owned settings did not exist. Once a migration state and a
        // current settings file exist, it must not be adopted as a new source
        // after partial cleanup of the owned directory.
        let fallback_dir = roots.appdata.join(SETTINGS_SOURCE_DIR_NAMES[1]);
        validate_directory_if_exists(&fallback_dir)?;
        let fallback_path = fallback_dir.join("settings.json");
        read_regular_text(&fallback_path, "legacy fallback settings")?
            .map(|content| (fallback_path, content))
    } else {
        None
    };
    let legacy = legacy_source
        .map(|(path, content)| {
            let (_, json) = settings::normalized_json(&content).map_err(|error| {
                format!(
                    "Legacy settings at {} are invalid; migration was stopped: {error}",
                    path.display()
                )
            })?;
            let hash = updater::sha256_hex(json.as_bytes())?;
            Ok::<_, String>((json, hash))
        })
        .transpose()?;
    if !direct_present && previous.is_some() && current.is_none() {
        let fallback_hash = legacy.as_ref().map(|(_, hash)| hash.as_str());
        let recorded_hash = previous.and_then(|state| state.source_settings_sha256.as_deref());
        if fallback_hash != recorded_hash {
            return Err(
                "Current settings are missing and the upstream fallback no longer matches the recorded migration source; automatic recovery was refused."
                    .to_string(),
            );
        }
    }

    let mut source_changed = false;
    let (source_hash, current_json, current_hash) = match (legacy, current) {
        (Some((legacy_json, legacy_hash)), None) => {
            let current_hash = legacy_hash.clone();
            (Some(legacy_hash), legacy_json, current_hash)
        }
        (Some((legacy_json, legacy_hash)), Some((current_json, current_hash))) => {
            if legacy_hash == current_hash {
                (Some(legacy_hash), current_json, current_hash)
            } else if let Some(previous) = previous {
                let old_source_unchanged =
                    previous.source_settings_sha256.as_deref() == Some(legacy_hash.as_str());
                let current_unchanged = previous.current_settings_sha256 == current_hash;
                match (
                    previous.settings_pending,
                    old_source_unchanged,
                    current_unchanged,
                ) {
                    (true, true, true) => {
                        (Some(legacy_hash), current_json, current_hash)
                    }
                    (true, true, false) | (true, false, true) => {
                        source_changed = true;
                        let replacement_hash = legacy_hash.clone();
                        (Some(legacy_hash), legacy_json, replacement_hash)
                    }
                    (true, false, false) => {
                        return Err(
                            "Both legacy and current settings changed during an interrupted settings commit; automatic conflict resolution was refused."
                                .to_string(),
                        )
                    }
                    (false, true, _) => {
                        // The old app did not change its source. A difference
                        // here is a legitimate setting saved by v2.2.4.
                        (Some(legacy_hash), current_json, current_hash)
                    }
                    (false, false, true) => {
                        source_changed = true;
                        let replacement_hash = legacy_hash.clone();
                        (Some(legacy_hash), legacy_json, replacement_hash)
                    }
                    (false, false, false) => {
                        return Err(
                            "Both legacy and current settings changed after migration preparation; automatic conflict resolution was refused."
                                .to_string(),
                        )
                    }
                }
            } else {
                return Err(
                    "Legacy and current settings differ but no trusted migration state exists; automatic conflict resolution was refused."
                        .to_string(),
                );
            }
        }
        (None, Some((current_json, current_hash))) => (
            previous.and_then(|state| state.source_settings_sha256.clone()),
            current_json,
            current_hash,
        ),
        (None, None) => {
            if previous.is_some_and(|state| state.source_settings_sha256.is_some()) {
                return Err(
                    "Both the migrated settings and their recorded source are missing; defaults were not substituted."
                        .to_string(),
                );
            }
            let json = serde_json::to_string_pretty(&SettingsFile::default())
                .map_err(|error| format!("Unable to create default settings: {error}"))?;
            let hash = updater::sha256_hex(json.as_bytes())?;
            (None, json, hash)
        }
    };

    let disk_is_canonical = read_regular_text(&current_path, "current settings")?.as_deref()
        == Some(current_json.as_str());
    if !disk_is_canonical {
        let provisional = MigrationState {
            schema_version: 1,
            stage: MigrationStage::Prepared,
            source_settings_sha256: source_hash.clone(),
            current_settings_sha256: current_hash.clone(),
            settings_pending: true,
        };
        // Commit the intent first. If this write fails, the current settings
        // remain untouched; if the later settings write is interrupted, the
        // next launch can distinguish recovery from a user-created conflict.
        write_state(state_path, &provisional)?;
        settings::write_file_atomic(&current_path, &current_json).map_err(|error| {
            format!(
                "Unable to write migrated settings at {}: {error}",
                current_path.display()
            )
        })?;
        let persisted = std::fs::read_to_string(&current_path).map_err(|error| {
            format!(
                "Unable to read back migrated settings at {}: {error}",
                current_path.display()
            )
        })?;
        let (_, persisted_json) = settings::normalized_json(&persisted)?;
        let persisted_hash = updater::sha256_hex(persisted_json.as_bytes())?;
        if persisted_hash != current_hash {
            return Err("Migrated settings failed read-back verification.".to_string());
        }
    }

    Ok(SettingsPreparation {
        source_hash,
        current_hash,
        source_changed,
    })
}

fn prepare_cache(roots: &Roots, trusted_migration_state: bool) -> Result<Vec<String>, String> {
    let current = roots.appdata.join(CURRENT_DIR_NAME).join(CACHE_FILE_NAME);
    let mut messages = Vec::new();

    let current_cache = read_valid_cache(&current)?;
    let direct_dir = roots.appdata.join(SETTINGS_SOURCE_DIR_NAMES[0]);
    let direct_readable = match validate_directory_if_exists(&direct_dir) {
        Ok(()) => true,
        Err(_) if trusted_migration_state => false,
        Err(error) => return Err(error),
    };
    let direct_path = direct_dir.join(CACHE_FILE_NAME);
    let legacy_cache = if direct_readable && path_entry_exists(&direct_path)? {
        // A present direct v2.2.3 cache owns the migration decision even when
        // it is stale or invalid.
        read_valid_cache(&direct_path)?
    } else if current_cache.is_none() {
        let fallback_dir = roots.appdata.join(SETTINGS_SOURCE_DIR_NAMES[1]);
        let fallback_readable = match validate_directory_if_exists(&fallback_dir) {
            Ok(()) => true,
            Err(_) if trusted_migration_state => false,
            Err(error) => return Err(error),
        };
        if fallback_readable {
            read_valid_cache(&fallback_dir.join(CACHE_FILE_NAME))?
        } else {
            None
        }
    } else {
        None
    };
    let selected = match (current_cache, legacy_cache) {
        (Some(current), Some(legacy)) if legacy.0 > current.0 => Some(legacy),
        (Some(current), _) => Some(current),
        (None, legacy) => legacy,
    };

    if let Some((_, json)) = selected {
        let current_content = std::fs::read_to_string(&current).ok();
        if current_content.as_deref() != Some(json.as_str()) {
            settings::write_file_atomic(&current, &json).map_err(|error| {
                format!(
                    "Unable to migrate usage cache to {}: {error}",
                    current.display()
                )
            })?;
            messages.push(format!(
                "fresh usage cache migrated sha256={}",
                updater::sha256_hex(json.as_bytes())?
            ));
        }
    } else if current.exists() {
        std::fs::remove_file(&current).map_err(|error| {
            format!("Unable to discard invalid or stale current cache: {error}")
        })?;
        messages.push("invalid or stale usage cache discarded".to_string());
    }
    Ok(messages)
}

fn read_valid_cache(path: &Path) -> Result<Option<(u64, String)>, String> {
    let Some(content) = read_regular_text(path, "usage cache")? else {
        return Ok(None);
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let Some(saved_unix) = value.get("saved_unix").and_then(serde_json::Value::as_u64) else {
        return Ok(None);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if saved_unix > now.saturating_add(5 * 60)
        || now.saturating_sub(saved_unix) > CACHE_MAX_AGE_SECS
    {
        return Ok(None);
    }
    let normalized = serde_json::to_string(&value)
        .map_err(|error| format!("Unable to normalize usage cache: {error}"))?;
    Ok(Some((saved_unix, normalized)))
}

fn read_regular_text(path: &Path, description: &str) -> Result<Option<String>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Unable to inspect {description} at {}: {error}",
                path.display()
            ))
        }
    };
    if metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
        || !metadata.is_file()
    {
        return Err(format!(
            "Refusing to read non-regular {description} at {}.",
            path.display()
        ));
    }
    std::fs::read_to_string(path).map(Some).map_err(|error| {
        format!(
            "Unable to read {description} at {}: {error}",
            path.display()
        )
    })
}

fn path_entry_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Unable to inspect {}: {error}", path.display())),
    }
}

fn validate_migration_directories(roots: &Roots) -> Result<(), String> {
    let mut seen = HashSet::new();
    for root in [&roots.appdata, &roots.local_appdata] {
        let path = root.join(CURRENT_DIR_NAME);
        if seen.insert(normalized_path_key(&path)) {
            validate_directory_if_exists(&path)?;
        }
    }
    Ok(())
}

fn validate_directory_if_exists(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Unable to inspect migration directory {}: {error}",
                path.display()
            ))
        }
    };
    if metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
        || !metadata.is_dir()
    {
        return Err(format!(
            "Refusing non-directory or reparse-point migration path {}.",
            path.display()
        ));
    }
    Ok(())
}

fn migrate_startup_value(current_exe: &Path) -> Result<(), String> {
    let current = read_run_value(CURRENT_RUN_VALUE_NAME)?;
    if let Some(value) = current.as_deref() {
        if !run_value_owned_by_current_exe(value, current_exe) {
            return Err(format!(
                "The {CURRENT_RUN_VALUE_NAME} startup value points to another executable; it was not overwritten."
            ));
        }
    }

    let mut owned_legacy_values = Vec::new();
    if let Some(value) = read_run_value(OWNED_RETIRED_RUN_VALUE_NAME)? {
        if !run_value_owned_by_current_exe(&value, current_exe) {
            return Err(format!(
                "The retired startup value {OWNED_RETIRED_RUN_VALUE_NAME} points to another executable; migration was stopped."
            ));
        }
        owned_legacy_values.push(OWNED_RETIRED_RUN_VALUE_NAME);
    }

    let current_needs_rewrite = current
        .as_deref()
        .is_some_and(|value| !run_value_points_to_current_exe(value, current_exe));
    if (current.is_none() && !owned_legacy_values.is_empty()) || current_needs_rewrite {
        write_run_value(CURRENT_RUN_VALUE_NAME, current_exe)?;
        let written = read_run_value(CURRENT_RUN_VALUE_NAME)?
            .ok_or_else(|| "The new startup value disappeared after it was written.".to_string())?;
        if !run_value_points_to_current_exe(&written, current_exe) {
            return Err("The new startup value failed read-back verification.".to_string());
        }
    }
    Ok(())
}

fn run_value_owned_by_current_exe(value: &str, current_exe: &Path) -> bool {
    if run_value_points_to_current_exe(value, current_exe) {
        return true;
    }
    let value = value.trim().trim_matches('"');
    let configured = PathBuf::from(value);
    let Some(parent) = configured.parent() else {
        return false;
    };
    configured
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("ai-usage-monitor.exe"))
        && current_exe
            .parent()
            .is_some_and(|current_parent| paths_equal_case_insensitive(parent, current_parent))
        && current_exe
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("gengchou.exe"))
}

fn run_value_points_to_current_exe(value: &str, current_exe: &Path) -> bool {
    let configured = PathBuf::from(value.trim().trim_matches('"'));
    paths_equal_case_insensitive(&configured, current_exe)
}

fn current_exe_has_canonical_name(current_exe: &Path) -> bool {
    current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("gengchou.exe"))
}

fn read_run_value(name: &str) -> Result<Option<String>, String> {
    unsafe {
        let Some(key) = open_run_key(KEY_READ)? else {
            return Ok(None);
        };
        let name_wide = wide(name);
        let mut data_size = 0u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let query = RegQueryValueExW(
            key,
            PCWSTR::from_raw(name_wide.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut data_size),
        );
        if query != ERROR_SUCCESS {
            let _ = RegCloseKey(key);
            if query == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            return Err(format!("Unable to inspect startup value {name}: {query:?}"));
        }
        if value_type != REG_SZ || data_size == 0 || data_size % 2 != 0 {
            let _ = RegCloseKey(key);
            return Err(format!(
                "Startup value {name} is not a valid REG_SZ string."
            ));
        }
        let mut bytes = vec![0u8; data_size as usize];
        let query = RegQueryValueExW(
            key,
            PCWSTR::from_raw(name_wide.as_ptr()),
            None,
            Some(&mut value_type),
            Some(bytes.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(key);
        if query != ERROR_SUCCESS {
            return Err(format!("Unable to read startup value {name}: {query:?}"));
        }
        let wide = std::slice::from_raw_parts(bytes.as_ptr() as *const u16, data_size as usize / 2);
        Ok(Some(
            String::from_utf16_lossy(wide)
                .trim_end_matches('\0')
                .to_string(),
        ))
    }
}

fn write_run_value(name: &str, current_exe: &Path) -> Result<(), String> {
    unsafe {
        let key = open_or_create_run_key(KEY_SET_VALUE)?;
        let name_wide = wide(name);
        let value_wide = wide(&format!("\"{}\"", current_exe.display()));
        let bytes =
            std::slice::from_raw_parts(value_wide.as_ptr() as *const u8, value_wide.len() * 2);
        let result = RegSetValueExW(
            key,
            PCWSTR::from_raw(name_wide.as_ptr()),
            0,
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(key);
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Unable to write startup value {name}: {result:?}"))
        }
    }
}

fn delete_run_value(name: &str) -> Result<(), String> {
    unsafe {
        let Some(key) = open_run_key(KEY_SET_VALUE)? else {
            return Ok(());
        };
        let name_wide = wide(name);
        let result = RegDeleteValueW(key, PCWSTR::from_raw(name_wide.as_ptr()));
        let _ = RegCloseKey(key);
        match result {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            error => Err(format!("Unable to delete startup value {name}: {error:?}")),
        }
    }
}

unsafe fn open_run_key(access: REG_SAM_FLAGS) -> Result<Option<HKEY>, String> {
    let path = wide(&run_key_path());
    let mut key = HKEY::default();
    let result = RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR::from_raw(path.as_ptr()),
        0,
        access,
        &mut key,
    );
    if result == ERROR_SUCCESS {
        Ok(Some(key))
    } else if result == ERROR_FILE_NOT_FOUND {
        Ok(None)
    } else {
        Err(format!(
            "Unable to open the Windows startup registry key: {result:?}"
        ))
    }
}

unsafe fn open_or_create_run_key(access: REG_SAM_FLAGS) -> Result<HKEY, String> {
    let path = wide(&run_key_path());
    let mut key = HKEY::default();
    let result = RegCreateKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR::from_raw(path.as_ptr()),
        0,
        PCWSTR::null(),
        REG_OPTION_NON_VOLATILE,
        access,
        None,
        &mut key,
        None,
    );
    if result == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(format!(
            "Unable to create the Windows startup registry key: {result:?}"
        ))
    }
}

fn run_key_path() -> String {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("GENGCHOU_MIGRATION_TEST_RUN_KEY_PATH") {
        if !path.is_empty() {
            return path.to_string_lossy().to_string();
        }
    }
    RUN_KEY_PATH.to_string()
}

fn cleanup_legacy_artifacts(roots: &Roots, current_exe: &Path) -> Result<(), String> {
    refuse_executable_inside_legacy_directory(roots, current_exe)?;
    let original_run_value = read_run_value(OWNED_RETIRED_RUN_VALUE_NAME)?;
    if let Some(value) = original_run_value.as_deref() {
        if !run_value_owned_by_current_exe(value, current_exe) {
            return Err(format!(
                "The retired startup value {OWNED_RETIRED_RUN_VALUE_NAME} changed ownership; it was not removed."
            ));
        }
    }
    let app_dir = roots.appdata.join(OWNED_RETIRED_DIR_NAME);
    let local_dir = roots.local_appdata.join(OWNED_RETIRED_DIR_NAME);
    // Inventory every owned directory before deleting anything. This keeps a
    // cleanup exception (for example, a user-created note) from partially
    // removing the rollback source or old startup value.
    validate_legacy_appdata_dir(&app_dir)?;
    if normalized_path_key(&local_dir) != normalized_path_key(&app_dir) {
        validate_legacy_local_dir(&local_dir)?;
    }

    if let Some(original) = original_run_value.as_deref() {
        let current = read_run_value(OWNED_RETIRED_RUN_VALUE_NAME)?.ok_or_else(|| {
            "The retired startup value changed during cleanup; deletion was skipped.".to_string()
        })?;
        if current != original || !run_value_owned_by_current_exe(&current, current_exe) {
            return Err(
                "The retired startup value changed ownership during cleanup; it was not removed."
                    .to_string(),
            );
        }
        delete_run_value(OWNED_RETIRED_RUN_VALUE_NAME)?;
        if read_run_value(OWNED_RETIRED_RUN_VALUE_NAME)?.is_some() {
            return Err(format!(
                "The retired startup value {OWNED_RETIRED_RUN_VALUE_NAME} still exists after deletion."
            ));
        }
    }
    cleanup_legacy_appdata_dir(&app_dir)?;
    if normalized_path_key(&local_dir) != normalized_path_key(&app_dir) {
        cleanup_legacy_local_dir(&local_dir)?;
    }
    if owned_retired_artifacts_exist(roots)? {
        return Err(
            "A retired migration artifact appeared during cleanup; completion was deferred."
                .to_string(),
        );
    }
    Ok(())
}

fn owned_retired_artifacts_exist(roots: &Roots) -> Result<bool, String> {
    for dir in [
        roots.appdata.join(OWNED_RETIRED_DIR_NAME),
        roots.local_appdata.join(OWNED_RETIRED_DIR_NAME),
    ] {
        if path_entry_exists(&dir)? {
            return Ok(true);
        }
    }
    if read_run_value(OWNED_RETIRED_RUN_VALUE_NAME)?.is_some() {
        return Ok(true);
    }
    Ok(false)
}

fn cleanup_legacy_appdata_dir(dir: &Path) -> Result<(), String> {
    if !path_entry_exists(dir)? {
        return Ok(());
    }
    validate_legacy_appdata_dir(dir)?;
    for name in [
        "settings.json",
        "settings.tmp",
        "usage-cache.json",
        "usage-cache.tmp",
    ] {
        remove_regular_file_if_exists(&dir.join(name))?;
    }
    remove_empty_directory_or_report_unknown(dir)
}

fn cleanup_legacy_local_dir(dir: &Path) -> Result<(), String> {
    if !path_entry_exists(dir)? {
        return Ok(());
    }
    validate_legacy_local_dir(dir)?;
    for name in ["diagnose.log", "diagnose.log.old"] {
        remove_regular_file_if_exists(&dir.join(name))?;
    }
    let updates = dir.join("updates");
    cleanup_legacy_updates_dir(&updates)?;
    remove_empty_directory_or_report_unknown(dir)
}

fn cleanup_legacy_updates_dir(dir: &Path) -> Result<(), String> {
    if !path_entry_exists(dir)? {
        return Ok(());
    }
    validate_legacy_updates_dir(dir)?;
    for entry in std::fs::read_dir(dir)
        .map_err(|error| format!("Unable to inspect {}: {error}", dir.display()))?
    {
        let entry = entry.map_err(|error| format!("Unable to inspect updater entry: {error}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let known = matches!(
            name.as_str(),
            "updater-helper.exe" | "update-download.exe" | "update-download.exe.part"
        ) || is_owned_ready_marker_name(&name);
        if known {
            remove_regular_file_if_exists(&entry.path())?;
        }
    }
    remove_empty_directory_or_report_unknown(dir)
}

fn validate_legacy_appdata_dir(dir: &Path) -> Result<(), String> {
    if !path_entry_exists(dir)? {
        return Ok(());
    }
    validate_directory_if_exists(dir)?;
    for entry in read_directory_entries(dir)? {
        let name = entry.file_name().to_string_lossy().to_string();
        if [
            "settings.json",
            "settings.tmp",
            "usage-cache.json",
            "usage-cache.tmp",
        ]
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known))
        {
            validate_regular_migration_file(&entry.path())?;
        } else {
            return Err(unknown_entry_error(dir, &name));
        }
    }
    Ok(())
}

fn validate_legacy_local_dir(dir: &Path) -> Result<(), String> {
    if !path_entry_exists(dir)? {
        return Ok(());
    }
    validate_directory_if_exists(dir)?;
    for entry in read_directory_entries(dir)? {
        let name = entry.file_name().to_string_lossy().to_string();
        if ["diagnose.log", "diagnose.log.old"]
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known))
        {
            validate_regular_migration_file(&entry.path())?;
        } else if name.eq_ignore_ascii_case("updates") {
            validate_legacy_updates_dir(&entry.path())?;
        } else {
            return Err(unknown_entry_error(dir, &name));
        }
    }
    Ok(())
}

fn validate_legacy_updates_dir(dir: &Path) -> Result<(), String> {
    if !path_entry_exists(dir)? {
        return Ok(());
    }
    validate_directory_if_exists(dir)?;
    for entry in read_directory_entries(dir)? {
        let name = entry.file_name().to_string_lossy().to_string();
        let known = matches!(
            name.as_str(),
            "updater-helper.exe" | "update-download.exe" | "update-download.exe.part"
        ) || is_owned_ready_marker_name(&name);
        if known {
            validate_regular_migration_file(&entry.path())?;
        } else {
            return Err(unknown_entry_error(dir, &name));
        }
    }
    Ok(())
}

fn read_directory_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|error| format!("Unable to inspect {}: {error}", dir.display()))?
    {
        entries.push(entry.map_err(|error| {
            format!("Unable to inspect an entry in {}: {error}", dir.display())
        })?);
    }
    Ok(entries)
}

fn validate_regular_migration_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Unable to inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
        || !metadata.is_file()
    {
        return Err(format!(
            "Refusing non-regular migration artifact {}.",
            path.display()
        ));
    }
    Ok(())
}

fn unknown_entry_error(dir: &Path, name: &str) -> String {
    format!("Migration left unknown files in {}: {name}", dir.display())
}

fn is_owned_ready_marker_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("update-ready-") else {
        return false;
    };
    if let Some(marker) = rest.strip_suffix(".marker") {
        return has_decimal_parts(marker, 3);
    }
    let Some((marker, writing)) = rest.split_once(".marker.writing-") else {
        return false;
    };
    has_decimal_parts(marker, 3) && has_decimal_parts(writing, 2)
}

fn has_decimal_parts(value: &str, expected: usize) -> bool {
    let mut parts = value.split('-');
    (0..expected).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }) && parts.next().is_none()
}

fn remove_regular_file_if_exists(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Unable to inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
        || !metadata.is_file()
    {
        return Err(format!(
            "Refusing to remove non-regular migration artifact {}.",
            path.display()
        ));
    }
    std::fs::remove_file(path)
        .map_err(|error| format!("Unable to remove {}: {error}", path.display()))
}

fn remove_empty_directory_or_report_unknown(dir: &Path) -> Result<(), String> {
    let entries = read_directory_entries(dir)?
        .into_iter()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if !entries.is_empty() {
        return Err(format!(
            "Migration left unknown files in {}: {}",
            dir.display(),
            entries.join(", ")
        ));
    }
    std::fs::remove_dir(dir).map_err(|error| {
        format!(
            "Unable to remove empty directory {}: {error}",
            dir.display()
        )
    })
}

fn refuse_executable_inside_legacy_directory(
    roots: &Roots,
    current_exe: &Path,
) -> Result<(), String> {
    for dir in [
        roots.appdata.join(OWNED_RETIRED_DIR_NAME),
        roots.local_appdata.join(OWNED_RETIRED_DIR_NAME),
    ] {
        if path_is_within(current_exe, &dir) {
            return Err(format!(
                "The running executable is inside retired directory {}; move gengchou.exe before migration.",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn path_is_within(path: &Path, parent: &Path) -> bool {
    let path = normalized_path_key(path);
    let mut parent = normalized_path_key(parent);
    if !parent.ends_with('\\') {
        parent.push('\\');
    }
    path.starts_with(&parent)
}

fn paths_equal_case_insensitive(left: &Path, right: &Path) -> bool {
    normalized_path_key(left) == normalized_path_key(right)
}

fn normalized_path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gengchou-migration-{name}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn roots(&self) -> Roots {
            Roots {
                appdata: self.0.join("Roaming"),
                local_appdata: self.0.join("Local"),
            }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn owned_startup_path_accepts_current_and_same_directory_legacy_exe() {
        let current = Path::new(r"C:\Tools\gengchou.exe");
        assert!(run_value_owned_by_current_exe(
            r#""C:\Tools\gengchou.exe""#,
            current
        ));
        assert!(run_value_owned_by_current_exe(
            r#""C:\Tools\ai-usage-monitor.exe""#,
            current
        ));
        assert!(!run_value_owned_by_current_exe(
            r#""C:\Other\ai-usage-monitor.exe""#,
            current
        ));
        assert!(!run_value_points_to_current_exe(
            r#""C:\Tools\ai-usage-monitor.exe""#,
            current
        ));
    }

    #[test]
    fn migration_completion_requires_the_canonical_executable_name() {
        assert!(current_exe_has_canonical_name(Path::new(
            r"C:\Tools\gengchou.exe"
        )));
        assert!(current_exe_has_canonical_name(Path::new(
            r"C:\Tools\GENGCHOU.EXE"
        )));
        assert!(!current_exe_has_canonical_name(Path::new(
            r"C:\Tools\ai-usage-monitor.exe"
        )));
    }

    #[test]
    fn legacy_directory_containment_is_case_insensitive_and_component_safe() {
        assert!(path_is_within(
            Path::new(r"C:\Users\A\AppData\Local\AIUsageMonitor\gengchou.exe"),
            Path::new(r"c:\users\a\appdata\local\aiusagemonitor")
        ));
        assert!(!path_is_within(
            Path::new(r"C:\Users\A\AppData\Local\AIUsageMonitor2\gengchou.exe"),
            Path::new(r"C:\Users\A\AppData\Local\AIUsageMonitor")
        ));
    }

    #[test]
    fn settings_are_normalized_and_migrated_to_the_current_directory() {
        let root = TestRoot::new("settings-copy");
        let roots = root.roots();
        let legacy = roots
            .appdata
            .join(SETTINGS_SOURCE_DIR_NAMES[0])
            .join("settings.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"poll_interval_ms":60000,"tray_offset":4}"#).unwrap();

        let prepared = prepare_settings(&roots, None, &state_path(&roots)).unwrap();
        let current = roots.appdata.join(CURRENT_DIR_NAME).join("settings.json");
        let current_content = std::fs::read_to_string(current).unwrap();
        let (_, normalized) = settings::normalized_json(&current_content).unwrap();

        assert_eq!(
            updater::sha256_hex(normalized.as_bytes()).unwrap(),
            prepared.current_hash
        );
        assert_eq!(
            prepared.source_hash.as_deref(),
            Some(prepared.current_hash.as_str())
        );
        assert!(!prepared.source_changed);
    }

    #[test]
    fn conflicting_settings_without_state_are_refused() {
        let root = TestRoot::new("settings-conflict");
        let roots = root.roots();
        let legacy = roots
            .appdata
            .join(SETTINGS_SOURCE_DIR_NAMES[0])
            .join("settings.json");
        let current = roots.appdata.join(CURRENT_DIR_NAME).join("settings.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(legacy, r#"{"tray_offset":1}"#).unwrap();
        std::fs::write(current, r#"{"tray_offset":2}"#).unwrap();

        let error = prepare_settings(&roots, None, &state_path(&roots)).unwrap_err();
        assert!(error.contains("no trusted migration state"));
    }

    #[test]
    fn direct_v2_2_3_settings_take_priority_over_upstream_fallback() {
        let root = TestRoot::new("legacy-settings-priority");
        let roots = root.roots();
        for (name, offset) in [
            (SETTINGS_SOURCE_DIR_NAMES[0], 1),
            (SETTINGS_SOURCE_DIR_NAMES[1], 2),
        ] {
            let path = roots.appdata.join(name).join("settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!(r#"{{"tray_offset":{offset}}}"#)).unwrap();
        }

        let prepared = prepare_settings(&roots, None, &state_path(&roots)).unwrap();
        let current = roots.appdata.join(CURRENT_DIR_NAME).join("settings.json");
        let settings: SettingsFile =
            serde_json::from_str(&std::fs::read_to_string(current).unwrap()).unwrap();
        assert_eq!(settings.tray_offset, 1);
        assert_eq!(
            prepared.source_hash.as_deref(),
            Some(prepared.current_hash.as_str())
        );
    }

    #[test]
    fn direct_v2_2_3_cache_takes_priority_over_newer_upstream_fallback() {
        let root = TestRoot::new("legacy-cache-priority");
        let roots = root.roots();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        for (name, saved_unix) in [
            (SETTINGS_SOURCE_DIR_NAMES[0], now.saturating_sub(60)),
            (SETTINGS_SOURCE_DIR_NAMES[1], now),
        ] {
            let path = roots.appdata.join(name).join(CACHE_FILE_NAME);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!(r#"{{"saved_unix":{saved_unix}}}"#)).unwrap();
        }

        prepare_cache(&roots, false).unwrap();
        let current = roots.appdata.join(CURRENT_DIR_NAME).join(CACHE_FILE_NAME);
        let copied: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(current).unwrap()).unwrap();
        assert_eq!(copied["saved_unix"].as_u64(), Some(now.saturating_sub(60)));
    }

    #[test]
    fn non_directory_current_path_is_refused_before_traversal() {
        let root = TestRoot::new("non-directory");
        let roots = root.roots();
        std::fs::create_dir_all(&roots.appdata).unwrap();
        std::fs::write(roots.appdata.join(CURRENT_DIR_NAME), "not a directory").unwrap();

        let error = validate_migration_directories(&roots).unwrap_err();
        assert!(error.contains("non-directory or reparse-point"));
    }

    #[test]
    fn unsafe_retired_source_blocks_first_copy_but_not_trusted_current_settings() {
        let root = TestRoot::new("unsafe-retired-source");
        let roots = root.roots();
        std::fs::create_dir_all(&roots.appdata).unwrap();
        std::fs::write(
            roots.appdata.join(OWNED_RETIRED_DIR_NAME),
            "not a directory",
        )
        .unwrap();

        let first_error = prepare_settings(&roots, None, &state_path(&roots)).unwrap_err();
        assert!(first_error.contains("non-directory or reparse-point"));

        let current = roots.appdata.join(CURRENT_DIR_NAME).join("settings.json");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        let (_, current_json) = settings::normalized_json(r#"{"tray_offset":8}"#).unwrap();
        std::fs::write(&current, &current_json).unwrap();
        let current_hash = updater::sha256_hex(current_json.as_bytes()).unwrap();
        let state = MigrationState {
            schema_version: 1,
            stage: MigrationStage::ReadySeen,
            source_settings_sha256: Some(current_hash.clone()),
            current_settings_sha256: current_hash,
            settings_pending: false,
        };

        assert!(prepare_settings(&roots, Some(&state), &state_path(&roots)).is_ok());
        assert!(prepare_cache(&roots, true).is_ok());
    }

    #[test]
    fn changed_legacy_source_replaces_an_unchanged_prepared_target() {
        let root = TestRoot::new("settings-retry");
        let roots = root.roots();
        let legacy = roots
            .appdata
            .join(SETTINGS_SOURCE_DIR_NAMES[0])
            .join("settings.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"tray_offset":1}"#).unwrap();
        let first = prepare_settings(&roots, None, &state_path(&roots)).unwrap();
        let state = MigrationState {
            schema_version: 1,
            stage: MigrationStage::ReadySeen,
            source_settings_sha256: first.source_hash,
            current_settings_sha256: first.current_hash,
            settings_pending: false,
        };

        std::fs::write(&legacy, r#"{"tray_offset":9}"#).unwrap();
        let retried = prepare_settings(&roots, Some(&state), &state_path(&roots)).unwrap();

        assert!(retried.source_changed);
        assert_eq!(
            retried.source_hash.as_deref(),
            Some(retried.current_hash.as_str())
        );
    }

    #[test]
    fn unknown_legacy_files_block_directory_cleanup() {
        let root = TestRoot::new("unknown-file");
        let roots = root.roots();
        let legacy_dir = roots.appdata.join(OWNED_RETIRED_DIR_NAME);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("user-note.txt"), "keep").unwrap();

        let error = cleanup_legacy_appdata_dir(&legacy_dir).unwrap_err();
        assert!(error.contains("unknown files"));
        assert!(legacy_dir.join("user-note.txt").exists());
    }

    #[test]
    fn cleanup_recognizes_only_exact_owned_ready_marker_names() {
        assert!(is_owned_ready_marker_name("update-ready-123-456-7.marker"));
        assert!(is_owned_ready_marker_name(
            "update-ready-123-456-7.marker.writing-123-789"
        ));
        assert!(!is_owned_ready_marker_name("notes.marker.writing-backup"));
        assert!(!is_owned_ready_marker_name(
            "update-ready-note.marker.writing-123-789"
        ));
        assert!(!is_owned_ready_marker_name("update-ready-123-456.marker"));
    }
}
