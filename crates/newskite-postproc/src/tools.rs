//! External tool discovery and a small, shell-free process runner.
//!
//! Every tool is spawned directly with an argument vector (never through a shell), with
//! stdin connected to the null device so a tool can never block on an interactive
//! prompt, stdout/stderr captured by background reader threads (so a chatty tool can
//! never deadlock on a full pipe) and a wall-clock timeout after which the child is
//! killed.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::PostError;

/// Paths to the external post-processing tools. `None` means the tool was not found and
/// the corresponding step degrades gracefully (it is reported as skipped). Extraction
/// does not need any of the archive tools: they are only a fallback for archives the
/// native backends report as unsupported (see the crate documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tools {
    /// `par2` (par2cmdline) used for PAR2 verify/repair.
    pub par2: Option<PathBuf>,
    /// 7-Zip command-line binary (`7z`, `7zz` or `7za`), extraction fallback.
    pub sevenz: Option<PathBuf>,
    /// `unrar` (preferred fallback for RAR archives).
    pub unrar: Option<PathBuf>,
    /// Info-ZIP `unzip` (single-volume zip fallback when 7-Zip is missing).
    pub unzip: Option<PathBuf>,
}

impl Tools {
    /// No tools at all: PAR2 is skipped, extraction runs natively without a fallback.
    pub fn none() -> Self {
        Self::default()
    }

    /// True when at least one external extraction tool (fallback) is available.
    pub fn can_extract(&self) -> bool {
        self.sevenz.is_some() || self.unrar.is_some() || self.unzip.is_some()
    }

    /// One-line human summary, e.g. `par2: /usr/bin/par2, 7z: missing, ...`.
    pub fn summary(&self) -> String {
        fn show(p: &Option<PathBuf>) -> String {
            p.as_ref().map_or_else(|| "missing".to_string(), |p| p.display().to_string())
        }
        format!(
            "par2: {}, 7z: {}, unrar: {}, unzip: {}",
            show(&self.par2),
            show(&self.sevenz),
            show(&self.unrar),
            show(&self.unzip)
        )
    }
}

/// Program names searched for each tool, in order of preference.
const PAR2_NAMES: &[&str] = &["par2", "par2cmdline"];
const SEVENZ_NAMES: &[&str] = &["7z", "7zz", "7za"];
const UNRAR_NAMES: &[&str] = &["unrar"];
const UNZIP_NAMES: &[&str] = &["unzip"];

/// Look the tools up on `PATH`, plus a few well-known install locations that are often
/// missing from the `PATH` of GUI applications (Homebrew on macOS, the default 7-Zip and
/// WinRAR folders on Windows).
pub fn detect_tools() -> Tools {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(extra_search_dirs());
    detect_tools_in(&dirs)
}

/// Like [`detect_tools`] but searching only `dirs` (in order). Useful for tests and for
/// a user-configured tool folder.
pub fn detect_tools_in(dirs: &[PathBuf]) -> Tools {
    Tools {
        par2: find_tool(PAR2_NAMES, dirs),
        sevenz: find_tool(SEVENZ_NAMES, dirs),
        unrar: find_tool(UNRAR_NAMES, dirs),
        unzip: find_tool(UNZIP_NAMES, dirs),
    }
}

/// Find the first of `names` that exists as an executable file in one of `dirs`.
/// Earlier names win over later ones; for one name, earlier directories win.
/// On Windows `.exe` and `.com` are appended (batch files are deliberately not
/// considered: they would be run through `cmd.exe`, i.e. a shell).
pub fn find_tool(names: &[&str], dirs: &[PathBuf]) -> Option<PathBuf> {
    names.iter().find_map(|name| {
        dirs.iter()
            .filter(|d| !d.as_os_str().is_empty())
            .find_map(|dir| candidate_files(dir, name).into_iter().find(|p| is_executable(p)))
    })
}

fn candidate_files(dir: &Path, name: &str) -> Vec<PathBuf> {
    if cfg!(windows) {
        vec![dir.join(format!("{name}.exe")), dir.join(format!("{name}.com"))]
    } else {
        vec![dir.join(name)]
    }
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(windows)]
fn extra_search_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            v.push(base.join("7-Zip"));
            v.push(base.join("WinRAR"));
            v.push(base.join("par2cmdline"));
        }
    }
    v
}

#[cfg(target_os = "macos")]
fn extra_search_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin"), PathBuf::from("/opt/local/bin")]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn extra_search_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
}

#[cfg(not(any(unix, windows)))]
fn extra_search_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// Result of one tool run.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolRun {
    /// Exit code; `None` when killed by a signal or by the timeout.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// True when stdout or stderr exceeded the capture limit (only the tail was kept).
    pub truncated: bool,
}

impl ToolRun {
    /// stdout followed by stderr, for message matching.
    pub fn combined(&self) -> String {
        let mut s = String::with_capacity(self.stdout.len() + self.stderr.len() + 1);
        s.push_str(&self.stdout);
        s.push('\n');
        s.push_str(&self.stderr);
        s
    }
}

/// A command line ready to run, plus a display form that never contains a password.
#[derive(Debug, Clone)]
pub(crate) struct ToolCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// `(index into args, visible prefix)` for arguments holding secrets; shown as
    /// `<prefix>***` by [`ToolCommand::display`].
    pub secret_args: Vec<(usize, String)>,
    pub cwd: Option<PathBuf>,
}

impl ToolCommand {
    pub fn new(program: &Path) -> Self {
        Self { program: program.to_path_buf(), args: Vec::new(), secret_args: Vec::new(), cwd: None }
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_os_string());
        self
    }

    /// Add an argument that contains a secret; `prefix` (e.g. `-p`) stays visible in logs.
    pub fn secret_arg(mut self, prefix: &str, secret: &str) -> Self {
        self.secret_args.push((self.args.len(), prefix.to_string()));
        self.args.push(OsString::from(format!("{prefix}{secret}")));
        self
    }

    pub fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    /// Program name and arguments for logs, with secret arguments redacted.
    pub fn display(&self) -> String {
        let prog = self
            .program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.program.display().to_string());
        let mut s = prog;
        for (i, a) in self.args.iter().enumerate() {
            s.push(' ');
            if let Some((_, prefix)) = self.secret_args.iter().find(|(idx, _)| *idx == i) {
                // Keep the switch prefix only (e.g. "-p"), never the value.
                s.push_str(prefix);
                s.push_str("***");
            } else {
                let a = a.to_string_lossy();
                if a.contains(' ') {
                    s.push('"');
                    s.push_str(&a);
                    s.push('"');
                } else {
                    s.push_str(&a);
                }
            }
        }
        s
    }

    /// Spawn the tool, wait up to `timeout`, and capture at most `capture_limit` bytes of
    /// each output stream (the tail is kept when the limit is exceeded).
    pub fn run(&self, timeout: Duration, capture_limit: usize) -> Result<ToolRun, PostError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: never flash a console window from the GUI.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| {
            PostError::Tool(format!("could not start {}: {e}", self.program.display()))
        })?;

        let out_rx = child.stdout.take().map(|s| spawn_reader(s, capture_limit));
        let err_rx = child.stderr.take().map(|s| spawn_reader(s, capture_limit));

        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        timed_out = true;
                        // Best effort: the child may have exited in the meantime.
                        let _ = child.kill();
                        break child.wait().ok();
                    }
                    thread::sleep(poll_interval(start.elapsed()));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PostError::Io(e));
                }
            }
        };

        // A grandchild that inherited the pipes could keep them open after the tool
        // exited; never wait on that for long.
        let (stdout, t1) = collect_reader(out_rx);
        let (stderr, t2) = collect_reader(err_rx);
        Ok(ToolRun {
            code: if timed_out { None } else { status.and_then(|s| s.code()) },
            stdout,
            stderr,
            timed_out,
            truncated: t1 || t2,
        })
    }
}

/// Poll quickly at first (listing commands finish in milliseconds), then back off.
fn poll_interval(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(1) {
        Duration::from_millis(10)
    } else {
        Duration::from_millis(100)
    }
}

/// How long to wait for the output streams to close once the tool has exited.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

fn spawn_reader(r: impl Read + Send + 'static, limit: usize) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // The receiver may be gone after the grace period; nothing to do then.
        let _ = tx.send(read_capped(r, limit));
    });
    rx
}

fn collect_reader(rx: Option<mpsc::Receiver<(Vec<u8>, bool)>>) -> (String, bool) {
    match rx.map(|rx| rx.recv_timeout(DRAIN_GRACE)) {
        Some(Ok((bytes, truncated))) => (String::from_utf8_lossy(&bytes).into_owned(), truncated),
        _ => (String::new(), false),
    }
}

/// Read a stream to the end, keeping at most `limit` bytes (the most recent ones).
fn read_capped(mut r: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > limit {
                    let excess = buf.len() - limit;
                    buf.drain(..excess);
                    truncated = true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (buf, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_redacts_secrets() {
        let c = ToolCommand::new(Path::new("/usr/bin/7z"))
            .arg("x")
            .secret_arg("-p", "hunter2")
            .arg("my file.7z");
        let d = c.display();
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("-p***"), "{d}");
        assert!(d.starts_with("7z x"), "{d}");
        assert!(d.contains("\"my file.7z\""), "{d}");
    }

    #[test]
    fn display_redacts_separate_secret_value() {
        let c = ToolCommand::new(Path::new("unzip")).arg("-P").secret_arg("", "pw");
        assert_eq!(c.display(), "unzip -P ***");
        assert!(!c.display().contains("pw"));
    }

    #[test]
    fn read_capped_keeps_tail() {
        let data: Vec<u8> = (0..100u8).collect();
        let (v, t) = read_capped(&data[..], 10);
        assert!(t);
        assert_eq!(v, (90..100u8).collect::<Vec<_>>());
        let (v, t) = read_capped(&data[..], 1000);
        assert!(!t);
        assert_eq!(v.len(), 100);
    }

    #[test]
    fn find_tool_respects_name_order_and_missing() {
        let tmp = std::env::temp_dir().join(format!("nk-find-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(find_tool(&["7z", "7za"], std::slice::from_ref(&tmp)), None);
        let file = if cfg!(windows) { tmp.join("7za.exe") } else { tmp.join("7za") };
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Not executable yet: must not be found.
            assert_eq!(find_tool(&["7z", "7za"], std::slice::from_ref(&tmp)), None);
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(find_tool(&["7z", "7za"], std::slice::from_ref(&tmp)), Some(file));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn run_captures_output_and_times_out() {
        let sh = Path::new("/bin/sh");
        if !sh.exists() {
            return;
        }
        let r = ToolCommand::new(sh).arg("-c").arg("echo out; echo err 1>&2; exit 3")
            .run(Duration::from_secs(10), 1024)
            .unwrap();
        assert_eq!(r.code, Some(3));
        assert_eq!(r.stdout.trim(), "out");
        assert_eq!(r.stderr.trim(), "err");
        let start = Instant::now();
        let r = ToolCommand::new(sh).arg("-c").arg("exec sleep 5").run(Duration::from_millis(200), 1024).unwrap();
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(r.code, None);
    }

    #[test]
    fn run_missing_program_is_tool_error() {
        let r = ToolCommand::new(Path::new("/definitely/not/a/tool-xyz")).run(Duration::from_secs(1), 10);
        assert!(matches!(r, Err(PostError::Tool(_))));
    }
}
