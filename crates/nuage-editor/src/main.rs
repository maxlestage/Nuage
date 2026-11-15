//! Nuage DB Editor: double-click, and a window opens (no console window on
//! Windows). If it cannot open (no graphics), or fails, the reason is said
//! where a person sees it: in a message box on Windows, in a log file next
//! to the editor's settings everywhere, and on the terminal when there is one.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use nuage_editor::i18n::{Lang, t, tf};

fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|info| {
        let l = Lang::of_system();
        report(&tf(l, "Nuage DB Editor stopped unexpectedly (a bug): {}", &[info]));
    }));
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../nuage-site/static/icon-192.png")).ok();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("Nuage DB Editor")
        .with_app_id(nuage_editor::APP_ID)
        .with_inner_size([1000.0, 760.0])
        .with_min_inner_size([640.0, 480.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let started = eframe::run_native(
        "Nuage DB Editor",
        options,
        Box::new(|cc| {
            let prefs: nuage_editor::Prefs =
                cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
            let setup = nuage_editor::Setup::system(eframe::storage_dir(nuage_editor::APP_ID));
            Ok(Box::new(nuage_editor::Editor::new(prefs, setup)))
        }),
    );
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let l = Lang::of_system();
            report(&format!(
                "{} {}",
                tf(l, "Nuage DB Editor could not open its window: {}.", &[&e.to_string().trim_end_matches('.')]),
                t(
                    l,
                    "It needs a computer with a screen and graphics (OpenGL 2 or later); on Linux, a desktop (X11 or Wayland)."
                ),
            ));
            ExitCode::FAILURE
        }
    }
}

/// Says `message` where a person sees it.
fn report(message: &str) {
    eprintln!("nuage-editor: {message}");
    let mut where_logged = None;
    if let Some(dir) = eframe::storage_dir(nuage_editor::APP_ID) {
        let file = dir.join("nuage-editor.log");
        let written = std::fs::create_dir_all(&dir).and_then(|()| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
            writeln!(f, "{message}")
        });
        if written.is_ok() {
            where_logged = Some(file);
        }
    }
    #[cfg(windows)]
    {
        let l = Lang::of_system();
        let text = match &where_logged {
            Some(file) => format!("{message}\n\n{}", tf(l, "This message is also in {}.", &[&file.display()])),
            None => message.to_owned(),
        };
        message_box(&text);
    }
    #[cfg(not(windows))]
    let _ = where_logged;
}

/// A message box: on Windows, an editor started by a double-click has no
/// console to print to.
#[cfg(windows)]
#[allow(unsafe_code)]
fn message_box(text: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(window: *mut std::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Nuage DB Editor"));
    // SAFETY: both strings end with a NUL and live until the call returns. 0x10: an error icon.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10);
    }
}
