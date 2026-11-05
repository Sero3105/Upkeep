//! Process execution. Everything Upkeep runs goes through `Runner`, so tests swap in a mock.

use crate::config::Config;
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub status: i32,
    pub out: String,
    pub err: String,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl RunResult {
    pub fn ok(&self) -> bool {
        self.status == 0 && !self.timed_out && !self.cancelled
    }
    pub fn fail(status: i32, err: impl Into<String>) -> Self {
        RunResult { status, err: err.into(), ..Default::default() }
    }
}

#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct RunOpts<'a> {
    pub timeout: Duration,
    pub env: Vec<(&'a str, &'a str)>,
    pub on_line: Option<&'a (dyn Fn(&str) + Sync)>,
    pub cancel: Option<CancelToken>,
    /// New session without a controlling terminal: a `sudo` prompt then fails cleanly
    /// instead of grabbing the TUI's terminal. False for interactive CLI runs.
    pub detach_tty: bool,
}

impl Default for RunOpts<'_> {
    fn default() -> Self {
        RunOpts { timeout: Duration::from_secs(60), env: vec![], on_line: None, cancel: None, detach_tty: true }
    }
}

pub trait Runner: Send + Sync {
    fn run(&self, argv: &[&str], opts: &RunOpts) -> RunResult;
    fn which(&self, cmd: &str) -> Option<String>;
}

pub fn expand_tilde(path: &str, home: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

/// Drop ANSI escape sequences (colors, cursor moves) from tool output.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('[') => {
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = it.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && it.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub struct SystemRunner {
    path: String,
}

impl SystemRunner {
    pub fn new() -> Self {
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into());
        // Homebrew may be missing from PATH in non-login shells; probing it is harmless.
        SystemRunner { path: format!("{path}:/opt/homebrew/bin:/usr/local/bin") }
    }
}

impl Runner for SystemRunner {
    fn which(&self, cmd: &str) -> Option<String> {
        let home = std::env::var("HOME").unwrap_or_default();
        let is_exec = |p: &str| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file())
                && std::ffi::CString::new(p).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0)
        };
        if cmd.contains('/') {
            return is_exec(cmd).then(|| cmd.to_string());
        }
        self.path
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| format!("{}/{cmd}", expand_tilde(d, &home)))
            .find(|p| is_exec(p))
    }

    fn run(&self, argv: &[&str], o: &RunOpts) -> RunResult {
        let Some(exe) = argv.first().and_then(|a| self.which(a)) else {
            return RunResult::fail(127, format!("command not found: {}", argv.first().unwrap_or(&"")));
        };
        let mut cmd = Command::new(exe);
        cmd.args(&argv[1..]).env("PATH", &self.path).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in &o.env {
            cmd.env(k, v);
        }
        if o.detach_tty {
            cmd.stdin(Stdio::null());
            // SAFETY: setsid is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return RunResult::fail(126, format!("spawn failed: {e}")),
        };
        let pid = child.id() as i32;
        let (tx, rx) = mpsc::channel::<(u8, String)>();
        let pumps = [
            pump(child.stdout.take().unwrap(), 0, tx.clone()),
            pump(child.stderr.take().unwrap(), 1, tx),
        ];

        let (mut out, mut err) = (String::new(), String::new());
        let deadline = Instant::now() + o.timeout;
        let (mut timed_out, mut cancelled) = (false, false);
        let mut killed_at: Option<Instant> = None;
        let mut exited_at: Option<Instant> = None;
        let signal = |sig| unsafe { libc::kill(if o.detach_tty { -pid } else { pid }, sig) };
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((stream, line)) => {
                    if let Some(f) = o.on_line {
                        let clean = strip_ansi(&line);
                        if !clean.trim().is_empty() {
                            f(&clean);
                        }
                    }
                    let buf = if stream == 0 { &mut out } else { &mut err };
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if killed_at.is_none() && (o.cancel.as_ref().is_some_and(|c| c.is_cancelled()) || Instant::now() > deadline) {
                cancelled = o.cancel.as_ref().is_some_and(|c| c.is_cancelled());
                timed_out = !cancelled;
                signal(libc::SIGTERM);
                killed_at = Some(Instant::now());
            } else if killed_at.is_some_and(|k| k.elapsed() > Duration::from_secs(5)) {
                signal(libc::SIGKILL);
            }
            // A grandchild may keep the pipes open after the child exits: don't wait forever.
            if exited_at.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                exited_at = Some(Instant::now());
            }
            if exited_at.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
                break;
            }
        }
        let status = child.wait().map(|s| s.code().unwrap_or(128 + std::os::unix::process::ExitStatusExt::signal(&s).unwrap_or(0))).unwrap_or(-1);
        if exited_at.is_none() {
            for p in pumps {
                let _ = p.join();
            }
        }
        RunResult { status, out, err, timed_out, cancelled }
    }
}

fn pump(mut r: impl Read + Send + 'static, stream: u8, tx: mpsc::Sender<(u8, String)>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut buf, mut line) = ([0u8; 8192], Vec::new());
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            for &b in &buf[..n] {
                // `\r` progress updates count as lines too.
                if b == b'\n' || (b == b'\r' && !line.is_empty()) {
                    let _ = tx.send((stream, String::from_utf8_lossy(&line).into_owned()));
                    line.clear();
                } else if b != b'\r' {
                    line.push(b);
                }
            }
        }
        if !line.is_empty() {
            let _ = tx.send((stream, String::from_utf8_lossy(&line).into_owned()));
        }
    })
}

/// Shared scan/exec context.
#[derive(Clone)]
pub struct Ctx {
    pub runner: Arc<dyn Runner>,
    pub config: Config,
    pub home: String,
    pub env: HashMap<String, String>,
    pub app_dirs: Vec<String>,
    /// Root of Homebrew Cellars, overridable for tests.
    pub cellars: Vec<String>,
}

impl Ctx {
    pub fn system(config: Config) -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        Ctx {
            runner: Arc::new(SystemRunner::new()),
            config,
            app_dirs: vec!["/Applications".into(), "/Applications/Utilities".into(), format!("{home}/Applications")],
            cellars: vec!["/opt/homebrew/Cellar".into(), "/usr/local/Cellar".into()],
            env: std::env::vars().collect(),
            home,
        }
    }

    pub fn has(&self, cmd: &str) -> bool {
        self.runner.which(cmd).is_some()
    }

    pub fn run(&self, argv: &[&str], secs: u64) -> RunResult {
        self.runner.run(argv, &RunOpts { timeout: Duration::from_secs(secs), ..Default::default() })
    }

    /// stdout of a successful command.
    pub fn out(&self, argv: &[&str], secs: u64) -> Option<String> {
        let r = self.run(argv, secs);
        r.ok().then_some(r.out)
    }

    pub fn path(&self, p: &str) -> String {
        expand_tilde(p, &self.home)
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Canned responses keyed by the joined argv. Each key holds a queue; the last entry repeats.
    #[derive(Default)]
    pub struct MockRunner {
        pub responses: Mutex<HashMap<String, VecDeque<RunResult>>>,
        pub calls: Mutex<Vec<String>>,
        pub bins: Vec<String>,
    }

    impl MockRunner {
        pub fn new(bins: &[&str]) -> Self {
            MockRunner { bins: bins.iter().map(|s| s.to_string()).collect(), ..Default::default() }
        }
        pub fn on(self, cmd: &str, status: i32, out: &str) -> Self {
            let r = RunResult { status, out: out.into(), ..Default::default() };
            self.responses.lock().unwrap().entry(cmd.into()).or_default().push_back(r);
            self
        }
    }

    impl Runner for MockRunner {
        fn run(&self, argv: &[&str], o: &RunOpts) -> RunResult {
            let key = argv.join(" ");
            self.calls.lock().unwrap().push(key.clone());
            let mut map = self.responses.lock().unwrap();
            let r = match map.get_mut(&key) {
                Some(q) if q.len() > 1 => q.pop_front().unwrap(),
                Some(q) => q.front().cloned().unwrap(),
                None => RunResult::fail(127, format!("mock: no response for `{key}`")),
            };
            if let Some(f) = o.on_line {
                r.out.lines().for_each(f);
            }
            r
        }
        fn which(&self, cmd: &str) -> Option<String> {
            self.bins.iter().any(|b| b == cmd).then(|| format!("/mock/bin/{cmd}"))
        }
    }

    pub fn ctx(runner: MockRunner) -> Ctx {
        Ctx {
            runner: Arc::new(runner),
            config: Config::default(),
            home: "/nonexistent-home".into(),
            env: HashMap::new(),
            app_dirs: vec![],
            cellars: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\u{1b}[1;32m==>\u{1b}[0m Upgrading"), "==> Upgrading");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}ok"), "ok");
    }

    #[test]
    fn expands_tilde() {
        assert_eq!(expand_tilde("~/.dotnet/tools", "/Users/x"), "/Users/x/.dotnet/tools");
        assert_eq!(expand_tilde("~other", "/Users/x"), "~other");
    }

    #[test]
    fn runs_real_process_with_streaming_and_timeout() {
        let r = SystemRunner::new();
        let lines = std::sync::Mutex::new(vec![]);
        let cb = |l: &str| lines.lock().unwrap().push(l.to_string());
        let res = r.run(&["sh", "-c", "echo one; echo two >&2; exit 3"], &RunOpts { on_line: Some(&cb), ..Default::default() });
        assert_eq!(res.status, 3);
        assert_eq!(res.out, "one\n");
        assert_eq!(res.err, "two\n");
        assert_eq!(lines.lock().unwrap().len(), 2);

        let slow = r.run(&["sleep", "5"], &RunOpts { timeout: Duration::from_millis(300), ..Default::default() });
        assert!(slow.timed_out && !slow.ok());
        assert_eq!(r.run(&["definitely-not-a-cmd-xyz"], &RunOpts::default()).status, 127);
    }
}
