use std::thread;
use std::time::Duration;

mod atomic_file;

fn main() {
    let current_exe = std::env::current_exe().expect("parent fixture has no executable path");
    let ready_path = current_exe.with_extension("parent-ready");
    atomic_file::write_atomic(&ready_path, std::process::id().to_string().as_bytes())
        .expect("parent fixture could not publish its ready marker");

    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
