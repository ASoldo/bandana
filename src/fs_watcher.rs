use crossbeam::channel::{Receiver, Sender, unbounded};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

pub struct WatchWorker {
    _thread: thread::JoinHandle<()>,
}

impl WatchWorker {
    pub fn start(root: PathBuf, tx: Sender<Event>) -> Self {
        let handle = thread::spawn(move || {
            let (inner_tx, inner_rx) = unbounded::<notify::Result<Event>>();

            let mut watcher = RecommendedWatcher::new(
                move |res| {
                    let _ = inner_tx.send(res);
                },
                Config::default(),
            )
            .expect("watcher");

            // Watch only the interesting inputs (avoid target/ & .git/ loops)
            let _ = watcher.watch(&root.join("src"), RecursiveMode::Recursive);
            let _ = watcher.watch(&root.join("design"), RecursiveMode::Recursive);
            let _ = watcher.watch(&root.join("Cargo.toml"), RecursiveMode::NonRecursive);
            let _ = watcher.watch(&root.join("project.ron"), RecursiveMode::NonRecursive);

            // Simple debounce window
            let mut last_fire = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);

            while let Ok(res) = inner_rx.recv() {
                let Ok(event) = res else { continue };

                // Skip noisy event kinds quickly
                if matches!(event.kind, EventKind::Access(_) | EventKind::Other) {
                    continue;
                }

                // Ignore anything under target/ or .git/
                let interesting = event.paths.iter().any(|p| {
                    let rel = p.strip_prefix(&root).unwrap_or(p);
                    let s = rel.to_string_lossy();
                    !(s.starts_with("target/")
                        || s == "target"
                        || s.contains("/target/")
                        || s.starts_with(".git/")
                        || s == ".git"
                        || s.contains("/.git/"))
                });
                if !interesting {
                    continue;
                }

                // Debounce bursts to a single notification
                if last_fire.elapsed() < Duration::from_millis(250) {
                    continue;
                }
                last_fire = Instant::now();

                let _ = tx.send(event);
            }
        });

        Self { _thread: handle }
    }
}
