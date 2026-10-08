//! The block explorer: a window onto a node on this computer (its height, difficulty, estimated hash rate, emission, pool
//! and latest blocks). **Experimental and unaudited. Nothing on any network it shows has value.**
//!
//! With no arguments it shows the node the wallet app runs (`--app-dir FOLDER` names another wallet app folder;
//! `TENERO_APP_DIR` works as for the wallet app). `--data FOLDER` with `--network NAME` or `--control IP:PORT` names a node
//! directly. On Windows the program has no console window: a mistake in the arguments is shown in the window.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;

use tenero_explorer::core::{parse_args, resolve};
use tenero_explorer::ui::App;
use tenero_gui::settings::default_app_dir;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app_dir = std::env::var_os("TENERO_APP_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_app_dir);
    let source = parse_args(&args).and_then(|a| resolve(&a, &app_dir));
    let icon = eframe::egui::IconData {
        rgba: include_bytes!("../../../../assets/tenero-icon-128.rgba").to_vec(),
        width: 128,
        height: 128,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_icon(icon)
            .with_title("Tenero block explorer: TEST NETWORK, NO VALUE")
            .with_inner_size([1150.0, 800.0])
            .with_min_inner_size([760.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tenero block explorer",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, source)))),
    )
}
