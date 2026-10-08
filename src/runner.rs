use anyhow::{Context, Result};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
}

impl Cmd {
    pub fn new(program: &str) -> Self {
        Self { program: program.into(), args: vec![], cwd: None, timeout: Duration::from_secs(120) }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(items.into_iter().map(Into::into));
        self
    }
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }
    pub fn line(&self) -> String {
        std::iter::once(self.program.clone()).chain(self.args.iter().cloned()).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }
    pub fn ok(stdout: &str) -> Self {
        Self { status: Some(0), stdout: stdout.into(), ..Default::default() }
    }
    pub fn fail(code: i32, stderr: &str) -> Self {
        Self { status: Some(code), stderr: stderr.into(), ..Default::default() }
    }
}

pub trait Runner {
    fn run(&self, cmd: &Cmd) -> Result<Output>;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let mut command = Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(dir) = &cmd.cwd {
            command.current_dir(dir);
        }
        let mut child = command.spawn().with_context(|| format!("falha ao executar {}", cmd.program))?;
        let mut out = child.stdout.take().expect("stdout piped");
        let mut err = child.stderr.take().expect("stderr piped");

        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();

        let _out_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = out.read_to_end(&mut b);
            let _ = out_tx.send(b);
        });
        let _err_t = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = err.read_to_end(&mut b);
            let _ = err_tx.send(b);
        });
        let status = match child.wait_timeout(cmd.timeout) {
            Ok(s) => s,
            Err(e) => {
                terminate_group(&mut child);
                return Err(e.into());
            }
        };
        let timed_out = status.is_none();
        let code = match status {
            Some(s) => {
                // netos que ficaram segurando stdout/stderr travariam os leitores
                kill_group(child.id());
                s.code()
            }
            None => {
                terminate_group(&mut child);
                None
            }
        };

        // Collect output with shared deadline to prevent blocking on escaped descendants
        let deadline = Instant::now() + Duration::from_secs(5);
        let stdout_bytes = out_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        let stderr_bytes = err_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_default();

        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        Ok(Output { status: code, stdout, stderr, timed_out })
    }
}

/// Mata o que sobrou no grupo de um processo que já terminou.
pub fn kill_group(pgid: u32) {
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

/// SIGTERM no grupo do filho, espera até 5s, e SIGKILL no grupo para pegar netos teimosos.
pub fn terminate_group(child: &mut Child) {
    let pgid = child.id() as libc::pid_t;
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let _ = child.wait_timeout(Duration::from_secs(5));
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeRunner {
    pub calls: std::cell::RefCell<Vec<Cmd>>,
    rules: std::cell::RefCell<Vec<(String, Output)>>,
}

#[cfg(test)]
impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }
    /// Responde `out` uma única vez ao primeiro comando cuja linha contém `needle`.
    pub fn on(&self, needle: &str, out: Output) {
        self.rules.borrow_mut().push((needle.into(), out));
    }
    pub fn lines(&self) -> Vec<String> {
        self.calls.borrow().iter().map(Cmd::line).collect()
    }
}

#[cfg(test)]
impl Runner for FakeRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.calls.borrow_mut().push(cmd.clone());
        let line = cmd.line();
        let mut rules = self.rules.borrow_mut();
        match rules.iter().position(|(n, _)| line.contains(n.as_str())) {
            Some(i) => Ok(rules.remove(i).1),
            None => anyhow::bail!("FakeRunner: comando inesperado: {line}"),
        }
    }
}

#[cfg(test)]
struct EscapedProcessGuard {
    pidfile: std::path::PathBuf,
}

#[cfg(test)]
impl Drop for EscapedProcessGuard {
    fn drop(&mut self) {
        if let Ok(pid_str) = std::fs::read_to_string(&self.pidfile) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn read_pid(path: &std::path::Path) -> i32 {
        let start = Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(path) {
                if let Ok(pid) = s.trim().parse() {
                    return pid;
                }
            }
            assert!(start.elapsed() < Duration::from_secs(5), "pidfile não apareceu");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_dead(pid: i32) {
        std::thread::sleep(Duration::from_millis(300));
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "processo {pid} ainda vivo");
    }

    #[test]
    fn captures_stdout_and_exit_code() {
        let out = RealRunner.run(&Cmd::new("sh").args(["-c", "echo oi; echo erro >&2; exit 3"])).unwrap();
        assert_eq!(out.status, Some(3));
        assert_eq!(out.stdout.trim(), "oi");
        assert_eq!(out.stderr.trim(), "erro");
        assert!(!out.success());
    }

    #[test]
    fn runs_in_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let out = RealRunner.run(&Cmd::new("pwd").cwd(dir.path())).unwrap();
        assert_eq!(std::fs::canonicalize(out.stdout.trim()).unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn timeout_kills_process_group_including_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let started = Instant::now();
        let out = RealRunner
            .run(&Cmd::new("sh").args(["-c", &script]).timeout(Duration::from_millis(300)))
            .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_dead(read_pid(&pidfile));
    }

    #[test]
    fn leftovers_holding_the_pipe_do_not_block_and_are_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > {}", pidfile.display());
        let started = Instant::now();
        let out = RealRunner.run(&Cmd::new("sh").args(["-c", &script])).unwrap();
        assert!(out.success());
        assert!(started.elapsed() < Duration::from_secs(5), "runner esperou o neto");
        assert_dead(read_pid(&pidfile));
    }

    #[test]
    fn escaped_descendant_holding_pipe_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let marker = dir.path().join("marker");

        // Deterministic escape: perl calls setsid, creates marker, sleeps; shell waits for marker then exits
        let script = format!(
            "perl -e 'use POSIX; setsid(); open(F, \">\", \"{}\"); close(F); sleep 30' & echo $! > {}; until test -f {}; do sleep 0.01; done; exit 0",
            marker.display(), pidfile.display(), marker.display()
        );

        let _guard = EscapedProcessGuard { pidfile: pidfile.clone() };

        let started = Instant::now();
        let out = RealRunner.run(&Cmd::new("sh").args(["-c", &script])).unwrap();
        let elapsed = started.elapsed();

        assert!(out.success(), "command should exit successfully");
        // Proof that a reader was held: elapsed should be ~4s (waiting for escaped perl to be killed by recv_timeout)
        assert!(
            elapsed >= Duration::from_secs(4),
            "runner should have waited for reader thread timeout; took {:?}",
            elapsed
        );
        // Upper bound: shared deadline is 5s, plus some overhead
        assert!(
            elapsed < Duration::from_secs(8),
            "runner should not block beyond shared deadline + overhead (took {:?})",
            elapsed
        );
    }

    #[test]
    fn fake_runner_matches_by_substring_once() {
        let fake = FakeRunner::new();
        fake.on("rev-parse", Output::ok("abc\n"));
        let out = fake.run(&Cmd::new("git").args(["rev-parse", "HEAD"])).unwrap();
        assert_eq!(out.stdout, "abc\n");
        assert!(fake.run(&Cmd::new("git").args(["rev-parse", "HEAD"])).is_err());
        assert_eq!(fake.lines(), vec!["git rev-parse HEAD", "git rev-parse HEAD"]);
    }
}
