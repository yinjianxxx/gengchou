use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Rotate the log once it grows past this size; one previous generation is
/// kept as `diagnose.log.old`.
const MAX_LOG_BYTES: u64 = 1_000_000;

struct DiagnoseState {
    file: Mutex<File>,
}

static DIAGNOSE_STATE: OnceLock<DiagnoseState> = OnceLock::new();

pub fn log_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("AIUsageMonitor").join("diagnose.log")
}

pub fn init() -> Result<PathBuf, String> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(metadata) = std::fs::metadata(&path) {
        if metadata.len() > MAX_LOG_BYTES {
            let old = path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
    }

    // Append (never truncate): relaunched instances share the file, and the
    // record of why a previous instance died must survive its successor.
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("Unable to open diagnostic log file {}: {e}", path.display()))?;

    let _ = DIAGNOSE_STATE.set(DiagnoseState {
        file: Mutex::new(file),
    });

    log(format!(
        "--- diagnostic logging started v{} ---",
        env!("CARGO_PKG_VERSION")
    ));
    Ok(path)
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
