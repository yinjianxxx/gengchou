use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Rotate the log once it grows past this size; one previous generation is
/// kept as `diagnose.log.old`.
const MAX_LOG_BYTES: u64 = 1_000_000;
const FILE_ATTRIBUTE_REPARSE_POINT_VALUE: u32 = 0x0000_0400;

struct DiagnoseState {
    file: Mutex<File>,
}

static DIAGNOSE_STATE: OnceLock<DiagnoseState> = OnceLock::new();

fn log_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "LOCALAPPDATA is unavailable; diagnostic logging was refused.".to_string()
        })?;
    if !base.is_absolute()
        || base
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(format!(
            "LOCALAPPDATA is not a clean absolute path: {}",
            base.display()
        ));
    }
    Ok(base.join("Gengchou").join("diagnose.log"))
}

pub fn init() -> Result<PathBuf, String> {
    let path = log_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "Diagnostic log path has no parent directory.".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Unable to create diagnostic directory {}: {error}",
            parent.display()
        )
    })?;
    validate_regular_directory(parent)?;
    validate_regular_file_if_exists(&path)?;

    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if metadata.len() > MAX_LOG_BYTES {
            let old = path.with_extension("log.old");
            validate_regular_file_if_exists(&old)?;
            match std::fs::remove_file(&old) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "Unable to remove rotated diagnostic log {}: {error}",
                        old.display()
                    ))
                }
            }
            std::fs::rename(&path, &old).map_err(|error| {
                format!(
                    "Unable to rotate diagnostic log {}: {error}",
                    path.display()
                )
            })?;
        }
    }

    // Append (never truncate): relaunched instances share the file, and the
    // record of why a previous instance died must survive its successor.
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("Unable to open diagnostic log file {}: {e}", path.display()))?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "Unable to verify opened diagnostic log {}: {error}",
            path.display()
        )
    })?;
    if !opened_metadata.is_file()
        || opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
    {
        return Err(format!(
            "Refusing non-regular diagnostic log {}.",
            path.display()
        ));
    }

    let _ = DIAGNOSE_STATE.set(DiagnoseState {
        file: Mutex::new(file),
    });

    log(format!(
        "--- diagnostic logging started v{} ---",
        env!("CARGO_PKG_VERSION")
    ));
    Ok(path)
}

fn validate_regular_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Unable to inspect {}: {error}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
    {
        return Err(format!(
            "Refusing non-directory or reparse-point diagnostic path {}.",
            path.display()
        ));
    }
    Ok(())
}

fn validate_regular_file_if_exists(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Unable to inspect {}: {error}", path.display())),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
    {
        return Err(format!(
            "Refusing non-regular diagnostic file {}.",
            path.display()
        ));
    }
    Ok(())
}

pub fn is_enabled() -> bool {
    DIAGNOSE_STATE.get().is_some()
}

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn log(message: impl AsRef<str>) {
    let Some(state) = DIAGNOSE_STATE.get() else {
        return;
    };

    // Recover a poisoned lock: the panic hook logs from panicking threads.
    let mut file = match state.file.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let _ = writeln!(
        file,
        "[{} pid={}] {}",
        timestamp(),
        std::process::id(),
        message.as_ref()
    );
    let _ = file.flush();
}

pub fn log_error(context: &str, error: impl std::fmt::Display) {
    log(format!("{context}: {error}"));
}
