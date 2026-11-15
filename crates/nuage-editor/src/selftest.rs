//! `nuage-editor --self-test <file>`: the real window opens, draws a few
//! frames, writes what it used to `<file>` and closes. The release and CI
//! jobs run it on each system, to know the window opens there (and with
//! which renderer) before anyone downloads it.

use std::path::Path;

/// The frames drawn before the window closes.
pub const FRAMES: u64 = 5;

/// What a self-test writes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    /// The renderer that drew the window (`glow` or `wgpu`).
    pub renderer: String,
    /// The frames drawn.
    pub frames: u64,
    /// The editor's version.
    pub version: String,
    /// The renderers that failed to start before it, and why.
    pub failed: Vec<(String, String)>,
}

impl Report {
    pub fn new(renderer: &str, frames: u64, failed: &[(String, String)]) -> Report {
        Report {
            renderer: renderer.to_owned(),
            frames,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            failed: failed.to_vec(),
        }
    }

    /// Writes it to `file`, as JSON.
    pub fn write(&self, file: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(file, json + "\n")
    }

    /// The report in `file`.
    pub fn read(file: &Path) -> std::io::Result<Report> {
        serde_json::from_slice(&std::fs::read(file)?).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_and_read_back() {
        let file = std::env::temp_dir().join(format!("nuage-editor-self-test-{}.json", std::process::id()));
        let report = Report::new("glow", FRAMES, &[("wgpu".into(), "no adapter".into())]);
        report.write(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"renderer\": \"glow\"") && text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert_eq!(Report::read(&file).unwrap(), report);
        let _ = std::fs::remove_file(&file);
    }
}
