use crate::project::Diagnostic;
use crossbeam::channel::{Receiver, Sender, unbounded};
use serde::Deserialize;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

pub enum BuildJob {
    Check { root: PathBuf },
}

pub enum BuildResult {
    Ok {
        duration_ms: u128,
    },
    Err {
        duration_ms: u128,
        diagnostics: Vec<Diagnostic>,
    },
}

pub struct BuildWorker;

impl BuildWorker {
    #[allow(dead_code)]
    pub fn start() -> (Sender<BuildJob>, Receiver<BuildResult>) {
        let (tx, rx) = unbounded::<BuildJob>();
        let (otx, orx) = unbounded::<BuildResult>();

        thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                match job {
                    BuildJob::Check { root } => {
                        let t0 = Instant::now();
                        let mut cmd = Command::new("cargo");
                        cmd.arg("check")
                            .arg("--message-format=json")
                            .current_dir(&root)
                            .stdout(Stdio::piped())
                            .stderr(Stdio::null());

                        let mut child = match cmd.spawn() {
                            Ok(c) => c,
                            Err(e) => {
                                let _ = otx.send(BuildResult::Err {
                                    duration_ms: 0,
                                    diagnostics: vec![Diagnostic {
                                        file: root.clone(),
                                        line: 0,
                                        col: 0,
                                        msg: format!("failed to spawn cargo: {e}"),
                                    }],
                                });
                                continue;
                            }
                        };

                        let stdout = child.stdout.take().expect("stdout");
                        let reader = std::io::BufReader::new(stdout);
                        let mut diags = Vec::<Diagnostic>::new();

                        for line in reader.lines().flatten() {
                            if let Ok(msg) = serde_json::from_str::<CargoMessage>(&line) {
                                if let Some(diag) = msg.to_diag() {
                                    diags.push(diag);
                                }
                            }
                        }
                        let _ = child.wait();

                        let dt = t0.elapsed().as_millis();
                        if diags.is_empty() {
                            let _ = otx.send(BuildResult::Ok { duration_ms: dt });
                        } else {
                            let _ = otx.send(BuildResult::Err {
                                duration_ms: dt,
                                diagnostics: diags,
                            });
                        }
                    }
                }
            }
        });

        (tx, orx)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum CargoMessage {
    #[serde(rename_all = "camelCase")]
    CompilerMessage { message: RustcMessage },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct RustcMessage {
    message: MessageDetail,
}

#[derive(Debug, Deserialize)]
struct MessageDetail {
    code: Option<Code>,
    message: String,  // the human-readable text
    level: String,    // "error", "warning", etc.
    spans: Vec<Span>, // spans live here
}

#[derive(Debug, Deserialize)]
struct Code {
    code: String,
}

#[derive(Debug, Deserialize)]
struct Span {
    file_name: String,
    line_start: u32,
    column_start: u32,
}

impl CargoMessage {
    fn to_diag(self) -> Option<Diagnostic> {
        match self {
            CargoMessage::CompilerMessage { message } => {
                // spans are under message.message.spans
                let span = message.message.spans.get(0)?;
                Some(Diagnostic {
                    file: PathBuf::from(&span.file_name),
                    line: span.line_start,
                    col: span.column_start,
                    // level + human message path
                    msg: format!(
                        "[{}] {}",
                        message.message.level,
                        message.message.message.trim()
                    ),
                })
            }
            CargoMessage::Other => None,
        }
    }
}
