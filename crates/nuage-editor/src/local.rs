//! A Nuage DB server installed on this computer by its installer: where its
//! settings file is, and what the editor needs from it (its port and its
//! management token). The installers protect that file (it holds the
//! encryption key): reading it takes the administrator's password, which
//! the system asks for in a window of its own.

use std::path::PathBuf;

use crate::secret::Secret;

/// What the editor takes from the settings file of a server of this computer.
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    /// The settings file.
    pub file: PathBuf,
    /// Where the server answers on this computer (`http://127.0.0.1:<PORT>`).
    pub address: String,
    /// Its public address (`SITE_URL`, written by the installer given a
    /// domain): the one its applications are given.
    pub site_url: Option<String>,
    /// Its management token, when the file has one.
    pub token: Option<Secret>,
}

impl Found {
    /// Never the token.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.address, self.file.display())
    }
}

/// Where the installers put the settings file, on this system.
pub fn settings_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if cfg!(target_os = "macos") {
        files.push(PathBuf::from("/usr/local/etc/nuage/nuage.env"));
    } else if cfg!(windows) {
        if let Some(data) = std::env::var_os("ProgramData") {
            files.push(PathBuf::from(data).join("Nuage").join("nuage.env"));
        }
    } else {
        files.push(PathBuf::from("/etc/nuage/nuage.env"));
    }
    files
}

/// The settings file of a server of this computer, if one is installed.
pub fn installed() -> Option<PathBuf> {
    settings_files().into_iter().find(|f| f.exists())
}

/// The variables of a settings file: `KEY=value` lines, `#` comments, an
/// optional `export `, values as they are or between quotes (what the
/// installers write, and what the server reads). Later lines win.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut vars = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let value = [('"', '"'), ('\'', '\'')]
            .iter()
            .find_map(|&(a, b)| value.strip_prefix(a)?.strip_suffix(b))
            .filter(|_| value.len() >= 2)
            .unwrap_or(value);
        vars.push((key.trim().to_owned(), value.to_owned()));
    }
    vars
}

/// What the editor needs, from the text of the settings file `file`.
pub fn found_in(file: PathBuf, text: &str) -> Found {
    let vars = parse(text);
    let get = |k: &str| vars.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let port = get("PORT").unwrap_or_else(|| "8080".into());
    Found {
        file,
        address: format!("http://127.0.0.1:{port}"),
        site_url: get("SITE_URL"),
        token: get("NUAGE_ADMIN_TOKEN").map(Secret::new),
    }
}

/// Why the settings file could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// No server is installed here by the installers.
    NotInstalled,
    /// The file is for administrators only: [`read_as_administrator`] asks for the password.
    NeedsAdministrator(PathBuf),
    /// The person closed the password window, or the system refused.
    Cancelled,
    /// Something else: the system's message.
    Failed(String),
}

/// Reads the settings file of a server of this computer, as the person
/// running the editor.
pub fn read() -> Result<Found, ReadError> {
    let file = installed().ok_or(ReadError::NotInstalled)?;
    match std::fs::read_to_string(&file) {
        Ok(text) => Ok(found_in(file, &text)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(ReadError::NeedsAdministrator(file)),
        Err(e) => Err(ReadError::Failed(e.to_string())),
    }
}

/// Whether [`read_as_administrator`] can ask for the password here.
pub fn can_ask_administrator() -> bool {
    if cfg!(target_os = "macos") {
        true
    } else if cfg!(windows) {
        false
    } else {
        // polkit's window, on a desktop.
        which("pkexec")
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Reads the settings file with the administrator's rights: the system asks
/// for the password in a window of its own (macOS, and Linux desktops with
/// polkit). Only the lines the editor needs are kept from what is read;
/// the encryption key never leaves the file.
pub fn read_as_administrator(file: &std::path::Path) -> Result<Found, ReadError> {
    // The path is one of `settings_files()`: fixed by the installers, no user input.
    let path = file.to_string_lossy().into_owned();
    if !settings_files().iter().any(|f| f.as_path() == file) {
        return Err(ReadError::Failed(format!("{path} is not a settings file of the installers")));
    }
    let output = if cfg!(target_os = "macos") {
        let script = format!(
            "do shell script \"/usr/bin/grep -E '^(PORT|NUAGE_ADMIN_TOKEN|SITE_URL)=' '{path}'\" with prompt \"Nuage DB Editor reads the port and the management token of the Nuage DB server of this computer.\" with administrator privileges"
        );
        std::process::Command::new("/usr/bin/osascript").arg("-e").arg(script).output()
    } else {
        std::process::Command::new("pkexec")
            .args(["/usr/bin/grep", "-E", "^(PORT|NUAGE_ADMIN_TOKEN|SITE_URL)=", &path])
            .output()
    };
    let output = output.map_err(|e| ReadError::Failed(e.to_string()))?;
    if output.status.success() {
        return Ok(found_in(file.to_path_buf(), &String::from_utf8_lossy(&output.stdout)));
    }
    let said = String::from_utf8_lossy(&output.stderr).to_lowercase();
    // macOS: "User canceled. (-128)"; pkexec: exit code 126 when dismissed, 127 when refused.
    if said.contains("cancel") || matches!(output.status.code(), Some(126 | 127)) {
        Err(ReadError::Cancelled)
    } else {
        Err(ReadError::Failed(String::from_utf8_lossy(&output.stderr).trim().to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installers_settings_file() {
        let text = "\u{feff}# Nuage DB settings\r\nPORT=8091\r\nNUAGE_ENCRYPTION_KEY=abc\r\nexport NUAGE_ADMIN_TOKEN=\"0123\"\r\nSITE_URL=https://nuage.example.com\r\n";
        let found = found_in(PathBuf::from("/etc/nuage/nuage.env"), text);
        assert_eq!(found.address, "http://127.0.0.1:8091");
        assert_eq!(found.site_url.as_deref(), Some("https://nuage.example.com"));
        assert_eq!(found.token.as_ref().map(Secret::expose), Some("0123"));
        assert!(!found.describe().contains("0123"), "never the token");
        assert!(!format!("{found:?}").contains("0123"), "never the token");
        // No port: the server's own default.
        let found = found_in(PathBuf::from("x"), "NUAGE_ADMIN_TOKEN=\n");
        assert_eq!((found.address.as_str(), found.token, found.site_url), ("http://127.0.0.1:8080", None, None));
        assert!(!settings_files().is_empty());
        // Only the installers' files are read with the administrator's rights.
        assert!(matches!(read_as_administrator(std::path::Path::new("/etc/passwd")), Err(ReadError::Failed(_))));
    }
}
