//! Install engine: a plan of steps (commands + files) and a runner that
//! executes it, streaming progress events to the UI. GTK-free by design so
//! the whole module unit-tests and dry-runs headless.

pub mod plan;

use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};

pub const LOG_PATH: &str = "/tmp/lumen-install.log";

#[derive(Debug, Clone)]
pub enum Action {
    /// argv exec, no shell interpretation.
    Cmd(Vec<String>),
    /// `sh -c` script for pipelines/substitutions.
    Shell(String),
    WriteFile {
        path: String,
        contents: String,
        mode: u32,
    },
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Cmd(argv) => write!(f, "$ {}", argv.join(" ")),
            Action::Shell(script) => write!(f, "$ sh -c {script:?}"),
            Action::WriteFile { path, mode, .. } => write!(f, "write {path} (mode {mode:o})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Step {
    pub title: String,
    pub actions: Vec<Action>,
}

/// Progress events streamed from the runner thread to the UI.
#[derive(Debug, Clone)]
pub enum Event {
    StepStarted {
        index: usize,
        total: usize,
        title: String,
    },
    Line(String),
    Finished(Result<(), String>),
}

pub fn print_plan(steps: &[Step]) {
    for (i, step) in steps.iter().enumerate() {
        println!("== step {}/{}: {}", i + 1, steps.len(), step.title);
        for action in &step.actions {
            println!("   {action}");
        }
    }
}

/// Execute the plan, sending Events on `tx`. Blocking: call from a worker
/// thread. Every line of output also lands in LOG_PATH.
pub fn run(steps: &[Step], tx: &async_channel::Sender<Event>) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .ok();
    let mut emit = |line: String| {
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{line}");
        }
        let _ = tx.send_blocking(Event::Line(line));
    };

    let total = steps.len();
    for (index, step) in steps.iter().enumerate() {
        let _ = tx.send_blocking(Event::StepStarted {
            index,
            total,
            title: step.title.clone(),
        });
        emit(format!("==> {}", step.title));
        for action in &step.actions {
            emit(action.to_string());
            if let Err(err) = run_action(action, &mut emit) {
                emit(format!("FAILED: {err}"));
                let _ = tx.send_blocking(Event::Finished(Err(format!(
                    "step \"{}\" failed: {err}",
                    step.title
                ))));
                return;
            }
        }
    }
    let _ = tx.send_blocking(Event::Finished(Ok(())));
}

fn run_action(action: &Action, emit: &mut dyn FnMut(String)) -> Result<(), String> {
    match action {
        Action::WriteFile {
            path,
            contents,
            mode,
        } => {
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
            }
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(*mode)
                .open(path)
                .map_err(|e| format!("open {path}: {e}"))?;
            f.write_all(contents.as_bytes())
                .map_err(|e| format!("write {path}: {e}"))?;
            Ok(())
        }
        Action::Cmd(argv) => {
            let (prog, args) = argv.split_first().ok_or("empty command")?;
            run_child(Command::new(prog).args(args), emit)
        }
        Action::Shell(script) => run_child(Command::new("sh").arg("-c").arg(script), emit),
    }
}

fn run_child(cmd: &mut Command, emit: &mut dyn FnMut(String)) -> Result<(), String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    // Relay stderr from a helper thread; stdout on this one.
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            BufReader::new(pipe)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
        })
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            emit(line);
        }
    }
    let stderr_lines = stderr
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    for line in &stderr_lines {
        emit(line.clone());
    }

    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        let tail = stderr_lines
            .iter()
            .rev()
            .take(3)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ");
        Err(format!("exit {status}: {tail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_display_is_readable() {
        let a = Action::Cmd(vec!["zpool".into(), "export".into(), "boot".into()]);
        assert_eq!(a.to_string(), "$ zpool export boot");
        let w = Action::WriteFile {
            path: "/etc/hostname".into(),
            contents: "lumen\n".into(),
            mode: 0o644,
        };
        assert!(w.to_string().contains("mode 644"));
    }
}
