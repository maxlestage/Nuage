//! Nuage DB Editor: double-click, and a window opens (release builds on
//! Windows have no console window; debug builds keep it, for their messages).
//! A bug in the window shows a page saying so (see `nuage_editor::guard`).
//!
//! The window is drawn by the renderers this system has, in turn: on Windows
//! Direct3D 12 (which falls back to WARP, Windows' software renderer, where
//! there is no graphics driver), then OpenGL; elsewhere OpenGL.
//! `NUAGE_EDITOR_RENDERER=glow` or `wgpu` tries that one first. If none opens
//! the window, the reason is said where a person sees it: a message box or a
//! dialog, the terminal when there is one, and `nuage-editor.log` next to the
//! editor's preferences (the only thing ever written to it).
//!
//! `nuage-editor --self-test <file>` opens the window, draws a few frames,
//! writes which renderer drew them to `<file>`, and closes (see
//! `nuage_editor::selftest`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use eframe::Renderer;
use eframe::egui;
use nuage_editor::i18n::{Lang, tf};
use nuage_editor::selftest::{self, Report};
use nuage_editor::{Editor, Prefs, Setup};

/// The exit code when no renderer opened the window.
const NO_WINDOW: u8 = 3;

/// The longest a self-test may take.
const SELF_TEST_LIMIT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    nuage_editor::guard::install_hook();
    let mut self_test = None;
    // Finder adds -psn_…, desktop launchers %U: what is not the editor's is ignored.
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--self-test" {
            self_test = args.next().map(PathBuf::from);
        } else if arg == "--version" {
            let _ = writeln!(std::io::stdout(), "nuage-editor {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
    }
    if self_test.is_some() {
        // A window that never draws must not hold a release job forever.
        let _ = std::thread::Builder::new().name("nuage-editor-watchdog".into()).spawn(|| {
            std::thread::sleep(SELF_TEST_LIMIT);
            say(&format!("--self-test: no window within {} s", SELF_TEST_LIMIT.as_secs()));
            std::process::exit(4);
        });
    }
    let storage = eframe::storage_dir(nuage_editor::STORAGE_ID);
    // Where a start that crashed in the graphics driver is noticed, the next time.
    let marker = storage.as_ref().map(|dir| dir.join("starting"));
    let renderers = renderers(marker.as_deref());
    let mut failed: Vec<(String, String)> = Vec::new();
    for renderer in renderers.iter().copied() {
        let marker = marker.as_ref().filter(|_| renderers.len() > 1);
        if let Some(marker) = marker {
            let _ = std::fs::create_dir_all(marker.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(marker, renderer.to_string());
        }
        let window = WindowSetup {
            renderer,
            storage: storage.clone(),
            marker: marker.cloned(),
            self_test: self_test.clone(),
            failed: failed.clone(),
        };
        let started = eframe::run_native(
            "Nuage DB Editor",
            options(renderer, storage.as_deref(), self_test.is_some()),
            Box::new(move |cc| Ok(Box::new(Window::new(cc, window)))),
        );
        if let Some(marker) = marker {
            let _ = std::fs::remove_file(marker);
        }
        match started {
            Ok(()) => return finished(self_test.as_deref()),
            Err(e) => failed.push((renderer.to_string(), e.to_string())),
        }
    }
    no_window(&failed, self_test.is_none());
    ExitCode::from(NO_WINDOW)
}

/// The renderers to try, in order.
fn renderers(marker: Option<&Path>) -> Vec<Renderer> {
    #[cfg(windows)]
    let mut list = vec![Renderer::Wgpu, Renderer::Glow];
    #[cfg(not(windows))]
    let mut list = vec![Renderer::Glow];
    // The one whose start crashed last time (the marker stayed) goes last.
    let crashed = marker.and_then(|m| std::fs::read_to_string(m).ok()).and_then(|n| n.trim().parse::<Renderer>().ok());
    if let Some(crashed) = crashed {
        list.sort_by_key(|r| *r == crashed);
    }
    // Support and tests: NUAGE_EDITOR_RENDERER=glow|wgpu goes first.
    let chosen = std::env::var("NUAGE_EDITOR_RENDERER").ok().and_then(|n| n.trim().parse::<Renderer>().ok());
    if let Some(chosen) = chosen {
        list.sort_by_key(|r| *r != chosen);
    }
    list
}

fn options(renderer: Renderer, storage: Option<&Path>, self_test: bool) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Nuage DB Editor")
        // Wayland and X11 group the window under the .desktop entry of this name.
        .with_app_id(nuage_editor::APP_ID)
        .with_inner_size([1000.0, 760.0])
        .with_min_inner_size([640.0, 480.0]);
    // A broken image only means no icon.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../nuage-site/static/icon-192.png")) {
        viewport = viewport.with_icon(icon);
    }
    let persistence_path = if self_test {
        // The person's preferences stay as they are.
        Some(std::env::temp_dir().join(format!("nuage-editor-self-test-{}.ron", std::process::id())))
    } else {
        // The folder of the first versions (eframe would otherwise name it after the app id).
        storage.map(|dir| dir.join("app.ron"))
    };
    eframe::NativeOptions { viewport, renderer, persistence_path, persist_window: !self_test, ..Default::default() }
}

/// What a window is opened with.
struct WindowSetup {
    renderer: Renderer,
    storage: Option<PathBuf>,
    marker: Option<PathBuf>,
    self_test: Option<PathBuf>,
    /// The renderers that failed before this one.
    failed: Vec<(String, String)>,
}

/// The editor in its window: it counts the frames drawn, for the start
/// marker and the self-test.
struct Window {
    editor: Editor,
    setup: WindowSetup,
    frames: u64,
}

impl Window {
    fn new(cc: &eframe::CreationContext<'_>, setup: WindowSetup) -> Window {
        let editor = if setup.self_test.is_some() {
            let vault = Arc::new(nuage_editor::vault::MemoryVault::default());
            Editor::new(Prefs::default(), Setup::with(vault, Box::new(nuage_editor::clip::FakeSink::default())))
        } else {
            let prefs: Prefs = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
            Editor::new(prefs, Setup::system(setup.storage.clone()))
        };
        Window { editor, setup, frames: 0 }
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.editor.frame(ui);
        self.frames += 1;
        if self.frames == 3
            && let Some(marker) = self.setup.marker.take()
        {
            // Drawn: the renderer works on this computer.
            let _ = std::fs::remove_file(marker);
        }
        if let Some(file) = &self.setup.self_test {
            if self.frames == selftest::FRAMES {
                let report = Report::new(&self.setup.renderer.to_string(), self.frames, &self.setup.failed);
                if let Err(e) = report.write(file) {
                    say(&format!("--self-test: {}: {e}", file.display()));
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ui.ctx().request_repaint();
            }
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.setup.self_test.is_none() {
            eframe::App::save(&mut self.editor, storage);
        }
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}

/// The window closed: the editor's exit code (a self-test checks what it wrote).
fn finished(self_test: Option<&Path>) -> ExitCode {
    let Some(file) = self_test else { return ExitCode::SUCCESS };
    match Report::read(file) {
        Ok(report) if report.frames >= selftest::FRAMES => {
            let _ = writeln!(
                std::io::stdout(),
                "nuage-editor --self-test: {} frames drawn with {}",
                report.frames,
                report.renderer
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            say(&format!("--self-test: only {} frames", report.frames));
            ExitCode::FAILURE
        }
        Err(e) => {
            say(&format!("--self-test: {}: {e}", file.display()));
            ExitCode::FAILURE
        }
    }
}

/// On standard error, when there is one (never a panic when it is gone).
fn say(message: &str) {
    let _ = writeln!(std::io::stderr(), "nuage-editor: {message}");
}

/// No renderer opened the window: why, where a person sees it (`dialog`:
/// also in a window of the system's; not for a self-test, which nobody watches).
fn no_window(failed: &[(String, String)], dialog: bool) {
    let l = Lang::of_system();
    let why = failed.iter().map(|(r, e)| format!("{r}: {}", e.trim().trim_end_matches('.'))).collect::<Vec<_>>();
    let message = tf(
        l,
        "Nuage DB Editor could not open its window on this computer (graphics: {}). The web console works in a browser: http://127.0.0.1:8080/admin for a server on this computer.",
        &[&why.join("; ")],
    );
    say(&message);
    let logged = log(&message);
    if dialog {
        let text = match &logged {
            Some(file) => format!("{message}\n\n{}", tf(l, "This message is also in {}.", &[&file.display()])),
            None => message,
        };
        show_dialog(&text);
    }
}

/// Appends `message` to `nuage-editor.log`, next to the editor's preferences: where it is.
fn log(message: &str) -> Option<PathBuf> {
    let dir = eframe::storage_dir(nuage_editor::STORAGE_ID)?;
    let file = dir.join("nuage-editor.log");
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let written = std::fs::create_dir_all(&dir).and_then(|()| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
        writeln!(f, "[{since}] nuage-editor {}: {message}", env!("CARGO_PKG_VERSION"))
    });
    written.ok().map(|()| file)
}

/// A message box: an editor started by a double-click has no terminal to print to.
#[cfg(windows)]
#[allow(unsafe_code)]
fn show_dialog(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Nuage DB Editor"));
    // SAFETY: both strings end with a NUL and live until the call returns; no owner window.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_OK | MB_ICONERROR);
    }
}

/// macOS: an alert (the text escaped for AppleScript); Linux desktops: zenity or kdialog when present.
#[cfg(not(windows))]
fn show_dialog(text: &str) {
    use std::process::{Command, Stdio};
    let quiet = |c: &mut Command| c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    if cfg!(target_os = "macos") {
        let script = format!(
            "display alert \"Nuage DB Editor\" message {} as critical",
            nuage_editor::local::applescript_string(text)
        );
        let _ = quiet(Command::new("/usr/bin/osascript").arg("-e").arg(script));
        return;
    }
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    let zenity =
        quiet(Command::new("zenity").args(["--error", "--no-markup", "--title=Nuage DB Editor", "--text"]).arg(text));
    if zenity.is_err() {
        let _ = quiet(Command::new("kdialog").args(["--title", "Nuage DB Editor", "--error"]).arg(text));
    }
}
