use std::io;
use std::path::Path;
use std::process::Command;

/// Result of running the exporter.
pub struct ExportResult {
    /// exit code (or -1 if unknown)
    pub status: i32,
    /// stdout collected from the exporter
    pub stdout: String,
    /// stderr collected from the exporter
    pub stderr: String,
}

impl ExportResult {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Run the metadata exporter:
/// `cargo run --bin export_schema --features bandana_export`
///
/// - `root`: your game workspace root (where Cargo.toml for the game lives)
/// - `extra_env`: optional `(KEY, VALUE)` environment pairs to inject
///
/// This call is synchronous: it blocks until the export finishes and returns
/// the collected stdout/stderr so you can display them in your UI console.
pub fn export_schema(root: &Path, extra_env: &[(&str, &str)]) -> io::Result<ExportResult> {
    let mut cmd = Command::new("cargo");
    cmd.arg("run")
        .arg("--bin")
        .arg("export_schema")
        .arg("--features")
        .arg("bandana_export")
        .current_dir(root);

    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let out = cmd.output()?;

    Ok(ExportResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}
