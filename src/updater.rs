use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

const GITHUB_API_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_API_VERSION: &str = "2022-11-28";
const RELEASE_ASSET_NAME: &str = "ai-usage-monitor.exe";
// Fixed asset produced by .github/workflows/release.yml; self-updates refuse
// to apply a download whose SHA-256 does not match this manifest.
const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS";
const HELPER_EXE_NAME: &str = "updater-helper.exe";
const DOWNLOAD_EXE_NAME: &str = "update-download.exe";
const UPDATE_READY_ENV: &str = "AIUM_UPDATE_READY_FILE";
#[cfg(debug_assertions)]
const UPDATE_TEST_READY_DIR_ENV: &str = "AIUM_UPDATE_TEST_READY_DIR";
#[cfg(debug_assertions)]
const UPDATE_TEST_NO_UI_ENV: &str = "AIUM_UPDATE_TEST_NO_UI";
const UPDATE_READY_PREFIX: &str = "update-ready-";
const UPDATE_READY_SUFFIX: &str = ".marker";
const UPDATE_READY_CONTENT: &[u8] = b"AIUM update ready\n";
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_READY_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_READY_GRACE: Duration = Duration::from_secs(2);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const REPLACE_RETRY_DELAY: Duration = Duration::from_millis(500);
const REPLACE_ATTEMPTS: usize = 60;
const CREATE_NO_WINDOW: u32 = 0x08000000;
const CREATE_NEW_CONSOLE: u32 = 0x00000010;
// Keep this aligned with the package identifier used in winget-pkgs.
const WINGET_PACKAGE_ID: &str = "yinjianxxx.AIUsageMonitor";

static UPDATE_READY_CONFIRMED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallChannel {
    Portable,
    Winget,
}

#[derive(Clone, Debug)]
pub struct ReleaseDescriptor {
    pub latest_version: String,
    asset_url: String,
    checksums_url: Option<String>,
}

#[derive(Debug)]
pub enum UpdateCheckResult {
    UpToDate,
    Available(ReleaseDescriptor),
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

pub fn handle_cli_mode(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) == Some("--apply-update") {
        let result = if args.len() != 6 {
            Err(
                "The updater requires target, source, parent PID, and expected SHA-256 arguments."
                    .to_string(),
            )
        } else {
            let target = PathBuf::from(&args[2]);
            let source = PathBuf::from(&args[3]);
            parse_update_pid(&args[4]).and_then(|pid| {
                parse_expected_sha256(&args[5])
                    .and_then(|expected_sha256| apply_update(target, source, pid, &expected_sha256))
            })
        };

        return Some(match result {
            Ok(()) => 0,
            Err(error) => {
                if !updater_ui_disabled_for_tests() {
                    show_error_message("Update failed", &error);
                }
                1
            }
        });
    }

    None
}

fn parse_update_pid(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| "The updater received an invalid parent process ID.".to_string())
}

fn parse_expected_sha256(value: &str) -> Result<String, String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("The updater received an invalid expected SHA-256 value.".to_string());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn current_install_channel() -> InstallChannel {
    match std::env::current_exe() {
        Ok(path) if is_winget_install_path(&path) => InstallChannel::Winget,
        _ => InstallChannel::Portable,
    }
}

/// True when Cargo.toml carries a GitHub repository the release check can query.
pub fn update_channel_configured() -> bool {
    github_repo().is_ok()
}

pub fn check_for_updates() -> Result<UpdateCheckResult, String> {
    match fetch_latest_release()? {
        Some(release) => Ok(UpdateCheckResult::Available(release)),
        None => Ok(UpdateCheckResult::UpToDate),
    }
}

pub fn begin_winget_update() -> Result<(), String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Unable to locate current executable: {e}"))?;
    let current_dir = current_exe
        .parent()
        .ok_or_else(|| "Unable to determine the app directory for restart.".to_string())?;
    let command = winget_upgrade_command(
        std::process::id(),
        &current_exe.to_string_lossy(),
        &current_dir.to_string_lossy(),
    );

    Command::new("powershell.exe")
        .arg("-NoLogo")
        .arg("-Command")
        .arg(&command)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map_err(|e| format!("Unable to launch WinGet update command: {e}"))?;

    Ok(())
}

pub fn begin_self_update(release: &ReleaseDescriptor) -> Result<(), String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Unable to locate current executable: {e}"))?;
    ensure_target_location_writable(&current_exe)?;

    let stage_dir = updates_dir()?;
    std::fs::create_dir_all(&stage_dir)
        .map_err(|e| format!("Unable to create updater working directory: {e}"))?;

    let helper_path = stage_dir.join(HELPER_EXE_NAME);
    let download_path = stage_dir.join(DOWNLOAD_EXE_NAME);
    let partial_download_path = stage_dir.join(format!("{DOWNLOAD_EXE_NAME}.part"));

    if helper_path.exists() {
        let _ = std::fs::remove_file(&helper_path);
    }
    if download_path.exists() {
        let _ = std::fs::remove_file(&download_path);
    }
    if partial_download_path.exists() {
        let _ = std::fs::remove_file(&partial_download_path);
    }

    download_release_asset(&release.asset_url, &partial_download_path, &download_path)?;
    let expected_sha256 = match verify_download_checksum(release, &download_path) {
        Ok(expected_sha256) => expected_sha256,
        Err(error) => {
            let _ = std::fs::remove_file(&download_path);
            return Err(error);
        }
    };
    std::fs::copy(&current_exe, &helper_path)
        .map_err(|e| format!("Unable to prepare updater helper: {e}"))?;

    let pid = std::process::id().to_string();
    let target = current_exe.to_string_lossy().to_string();
    let source = download_path.to_string_lossy().to_string();

    Command::new(&helper_path)
        .arg("--apply-update")
        .arg(target)
        .arg(source)
        .arg(pid)
        .arg(expected_sha256)
        // A process installed by an earlier update inherits this variable.
        // The helper must never mistake that stale marker for its own child.
        .env_remove(UPDATE_READY_ENV)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Unable to launch updater helper: {e}"))?;

    Ok(())
}

fn apply_update(
    target: PathBuf,
    source: PathBuf,
    pid: u32,
    expected_sha256: &str,
) -> Result<(), String> {
    // Replacing a live image is not a safe fallback: even when Windows allows
    // the rename, the old process still owns the single-instance mutex.
    // A wait failure means that process may still be alive, so do not launch a
    // competing instance on that path.
    wait_for_process_exit(pid, PROCESS_EXIT_TIMEOUT)?;

    // From this point onward the previous process is known to be gone. Every
    // failure before the atomic replace must restart the unchanged target,
    // including a staged download removed while the helper was starting.
    if !source.exists() {
        return relaunch_unchanged_target(
            &target,
            format!("Downloaded update not found at {}", source.display()),
        );
    }

    let backup = backup_path_for(&target);
    if backup.exists() {
        return relaunch_unchanged_target(
            &target,
            format!(
                "A previous update backup still exists at {}. The app must complete one healthy launch before another update can replace it.",
                backup.display()
            ),
        );
    }

    let original_hash = match file_sha256_hex(&target, "the installed app") {
        Ok(hash) => hash,
        Err(error) => return relaunch_unchanged_target(&target, error),
    };
    let ready_marker = match new_ready_marker_path() {
        Ok(marker) => marker,
        Err(error) => return relaunch_unchanged_target(&target, error),
    };
    let prepared = match prepare_staged_binary(&source, &target, expected_sha256) {
        Ok(prepared) => prepared,
        Err(error) => return relaunch_unchanged_target(&target, error),
    };

    if let Err(error) = replace_target_binary(&target, &prepared.path, &backup) {
        return restore_and_relaunch_previous(
            &target,
            &backup,
            &original_hash,
            format!("Unable to install the update: {error}"),
        );
    }

    let installed_hash = file_sha256_hex(&target, "the installed update");
    if installed_hash.as_deref() != Ok(expected_sha256) {
        let detail = installed_hash
            .map(|actual| {
                format!("the installed file hash was {actual}, expected {expected_sha256}")
            })
            .unwrap_or_else(|error| error);
        return restore_and_relaunch_previous(
            &target,
            &backup,
            &original_hash,
            format!("The installed update could not be verified: {detail}"),
        );
    }

    let mut child = match relaunch_target(&target, Some(&ready_marker)) {
        Ok(child) => child,
        Err(error) => {
            return restore_and_relaunch_previous(&target, &backup, &original_hash, error)
        }
    };

    if let Err(error) = wait_for_update_ready(
        &mut child,
        &ready_marker,
        UPDATE_READY_TIMEOUT,
        UPDATE_READY_GRACE,
        READY_POLL_INTERVAL,
    ) {
        if let Err(stop_error) = stop_child_for_rollback(&mut child) {
            return Err(format!(
                "{error} The new process could not be stopped safely ({stop_error}), so rollback was not attempted. The previous binary is preserved at {}.",
                backup.display()
            ));
        }

        return restore_and_relaunch_previous(&target, &backup, &original_hash, error);
    }

    // The child owns the mutex and has completed UI initialization before it
    // writes the marker. Only now is the rollback asset disposable.
    for path in [&backup, &source, &ready_marker] {
        if let Err(error) = remove_file_if_exists(path) {
            crate::diagnose::log(format!(
                "update ready; deferred cleanup path={} error={error}",
                path.display()
            ));
        }
    }

    Ok(())
}

/// Confirm that this process has reached the updater's readiness milestone.
///
/// Call this only after the process owns the single-instance mutex and the
/// main window, tray icons, and first render have initialized successfully.
/// It is idempotent within a process. Normal launches have no marker to write;
/// at that same healthy milestone they may remove a backup orphaned by a
/// helper that was interrupted after a previous successful replacement.
pub fn confirm_update_ready() -> Result<(), String> {
    if UPDATE_READY_CONFIRMED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let result = (|| {
        if let Some(marker) = std::env::var_os(UPDATE_READY_ENV) {
            let marker = PathBuf::from(marker);
            validate_ready_marker_path(&marker)?;
            write_ready_marker(&marker)?;
            // Do not let a later helper inherit a marker for this transaction.
            std::env::remove_var(UPDATE_READY_ENV);
            return Ok(());
        }

        let current_exe = std::env::current_exe()
            .map_err(|error| format!("Unable to locate the running app: {error}"))?;
        let backup = backup_path_for(&current_exe);
        remove_file_if_exists(&backup).map_err(|error| {
            format!(
                "Unable to clean up the confirmed update backup at {}: {error}",
                backup.display()
            )
        })
    })();

    if result.is_err() {
        UPDATE_READY_CONFIRMED.store(false, Ordering::SeqCst);
    }
    result
}

#[derive(Debug)]
struct PreparedBinary {
    path: PathBuf,
}

fn fetch_latest_release() -> Result<Option<ReleaseDescriptor>, String> {
    let (owner, repo) = github_repo()?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let agent = build_agent()?;

    let response = agent
        .get(&url)
        .set("Accept", GITHUB_API_ACCEPT)
        .set("User-Agent", user_agent())
        .set("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .call()
        .map_err(|e| format!("Unable to check GitHub releases: {e}"))?;

    let release: GitHubRelease = response
        .into_json()
        .map_err(|e| format!("Unable to parse GitHub release data: {e}"))?;

    let latest_version = release.tag_name.trim_start_matches('v').to_string();
    if !is_version_newer(&latest_version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }

    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(RELEASE_ASSET_NAME))
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| asset.name.to_ascii_lowercase().ends_with(".exe"))
        })
        .ok_or_else(|| {
            "No Windows executable asset was found in the latest release.".to_string()
        })?;

    let checksums_url = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(CHECKSUMS_ASSET_NAME))
        .map(|asset| asset.browser_download_url.clone());

    Ok(Some(ReleaseDescriptor {
        latest_version,
        asset_url: asset.browser_download_url.clone(),
        checksums_url,
    }))
}

fn build_agent() -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new()
        .map_err(|e| format!("Unable to initialize TLS support for update checks: {e}"))?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn download_release_asset(url: &str, partial_path: &Path, final_path: &Path) -> Result<(), String> {
    let agent = build_agent()?;
    let response = agent
        .get(url)
        .set("User-Agent", user_agent())
        .call()
        .map_err(|e| format!("Unable to download the latest release: {e}"))?;

    let mut reader = response.into_reader();
    let mut file = File::create(partial_path)
        .map_err(|e| format!("Unable to create temporary download file: {e}"))?;

    io::copy(&mut reader, &mut file)
        .map_err(|e| format!("Unable to write the downloaded update: {e}"))?;
    file.flush()
        .map_err(|e| format!("Unable to finalize the downloaded update: {e}"))?;

    std::fs::rename(partial_path, final_path)
        .map_err(|e| format!("Unable to finalize the downloaded update file: {e}"))?;

    Ok(())
}

/// Compare the downloaded binary against the release's SHA256SUMS manifest.
/// Release checks tolerate a missing manifest (the user can still see an
/// update exists), but applying one without a verifiable hash is refused.
fn verify_download_checksum(
    release: &ReleaseDescriptor,
    download: &Path,
) -> Result<String, String> {
    let checksums_url = release.checksums_url.as_deref().ok_or_else(|| {
        format!(
            "The release does not provide a {CHECKSUMS_ASSET_NAME} file; refusing to apply an unverified update. Download it manually from the GitHub release page."
        )
    })?;

    let agent = build_agent()?;
    let manifest = agent
        .get(checksums_url)
        .set("User-Agent", user_agent())
        .call()
        .map_err(|e| format!("Unable to download the release {CHECKSUMS_ASSET_NAME} file: {e}"))?
        .into_string()
        .map_err(|e| format!("Unable to read the release {CHECKSUMS_ASSET_NAME} file: {e}"))?;

    let expected = expected_checksum_from_manifest(&manifest, RELEASE_ASSET_NAME)
        .ok_or_else(|| {
            format!("{CHECKSUMS_ASSET_NAME} has no valid SHA-256 entry for {RELEASE_ASSET_NAME}; refusing to apply an unverified update.")
        })?;
    let expected = parse_expected_sha256(&expected).map_err(|_| {
        format!("{CHECKSUMS_ASSET_NAME} has an invalid SHA-256 entry for {RELEASE_ASSET_NAME}.")
    })?;

    let actual = file_sha256_hex(download, "the downloaded update")?;
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(expected)
    } else {
        Err(format!(
            "The downloaded update failed SHA-256 verification (expected {expected}, got {actual}). The download may be corrupted or tampered with."
        ))
    }
}

/// Parse `<hex>  <name>` manifest lines (the format release.yml writes).
fn expected_checksum_from_manifest(manifest: &str, asset_name: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name.eq_ignore_ascii_case(asset_name)
            && hash.len() == 64
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
    })
}

fn file_sha256_hex(path: &Path, description: &str) -> Result<String, String> {
    let contents = std::fs::read(path)
        .map_err(|e| format!("Unable to read {description} at {}: {e}", path.display()))?;
    sha256_hex(&contents)
}

/// SHA-256 via the Windows CNG provider: no extra hashing crate needed.
fn sha256_hex(data: &[u8]) -> Result<String, String> {
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
        BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE,
        BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS, BCRYPT_SHA256_ALGORITHM,
    };

    unsafe {
        let mut algorithm = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut algorithm,
            BCRYPT_SHA256_ALGORITHM,
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .is_err()
        {
            return Err("Unable to initialize SHA-256 for update verification.".to_string());
        }

        let mut hash = BCRYPT_HASH_HANDLE::default();
        let mut digest = [0u8; 32];
        let result = if BCryptCreateHash(algorithm, &mut hash, None, None, 0).is_err() {
            Err("Unable to initialize SHA-256 for update verification.".to_string())
        } else {
            let hashed = if BCryptHashData(hash, data, 0).is_err()
                || BCryptFinishHash(hash, &mut digest, 0).is_err()
            {
                Err("Unable to compute the update's SHA-256 hash.".to_string())
            } else {
                Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
            };
            let _ = BCryptDestroyHash(hash);
            hashed
        };
        let _ = BCryptCloseAlgorithmProvider(algorithm, 0);
        result
    }
}

fn prepare_staged_binary(
    source: &Path,
    target: &Path,
    expected_sha256: &str,
) -> Result<PreparedBinary, String> {
    prepare_staged_binary_with(source, target, expected_sha256, |from, to| {
        std::fs::copy(from, to)
    })
}

fn prepare_staged_binary_with<F>(
    source: &Path,
    target: &Path,
    expected_sha256: &str,
    mut copy_file: F,
) -> Result<PreparedBinary, String>
where
    F: FnMut(&Path, &Path) -> io::Result<u64>,
{
    if !target.is_file() {
        return Err(format!(
            "The installed app was not found at {}.",
            target.display()
        ));
    }

    let staged = staged_path_for(target);
    remove_file_if_exists(&staged).map_err(|error| {
        format!(
            "Unable to remove an incomplete staged update at {}: {error}",
            staged.display()
        )
    })?;

    let source_hash = file_sha256_hex(source, "the downloaded update")?;
    if source_hash != expected_sha256 {
        return Err(format!(
            "The downloaded update no longer matches the release SHA-256 (expected {expected_sha256}, got {source_hash})."
        ));
    }
    if let Err(error) = copy_file(source, &staged) {
        let cleanup_error = remove_file_if_exists(&staged).err();
        return Err(match cleanup_error {
            Some(cleanup) => format!(
                "Unable to stage the update beside the installed app: {error}. The partial file at {} also could not be removed: {cleanup}",
                staged.display()
            ),
            None => format!("Unable to stage the update beside the installed app: {error}"),
        });
    }

    if let Err(error) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&staged)
        .and_then(|file| file.sync_all())
    {
        let _ = remove_file_if_exists(&staged);
        return Err(format!(
            "Unable to flush the staged update at {}: {error}",
            staged.display()
        ));
    }

    let staged_hash = match file_sha256_hex(&staged, "the staged update") {
        Ok(hash) => hash,
        Err(error) => {
            let _ = remove_file_if_exists(&staged);
            return Err(error);
        }
    };
    if staged_hash != expected_sha256 {
        let _ = remove_file_if_exists(&staged);
        return Err(format!(
            "The staged update failed SHA-256 verification (expected {expected_sha256}, got {staged_hash})."
        ));
    }

    Ok(PreparedBinary { path: staged })
}

fn replace_target_binary(target: &Path, staged: &Path, backup: &Path) -> Result<(), String> {
    replace_target_binary_with(
        target,
        staged,
        backup,
        REPLACE_ATTEMPTS,
        REPLACE_RETRY_DELAY,
        windows_replace_file,
    )
}

fn replace_target_binary_with<F>(
    target: &Path,
    staged: &Path,
    backup: &Path,
    attempts: usize,
    retry_delay: Duration,
    mut replace_file: F,
) -> Result<(), String>
where
    F: FnMut(&Path, &Path, &Path) -> Result<(), String>,
{
    if backup.exists() {
        return Err(format!(
            "Refusing to overwrite the existing recovery copy at {}.",
            backup.display()
        ));
    }

    let mut last_error = "the atomic replace was not attempted".to_string();
    for attempt in 0..attempts.max(1) {
        match replace_file(target, staged, backup) {
            Ok(()) => {
                if !target.is_file() || !backup.is_file() || staged.exists() {
                    return Err(format!(
                        "ReplaceFileW reported success but left an unexpected file layout (target={}, backup={}, staged={}).",
                        target.exists(),
                        backup.exists(),
                        staged.exists()
                    ));
                }
                return Ok(());
            }
            Err(error) => last_error = error,
        }

        // ReplaceFileW documents a partial-failure state in which the old file
        // has already become the backup. Never retry from that changed layout.
        if backup.exists() || !target.exists() || !staged.exists() {
            break;
        }
        if attempt + 1 < attempts.max(1) && !retry_delay.is_zero() {
            std::thread::sleep(retry_delay);
        }
    }

    Err(format!(
        "Unable to atomically replace {}: {last_error}",
        target.display()
    ))
}

fn windows_replace_file(target: &Path, replacement: &Path, backup: &Path) -> Result<(), String> {
    let target_wide = wide_path(target);
    let replacement_wide = wide_path(replacement);
    let backup_wide = wide_path(backup);

    unsafe {
        ReplaceFileW(
            PCWSTR::from_raw(target_wide.as_ptr()),
            PCWSTR::from_raw(replacement_wide.as_ptr()),
            PCWSTR::from_raw(backup_wide.as_ptr()),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
        .map_err(|error| error.to_string())
    }
}

fn restore_and_relaunch_previous(
    target: &Path,
    backup: &Path,
    original_hash: &str,
    reason: String,
) -> Result<(), String> {
    if let Err(rollback_error) = restore_original_if_needed(target, backup, original_hash) {
        return Err(format!(
            "{reason} Rollback could not be completed: {rollback_error}. The recovery copy, when available, has been retained at {}.",
            backup.display()
        ));
    }

    match relaunch_target(target, None) {
        Ok(_) => Err(format!(
            "{reason} The previous version was restored and restarted."
        )),
        Err(relaunch_error) => Err(format!(
            "{reason} The previous version was restored, but could not be restarted automatically: {relaunch_error}"
        )),
    }
}

fn relaunch_unchanged_target(target: &Path, reason: String) -> Result<(), String> {
    match relaunch_target(target, None) {
        Ok(_) => Err(format!("{reason} The existing version was restarted.")),
        Err(relaunch_error) => Err(format!(
            "{reason} The existing version could not be restarted automatically: {relaunch_error}"
        )),
    }
}

fn restore_original_if_needed(
    target: &Path,
    backup: &Path,
    original_hash: &str,
) -> Result<(), String> {
    if file_sha256_hex(target, "the installed app").as_deref() == Ok(original_hash) {
        return Ok(());
    }
    if !backup.is_file() {
        return Err("the original binary is not present at the recovery path".to_string());
    }

    let backup_hash = file_sha256_hex(backup, "the recovery copy")?;
    if backup_hash != original_hash {
        return Err(format!(
            "the recovery copy failed verification (expected {original_hash}, got {backup_hash})"
        ));
    }

    rollback_target_binary(target, backup)?;
    let restored_hash = file_sha256_hex(target, "the restored app")?;
    if restored_hash != original_hash {
        return Err(format!(
            "the restored app failed verification (expected {original_hash}, got {restored_hash})"
        ));
    }
    Ok(())
}

fn rollback_target_binary(target: &Path, backup: &Path) -> Result<(), String> {
    rollback_target_binary_with(target, backup, windows_replace_file)
}

fn rollback_target_binary_with<F>(
    target: &Path,
    backup: &Path,
    mut replace_file: F,
) -> Result<(), String>
where
    F: FnMut(&Path, &Path, &Path) -> Result<(), String>,
{
    let backup_hash = file_sha256_hex(backup, "the recovery copy")?;
    let restore = restore_path_for(target);
    remove_file_if_exists(&restore).map_err(|error| {
        format!(
            "Unable to clear the old rollback staging file at {}: {error}",
            restore.display()
        )
    })?;
    std::fs::copy(backup, &restore).map_err(|error| {
        format!(
            "Unable to copy the recovery binary to {}: {error}",
            restore.display()
        )
    })?;
    if file_sha256_hex(&restore, "the rollback staging file")? != backup_hash {
        return Err(format!(
            "The rollback staging file at {} failed verification; the original backup was retained.",
            restore.display()
        ));
    }

    if !target.exists() {
        std::fs::rename(&restore, target).map_err(|error| {
            format!("Unable to restore the missing target from its verified copy: {error}")
        })?;
        return Ok(());
    }

    let failed = unique_sibling_path(target, "failed");
    match replace_file(target, &restore, &failed) {
        Ok(()) => {}
        Err(error) => {
            // If ReplaceFileW reached a partial state, recover a missing target
            // from another copy. The authoritative .old is never consumed.
            if !target.exists() {
                remove_file_if_exists(&restore).map_err(|cleanup| {
                    format!(
                        "Rollback failed ({error}) and its staging file could not be reset: {cleanup}"
                    )
                })?;
                std::fs::copy(backup, &restore).map_err(|copy_error| {
                    format!(
                        "Rollback failed ({error}); the missing target also could not be recovered from the retained backup: {copy_error}"
                    )
                })?;
                std::fs::rename(&restore, target).map_err(|rename_error| {
                    format!(
                        "Rollback failed ({error}); the verified backup remains at {}, but restoring the target name also failed: {rename_error}",
                        backup.display()
                    )
                })?;
            }

            if file_sha256_hex(target, "the restored app").as_deref() != Ok(backup_hash.as_str()) {
                return Err(format!(
                    "Atomic rollback failed: {error}. The original backup remains at {}.",
                    backup.display()
                ));
            }
        }
    }

    let restored_hash = file_sha256_hex(target, "the restored app")?;
    if restored_hash != backup_hash {
        return Err(format!(
            "The rollback target failed verification; the original backup remains at {}.",
            backup.display()
        ));
    }

    let _ = remove_file_if_exists(&failed);
    let _ = remove_file_if_exists(&restore);
    Ok(())
}

fn relaunch_target(target: &Path, ready_marker: Option<&Path>) -> Result<Child, String> {
    let mut command = Command::new(target);
    if let Some(parent) = target.parent() {
        command.current_dir(parent);
    }

    match ready_marker {
        Some(marker) => {
            command.env(UPDATE_READY_ENV, marker);
        }
        None => {
            command.env_remove(UPDATE_READY_ENV);
        }
    }

    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("The app could not be restarted automatically: {e}"))
}

fn wait_for_update_ready(
    child: &mut Child,
    marker: &Path,
    timeout: Duration,
    ready_grace: Duration,
    poll_interval: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Unable to monitor the restarted app: {error}"))?
        {
            return Err(format!(
                "The updated app exited before confirming a healthy startup ({status})."
            ));
        }

        if marker.exists() {
            let contents = std::fs::read(marker)
                .map_err(|error| format!("Unable to read the update readiness marker: {error}"))?;
            if contents == UPDATE_READY_CONTENT {
                wait_for_ready_survival(child, ready_grace, poll_interval)?;
                return Ok(());
            }
            return Err("The updated app wrote an invalid readiness marker.".to_string());
        }

        if started.elapsed() >= timeout {
            return Err("Timed out waiting for the updated app to become ready.".to_string());
        }
        std::thread::sleep(poll_interval.min(timeout.saturating_sub(started.elapsed())));
    }
}

fn wait_for_ready_survival(
    child: &mut Child,
    grace: Duration,
    poll_interval: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            format!("Unable to monitor the restarted app after readiness: {error}")
        })? {
            return Err(format!(
                "The updated app confirmed readiness but exited during the startup grace period ({status})."
            ));
        }

        let remaining = grace.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(poll_interval.min(remaining));
    }
}

fn stop_child_for_rollback(child: &mut Child) -> Result<(), String> {
    if child
        .try_wait()
        .map_err(|error| format!("Unable to inspect the updated process: {error}"))?
        .is_some()
    {
        return Ok(());
    }

    if let Err(kill_error) = child.kill() {
        if child
            .try_wait()
            .map_err(|error| format!("Unable to inspect the updated process: {error}"))?
            .is_none()
        {
            return Err(format!(
                "Unable to terminate the updated process: {kill_error}"
            ));
        }
        return Ok(());
    }

    child
        .wait()
        .map_err(|error| format!("Unable to wait for the updated process to stop: {error}"))?;
    Ok(())
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), String> {
    if pid == 0 {
        return Err("The updater cannot wait for process ID 0.".to_string());
    }

    unsafe {
        let handle = match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(handle) => handle,
            // The helper starts after the app requests shutdown. If the PID is
            // already gone, OpenProcess reports ERROR_INVALID_PARAMETER; that
            // is the success state we were waiting for, not a monitor failure.
            Err(error) if error.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => {
                return Ok(())
            }
            Err(error) => {
                return Err(format!(
                    "Unable to monitor the running app process: {error}"
                ))
            }
        };

        let result = WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128) as u32);
        let _ = windows::Win32::Foundation::CloseHandle(handle);

        if result == WAIT_OBJECT_0 {
            Ok(())
        } else if result == WAIT_TIMEOUT {
            Err("Timed out waiting for the running app to exit.".to_string())
        } else {
            Err("Unable to confirm that the running app has exited.".to_string())
        }
    }
}

fn updates_dir() -> Result<PathBuf, String> {
    // AIUsageMonitor, not the upstream ClaudeCodeUsageMonitor: the staging
    // file names are fixed, so sharing the upstream directory would let two
    // side-by-side apps overwrite each other's pending update.
    dirs::data_local_dir()
        .map(|dir| dir.join("AIUsageMonitor").join("updates"))
        .or_else(|| Some(std::env::temp_dir().join("AIUsageMonitor").join("updates")))
        .ok_or_else(|| "Unable to resolve a writable local updates directory.".to_string())
}

fn ready_markers_dir() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(UPDATE_TEST_READY_DIR_ENV) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Err(format!("{UPDATE_TEST_READY_DIR_ENV} must not be empty."));
        }
        return Ok(path);
    }

    updates_dir()
}

fn new_ready_marker_path() -> Result<PathBuf, String> {
    let directory = ready_markers_dir()?;
    std::fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Unable to create the update readiness directory at {}: {error}",
            directory.display()
        )
    })?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for counter in 0..16u8 {
        let candidate = directory.join(format!(
            "{UPDATE_READY_PREFIX}{}-{nanos}-{counter}{UPDATE_READY_SUFFIX}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err("Unable to reserve a unique update readiness marker.".to_string())
}

fn validate_ready_marker_path(marker: &Path) -> Result<(), String> {
    let expected_parent = ready_markers_dir()?;
    let parent = marker
        .parent()
        .ok_or_else(|| "The update readiness marker has no parent directory.".to_string())?;
    let name = marker
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The update readiness marker name is invalid.".to_string())?;

    if normalize_path(parent) != normalize_path(&expected_parent)
        || !name.starts_with(UPDATE_READY_PREFIX)
        || !name.ends_with(UPDATE_READY_SUFFIX)
    {
        return Err(format!(
            "Refusing to write an update readiness marker outside {}.",
            expected_parent.display()
        ));
    }
    Ok(())
}

fn write_ready_marker(marker: &Path) -> Result<(), String> {
    if marker.exists() {
        return if std::fs::read(marker).ok().as_deref() == Some(UPDATE_READY_CONTENT) {
            Ok(())
        } else {
            Err("The update readiness marker already exists with invalid content.".to_string())
        };
    }

    let temporary = unique_sibling_path(marker, "writing");
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("Unable to create the update readiness marker: {error}"))?;
        file.write_all(UPDATE_READY_CONTENT)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("Unable to persist the update readiness marker: {error}"))?;
        std::fs::rename(&temporary, marker)
            .map_err(|error| format!("Unable to publish the update readiness marker: {error}"))
    })();

    if write_result.is_err()
        && marker.exists()
        && std::fs::read(marker).ok().as_deref() == Some(UPDATE_READY_CONTENT)
    {
        let _ = remove_file_if_exists(&temporary);
        return Ok(());
    }
    if write_result.is_err() {
        let _ = remove_file_if_exists(&temporary);
    }
    write_result
}

fn updater_ui_disabled_for_tests() -> bool {
    #[cfg(debug_assertions)]
    {
        return std::env::var(UPDATE_TEST_NO_UI_ENV).as_deref() == Ok("1");
    }

    #[cfg(not(debug_assertions))]
    false
}

fn winget_upgrade_command(pid: u32, target: &str, working_dir: &str) -> String {
    let target = powershell_single_quoted(target);
    let working_dir = powershell_single_quoted(working_dir);
    let package_id = WINGET_PACKAGE_ID;

    format!(
        concat!(
            "$ErrorActionPreference = 'Stop'; ",
            "$pidToWait = {pid}; ",
            "$target = '{target}'; ",
            "$workingDir = '{working_dir}'; ",
            "$running = Get-Process -Id $pidToWait -ErrorAction SilentlyContinue; ",
            "if ($null -ne $running) {{ ",
            "try {{ Wait-Process -Id $pidToWait -Timeout 30 -ErrorAction Stop }} ",
            "catch {{ ",
            "$running = Get-Process -Id $pidToWait -ErrorAction SilentlyContinue; ",
            "if ($null -ne $running) {{ Write-Error 'Timed out waiting for AI Usage Monitor to exit; WinGet was not started.'; exit 20 }} ",
            "}} ",
            "}}; ",
            "winget upgrade --id {package_id} --exact; ",
            "$exitCode = $LASTEXITCODE; ",
            "if ($exitCode -eq 0) {{ ",
            "Start-Sleep -Seconds 2; ",
            "$installed = Get-Command ai-usage-monitor -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1; ",
            "if ($null -ne $installed) {{ ",
            "$restartTarget = $installed.Source; ",
            "$restartDir = Split-Path -Parent $restartTarget ",
            "}} elseif (Test-Path -LiteralPath $target -PathType Leaf) {{ ",
            "$restartTarget = $target; $restartDir = $workingDir ",
            "}} else {{ ",
            "Write-Error 'WinGet completed, but the updated AI Usage Monitor command could not be located.'; exit 21 ",
            "}}; ",
            "Start-Process -FilePath $restartTarget -WorkingDirectory $restartDir; ",
            "exit 0 ",
            "}}; ",
            "Write-Host ''; ",
            "Write-Host 'WinGet update failed with exit code' $exitCode; ",
            "Read-Host 'Press Enter to close'; ",
            "exit $exitCode"
        ),
        pid = pid,
        target = target,
        working_dir = working_dir,
        package_id = package_id,
    )
}

fn powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

fn backup_path_for(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app.exe");
    target.with_file_name(format!("{file_name}.old"))
}

fn staged_path_for(target: &Path) -> PathBuf {
    sibling_path_for(target, "new")
}

fn restore_path_for(target: &Path) -> PathBuf {
    sibling_path_for(target, "restore")
}

fn sibling_path_for(target: &Path, suffix: &str) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app.exe");
    target.with_file_name(format!("{file_name}.{suffix}"))
}

fn unique_sibling_path(target: &Path, suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app.exe");
    target.with_file_name(format!(
        "{file_name}.{suffix}-{}-{nanos}",
        std::process::id()
    ))
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_target_location_writable(target: &Path) -> Result<(), String> {
    let parent = target.parent().ok_or_else(|| {
        "Unable to determine the install directory for the current executable.".to_string()
    })?;

    let probe_path = parent.join(".__aium_update_probe");
    match File::create(&probe_path) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe_path);
            Ok(())
        }
        Err(error) => Err(format!(
            "The current install location is not writable. Move the app to a user-writable folder or install it somewhere outside Program Files. {error}"
        )),
    }
}

fn github_repo() -> Result<(&'static str, &'static str), String> {
    let repository = env!("CARGO_PKG_REPOSITORY").trim_end_matches('/');
    let parts: Vec<&str> = repository.split('/').collect();
    if parts.len() < 2 {
        return Err("Package repository URL is not configured for GitHub releases.".to_string());
    }

    let owner = parts[parts.len() - 2];
    let repo = parts[parts.len() - 1];
    if owner.is_empty() || repo.is_empty() {
        return Err("Package repository URL is not configured for GitHub releases.".to_string());
    }

    Ok((owner, repo))
}

fn user_agent() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
}

fn is_winget_install_path(path: &Path) -> bool {
    let normalized_path = normalize_path(path);
    winget_install_roots()
        .into_iter()
        .map(|root| normalize_path(&root))
        .any(|root| normalized_path.starts_with(&root))
}

fn winget_install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        roots.push(
            PathBuf::from(local_app_data)
                .join("Microsoft")
                .join("WinGet")
                .join("Packages"),
        );
    }

    if let Ok(program_files) = std::env::var("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("WinGet").join("Packages"));
    } else {
        roots.push(PathBuf::from(r"C:\Program Files\WinGet\Packages"));
    }

    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        roots.push(
            PathBuf::from(program_files_x86)
                .join("WinGet")
                .join("Packages"),
        );
    } else {
        roots.push(PathBuf::from(r"C:\Program Files (x86)\WinGet\Packages"));
    }

    roots
}

fn normalize_path(path: &Path) -> String {
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();

    normalized
        .strip_prefix("\\\\?\\unc\\")
        .map(|rest| format!("\\\\{rest}"))
        .or_else(|| normalized.strip_prefix("\\\\?\\").map(str::to_owned))
        .unwrap_or(normalized)
}

fn is_version_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(version: &str) -> (u32, u32, u32) {
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.').map(|part| part.parse::<u32>().unwrap_or(0));

    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

fn show_error_message(title: &str, message: &str) {
    unsafe {
        let title_wide = wide_str(title);
        let message_wide = wide_str(message);
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("aium-updater-{name}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("test directory should be creatable");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn manifest_lookup_finds_the_exe_entry() {
        let manifest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ai-usage-monitor.exe\n\
                        fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210  ai-usage-monitor-windows-x64.zip\n";
        assert_eq!(
            expected_checksum_from_manifest(manifest, RELEASE_ASSET_NAME).as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn manifest_lookup_rejects_missing_or_malformed_entries() {
        assert_eq!(
            expected_checksum_from_manifest("", RELEASE_ASSET_NAME),
            None
        );
        // Wrong asset name.
        assert_eq!(
            expected_checksum_from_manifest(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  other.exe",
                RELEASE_ASSET_NAME
            ),
            None
        );
        // Truncated hash.
        assert_eq!(
            expected_checksum_from_manifest("abc123  ai-usage-monitor.exe", RELEASE_ASSET_NAME),
            None
        );
        // Correct length but not hexadecimal.
        assert_eq!(
            expected_checksum_from_manifest(
                "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  ai-usage-monitor.exe",
                RELEASE_ASSET_NAME
            ),
            None
        );
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc"), the FIPS 180-2 test vector.
        assert_eq!(
            sha256_hex(b"abc").expect("hashing should succeed"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn version_comparison_ignores_prerelease_suffix() {
        assert!(is_version_newer("2.1.0", "2.0.0"));
        assert!(!is_version_newer("1.9.9", "2.0.0"));
        assert!(!is_version_newer("2.0.0", "2.0.0"));
    }

    #[test]
    fn updater_pid_must_be_nonzero_and_numeric() {
        assert_eq!(parse_update_pid("42"), Ok(42));
        assert!(parse_update_pid("0").is_err());
        assert!(parse_update_pid("not-a-pid").is_err());
    }

    #[test]
    fn expected_sha256_must_be_exactly_64_hex_characters() {
        let uppercase = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        assert_eq!(
            parse_expected_sha256(uppercase).as_deref(),
            Ok("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(parse_expected_sha256("abc123").is_err());
        assert!(parse_expected_sha256(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
        )
        .is_err());
    }

    #[test]
    fn waiting_for_a_live_process_times_out() {
        let error = wait_for_process_exit(std::process::id(), Duration::ZERO)
            .expect_err("the current test process is still alive");
        assert!(error.contains("Timed out"));
    }

    #[test]
    fn a_pid_that_has_already_disappeared_counts_as_exited() {
        wait_for_process_exit(u32::MAX, Duration::ZERO)
            .expect("a nonexistent process should already satisfy the wait");
    }

    #[test]
    fn ready_grace_rejects_a_child_that_exits() {
        let mut child = Command::new("cmd.exe")
            .arg("/C")
            .arg("exit 23")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("test child should start");

        let error = wait_for_ready_survival(
            &mut child,
            Duration::from_secs(1),
            Duration::from_millis(10),
        )
        .expect_err("an exiting child must fail the post-ready grace period");
        assert!(error.contains("startup grace period"));
    }

    #[test]
    fn interrupted_stage_copy_removes_partial_file_and_keeps_target() {
        let dir = TestDir::new("copy-failure");
        let target = dir.join("app.exe");
        let source = dir.join("download.exe");
        std::fs::write(&target, b"current").unwrap();
        std::fs::write(&source, b"replacement").unwrap();
        let expected_sha256 = sha256_hex(b"replacement").unwrap();

        let error = prepare_staged_binary_with(&source, &target, &expected_sha256, |_from, to| {
            std::fs::write(to, b"partial")?;
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "injected copy interruption",
            ))
        })
        .expect_err("the injected copy must fail");

        assert!(error.contains("injected copy interruption"));
        assert_eq!(std::fs::read(&target).unwrap(), b"current");
        assert!(!staged_path_for(&target).exists());
    }

    #[test]
    fn replaced_source_is_rejected_before_copy_and_target_stays_unchanged() {
        let dir = TestDir::new("source-replaced");
        let target = dir.join("app.exe");
        let source = dir.join("download.exe");
        std::fs::write(&target, b"current-version").unwrap();
        std::fs::write(&source, b"tampered-download").unwrap();
        let expected_sha256 = sha256_hex(b"expected-release").unwrap();

        let error = prepare_staged_binary_with(&source, &target, &expected_sha256, |_from, _to| {
            panic!("copy must not run for a source with the wrong fixed hash")
        })
        .expect_err("the replaced source must be rejected");

        assert!(error.contains("no longer matches the release SHA-256"));
        assert_eq!(std::fs::read(&target).unwrap(), b"current-version");
        assert!(!staged_path_for(&target).exists());
    }

    #[test]
    fn atomic_replace_failure_preserves_original_and_staged_files() {
        let dir = TestDir::new("replace-failure");
        let target = dir.join("app.exe");
        let staged = staged_path_for(&target);
        let backup = backup_path_for(&target);
        std::fs::write(&target, b"current").unwrap();
        std::fs::write(&staged, b"replacement").unwrap();

        let error = replace_target_binary_with(
            &target,
            &staged,
            &backup,
            1,
            Duration::ZERO,
            |_target, _replacement, _backup| Err("injected rename failure".to_string()),
        )
        .expect_err("the injected replace must fail");

        assert!(error.contains("injected rename failure"));
        assert_eq!(std::fs::read(&target).unwrap(), b"current");
        assert_eq!(std::fs::read(&staged).unwrap(), b"replacement");
        assert!(!backup.exists());
    }

    #[test]
    fn partial_replace_failure_recovers_target_without_consuming_backup() {
        let dir = TestDir::new("partial-replace");
        let target = dir.join("app.exe");
        let staged = staged_path_for(&target);
        let backup = backup_path_for(&target);
        std::fs::write(&target, b"current").unwrap();
        std::fs::write(&staged, b"replacement").unwrap();
        let original_hash = file_sha256_hex(&target, "test target").unwrap();

        let error = replace_target_binary_with(
            &target,
            &staged,
            &backup,
            1,
            Duration::ZERO,
            |target, _replacement, backup| {
                std::fs::rename(target, backup).unwrap();
                Err("injected partial replace".to_string())
            },
        )
        .expect_err("the injected replace must report failure");
        assert!(error.contains("injected partial replace"));

        restore_original_if_needed(&target, &backup, &original_hash)
            .expect("the retained backup should recover the missing target");
        assert_eq!(std::fs::read(&target).unwrap(), b"current");
        assert_eq!(std::fs::read(&backup).unwrap(), b"current");
    }

    #[test]
    fn rollback_failure_never_deletes_the_good_backup() {
        let dir = TestDir::new("rollback-failure");
        let target = dir.join("app.exe");
        let backup = backup_path_for(&target);
        std::fs::write(&target, b"broken-new-version").unwrap();
        std::fs::write(&backup, b"known-good-version").unwrap();

        let error = rollback_target_binary_with(&target, &backup, |_target, _restore, _failed| {
            Err("injected rollback rename failure".to_string())
        })
        .expect_err("the injected rollback must fail");

        assert!(error.contains("injected rollback rename failure"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"known-good-version");
        assert_eq!(std::fs::read(&target).unwrap(), b"broken-new-version");
    }

    #[test]
    fn replace_file_w_swaps_target_and_keeps_verified_backup() {
        let dir = TestDir::new("replace-file-w");
        let target = dir.join("app.exe");
        let staged = staged_path_for(&target);
        let backup = backup_path_for(&target);
        std::fs::write(&target, b"current").unwrap();
        std::fs::write(&staged, b"replacement").unwrap();

        replace_target_binary_with(
            &target,
            &staged,
            &backup,
            1,
            Duration::ZERO,
            windows_replace_file,
        )
        .expect("ReplaceFileW should support same-directory files");

        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&backup).unwrap(), b"current");
        assert!(!staged.exists());
    }

    #[test]
    fn winget_wait_is_strict_when_parent_is_still_running() {
        let command = winget_upgrade_command(123, r"C:\app.exe", r"C:\");
        assert!(command.contains("Get-Process -Id $pidToWait"));
        assert!(command.contains("Wait-Process -Id $pidToWait -Timeout 30"));
        assert!(command.contains("exit 20"));
        assert!(!command.contains("catch { }"));
    }

    #[test]
    fn winget_restart_prefers_the_new_portable_command_alias() {
        let command = winget_upgrade_command(123, r"C:\old-version\app.exe", r"C:\old-version");
        assert!(command.contains(
            "Get-Command ai-usage-monitor -CommandType Application -ErrorAction SilentlyContinue"
        ));
        assert!(command.contains("$restartTarget = $installed.Source"));
        assert!(command.contains("Test-Path -LiteralPath $target -PathType Leaf"));
        assert!(command.contains("Start-Process -FilePath $restartTarget"));
        assert!(command.contains("exit 21"));
    }
}
