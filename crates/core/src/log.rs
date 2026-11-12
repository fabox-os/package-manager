//! Log events for the terminal panel: sanitized, broadcast, never blocking.
//!
//! Sources and runners emit [`LogEvent`]s through a [`LogBroadcaster`]; the GUI and the
//! `last-run.log` writer subscribe. Everything is sanitized on the way in, so a token that
//! reaches a log line can never reach the screen or the disk.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use tokio::sync::broadcast;

pub const REDACTED: &str = "[redacted]";
/// Lines kept in the panel and in the broadcast backlog.
pub const MAX_LINES: usize = 2000;
/// Size cap of `last-run.log` before it is rotated.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// `$ command`
    Command,
    /// Output of a process or a plain message.
    Output,
    /// `Done` / `Failed: reason`
    Status,
    /// Marks the start of a new run; clears `last-run.log`. Not displayed.
    RunStart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    pub kind: LogKind,
    /// e.g. `APT • Ubuntu noble/main`, shown as `[prefix]`.
    pub prefix: Option<String>,
    pub message: String,
}

impl LogEvent {
    pub fn command(prefix: Option<String>, program: &str, args: &[&str]) -> Self {
        let mut line = format!("$ {program}");
        for a in args {
            line.push(' ');
            line.push_str(a);
        }
        // Keep the echo on one line (dpkg-query formats contain \n and \t).
        line = line
            .replace('\n', "\\n")
            .replace('\t', "\\t")
            .replace('\r', "\\r");
        // Scans pass thousands of package names; keep the echo readable.
        if line.chars().count() > 160 {
            line = line.chars().take(160).collect::<String>() + "…";
        }
        Self {
            kind: LogKind::Command,
            prefix,
            message: line,
        }
    }

    pub fn output(prefix: Option<String>, message: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Output,
            prefix,
            message: message.into(),
        }
    }

    pub fn status(prefix: Option<String>, message: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Status,
            prefix,
            message: message.into(),
        }
    }

    pub fn run_start() -> Self {
        Self {
            kind: LogKind::RunStart,
            prefix: None,
            message: String::new(),
        }
    }

    /// The line as shown in the panel and written to disk.
    pub fn render(&self) -> String {
        match (&self.kind, &self.prefix) {
            (LogKind::Command, _) | (_, None) => self.message.clone(),
            (_, Some(p)) => format!("[{p}] {}", self.message),
        }
    }

    fn sanitized(mut self) -> Self {
        self.message = sanitize(&self.message);
        self.prefix = self.prefix.map(|p| sanitize(&p));
        self
    }
}

/// Cheap to clone. `emit` never blocks and never fails: with no subscriber the event is
/// dropped, and a slow subscriber loses old lines instead of stalling the sender.
#[derive(Clone)]
pub struct LogBroadcaster {
    tx: broadcast::Sender<LogEvent>,
}

impl Default for LogBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBroadcaster {
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(MAX_LINES).0,
        }
    }

    pub fn emit(&self, event: LogEvent) {
        // `Err` only means nobody is subscribed.
        let _ = self.tx.send(event.sanitized());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEvent> {
        self.tx.subscribe()
    }
}

// ---------------------------------------------------------------- sanitizing

const SECRET_KEYS: [&str; 9] = [
    "token",
    "access_token",
    "auth_token",
    "password",
    "passwd",
    "pwd",
    "secret",
    "api_key",
    "apikey",
];
const GITHUB_PREFIXES: [&str; 6] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Redacts GitHub tokens, `key=value` secrets, bearer/authorization values and URL
/// credentials. When in doubt it redacts: a missing word is better than a leaked secret.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    // Redact the next word when the previous one introduces a credential.
    let mut redact_next = false;
    for (i, piece) in input.split_inclusive(char::is_whitespace).enumerate() {
        let word = piece.trim_end();
        let ws = &piece[word.len()..];
        let _ = i;
        if word.is_empty() {
            out.push_str(piece);
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if redact_next {
            redact_next = matches!(lower.as_str(), "bearer" | "token" | "basic");
            out.push_str(REDACTED);
            out.push_str(ws);
            continue;
        }
        if matches!(
            lower.as_str(),
            "bearer" | "authorization:" | "authorization"
        ) {
            redact_next = true;
        }
        out.push_str(&sanitize_word(word));
        out.push_str(ws);
    }
    out
}

fn sanitize_word(word: &str) -> String {
    let word = redact_url_userinfo(word);
    let word = redact_github_tokens(&word);
    redact_key_values(&word)
}

fn redact_url_userinfo(word: &str) -> String {
    let Some(scheme_end) = word.find("://") else {
        return word.to_owned();
    };
    let rest_start = scheme_end + 3;
    let rest = &word[rest_start..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}{REDACTED}@{}", &word[..rest_start], &rest[at + 1..]),
        None => word.to_owned(),
    }
}

fn redact_github_tokens(word: &str) -> String {
    let mut out = String::new();
    let mut rest = word;
    'scan: while !rest.is_empty() {
        for p in GITHUB_PREFIXES {
            if rest.starts_with(p) {
                let len = rest.find(|c: char| !is_token_char(c)).unwrap_or(rest.len());
                out.push_str(REDACTED);
                rest = &rest[len..];
                continue 'scan;
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn redact_key_values(word: &str) -> String {
    // Split at `?`, `&`, `;` while keeping them, then look for `key=value` per piece.
    let mut out = String::new();
    let mut piece_start = 0;
    let bytes = word.as_bytes();
    for i in 0..=word.len() {
        let at_end = i == word.len();
        if at_end || matches!(bytes[i], b'?' | b'&' | b';') {
            out.push_str(&redact_pair(&word[piece_start..i]));
            if !at_end {
                out.push(bytes[i] as char);
            }
            piece_start = i + 1;
        }
    }
    out
}

fn redact_pair(piece: &str) -> String {
    let Some((key, _value)) = piece.split_once('=') else {
        return piece.to_owned();
    };
    let key_clean = key
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .to_ascii_lowercase();
    if SECRET_KEYS.contains(&key_clean.as_str()) {
        format!("{key}={REDACTED}")
    } else {
        piece.to_owned()
    }
}

// ---------------------------------------------------------------- last-run.log

/// Append-only log file capped at [`MAX_LOG_BYTES`]: when full, the old file is kept as
/// `<name>.1` (replacing the previous one) and a new file is started.
pub struct LogFile {
    path: PathBuf,
    max_bytes: u64,
}

impl LogFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            max_bytes: MAX_LOG_BYTES,
        }
    }

    #[cfg(test)]
    fn with_max(path: PathBuf, max_bytes: u64) -> Self {
        Self { path, max_bytes }
    }

    /// Starts a fresh "last run": the previous content is discarded.
    pub fn start_run(&self) -> std::io::Result<()> {
        self.ensure_dir()?;
        fs::write(&self.path, b"")
    }

    pub fn append_line(&self, line: &str) -> std::io::Result<()> {
        self.ensure_dir()?;
        let size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size + line.len() as u64 + 1 > self.max_bytes {
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            fs::rename(&self.path, rotated)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{line}")
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        match self.path.parent() {
            Some(dir) => fs::create_dir_all(dir),
            None => Ok(()),
        }
    }
}

/// Writes every event to `last-run.log` on a dedicated thread, so file I/O never touches the
/// UI thread. Stops when the broadcaster is dropped.
pub fn spawn_file_logger(
    mut rx: broadcast::Receiver<LogEvent>,
    file: LogFile,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        let event = match rx.blocking_recv() {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let _ = file.append_line(&format!("[log] {n} lines were dropped"));
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let result = match event.kind {
            LogKind::RunStart => file.start_run(),
            _ => file.append_line(&event.render()),
        };
        if let Err(e) = result {
            tracing::warn!("could not write the run log: {e}");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_tokens_never_leak() {
        for t in [
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "gho_ABCDEF123456",
        ] {
            let s = sanitize(&format!("downloading with {t} now"));
            assert!(!s.contains(t), "{s}");
            assert!(s.contains(REDACTED));
        }
    }

    #[test]
    fn token_inside_url_and_quotes_is_redacted() {
        let s = sanitize("GET https://api.github.com/x?per_page=5&access_token=SECRET123&b=1 ok");
        assert!(!s.contains("SECRET123"), "{s}");
        assert!(s.contains("per_page=5") && s.contains("b=1"));
        let s = sanitize("url=\"https://x.test/a?token=SECRET123\"");
        assert!(!s.contains("SECRET123"), "{s}");
    }

    #[test]
    fn passwords_and_bearer_headers_are_redacted() {
        assert!(!sanitize("password=hunter2 done").contains("hunter2"));
        assert!(!sanitize("Authorization: Bearer abc.def.ghi").contains("abc.def.ghi"));
        assert!(!sanitize("Authorization: token abcdef").contains("abcdef"));
        assert!(!sanitize("curl -H bearer abcdef").contains("abcdef"));
    }

    #[test]
    fn url_credentials_are_redacted() {
        let s = sanitize("fetch https://user:pw123@example.org/repo");
        assert!(!s.contains("pw123") && !s.contains("user:"), "{s}");
        assert!(s.contains("example.org/repo"));
    }

    #[test]
    fn ordinary_lines_are_unchanged() {
        for l in [
            "Get:1 http://archive.ubuntu.com/ubuntu noble InRelease [256 kB]",
            "Unpacking firefox (132.0) over (131.0) ...",
            "[APT • Ubuntu noble/main] firefox: Updating…",
            "Hit the token limit? no.",
        ] {
            assert_eq!(sanitize(l), l);
        }
    }

    #[test]
    fn sanitize_preserves_whitespace_and_never_panics() {
        assert_eq!(sanitize("a  b\tc\n"), "a  b\tc\n");
        for s in ["", "=", "?&=;", "://@", "ghp_", "é=é?token=", "\u{0}"] {
            let _ = sanitize(s);
        }
    }

    #[test]
    fn broadcaster_sanitizes_prefix_and_message() {
        let b = LogBroadcaster::new();
        let mut rx = b.subscribe();
        b.emit(LogEvent::output(
            Some("GitHub ghp_secret123".into()),
            "token=abc",
        ));
        let e = rx.try_recv().expect("event");
        assert!(
            !e.render().contains("secret123") && !e.render().contains("abc"),
            "{}",
            e.render()
        );
    }

    #[test]
    fn emit_never_blocks_without_or_with_slow_subscribers() {
        let b = LogBroadcaster::new();
        b.emit(LogEvent::output(None, "nobody listens")); // no subscriber: fine
        let mut slow = b.subscribe();
        for i in 0..(MAX_LINES * 3) {
            b.emit(LogEvent::output(None, format!("line {i}")));
        }
        // The slow subscriber lost old lines but the sender was never stalled.
        assert!(matches!(
            slow.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }

    #[test]
    fn render_formats_prefix_and_commands() {
        assert_eq!(
            LogEvent::output(Some("APT • Ubuntu".into()), "hi").render(),
            "[APT • Ubuntu] hi"
        );
        assert_eq!(
            LogEvent::command(None, "apt", &["list", "--upgradable"]).render(),
            "$ apt list --upgradable"
        );
        let c = LogEvent::command(None, "dpkg-query", &["-f=${Package}\t${Version}\n"]).render();
        assert!(!c.contains('\n') && !c.contains('\t'), "{c}");
        let long: Vec<String> = (0..500).map(|i| format!("pkg{i}")).collect();
        let args: Vec<&str> = long.iter().map(String::as_str).collect();
        assert!(
            LogEvent::command(None, "apt-cache", &args)
                .render()
                .chars()
                .count()
                <= 161
        );
    }

    #[test]
    fn log_file_rotates_and_start_run_truncates() {
        let dir = std::env::temp_dir().join(format!("um-log-test-{}", std::process::id()));
        let path = dir.join("last-run.log");
        let f = LogFile::with_max(path.clone(), 30);
        f.start_run().expect("start");
        f.append_line("0123456789").expect("a");
        f.append_line("0123456789").expect("b");
        f.append_line("0123456789").expect("c"); // would exceed 30 bytes -> rotate
        assert!(dir.join("last-run.log.1").exists());
        assert!(fs::metadata(&path).expect("meta").len() <= 30);
        f.start_run().expect("restart");
        assert_eq!(fs::read_to_string(&path).expect("read"), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
