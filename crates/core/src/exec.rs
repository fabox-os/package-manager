//! Process execution behind a trait, so sources can be tested with fixtures
//! and without `apt` / `flatpak` installed. Always explicit args, never a shell.

use async_trait::async_trait;

use tokio::io::AsyncBufReadExt;

use crate::log::{LogBroadcaster, LogEvent};
use crate::source::SourceError;

/// `pkexec` by absolute path: a `pkexec` earlier in the user's `PATH` must never be used to
/// start the privileged helper.
pub const PKEXEC: &str = "/usr/bin/pkexec";

#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    /// Exit code, `None` if the process was killed by a signal.
    pub code: Option<i32>,
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Runs a command. Stdout is collected silently (scans produce thousands of lines).
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, SourceError>;

    /// Like [`run`](Self::run) but streams every output line to the log as it arrives.
    /// Used for commands that change the system.
    async fn run_logged(&self, program: &str, args: &[&str]) -> Result<CommandOutput, SourceError> {
        self.run(program, args).await
    }
}

/// Runs real processes on a background tokio task and reports them to the log.
pub struct SystemRunner {
    logs: LogBroadcaster,
    /// Shown as `[label]` in front of output lines, e.g. `APT`.
    label: String,
}

impl SystemRunner {
    pub fn new(logs: LogBroadcaster, label: impl Into<String>) -> Self {
        Self {
            logs,
            label: label.into(),
        }
    }

    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        stream_stdout: bool,
    ) -> Result<CommandOutput, SourceError> {
        tracing::debug!(program, ?args, "running command");
        let prefix = Some(self.label.clone());
        self.logs.emit(LogEvent::command(None, program, args));

        let mut child = tokio::process::Command::new(program)
            .args(args)
            // Parseable, locale-independent output.
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => SourceError::Unavailable(program.to_owned()),
                _ => SourceError::Other(format!("Could not run {program}: {e}")),
            })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(SourceError::Other(format!(
                "Could not read the output of {program}."
            )));
        };

        let out_logs = stream_stdout.then(|| (self.logs.clone(), prefix.clone()));
        let err_logs = Some((self.logs.clone(), prefix));
        let (stdout, stderr, status) = tokio::join!(
            read_lines(stdout, out_logs),
            read_lines(stderr, err_logs),
            child.wait()
        );
        let status =
            status.map_err(|e| SourceError::Other(format!("Could not run {program}: {e}")))?;
        Ok(CommandOutput {
            stdout,
            stderr,
            success: status.success(),
            code: status.code(),
        })
    }
}

/// Notices that tools print on every call and that tell the user nothing. Hidden from the
/// terminal panel only; the text is still captured in the command output.
fn is_known_noise(line: &str) -> bool {
    // `apt list` warns about scripted use although this app only reads its stable output.
    line.contains("apt does not have a stable CLI interface")
}

/// Reads lines until EOF, emitting each one as it arrives, and returns everything read.
/// Invalid UTF-8 is replaced, never an error.
async fn read_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    logs: Option<(LogBroadcaster, Option<String>)>,
) -> String {
    let mut reader = tokio::io::BufReader::new(reader);
    let mut all = String::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).into_owned();
                if let Some((logs, prefix)) = &logs {
                    let text = line.trim_end_matches(['\n', '\r']);
                    if !text.is_empty() && !is_known_noise(text) {
                        logs.emit(LogEvent::output(prefix.clone(), text));
                    }
                }
                all.push_str(&line);
            }
        }
    }
    all
}

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, SourceError> {
        self.exec(program, args, false).await
    }

    async fn run_logged(&self, program: &str, args: &[&str]) -> Result<CommandOutput, SourceError> {
        self.exec(program, args, true).await
    }
}

#[cfg(feature = "fake")]
pub mod scripted {
    use std::collections::HashMap;

    use super::*;

    /// Returns canned output keyed by `"program arg1 arg2"`. Unknown commands fail.
    #[derive(Default)]
    pub struct ScriptedRunner {
        responses: HashMap<String, CommandOutput>,
    }

    impl ScriptedRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with(mut self, command: &str, stdout: &str) -> Self {
            self.responses.insert(
                command.to_owned(),
                CommandOutput {
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                    success: true,
                    code: Some(0),
                },
            );
            self
        }

        pub fn with_failure(self, command: &str, stderr: &str) -> Self {
            self.with_exit(command, 1, stderr)
        }

        pub fn with_exit(mut self, command: &str, code: i32, stderr: &str) -> Self {
            self.responses.insert(
                command.to_owned(),
                CommandOutput {
                    stdout: String::new(),
                    stderr: stderr.to_owned(),
                    success: false,
                    code: Some(code),
                },
            );
            self
        }
    }

    #[async_trait]
    impl CommandRunner for ScriptedRunner {
        async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, SourceError> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.responses
                .get(&key)
                .cloned()
                .ok_or_else(|| SourceError::Other(format!("unscripted command: {key}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::LogKind;

    #[tokio::test]
    async fn the_apt_scripting_warning_is_not_shown_but_other_errors_are() {
        let logs = LogBroadcaster::new();
        let mut rx = logs.subscribe();
        let runner = SystemRunner::new(logs, "APT");
        let out = runner
            .run(
                "sh",
                &[
                    "-c",
                    "w='WARNING: apt does not have a stable'; echo \"$w CLI interface.\" >&2; echo 'E: real problem' >&2",
                ],
            )
            .await
            .expect("run");
        assert!(out.stderr.contains("stable CLI"), "still captured");
        let mut shown = String::new();
        while let Ok(e) = rx.try_recv() {
            shown.push_str(&e.render());
        }
        assert!(!shown.contains("stable CLI"), "{shown}");
        assert!(shown.contains("E: real problem"), "{shown}");
    }

    #[tokio::test]
    async fn run_logged_streams_lines_and_echoes_command() {
        let logs = LogBroadcaster::new();
        let mut rx = logs.subscribe();
        let runner = SystemRunner::new(logs, "TEST");
        let out = runner
            .run_logged("sh", &["-c", "echo one; echo two >&2; echo three"])
            .await
            .expect("run");
        assert!(out.success && out.stdout.contains("three"));
        let mut lines = Vec::new();
        while let Ok(e) = rx.try_recv() {
            lines.push(e);
        }
        assert_eq!(lines[0].kind, LogKind::Command);
        let text: Vec<String> = lines.iter().map(LogEvent::render).collect();
        assert!(text.contains(&"[TEST] one".to_owned()), "{text:?}");
        assert!(text.contains(&"[TEST] two".to_owned()), "{text:?}");
    }

    #[tokio::test]
    async fn plain_run_keeps_stdout_out_of_the_log_but_collects_it() {
        let logs = LogBroadcaster::new();
        let mut rx = logs.subscribe();
        let out = SystemRunner::new(logs, "TEST")
            .run("sh", &["-c", "echo secret-looking-scan-output"])
            .await
            .expect("run");
        assert!(out.stdout.contains("scan-output"));
        while let Ok(e) = rx.try_recv() {
            // The command echo may mention the script; its stdout must not be streamed.
            assert!(e.kind == LogKind::Command || !e.render().contains("scan-output"));
        }
    }

    #[tokio::test]
    async fn secrets_in_command_output_are_redacted_in_the_log() {
        let logs = LogBroadcaster::new();
        let mut rx = logs.subscribe();
        SystemRunner::new(logs, "TEST")
            .run_logged("sh", &["-c", "echo token=ghp_abcdefghijklmnop123"])
            .await
            .expect("run");
        while let Ok(e) = rx.try_recv() {
            assert!(
                !e.render().contains("abcdefghijklmnop123"),
                "{}",
                e.render()
            );
        }
    }

    #[tokio::test]
    async fn missing_program_is_unavailable_and_exit_code_is_kept() {
        let r = SystemRunner::new(LogBroadcaster::new(), "TEST");
        let err = r
            .run("definitely-not-a-real-program-xyz", &[])
            .await
            .expect_err("missing");
        assert!(matches!(err, SourceError::Unavailable(_)));
        let out = r.run("sh", &["-c", "exit 7"]).await.expect("run");
        assert_eq!(out.code, Some(7));
        assert!(!out.success);
    }
}
