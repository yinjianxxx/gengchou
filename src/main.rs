#![windows_subsystem = "windows"]

mod compact_layout;
mod compact_view;
mod diagnose;
mod localization;
mod models;
mod native_interop;
mod poller;
mod settings;
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

    // Diagnostic: render the detail popup with representative data to a BMP
    // and exit. Used to eyeball popup layout changes and to render README
    // previews. Optional tokens after the directory select the fixture
    // language (`en`/`zh`, default `zh`) and force a theme (`dark`/`light`,
    // default: follow the system theme).
    if let Some(pos) = args.iter().position(|arg| arg == "--dump-detail-popup") {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        let mut english = false;
        let mut force_dark = None;
        for token in args.iter().skip(pos + 2) {
            match token.to_ascii_lowercase().as_str() {
                "en" => english = true,
                "zh" | "zh-cn" => english = false,
                "dark" => force_dark = Some(true),
                "light" => force_dark = Some(false),
                _ => break,
            }
        }
        std::process::exit(window::dump_detail_popup(&dir, english, force_dark));
    }

    // Diagnostic: render both compact surfaces with representative data and
    // exit. This is the fast visual gate before launching a live Debug build.
    if let Some(pos) = args
        .iter()
        .position(|arg| arg == "--dump-widget" || arg == "--dump-compact-surfaces")
    {
        let dir = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        std::process::exit(window::dump_widget(&dir));
    }

    let Some(instance_guard) = window::acquire_single_instance() else {
        return;
    };

    diagnose::log("entering window::run");
    window::run(instance_guard);
}
