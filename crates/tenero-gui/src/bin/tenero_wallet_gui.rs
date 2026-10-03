//! The wallet app: a desktop window for the wallet that also starts and stops the node and the miner.
//! **Experimental and unaudited. Nothing on any network it uses has value.**
//!
//! `--app-dir FOLDER` (or `TENERO_APP_DIR`; the option wins) sets the folder where settings, wallets and node data live, for trying
//! things on a scratch copy or for a desktop shortcut that must open a particular one. On Windows the program has no console window.
//! The window and taskbar icon is the circular logo (`assets/tenero-icon-128.rgba`, made by `tools/make_icons.py`).

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;

use tenero_gui::settings::{default_app_dir, Settings};
use tenero_gui::ui::App;
use tenero_wallet::KdfParams;

fn main() -> eframe::Result {
    // `--app-dir FOLDER`: nothing else is accepted, and a mistake is not guessed at (there is no console to print to, so the program
    // simply opens on the default folder and the window says what it was given)
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut from_args: Option<PathBuf> = None;
    let mut arg_problem: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--app-dir" {
            match it.next() {
                Some(v) if !v.is_empty() => from_args = Some(PathBuf::from(v)),
                _ => arg_problem = Some("--app-dir needs a folder".to_string()),
            }
        } else {
            arg_problem = Some(format!("unknown argument {}", a.to_string_lossy()));
        }
    }
    let app_dir = from_args
        .or_else(|| {
            std::env::var_os("TENERO_APP_DIR")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(default_app_dir);
    let _ = std::fs::create_dir_all(&app_dir);
    // a settings file that does not read is not silently replaced: the window opens on the defaults and says so
    let (settings, mut notice) = match Settings::load(&app_dir) {
        Ok(s) => (s, None),
        Err(e) => (
            Settings::defaults(&app_dir, tenero_app::config::Network::Test),
            Some(format!(
                "{e} (using the defaults; the file was not changed)"
            )),
        ),
    };
    if let Some(p) = arg_problem {
        notice = Some(format!("{p} (opened on {})", app_dir.display()));
    }
    let icon = eframe::egui::IconData {
        rgba: include_bytes!("../../../../assets/tenero-icon-128.rgba").to_vec(),
        width: 128,
        height: 128,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_icon(icon)
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
