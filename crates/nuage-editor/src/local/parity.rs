//! The server of this computer as the installers leave it: the paths the
//! editor looks at are theirs, the settings file is read as the server reads
//! it, and only four lines ever come out of it.

use std::path::{Path, PathBuf};

use super::*;

// With the line endings of a checkout on Windows (core.autocrlf) taken off.
fn install_sh() -> String {
    include_str!("../../../../install.sh").replace("\r\n", "\n")
}

fn install_ps1() -> String {
    include_str!("../../../../install.ps1").replace("\r\n", "\n")
}

/// A fresh folder of the tests.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-editor-local-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The text of `script` from `from` to the first `to` after it.
fn between<'a>(script: &'a str, from: &str, to: &str) -> &'a str {
    let (_, rest) = script.split_once(from).unwrap();
    rest.split_once(to).unwrap().0
}

#[test]
fn the_paths_and_names_are_the_installers() {
    let (install_sh, install_ps1) = (install_sh(), install_ps1());
    for path in [LINUX_SETTINGS, LINUX_UNIT, MACOS_SETTINGS, MACOS_PLIST, MACOS_LOG] {
        assert!(install_sh.contains(path), "{path} is not in install.sh");
    }
    assert!(install_sh.contains(&format!("LABEL={MACOS_LABEL}")));
    for line in [
        format!("$BinDir = Join-Path $env:ProgramFiles '{WINDOWS_FOLDER}'"),
        format!("$Bin = Join-Path $BinDir '{WINDOWS_PROGRAM}'"),
        format!("$ConfDir = Join-Path $env:ProgramData '{WINDOWS_FOLDER}'"),
        format!("$EnvFile = Join-Path $ConfDir '{WINDOWS_SETTINGS}'"),
        format!("$Log = Join-Path $ConfDir '{WINDOWS_LOG}'"),
        format!("$Task = '{WINDOWS_TASK}'"),
    ] {
        assert!(install_ps1.contains(&line), "{line} is not in install.ps1");
    }
    // The installers write the keys the editor reads as KEY=value lines.
    for key in KEYS {
        assert!(install_sh.contains(&format!("{key}=")), "{key}");
    }
    for key in ["PORT", "NUAGE_BIND", "NUAGE_ADMIN_TOKEN"] {
        assert!(install_ps1.contains(&format!("{key}=")), "{key}");
    }
    let windows = Paths::under(Path::new("C:\\"), Kind::Windows);
    assert!(windows.settings.ends_with(Path::new(WINDOWS_FOLDER).join(WINDOWS_SETTINGS)));
    assert!(windows.service.ends_with(Path::new(WINDOWS_FOLDER).join(WINDOWS_PROGRAM)));
    assert_eq!(Paths::under(Path::new("/"), Kind::Linux).settings, Path::new(LINUX_SETTINGS));
    assert_eq!(Paths::under(Path::new("/"), Kind::MacOs).service, Path::new(MACOS_PLIST));
}

/// What the editor says of a file without a management token is true: an
/// update keeps the file as it is, and never adds one.
#[test]
fn an_update_adds_no_management_token() {
    let (install_sh, install_ps1) = (install_sh(), install_ps1());
    let upgrade = between(&install_sh, "say \"Keeping the settings and secrets of $ENV_FILE\"", "\nfi\n");
    assert!(upgrade.contains("NUAGE_PG_PORT") && !upgrade.contains("NUAGE_ADMIN_TOKEN"), "{upgrade}");
    let first = between(&install_sh, "if [ ! -f \"$ENV_FILE\" ]; then", "say \"Keeping the settings");
    assert!(first.contains("NUAGE_ADMIN_TOKEN=$admin_token"));
    let upgrade = between(&install_ps1, "Say \"Keeping the settings and secrets of $EnvFile\"", "\n}\n");
    assert!(upgrade.contains("PORT") && !upgrade.contains("NUAGE_ADMIN_TOKEN"), "{upgrade}");
}

/// The editor reads a settings file as the server does.
#[test]
fn read_as_the_server_reads_it() {
    for text in [
        "\u{feff}# Nuage DB settings\r\nPORT=8091\r\nNUAGE_NAME=nuage\r\n",
        "export PORT=8091\n  SITE_URL = https://x.example\n",
        "A=\"quoted value\"\nB='single'\nC=\nD=a=b\nA=later\nE=\"\nF=' spaced '\n",
        "# only a comment\n\n   \nNUAGE_ADMIN_TOKEN=0123456789abcdef0123456789abcdef\n",
    ] {
        assert_eq!(parse(text), nuage::config::parse_env_file(text).unwrap(), "{text:?}");
    }
}

#[test]
fn only_the_four_keys_are_kept() {
    let text = "NUAGE_ENCRYPTION_KEY=never-this\nNUAGE_ADMIN_PASSWORD=nor-this\nNUAGE_TOKEN=nuage_nor-this\nPORT=8091\nNUAGE_ADMIN_TOKEN=0123456789abcdef0123456789abcdef\n";
    let found = found_in(Path::new("x"), text, Via::File);
    let shown = format!("{found:?}");
    assert!(!shown.contains("never-this") && !shown.contains("nor-this"), "{shown}");
    assert_eq!(found.token.as_ref().map(Secret::expose), Some("0123456789abcdef0123456789abcdef"));
}

/// The exact grep the elevated read runs, on a file of every kind of line:
/// only the four keys come out (Linux and macOS have it; no rights needed here).
#[cfg(unix)]
#[test]
fn grep_keeps_only_the_four_lines() {
    let dir = folder("grep");
    let file = dir.join("nuage.env");
    write(
        &file,
        "# Nuage DB settings\nNUAGE_ENCRYPTION_KEY=never-this\nNUAGE_ADMIN_PASSWORD=nor-this\nexport PORT=8091\n  SITE_URL = https://x.example\nNUAGE_BIND=127.0.0.1\nNUAGE_PG_PORT=5432\nNUAGE_TOKEN=nuage_nor-this\nNUAGE_ADMIN_TOKEN=0123456789abcdef0123456789abcdef\n",
    );
    let grep = GREPS.iter().find(|g| Path::new(g).is_file()).unwrap();
    let argv = grep_argv(grep, &file.to_string_lossy());
    let ran = unix::run(&argv, &Stop::default()).unwrap();
    assert_eq!(ran.code, Some(0));
    let text = std::str::from_utf8(&ran.stdout).unwrap();
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(!text.contains("never-this") && !text.contains("nor-this") && !text.contains("5432"), "{text}");
    let found = found_in(&file, text, Via::Administrator);
    assert_eq!((found.port, found.site_url.as_deref()), (8091, Some("https://x.example")));
    assert!(found.this_computer_only() && found.token.is_some());
    // No line at all: grep says 1, and the editor says there is no token.
    write(&file, "NUAGE_NAME=nuage\n");
    let ran = unix::run(&argv, &Stop::default()).unwrap();
    assert_eq!(after_pkexec(ran.code, &ran.stderr), Ok(false));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_installation_is_found_by_its_files() {
    let root = folder("linux");
    let paths = Paths::under(&root, Kind::Linux);
    assert_eq!(detect(paths.clone(), |_| true), None, "nothing installed");
    write(&paths.settings, "PORT=8091\nNUAGE_ADMIN_TOKEN=0123456789abcdef0123456789abcdef\n");
    let mut asked = None;
    let found = detect(paths.clone(), |port| {
        asked = Some(port);
        false
    })
    .unwrap();
    assert_eq!(asked, Some(8091), "the file's port is asked");
    assert!(found.readable && !found.service && !found.answers && found.port == 8091, "{found:?}");
    // Without its service (Linux without systemd): still found, started by hand.
    write(&paths.service, "[Unit]\n");
    assert!(detect(paths.clone(), |_| true).unwrap().service);
    let read = read_file(&paths.settings).unwrap();
    assert_eq!((read.via, read.port), (Via::File, 8091));
    // For administrators only (root reads everything: then there is nothing to see here).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&paths.settings, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&paths.settings).is_err() {
            let found = detect(paths.clone(), |_| true).unwrap();
            assert!(!found.readable && found.port == DEFAULT_PORT, "{found:?}");
            assert_eq!(read_file(&paths.settings).unwrap_err(), ReadError::NeedsAdministrator(paths.settings.clone()));
        }
        std::fs::set_permissions(&paths.settings, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let _ = std::fs::remove_dir_all(root);

    // macOS: the settings file and the launchd daemon.
    let root = folder("macos");
    let paths = Paths::under(&root, Kind::MacOs);
    write(&paths.settings, "NUAGE_BIND=0.0.0.0\n");
    write(&paths.service, "<plist/>\n");
    let found = detect(paths.clone(), |port| port == DEFAULT_PORT).unwrap();
    assert!(found.readable && found.service && found.answers);
    assert_eq!(read_file(&paths.settings).unwrap().token, None);
    let _ = std::fs::remove_dir_all(root);

    // Windows: a folder for administrators only may hide even the file's name; the program shows it is there.
    let root = folder("windows");
    let paths = Paths::under(&root, Kind::Windows);
    assert_eq!(detect(paths.clone(), |_| true), None);
    write(&paths.service, "MZ");
    let found = detect(paths.clone(), |_| false).unwrap();
    assert!(!found.readable && found.service && found.port == DEFAULT_PORT, "{found:?}");
    let _ = std::fs::remove_dir_all(root);
}
