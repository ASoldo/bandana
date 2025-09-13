use crossbeam::channel::{Receiver, unbounded};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

pub enum RunnerMsg {
    Line(String),
    Exited(i32),
}

pub fn start(root: PathBuf, env_overrides: &[(&str, &str)]) -> Receiver<RunnerMsg> {
    let (tx, rx) = unbounded();

    let mut cmd = Command::new("cargo");
    cmd.arg("run")
        .arg("--bin")
        .arg("sample_game")
        .arg("--features")
        .arg("editor-bridge")
        .current_dir(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (k, v) in env_overrides {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().expect("spawn cargo run");
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();

    let tx2 = tx.clone();
    thread::spawn(move || {
        for l in BufReader::new(out).lines().flatten() {
            let _ = tx2.send(RunnerMsg::Line(l));
        }
    });
    let tx3 = tx.clone();
    thread::spawn(move || {
        for l in BufReader::new(err).lines().flatten() {
            let _ = tx3.send(RunnerMsg::Line(l));
        }
    });
    thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = tx.send(RunnerMsg::Exited(code));
    });

    rx
}
