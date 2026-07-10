#![windows_subsystem = "windows"]

mod diagnose;
mod localization;
mod models;
mod native_interop;
mod poller;
mod theme;
mod tray_icon;
mod updater;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Diagnostics are always on (append + rotation, see diagnose.rs); the old
    // --diagnose flag is accepted but no longer required.
    match diagnose::init() {
        Ok(path) => diagnose::log(format!("startup args={args:?} log_path={}", path.display())),
        Err(error) => {
            // Logging may not be available yet, but keep startup behavior unchanged.
            let _ = error;
        }
    }

    // Any panic must leave a trace in the diagnostic log; with the default
    // hook the process just vanished (stderr is invisible in a GUI subsystem).
    std::panic::set_hook(Box::new(|info| {
        diagnose::log(format!("PANIC: {info}"));
    }));

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        diagnose::log(format!("cli mode exited with code {exit_code}"));
        std::process::exit(exit_code);
    }

    // Diagnostic: render every tray icon state to BMP files and exit.
    if let Some(pos) = args.iter().position(|arg| arg == "--dump-tray-icons") {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        std::process::exit(tray_icon::dump_icons(&dir));
    }

    diagnose::log("entering window::run");
    window::run();
}
