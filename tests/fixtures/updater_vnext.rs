use std::path::PathBuf;
use std::thread;
use std::time::Duration;

mod atomic_file;

const UPDATE_READY_ENV: &str = "AIUM_UPDATE_READY_FILE";
const UPDATE_READY_CONTENT: &[u8] = b"AIUM update ready\n";

fn main() {
    let ready_path = std::env::var_os(UPDATE_READY_ENV)
        .map(PathBuf::from)
        .expect("update helper did not provide AIUM_UPDATE_READY_FILE");
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
    atomic_file::write_atomic(&ready_path, UPDATE_READY_CONTENT)
        .expect("vNext fixture could not signal update readiness");

    // Stay alive long enough for the helper and the test to verify hand-off.
    thread::sleep(Duration::from_secs(30));
}
