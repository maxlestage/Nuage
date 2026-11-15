//! A Nuage DB server installed on this computer by its installer (install.sh,
//! install.ps1): where its files are, whether it answers, and the four lines
//! the editor needs from its settings file: `PORT`, `NUAGE_ADMIN_TOKEN`,
//! `SITE_URL` and `NUAGE_BIND`. Nothing else is ever kept from that file: it
//! holds the encryption key.
//!
//! The installers protect it. The editor itself never runs as administrator:
//! it asks the system to run one of the system's own tools elevated, by
//! absolute path, with a command built from the constants below only (nothing
//! typed reaches it): pkexec and grep on Linux, osascript and grep on macOS,
//! cmd.exe and findstr on Windows. Only the four lines come back, on the
//! tool's output or through a pipe, never as an argument.
//!
//! Everything here may wait (a password window, a disk): the window calls it
//! from a job (see [`crate::jobs`]).

use std::io::Read as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zeroize::Zeroizing;

use crate::i18n::{Lang, t};
use crate::secret::Secret;

#[cfg(test)]
mod parity;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// What install.sh writes on Linux (each path is found in it by a test).
pub const LINUX_SETTINGS: &str = "/etc/nuage/nuage.env";
pub const LINUX_UNIT: &str = "/etc/systemd/system/nuage.service";
/// What install.sh writes on macOS.
pub const MACOS_SETTINGS: &str = "/usr/local/etc/nuage/nuage.env";
pub const MACOS_LABEL: &str = "io.github.maxlestage.nuage";
pub const MACOS_PLIST: &str = "/Library/LaunchDaemons/io.github.maxlestage.nuage.plist";
pub const MACOS_LOG: &str = "/usr/local/var/log/nuage.log";
/// What install.ps1 writes: under `%ProgramData%` (the settings, the log) and
/// `%ProgramFiles%` (the program), and the scheduled task that runs it.
pub const WINDOWS_FOLDER: &str = "Nuage";
pub const WINDOWS_SETTINGS: &str = "nuage.env";
pub const WINDOWS_LOG: &str = "nuage.log";
pub const WINDOWS_PROGRAM: &str = "nuage.exe";
pub const WINDOWS_TASK: &str = "Nuage DB";

/// The only keys taken from the settings file.
pub const KEYS: [&str; 4] = ["PORT", "NUAGE_ADMIN_TOKEN", "SITE_URL", "NUAGE_BIND"];

/// The port a server listens on when its file says none (the server's default).
pub const DEFAULT_PORT: u16 = 8080;

/// The system's tools, by absolute path (never looked up in `PATH`).
pub const PKEXEC: &str = "/usr/bin/pkexec";
pub const GREPS: [&str; 2] = ["/usr/bin/grep", "/bin/grep"];
pub const SYSTEMCTLS: [&str; 2] = ["/usr/bin/systemctl", "/bin/systemctl"];
pub const OSASCRIPT: &str = "/usr/bin/osascript";
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The lines grep keeps: the four keys, with the `export ` and the spaces
/// around `=` the server accepts too.
pub const GREP_PATTERN: &str =
    "^[[:space:]]*(export[[:space:]]+)?(NUAGE_ADMIN_TOKEN|PORT|NUAGE_BIND|SITE_URL)[[:space:]]*=";

/// The most read from the file or from an elevated tool (a settings file is a few KB).
const LIMIT: usize = 256 << 10;

/// The systems the installers set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Linux,
    MacOs,
    Windows,
}

impl Kind {
    /// This system, when the installers know it.
    pub fn of_system() -> Option<Kind> {
        if cfg!(target_os = "linux") {
            Some(Kind::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Kind::MacOs)
        } else if cfg!(windows) {
            Some(Kind::Windows)
        } else {
            None
        }
    }
}

/// Where an installer put a server's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub kind: Kind,
    /// The settings file (`nuage.env`).
    pub settings: PathBuf,
    /// What the installer added to start it: the systemd unit, the launchd
    /// daemon, or the program (Windows' scheduled task cannot be seen without rights).
    pub service: PathBuf,
    /// Its log file (`None` on Linux: the journal, `journalctl -u nuage`).
    pub log: Option<PathBuf>,
}

impl Paths {
    /// This system's (`None`: a system the installers do not know, or Windows
    /// did not say where its folders are).
    pub fn installed() -> Option<Paths> {
        match Kind::of_system()? {
            Kind::Windows => {
                #[cfg(windows)]
                {
                    let (data, programs) = windows::folders()?;
                    Some(Paths::windows(&data, &programs))
                }
                #[cfg(not(windows))]
                None
            }
            kind => Some(Paths::under(Path::new("/"), kind)),
        }
    }

    /// The layout of `kind` under `root`: the system's own at `/`, a test's
    /// in a folder (on Windows, `root` holds `ProgramData` and `Program Files`).
    pub fn under(root: &Path, kind: Kind) -> Paths {
        let at = |path: &str| root.join(path.trim_start_matches('/'));
        match kind {
            Kind::Linux => Paths { kind, settings: at(LINUX_SETTINGS), service: at(LINUX_UNIT), log: None },
            Kind::MacOs => {
                Paths { kind, settings: at(MACOS_SETTINGS), service: at(MACOS_PLIST), log: Some(at(MACOS_LOG)) }
            }
            Kind::Windows => Paths::windows(&root.join("ProgramData"), &root.join("Program Files")),
        }
    }

    fn windows(data: &Path, programs: &Path) -> Paths {
        let data = data.join(WINDOWS_FOLDER);
        Paths {
            kind: Kind::Windows,
            settings: data.join(WINDOWS_SETTINGS),
            service: programs.join(WINDOWS_FOLDER).join(WINDOWS_PROGRAM),
            log: Some(data.join(WINDOWS_LOG)),
        }
    }
}

/// Whether a file can be read by the person running the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Readable,
    /// It is there, for administrators only (`Path::exists` says `false` then).
    Protected,
    Absent,
}

fn access(path: &Path) -> Access {
    match std::fs::File::open(path) {
        Ok(file) if file.metadata().is_ok_and(|m| m.is_file()) => Access::Readable,
        Ok(_) => Access::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Access::Protected,
        Err(_) => Access::Absent,
    }
}

/// Whether something is at `path`, even where only administrators may look.
fn present(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
    }
}

/// A server an installer set up on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    pub paths: Paths,
    /// The person running the editor can read its settings file (else it is
    /// for administrators only, and the system asks for one).
    pub readable: bool,
    /// The installer's service is there (else the server is started by hand,
    /// as on Linux without systemd).
    pub service: bool,
    /// The port its health was asked on: the file's when it is readable, else 8080.
    pub port: u16,
    /// It answered (`/api/health`).
    pub answers: bool,
}

/// Looks for a server the installers set up at `paths`, and asks `answers`
/// whether something answers as Nuage DB on its port. No rights needed.
pub fn detect(paths: Paths, answers: impl FnOnce(u16) -> bool) -> Option<Detected> {
    let settings = access(&paths.settings);
    let service = present(&paths.service);
    let installed = match settings {
        Access::Readable | Access::Protected => true,
        // Windows: a folder of administrators only may hide even the file's name; the program says it is there.
        Access::Absent => paths.kind == Kind::Windows && service,
    };
    if !installed {
        return None;
    }
    let readable = settings == Access::Readable;
    let port = if readable { read_file(&paths.settings).ok().map(|f| f.port) } else { None };
    let port = port.unwrap_or(DEFAULT_PORT);
    Some(Detected { paths, readable, service, port, answers: answers(port) })
}

/// A server of this computer, if an installer set one up (on a job: it asks the port).
pub fn detect_installed() -> Option<Detected> {
    detect(Paths::installed()?, health_answers)
}

/// Whether Nuage DB answers `/api/health` on `port` of this computer (at most 2 s).
pub fn health_answers(port: u16) -> bool {
    let transport = crate::api::Transport {
        connect: std::time::Duration::from_secs(1),
        connect_loopback: std::time::Duration::from_secs(1),
        global: std::time::Duration::from_secs(2),
        body_limit: 64 << 10,
    };
    // 127.0.0.1, not localhost: that may be ::1, where an IPv4 server does not listen.
    let address = format!("http://127.0.0.1:{port}");
    crate::api::Server::new(&address, Secret::new(String::new()), Lang::En, &transport)
        .is_ok_and(|server| server.health().is_ok())
}

/// How the settings file was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// Directly: the person running the editor may read it (the account that
    /// installed on macOS). It can be read again next time: nothing is kept.
    File,
    /// By a system tool, elevated with an administrator's consent.
    Administrator,
}

/// What the editor takes from the settings file of a server of this computer.
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    /// The settings file.
    pub file: PathBuf,
    /// Where the server answers on this computer (`http://127.0.0.1:<PORT>`).
    pub address: String,
    pub port: u16,
    /// `NUAGE_BIND`: where it listens (`127.0.0.1`: for this computer only,
    /// behind a proxy; none: everywhere).
    pub bind: Option<String>,
    /// Its public address (`SITE_URL`, written by the installer given a
    /// domain): the one its applications are given.
    pub site_url: Option<String>,
    /// Its management token, when the file has one.
    pub token: Option<Secret>,
    pub via: Via,
}

impl Found {
    /// Never the token.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.address, self.file.display())
    }

    /// It listens for this computer only (`NUAGE_BIND=127.0.0.1`): no other
    /// device of the network reaches it.
    pub fn this_computer_only(&self) -> bool {
        self.bind.as_deref().and_then(|b| b.trim().parse::<IpAddr>().ok()).is_some_and(|ip| ip.is_loopback())
    }
}

/// One `KEY=value` line as the server reads it (`#` comments, an optional
/// `export `, spaces around `=`, a value as it is or between quotes).
fn parse_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").map_or(line, str::trim_start);
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    let value = [('"', '"'), ('\'', '\'')]
        .iter()
        .find_map(|&(a, b)| value.strip_prefix(a)?.strip_suffix(b))
        .filter(|_| value.len() >= 2)
        .unwrap_or(value);
    Some((key.trim(), value))
}

/// The lines of a settings file. A carriage return alone ends a line too (what
/// AppleScript hands back when it alters line endings).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.split(['\n', '\r'])
}

/// The variables of a settings file, as the server reads them (later lines win).
pub fn parse(text: &str) -> Vec<(String, String)> {
    lines(text).filter_map(parse_line).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

/// The four keys of the settings file `file`, from its text (or the lines an
/// elevated tool kept of it). Nothing else is copied out of `text`.
pub fn found_in(file: &Path, text: &str, via: Via) -> Found {
    let mut values: [Option<&str>; 4] = [None; 4];
    for (key, value) in lines(text).filter_map(parse_line) {
        if let Some(i) = KEYS.iter().position(|k| *k == key)
            && let Some(slot) = values.get_mut(i)
        {
            *slot = Some(value).filter(|v| !v.is_empty());
        }
    }
    let [port, token, site_url, bind] = values;
    let port = port.and_then(|p| p.trim().parse::<u16>().ok()).filter(|p| *p != 0).unwrap_or(DEFAULT_PORT);
    let bind = bind.map(str::to_owned);
    Found {
        file: file.to_path_buf(),
        address: local_address(bind.as_deref(), port),
        port,
        bind,
        site_url: site_url.map(str::to_owned),
        token: token.map(|t| Secret::new(t.to_owned())),
        via,
    }
}

/// Where a server listening on `bind` answers on this computer.
fn local_address(bind: Option<&str>, port: u16) -> String {
    let host = match bind.map(str::trim).and_then(|b| b.trim_matches(['[', ']']).parse::<IpAddr>().ok()) {
        // Everywhere (0.0.0.0, the default): this computer's IPv4 loopback.
        None => "127.0.0.1".to_owned(),
        Some(ip) if ip.is_unspecified() && ip.is_ipv6() => "[::1]".to_owned(),
        Some(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        Some(IpAddr::V6(v6)) => format!("[{v6}]"),
        Some(IpAddr::V4(v4)) => v4.to_string(),
    };
    format!("http://{host}:{port}")
}

/// Why the settings file was not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// No server was set up here by the installers.
    NotInstalled,
    /// The file is for administrators only: [`read_as_administrator`] asks the system.
    NeedsAdministrator(PathBuf),
    /// The person closed the system's window: nothing was read.
    Dismissed,
    /// The system did not accept who answered (a wrong password, an account
    /// that is not an administrator's): its words.
    NotAccepted(String),
    /// The system cannot ask here (no polkit agent, no pkexec): its words.
    NoPrompt(String),
    /// The elevated tool could not read the file: its words.
    Unreadable(String),
    /// Windows' elevated command ended without handing the lines over (its exit code; `None`: it did not end in time).
    NotHandedOver(Option<u32>),
    /// Something else: the system's words.
    Failed(String),
}

/// Reads the settings file directly (it is the reader's, or the editor runs
/// as root): its four keys, the rest wiped from memory.
pub fn read_file(file: &Path) -> Result<Found, ReadError> {
    let text = read_limited(file).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => ReadError::NeedsAdministrator(file.to_path_buf()),
        std::io::ErrorKind::NotFound => ReadError::NotInstalled,
        _ => ReadError::Unreadable(e.to_string()),
    })?;
    let text = std::str::from_utf8(&text).map_err(|e| ReadError::Unreadable(e.to_string()))?;
    Ok(found_in(file, text, Via::File))
}

/// At most [`LIMIT`] bytes of `file`, in a buffer wiped when dropped (made
/// large enough at once: a growing buffer would leave copies behind).
fn read_limited(file: &Path) -> std::io::Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(LIMIT + 64));
    std::fs::File::open(file)?.take(LIMIT as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Reads the settings file of the server this computer's installer set up,
/// as the person running the editor.
pub fn read() -> Result<Found, ReadError> {
    let paths = Paths::installed().ok_or(ReadError::NotInstalled)?;
    match access(&paths.settings) {
        Access::Readable => read_file(&paths.settings),
        Access::Protected => Err(ReadError::NeedsAdministrator(paths.settings)),
        Access::Absent if paths.kind == Kind::Windows && present(&paths.service) => {
            Err(ReadError::NeedsAdministrator(paths.settings))
        }
        Access::Absent => Err(ReadError::NotInstalled),
    }
}

/// Whether the system can ask for an administrator's consent here; if not,
/// why, in the system's terms.
pub fn can_ask_administrator() -> Result<(), String> {
    match Kind::of_system() {
        Some(Kind::Linux) if Path::new(PKEXEC).is_file() => Ok(()),
        Some(Kind::Linux) => Err(format!("{PKEXEC} is missing: polkit is not installed")),
        Some(Kind::MacOs) if Path::new(OSASCRIPT).is_file() => Ok(()),
        Some(Kind::MacOs) => Err(format!("{OSASCRIPT} is missing")),
        Some(Kind::Windows) => {
            #[cfg(windows)]
            return windows::can_ask();
            #[cfg(not(windows))]
            Ok(())
        }
        None => Err("no installer for this system".to_owned()),
    }
}

/// Asks to stop waiting for a system window: pkexec or osascript is stopped,
/// and their window with them. Windows' elevated command cannot be stopped by
/// the editor: it is left to end by itself.
#[derive(Clone, Debug, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn now(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn asked(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The sentence the macOS password window shows (AppleScript's `with prompt`).
pub fn read_prompt(l: Lang) -> &'static str {
    t(
        l,
        "Nuage DB Editor reads the port and the management token of the Nuage DB server of this computer (not its encryption key).",
    )
}

/// The sentence the macOS password window shows to start the server.
pub fn start_prompt(l: Lang) -> &'static str {
    t(l, "Nuage DB Editor starts the Nuage DB server of this computer.")
}

/// Reads the four lines of the settings file with an administrator's
/// consent, asked by the system in a window of its own. The macOS window says
/// [`read_prompt`] in `l`.
pub fn read_as_administrator(l: Lang, stop: &Stop) -> Result<Found, ReadError> {
    let paths = Paths::installed().ok_or(ReadError::NotInstalled)?;
    let _ = (l, stop);
    match paths.kind {
        #[cfg(unix)]
        Kind::Linux => unix::read_with_pkexec(&paths, stop),
        #[cfg(unix)]
        Kind::MacOs => unix::read_with_osascript(&paths, read_prompt(l), stop),
        #[cfg(windows)]
        Kind::Windows => windows::read_elevated(&paths),
        #[allow(unreachable_patterns)]
        _ => Err(ReadError::NotInstalled),
    }
}

/// Starts the server the installer set up, with an administrator's consent:
/// `systemctl start` on Linux, `launchctl kickstart` on macOS, its scheduled
/// task on Windows. Each does nothing to a server already running; nothing
/// here stops or restarts one.
pub fn start_service(l: Lang, stop: &Stop) -> Result<(), ReadError> {
    let paths = Paths::installed().ok_or(ReadError::NotInstalled)?;
    let _ = (l, stop);
    match paths.kind {
        #[cfg(unix)]
        Kind::Linux => unix::start_with_pkexec(stop),
        #[cfg(unix)]
        Kind::MacOs => unix::start_with_osascript(start_prompt(l), stop),
        #[cfg(windows)]
        Kind::Windows => windows::start_task(),
        #[allow(unreachable_patterns)]
        _ => Err(ReadError::NotInstalled),
    }
}

/// grep's arguments: the four keys' lines of `file`.
pub fn grep_argv(grep: &str, file: &str) -> Vec<String> {
    vec![grep.to_owned(), "-E".to_owned(), GREP_PATTERN.to_owned(), file.to_owned()]
}

/// pkexec running grep on `file` as root.
pub fn pkexec_read_argv(grep: &str, file: &str) -> Vec<String> {
    let mut argv = vec![PKEXEC.to_owned()];
    argv.extend(grep_argv(grep, file));
    argv
}

/// pkexec starting the systemd service (nothing happens to a running one).
pub fn pkexec_start_argv(systemctl: &str) -> Vec<String> {
    vec![PKEXEC.to_owned(), systemctl.to_owned(), "start".to_owned(), "nuage".to_owned()]
}

/// `text` as an AppleScript string, quotes included.
pub fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `text` as one word of a shell command, quotes included.
fn shell_word(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// osascript running `command` as root, after the system's password window,
/// which says `prompt`. The output's line endings stay as they are.
fn osascript_argv(command: &str, prompt: &str) -> Vec<String> {
    let script = format!(
        "do shell script {} with prompt {} with administrator privileges without altering line endings",
        applescript_string(command),
        applescript_string(prompt)
    );
    vec![OSASCRIPT.to_owned(), "-e".to_owned(), script]
}

/// osascript running grep on `file` as root (macOS).
pub fn osascript_read_argv(file: &str, prompt: &str) -> Vec<String> {
    let command = grep_argv("/usr/bin/grep", file).iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" ");
    osascript_argv(&command, prompt)
}

/// osascript starting the launchd daemon (kickstart without -k never kills
/// a running one; bootstrap loads it when it was unloaded).
pub fn osascript_start_argv(prompt: &str) -> Vec<String> {
    let command = format!(
        "{LAUNCHCTL} kickstart system/{MACOS_LABEL} || {LAUNCHCTL} bootstrap system {}",
        shell_word(MACOS_PLIST)
    );
    osascript_argv(&command, prompt)
}

/// Whether `path` can be written in a cmd.exe command line as it is, between
/// quotes: none of the characters cmd reads there.
pub fn safe_for_cmd(path: &str) -> bool {
    !path.is_empty() && !path.chars().any(|c| "%^&|<>()\"!".contains(c) || c.is_control())
}

/// cmd.exe's parameters on Windows: findstr keeps the four keys' lines of
/// `settings` and writes them into `pipe`. `/s` makes cmd take the outer
/// quotes off and run the rest as written. `/i`: findstr can miss lines when
/// it looks for several case-sensitive strings of different lengths; the
/// keys are told apart again when the lines are read.
pub fn findstr_parameters(findstr: &str, settings: &str, pipe: &str) -> String {
    let searches: String = KEYS.iter().map(|k| format!(" /c:\"{k}=\"")).collect();
    format!("/d /s /c \"\"{findstr}\" /b /l /i{searches} \"{settings}\" > {pipe}\"")
}

/// schtasks.exe's parameters on Windows: run the installer's task (nothing
/// happens to a running one: the task ignores a second instance).
pub fn schtasks_parameters() -> String {
    format!("/Run /TN \"{WINDOWS_TASK}\"")
}

/// What pkexec's exit means (grep's own: 0 lines, 1 none, 2 an error).
/// `Ok(true)`: lines came; `Ok(false)`: none.
pub fn after_pkexec(code: Option<i32>, stderr: &str) -> Result<bool, ReadError> {
    let said = stderr.trim().to_owned();
    match code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(2) => Err(ReadError::Unreadable(said)),
        // The password window was dismissed.
        Some(126) => Err(ReadError::Dismissed),
        // Not authorized, or no window could be shown at all.
        Some(127) if said.to_ascii_lowercase().contains("no authentication agent") => Err(ReadError::NoPrompt(said)),
        Some(127) => Err(ReadError::NotAccepted(said)),
        Some(code) => Err(ReadError::Failed(format!("pkexec: {code} {said}").trim_end().to_owned())),
        None => Err(ReadError::Failed("pkexec was stopped".to_owned())),
    }
}

/// AppleScript's error on standard error: `0:123: execution error: User
/// canceled. (-128)` gives the message and the number.
fn applescript_error(stderr: &str) -> (String, Option<i64>) {
    let text = stderr.trim();
    let text = text.split_once("execution error: ").map_or(text, |(_, rest)| rest);
    match text.rsplit_once(" (").and_then(|(message, n)| Some((message, n.strip_suffix(')')?.parse().ok()?))) {
        Some((message, n)) => (message.trim().to_owned(), Some(n)),
        None => (text.to_owned(), None),
    }
}

/// What osascript's exit means: `do shell script` fails with the command's
/// exit code (grep's 1: no line, 2: an error), `-128` when the password
/// window was cancelled, and other negative numbers for refusals.
pub fn after_osascript(code: Option<i32>, stderr: &str) -> Result<bool, ReadError> {
    if code == Some(0) {
        return Ok(true);
    }
    if code.is_none() {
        return Err(ReadError::Failed("osascript was stopped".to_owned()));
    }
    match applescript_error(stderr) {
        (_, Some(-128)) => Err(ReadError::Dismissed),
        (_, Some(1)) => Ok(false),
        (message, Some(2)) => Err(ReadError::Unreadable(message)),
        (message, _) => Err(ReadError::NotAccepted(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installers_settings_file() {
        let text = "\u{feff}# Nuage DB settings\r\nPORT=8091\r\nNUAGE_ENCRYPTION_KEY=abc\r\nexport NUAGE_ADMIN_TOKEN=\"0123\"\r\nSITE_URL=https://nuage.example.com\r\nNUAGE_BIND=127.0.0.1\r\n";
        let found = found_in(Path::new("/etc/nuage/nuage.env"), text, Via::Administrator);
        assert_eq!((found.address.as_str(), found.port), ("http://127.0.0.1:8091", 8091));
        assert_eq!(found.site_url.as_deref(), Some("https://nuage.example.com"));
        assert_eq!(found.token.as_ref().map(Secret::expose), Some("0123"));
        assert!(found.this_computer_only());
        assert!(!found.describe().contains("0123"), "never the token");
        assert!(!format!("{found:?}").contains("0123"), "never the token");
        // No port: the server's own default; an empty token is none.
        let found = found_in(Path::new("x"), "NUAGE_ADMIN_TOKEN=\nNUAGE_BIND=0.0.0.0\n", Via::File);
        assert!(!found.this_computer_only());
        assert_eq!((found.address.as_str(), found.token, found.site_url), ("http://127.0.0.1:8080", None, None));
        // AppleScript's line endings, spaces around =, a later line winning.
        let found = found_in(Path::new("x"), "PORT=1\r  export PORT = 8092\rNUAGE_BIND=::\r", Via::File);
        assert_eq!(found.address, "http://[::1]:8092");
        assert_eq!(local_address(Some("192.168.1.5"), 80), "http://192.168.1.5:80");
    }

    #[test]
    fn commands_are_made_of_constants() {
        assert_eq!(
            pkexec_read_argv("/usr/bin/grep", LINUX_SETTINGS),
            [PKEXEC, "/usr/bin/grep", "-E", GREP_PATTERN, LINUX_SETTINGS]
        );
        assert_eq!(pkexec_start_argv("/usr/bin/systemctl"), [PKEXEC, "/usr/bin/systemctl", "start", "nuage"]);
        let prompt = read_prompt(Lang::Fr);
        let argv = osascript_read_argv(MACOS_SETTINGS, prompt);
        assert_eq!(argv.len(), 3);
        assert_eq!(
            argv.get(2).map(String::as_str),
            Some(
                format!(
                    "do shell script \"'/usr/bin/grep' '-E' '{GREP_PATTERN}' '{MACOS_SETTINGS}'\" with prompt \"{prompt}\" with administrator privileges without altering line endings"
                )
                .as_str()
            )
        );
        assert_eq!(applescript_string("a \"b\" \\c"), "\"a \\\"b\\\" \\\\c\"");
        let start = osascript_start_argv("go");
        assert!(start.get(2).is_some_and(|s| s.starts_with(
            "do shell script \"/bin/launchctl kickstart system/io.github.maxlestage.nuage || /bin/launchctl bootstrap system '/Library/LaunchDaemons/io.github.maxlestage.nuage.plist'\""
        )));
        assert_eq!(
            findstr_parameters(
                r"C:\Windows\system32\findstr.exe",
                r"C:\ProgramData\Nuage\nuage.env",
                r"\\.\pipe\nuage-editor-00ff"
            ),
            r#"/d /s /c ""C:\Windows\system32\findstr.exe" /b /l /i /c:"PORT=" /c:"NUAGE_ADMIN_TOKEN=" /c:"SITE_URL=" /c:"NUAGE_BIND=" "C:\ProgramData\Nuage\nuage.env" > \\.\pipe\nuage-editor-00ff""#
        );
        assert_eq!(schtasks_parameters(), r#"/Run /TN "Nuage DB""#);
        assert!(safe_for_cmd(r"C:\Program Files\Nuage") && safe_for_cmd(r"D:\ProgramData"));
        for bad in [r"C:\100%", r"C:\a&b", r"C:\x^y", r"C:\(x86)", "C:\\\"", "C:\\!x", "", "C:\\a\nb"] {
            assert!(!safe_for_cmd(bad), "{bad:?}");
        }
    }

    #[test]
    fn each_answer_of_the_system_is_told_apart() {
        assert_eq!(after_pkexec(Some(0), ""), Ok(true));
        assert_eq!(after_pkexec(Some(1), ""), Ok(false));
        assert_eq!(after_pkexec(Some(126), ""), Err(ReadError::Dismissed));
        let refused = "Error executing command as another user: Not authorized\n\nThis incident has been reported.";
        assert!(
            matches!(after_pkexec(Some(127), refused), Err(ReadError::NotAccepted(s)) if s.contains("Not authorized"))
        );
        let alone = "Error executing command as another user: No authentication agent found.";
        assert!(matches!(after_pkexec(Some(127), alone), Err(ReadError::NoPrompt(_))));
        assert!(matches!(after_pkexec(Some(2), "grep: x: Permission denied"), Err(ReadError::Unreadable(_))));
        assert!(matches!(after_pkexec(None, ""), Err(ReadError::Failed(_))));
        assert_eq!(after_osascript(Some(0), ""), Ok(true));
        assert_eq!(
            after_osascript(Some(1), "0:180: execution error: User canceled. (-128)\n"),
            Err(ReadError::Dismissed)
        );
        assert_eq!(
            after_osascript(Some(1), "0:180: execution error: The command exited with a non-zero status. (1)\n"),
            Ok(false)
        );
        assert_eq!(
            after_osascript(Some(1), "0:180: execution error: grep: /x: No such file or directory (2)\n"),
            Err(ReadError::Unreadable("grep: /x: No such file or directory".into()))
        );
        assert_eq!(
            after_osascript(
                Some(1),
                "0:180: execution error: The administrator user name or password was incorrect. (-60007)"
            ),
            Err(ReadError::NotAccepted("The administrator user name or password was incorrect.".into()))
        );
    }

    #[test]
    fn a_stop_is_shared() {
        let stop = Stop::default();
        let copy = stop.clone();
        assert!(!copy.asked());
        stop.now();
        assert!(copy.asked());
    }
}
