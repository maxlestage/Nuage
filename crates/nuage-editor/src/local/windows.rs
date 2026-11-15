//! Windows: install.ps1 gives `%ProgramData%\Nuage` to SYSTEM and the
//! Administrators only, so the settings file cannot even be seen without
//! rights. The editor is never elevated itself (an unsigned program would
//! show "Publisher: Unknown" and run with administrator rights): it asks
//! Windows (UAC) to run Windows' own cmd.exe, which runs findstr on the four
//! lines the editor needs and writes them into a pipe the editor created.
//! The UAC window names "Windows Command Processor", by Microsoft, and its
//! details show this exact command. The encryption key never leaves the
//! elevated process.
//!
//! The editor's only module with `unsafe` code: Win32 calls, each with what
//! makes it sound.
#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_CANCELLED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, ReadFile,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    SHGetKnownFolderPath, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows_sys::core::GUID;
use zeroize::Zeroizing;

use super::{Found, LIMIT, Paths, ReadError, Via};

/// How long the elevated command may take once allowed (findstr takes a moment).
const WATCHDOG: Duration = Duration::from_secs(120);

/// The start of the pipes' names; the rest is random.
const PIPE_PREFIX: &str = r"\\.\pipe\nuage-editor-";

/// `text` as a NUL-terminated UTF-16 string.
fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// A kernel handle, closed when dropped.
struct Handle(HANDLE);

// SAFETY: a kernel handle is a process-wide value: any thread may use or close it.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid (made only from a successful call) and closed only here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A folder Windows knows (`%ProgramData%`, `%ProgramFiles%`), asked to
/// Windows rather than read from the environment.
fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `id` points to a GUID for the duration of the call; `path` receives a buffer
    // Windows allocates (null on some failures), released by CoTaskMemFree below.
    let result = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    let mut folder = None;
    if result == 0 && !path.is_null() {
        let mut len = 0;
        // SAFETY: on success `path` is a NUL-terminated UTF-16 string: every unit read up to its
        // NUL is inside it, and the slice is copied before the buffer is released.
        unsafe {
            while *path.add(len) != 0 {
                len += 1;
            }
            folder = Some(PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(path, len))));
        }
    }
    // SAFETY: the buffer came from SHGetKnownFolderPath, which says to release it so, null included.
    unsafe { CoTaskMemFree(path.cast_const().cast()) };
    folder
}

/// `%ProgramData%` and `%ProgramFiles%`.
pub(super) fn folders() -> Option<(PathBuf, PathBuf)> {
    Some((known_folder(&FOLDERID_ProgramData)?, known_folder(&FOLDERID_ProgramFiles)?))
}

/// Windows' own folder of programs (`C:\Windows\System32`), where cmd.exe, findstr and schtasks are.
fn system_folder() -> Option<PathBuf> {
    let mut buffer = vec![0u16; 520];
    let room = u32::try_from(buffer.len()).ok()?;
    // SAFETY: `buffer` has room for `room` UTF-16 units, as the call is told.
    let written = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), room) };
    let written = usize::try_from(written).ok()?;
    // 0: failed; more than the room: the size it would need (a System32 longer than 520 is no Windows).
    if written == 0 || written >= buffer.len() {
        return None;
    }
    buffer.truncate(written);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

/// Windows' paths, as cmd.exe will read them; refused when it would read them differently.
fn for_cmd(path: &Path) -> Result<String, ReadError> {
    match path.to_str() {
        Some(text) if super::safe_for_cmd(text) => Ok(text.to_owned()),
        _ => Err(ReadError::NoPrompt(format!(
            "{} has characters a command line cannot hold as they are",
            path.display()
        ))),
    }
}

/// Whether UAC can be asked: Windows says where its folders are, and their
/// paths can be written in cmd.exe's command line.
pub(super) fn can_ask() -> Result<(), String> {
    let system = system_folder().ok_or_else(|| "Windows did not say where System32 is".to_owned())?;
    let (data, _) = folders().ok_or_else(|| "Windows did not say where ProgramData is".to_owned())?;
    for path in [system, data] {
        for_cmd(&path).map_err(|e| match e {
            ReadError::NoPrompt(why) => why,
            other => format!("{other:?}"),
        })?;
    }
    Ok(())
}

/// A new pipe the editor reads, its name random (`getrandom`).
fn pipe_name() -> Result<String, ReadError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ReadError::Failed(format!("no random name for the pipe: {e}")))?;
    Ok(bytes.iter().fold(PIPE_PREFIX.to_owned(), |mut name, b| {
        name.push_str(&format!("{b:02x}"));
        name
    }))
}

/// Creates the pipe `name`, for reading. One instance, and only if no pipe has
/// that name already (FILE_FLAG_FIRST_PIPE_INSTANCE): nobody can have made it
/// first to listen. No remote clients. The default security descriptor:
/// SYSTEM, the Administrators and the editor's own account may write to it,
/// so an elevated command can, even when another administrator's password
/// allowed it.
fn create_pipe(name: &str) -> std::io::Result<Handle> {
    let wide_name = wide(OsStr::new(name));
    let room = u32::try_from(LIMIT).unwrap_or(u32::MAX);
    // SAFETY: the name is NUL-terminated and lives for the call; no security attributes (the default).
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            room,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Handle(handle))
}

/// Waits for the pipe's writer, and reads all it writes (at most `limit`
/// bytes), until it closes its end.
fn read_pipe(pipe: &Handle, limit: usize) -> std::io::Result<Zeroizing<Vec<u8>>> {
    // SAFETY: the pipe's handle is valid for the call; no OVERLAPPED: it blocks until a writer comes.
    if unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) } == 0 {
        // SAFETY: no other call came in between on this thread.
        let error = unsafe { GetLastError() };
        // Connected before the call, or connected and already gone (its bytes are still there).
        if error != ERROR_PIPE_CONNECTED && error != ERROR_NO_DATA {
            return Err(std::io::Error::from_raw_os_error(error.cast_signed()));
        }
    }
    let mut out = Zeroizing::new(Vec::with_capacity(limit + 64));
    let mut chunk = Zeroizing::new([0u8; 4096]);
    let room = u32::try_from(chunk.len()).unwrap_or(4096);
    while out.len() < limit {
        let mut read = 0u32;
        // SAFETY: `chunk` has room for `room` bytes; `read` receives the count; no OVERLAPPED.
        let ok = unsafe { ReadFile(pipe.0, chunk.as_mut_ptr(), room, &mut read, std::ptr::null_mut()) };
        if ok == 0 {
            // SAFETY: as above.
            let error = unsafe { GetLastError() };
            // The writer closed its end: that was all.
            if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA {
                break;
            }
            return Err(std::io::Error::from_raw_os_error(error.cast_signed()));
        }
        let read = usize::try_from(read).unwrap_or(0).min(limit - out.len());
        out.extend_from_slice(chunk.get(..read).unwrap_or_default());
    }
    Ok(out)
}

/// Opens the pipe `name` as a writer and closes it at once, so a reader still
/// waiting for its writer returns, with nothing. `true` when that was so: the
/// real writer never came.
fn unblock(name: &str) -> bool {
    let wide_name = wide(OsStr::new(name));
    // SAFETY: the name is NUL-terminated and lives for the call; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            wide_name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return false;
    }
    drop(Handle(handle));
    true
}

/// A process the editor waits for.
trait Waited {
    /// Its exit code, once it ended within `timeout` (`None`: it did not).
    fn wait_for(&mut self, timeout: Duration) -> Option<u32>;
}

impl Waited for Handle {
    fn wait_for(&mut self, timeout: Duration) -> Option<u32> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: the process handle is valid for the call.
        if unsafe { WaitForSingleObject(self.0, ms) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: as above; `code` receives the exit code.
        (unsafe { GetExitCodeProcess(self.0, &mut code) } != 0).then_some(code)
    }
}

/// Runs `program` with `parameters` as administrator, after Windows' UAC
/// window, hidden (no console flashes).
fn run_elevated(program: &Path, parameters: &str) -> Result<Handle, ReadError> {
    let verb = wide(OsStr::new("runas"));
    let file = wide(program.as_os_str());
    let parameters = wide(OsStr::new(parameters));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        // The process's handle, to wait for it; the call returns once it started.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    // SAFETY: `info` is initialized and its size given; its strings are NUL-terminated and
    // outlive the call. It blocks while the UAC window waits for the person.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: no other call came in between on this thread.
        let error = unsafe { GetLastError() };
        return Err(if error == ERROR_CANCELLED {
            ReadError::Dismissed
        } else {
            ReadError::NotAccepted(std::io::Error::from_raw_os_error(error.cast_signed()).to_string())
        });
    }
    if info.hProcess.is_null() {
        return Err(ReadError::Failed("Windows started the command without a handle on it".to_owned()));
    }
    Ok(Handle(info.hProcess))
}

/// What came through a pipe: the writer's exit code, whether the writer itself
/// opened the pipe, and its bytes.
struct Captured {
    code: Option<u32>,
    came: bool,
    bytes: Zeroizing<Vec<u8>>,
}

/// Creates the pipe `name`, starts its writer with `start`, and takes what it
/// writes. A watchdog waits for the writer at most [`WATCHDOG`]; a writer
/// that never opened the pipe leaves the reader unblocked, empty.
fn capture<P: Waited>(name: &str, start: impl FnOnce() -> Result<P, ReadError>) -> Result<Captured, ReadError> {
    let pipe = create_pipe(name).map_err(|e| ReadError::Failed(format!("{name}: {e}")))?;
    let (send, got) = mpsc::channel();
    std::thread::Builder::new()
        .name("nuage-editor-pipe".into())
        .spawn(move || {
            let _ = send.send(read_pipe(&pipe, LIMIT));
        })
        .map_err(|e| ReadError::Failed(e.to_string()))?;
    let mut process = match start() {
        Ok(process) => process,
        Err(e) => {
            unblock(name);
            return Err(e);
        }
    };
    let code = process.wait_for(WATCHDOG);
    // The writer ended (or is left behind): a reader still waiting gets an empty writer.
    let came = !unblock(name);
    let bytes = got
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| ReadError::Failed("the pipe was not read".to_owned()))?
        .map_err(|e| ReadError::Failed(e.to_string()))?;
    Ok(Captured { code, came, bytes })
}

/// What findstr's exit means: 0 lines, 1 none; anything else, or nothing
/// through the pipe, is Windows not handing the file over.
fn handed_over(paths: &Paths, captured: &Captured) -> Result<Found, ReadError> {
    match (captured.code, captured.came) {
        (Some(0), true) => {
            let text = std::str::from_utf8(&captured.bytes).map_err(|e| ReadError::Unreadable(e.to_string()))?;
            Ok(super::found_in(&paths.settings, text, Via::Administrator))
        }
        (Some(1), true) => Ok(super::found_in(&paths.settings, "", Via::Administrator)),
        (code, _) => Err(ReadError::NotHandedOver(code)),
    }
}

/// The settings file's four lines, with an administrator's consent (UAC).
pub(super) fn read_elevated(paths: &Paths) -> Result<Found, ReadError> {
    let system =
        system_folder().ok_or_else(|| ReadError::NoPrompt("Windows did not say where System32 is".to_owned()))?;
    let cmd = system.join("cmd.exe");
    let findstr = for_cmd(&system.join("findstr.exe"))?;
    let settings = for_cmd(&paths.settings)?;
    let name = pipe_name()?;
    let parameters = super::findstr_parameters(&findstr, &settings, &name);
    let captured = capture(&name, || run_elevated(&cmd, &parameters))?;
    handed_over(paths, &captured)
}

/// Starts the installer's scheduled task, with an administrator's consent.
pub(super) fn start_task() -> Result<(), ReadError> {
    let system =
        system_folder().ok_or_else(|| ReadError::NoPrompt("Windows did not say where System32 is".to_owned()))?;
    let mut process = run_elevated(&system.join("schtasks.exe"), &super::schtasks_parameters())?;
    match process.wait_for(Duration::from_secs(60)) {
        Some(0) => Ok(()),
        code => Err(ReadError::NotHandedOver(code)),
    }
}

/// A child of the tests: waited for as the elevated one is.
#[cfg(test)]
impl Waited for std::process::Child {
    fn wait_for(&mut self, timeout: Duration) -> Option<u32> {
        let until = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < until {
            if let Ok(Some(status)) = self.try_wait() {
                return status.code().and_then(|c| u32::try_from(c).ok());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::CommandExt as _;

    use super::*;

    /// cmd.exe running `parameters` as the elevated one would, without UAC:
    /// the editor's own account may write to its pipe.
    fn cmd(parameters: &str) -> Result<std::process::Child, ReadError> {
        let system = system_folder().unwrap();
        std::process::Command::new(system.join("cmd.exe"))
            .raw_arg(parameters)
            .spawn()
            .map_err(|e| ReadError::Failed(e.to_string()))
    }

    fn fixture(text: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("nuage-editor-pipe-{}-{}", std::process::id(), text.len()));
        let paths = Paths::under(&root, super::super::Kind::Windows);
        std::fs::create_dir_all(paths.settings.parent().unwrap()).unwrap();
        std::fs::write(&paths.settings, text).unwrap();
        paths
    }

    #[test]
    fn windows_says_where_its_folders_are() {
        let system = system_folder().unwrap();
        assert!(system.join("cmd.exe").is_file() && system.join("findstr.exe").is_file(), "{system:?}");
        let (data, programs) = folders().unwrap();
        assert!(data.is_dir() && programs.is_dir(), "{data:?} {programs:?}");
        assert_eq!(can_ask(), Ok(()));
        let name = pipe_name().unwrap();
        assert!(name.starts_with(PIPE_PREFIX) && name.len() == PIPE_PREFIX.len() + 32, "{name}");
    }

    #[test]
    fn only_the_four_lines_come_through_the_pipe() {
        let paths = fixture(
            "PORT=8091\r\nNUAGE_BIND=0.0.0.0\r\nNUAGE_ENCRYPTION_KEY=never-this\r\nNUAGE_ADMIN_PASSWORD=nor-this\r\nNUAGE_ADMIN_TOKEN=0123456789abcdef0123456789abcdef-test\r\nNUAGE_PG_PORT=5432\r\nSITE_URL=https://x.example\r\n",
        );
        let system = system_folder().unwrap();
        let findstr = for_cmd(&system.join("findstr.exe")).unwrap();
        let settings = for_cmd(&paths.settings).unwrap();
        let name = pipe_name().unwrap();
        let captured = capture(&name, || cmd(&super::super::findstr_parameters(&findstr, &settings, &name))).unwrap();
        let text = String::from_utf8(captured.bytes.to_vec()).unwrap();
        assert!(!text.contains("never-this") && !text.contains("nor-this") && !text.contains("5432"), "{text}");
        assert_eq!((captured.code, captured.came), (Some(0), true));
        let found = handed_over(&paths, &captured).unwrap();
        assert_eq!((found.port, found.site_url.as_deref()), (8091, Some("https://x.example")));
        assert_eq!(found.token.as_ref().map(|t| t.expose()), Some("0123456789abcdef0123456789abcdef-test"));
        // A file without a token: findstr finds no line (1), and the editor says there is no token.
        let paths = fixture("# nothing\r\nNUAGE_NAME=nuage\r\n");
        let settings = for_cmd(&paths.settings).unwrap();
        let name = pipe_name().unwrap();
        let captured = capture(&name, || cmd(&super::super::findstr_parameters(&findstr, &settings, &name))).unwrap();
        assert_eq!((captured.code, captured.came), (Some(1), true));
        assert_eq!(handed_over(&paths, &captured).unwrap().token, None);
    }

    #[test]
    fn a_writer_that_never_comes_does_not_block() {
        let name = pipe_name().unwrap();
        let started = std::time::Instant::now();
        let captured = capture(&name, || cmd("/d /c exit 3")).unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!((captured.code, captured.came, captured.bytes.len()), (Some(3), false, 0));
        let paths = Paths::under(Path::new(r"C:\x"), super::super::Kind::Windows);
        assert_eq!(handed_over(&paths, &captured), Err(ReadError::NotHandedOver(Some(3))));
    }

    /// The real thing, UAC included: GitHub's Windows runners elevate without
    /// asking (`cargo test -- --ignored elevated`); elsewhere, by hand.
    #[test]
    #[ignore = "asks Windows for an administrator's consent"]
    fn elevated_read_of_a_settings_file() {
        let paths = fixture(
            "PORT=8093\r\nNUAGE_ENCRYPTION_KEY=never-this\r\nNUAGE_ADMIN_TOKEN=0123456789abcdef0123456789abcdef-uac\r\n",
        );
        let found = read_elevated(&paths).unwrap();
        assert_eq!((found.port, found.via), (8093, Via::Administrator));
        assert_eq!(found.token.as_ref().map(|t| t.expose()), Some("0123456789abcdef0123456789abcdef-uac"));
    }

    #[test]
    fn a_pipe_name_cannot_be_taken_first() {
        let name = pipe_name().unwrap();
        let first = create_pipe(&name).unwrap();
        assert!(create_pipe(&name).is_err(), "a second pipe with the same name");
        drop(first);
    }
}
