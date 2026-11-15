//! Linux and macOS: the system's own tools, elevated after its password
//! window (pkexec, osascript), run with the commands of [`super`], and what
//! their exit means.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use zeroize::Zeroizing;

use super::{Found, LIMIT, Paths, ReadError, Stop, Via};

/// What a tool gave back.
pub(super) struct Ran {
    /// Its exit code (`None`: stopped by a signal, as by [`Stop`]).
    pub code: Option<i32>,
    /// Its output (the settings' lines): wiped when dropped.
    pub stdout: Zeroizing<Vec<u8>>,
    pub stderr: String,
    /// [`Stop`] stopped it.
    pub stopped: bool,
}

/// Runs `argv` (no shell), and waits for it, however long a person takes to
/// type a password, unless `stop` asks to stop it.
pub(super) fn run(argv: &[String], stop: &Stop) -> Result<Ran, ReadError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(ReadError::Failed("no command".to_owned()));
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ReadError::NoPrompt(format!("{program}: {e}")),
            _ => ReadError::Failed(format!("{program}: {e}")),
        })?;
    // Read while it runs: a full pipe would block it.
    let stdout = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut bytes = Zeroizing::new(Vec::with_capacity(LIMIT + 64));
            let _ = (&mut out).take(LIMIT as u64).read_to_end(&mut bytes);
            bytes
        })
    });
    let stderr = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut err).take(16 << 10).read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });
    let mut stopped = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if stop.asked() => {
                stopped = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(ReadError::Failed(format!("{program}: {e}"))),
        }
    };
    let stdout = stdout.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    Ok(Ran { code: status.and_then(|s| s.code()), stdout, stderr, stopped })
}

/// The lines a tool kept, as the settings of `file`.
fn found(file: &Path, lines: Option<&[u8]>) -> Result<Found, ReadError> {
    let text = std::str::from_utf8(lines.unwrap_or_default()).map_err(|e| ReadError::Unreadable(e.to_string()))?;
    Ok(super::found_in(file, text, Via::Administrator))
}

fn first_file(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|p| Path::new(p).is_file())
}

/// Linux: the settings file as root, through polkit's password window.
pub(super) fn read_with_pkexec(paths: &Paths, stop: &Stop) -> Result<Found, ReadError> {
    if !super::present(&paths.settings) {
        return Err(ReadError::NotInstalled);
    }
    if !Path::new(super::PKEXEC).is_file() {
        return Err(ReadError::NoPrompt(format!("{} is missing: polkit is not installed", super::PKEXEC)));
    }
    let grep = first_file(&super::GREPS).ok_or_else(|| ReadError::Failed("grep is missing".to_owned()))?;
    let file = paths.settings.to_string_lossy();
    let ran = run(&super::pkexec_read_argv(grep, &file), stop)?;
    if ran.stopped {
        return Err(ReadError::Dismissed);
    }
    let lines = super::after_pkexec(ran.code, &ran.stderr)?;
    found(&paths.settings, lines.then_some(ran.stdout.as_slice()))
}

/// macOS: the settings file, directly when it is the reader's (the account
/// that installed: no window, nothing kept), else as root through the
/// system's password window, which says `prompt`.
pub(super) fn read_with_osascript(paths: &Paths, prompt: &str, stop: &Stop) -> Result<Found, ReadError> {
    match super::read_file(&paths.settings) {
        Err(ReadError::NeedsAdministrator(_)) => {}
        direct => return direct,
    }
    let file = paths.settings.to_string_lossy();
    let ran = run(&super::osascript_read_argv(&file, prompt), stop)?;
    if ran.stopped {
        return Err(ReadError::Dismissed);
    }
    let lines = super::after_osascript(ran.code, &ran.stderr)?;
    found(&paths.settings, lines.then_some(ran.stdout.as_slice()))
}

/// Linux: `systemctl start nuage` as root, where systemd runs and the installer made its unit.
pub(super) fn start_with_pkexec(stop: &Stop) -> Result<(), ReadError> {
    if !Path::new("/run/systemd/system").is_dir() || !Path::new(super::LINUX_UNIT).exists() {
        return Err(ReadError::Failed("no systemd service nuage on this computer".to_owned()));
    }
    let systemctl =
        first_file(&super::SYSTEMCTLS).ok_or_else(|| ReadError::Failed("systemctl is missing".to_owned()))?;
    let ran = run(&super::pkexec_start_argv(systemctl), stop)?;
    match super::after_pkexec(ran.code, &ran.stderr) {
        Ok(true) => Ok(()),
        // systemctl's own failure (1): its words.
        Ok(false) => Err(ReadError::Failed(ran.stderr.trim().to_owned())),
        Err(e) => Err(e),
    }
}

/// macOS: the launchd daemon, started as root.
pub(super) fn start_with_osascript(prompt: &str, stop: &Stop) -> Result<(), ReadError> {
    let ran = run(&super::osascript_start_argv(prompt), stop)?;
    match super::after_osascript(ran.code, &ran.stderr) {
        Ok(true) => Ok(()),
        Ok(false) => Err(ReadError::Failed(ran.stderr.trim().to_owned())),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_run_and_stopped() {
        let ran =
            run(&["/bin/sh".into(), "-c".into(), "echo out; echo err >&2; exit 3".into()], &Stop::default()).unwrap();
        assert_eq!((ran.code, ran.stdout.as_slice(), ran.stderr.as_str()), (Some(3), &b"out\n"[..], "err\n"));
        let stop = Stop::default();
        stop.now();
        let ran = run(&["/bin/sleep".into(), "30".into()], &stop).unwrap();
        assert_eq!((ran.code, ran.stopped), (None, true));
        assert!(matches!(run(&["/nonexistent/pkexec".into()], &Stop::default()), Err(ReadError::NoPrompt(_))));
    }
}
