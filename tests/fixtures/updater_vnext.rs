use std::path::PathBuf;
use std::thread;
use std::time::Duration;

mod atomic_file;

const UPDATE_READY_ENV: &str = "GENGCHOU_UPDATE_READY_FILE";
const LEGACY_UPDATE_READY_ENV: &str = "AIUM_UPDATE_READY_FILE";
const UPDATE_READY_CONTENT: &[u8] = b"Gengchou update ready\n";
const LEGACY_UPDATE_READY_CONTENT: &[u8] = b"AIUM update ready\n";

fn main() {
    let current_ready = std::env::var_os(UPDATE_READY_ENV).map(PathBuf::from);
    let legacy_ready = std::env::var_os(LEGACY_UPDATE_READY_ENV).map(PathBuf::from);
    let (ready_path, ready_content) = match (current_ready, legacy_ready) {
        (Some(path), None) => (path, UPDATE_READY_CONTENT),
        (None, Some(path)) => (path, LEGACY_UPDATE_READY_CONTENT),
        (Some(_), Some(_)) => panic!("update helper provided both readiness protocols"),
        (None, None) => panic!("update helper did not provide a readiness protocol"),
    };
    let current_exe = std::env::current_exe().expect("vNext fixture has no executable path");
    let work_dir = current_exe
        .parent()
        .expect("vNext fixture has no working directory");

    atomic_file::write_atomic(
        &work_dir.join("vnext.pid"),
        std::process::id().to_string().as_bytes(),
    )
    .expect("vNext fixture could not publish its PID");
    atomic_file::write_atomic(
        &work_dir.join("vnext-ready-path.txt"),
        ready_path.to_string_lossy().as_bytes(),
    )
    .expect("vNext fixture could not record the ready path");
    atomic_file::write_atomic(&ready_path, ready_content)
        .expect("vNext fixture could not signal update readiness");

    // Stay alive long enough for the helper and the test to verify hand-off.
    thread::sleep(Duration::from_secs(30));
}
