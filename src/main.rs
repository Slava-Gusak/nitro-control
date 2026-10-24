#![windows_subsystem = "windows"]

mod acer_wmi;
mod config;
mod cpu_load;
mod daemon;
mod game;
mod gpu_load;
mod install;
mod ipc;
mod keyhook;
mod ui;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("daemon") => daemon::run(),
        Some("uninstall") => install::uninstall(),
        _ => {
            // Single window: focus the open one instead of starting another.
            if keyhook::focus_existing_ui() {
                return;
            }
            let _ = ui::run();
        }
    }
}
