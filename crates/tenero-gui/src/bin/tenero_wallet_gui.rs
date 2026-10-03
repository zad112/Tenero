//! The wallet app: a desktop window for the wallet that also starts and stops the node and the miner.
//! **Experimental and unaudited. Nothing on any network it uses has value.**
//!
//! `TENERO_APP_DIR` overrides the folder where settings, wallet and node data live (for trying things on a scratch
//! copy). On Windows the program has no console window.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;

use tenero_gui::settings::{default_app_dir, Settings};
use tenero_gui::ui::App;
use tenero_wallet::KdfParams;

fn main() -> eframe::Result {
    let app_dir = std::env::var_os("TENERO_APP_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(default_app_dir, PathBuf::from);
    let _ = std::fs::create_dir_all(&app_dir);
    // a settings file that does not read is not silently replaced: the window opens on the defaults and says so
    let (settings, notice) = match Settings::load(&app_dir) {
        Ok(s) => (s, None),
        Err(e) => (
            Settings::defaults(&app_dir, tenero_app::config::Network::Test),
            Some(format!(
                "{e} (using the defaults; the file was not changed)"
            )),
        ),
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Tenero wallet: TEST NETWORK, NO VALUE")
            .with_inner_size([1000.0, 760.0])
            .with_min_inner_size([780.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tenero wallet",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                app_dir,
                settings,
                KdfParams::DEFAULT,
                notice,
            )))
        }),
    )
}
