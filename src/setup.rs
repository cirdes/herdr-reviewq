use crate::runner::{kill_group, terminate_group};
use crate::state::PrKey;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub type Wrap = fn(&str) -> Vec<String>;

/// `mise trust`/`mise install` rodam direto; o resto roda dentro do ambiente do mise.
pub fn mise_wrap(step: &str) -> Vec<String> {
    if step.trim_start().starts_with("mise ") {
        vec!["sh".into(), "-c".into(), step.into()]
    } else {
        vec!["mise".into(), "exec".into(), "--".into(), "sh".into(), "-c".into(), step.into()]
    }
}

const TOKEN_PREFIXES: [&str; 12] =
    ["ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "npm_", "AKIA", "ASIA", "sk-"];
const SENSITIVE_KEYS: [&str; 7] = ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "KEY", "CREDENTIAL", "AUTH"];
const SENSITIVE_FLAGS: [&str; 4] = ["password", "passwd", "token", "secret"];

fn has_token_prefix(w: &str) -> bool {
    TOKEN_PREFIXES.iter().any(|p| {
        w.match_indices(p)
            .any(|(i, _)| i == 0 || !w[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric()))
    })
}

fn is_sensitive_key(k: &str) -> bool {
    let ku = k.trim_matches(|c| matches!(c, '"' | '\'' | '{' | ',')).to_uppercase();
    !ku.is_empty() && SENSITIVE_KEYS.iter().any(|s| ku.contains(s))
}

/// Redige uma palavra; devolve também se a próxima palavra deve ser mascarada.
fn redact_word(w: &str) -> (String, bool) {
    let lw = w.to_lowercase();
    if lw == "bearer" || lw == "basic" {
        return (w.to_string(), true);
    }
    if let Some(flag) = w.strip_prefix("--") {
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (flag, None),
        };
        if SENSITIVE_FLAGS.iter().any(|s| name.to_lowercase().contains(s)) {
            return match value {
                Some(_) => (format!("--{name}=***"), false),
                None => (w.to_string(), true),
            };
        }
    }
    if let Some(pos) = w.find("://") {
        let rest = &w[pos + 3..];
        if let Some(at) = rest.find('@') {
            if !rest[..at].contains('/') {
                return (format!("{}://***@{}", &w[..pos], &rest[at + 1..]), false);
            }
        }
    }
    if has_token_prefix(w) {
        return ("***".into(), false);
    }
    if let Some((k, v)) = w.split_once('=') {
        let ku = k.to_uppercase();
        let sensitive = SENSITIVE_KEYS.iter().any(|s| ku.contains(s)) || (ku.starts_with("BUNDLE_") && v.contains(':'));
        if sensitive {
            return if v.is_empty() { (w.to_string(), true) } else { (format!("{k}=***"), false) };
        }
    }
    if let Some((k, v)) = w.split_once(':') {
        if is_sensitive_key(k) {
            return if v.is_empty() { (w.to_string(), true) } else { (format!("{k}:***"), false) };
        }
    }
    (w.to_string(), false)
}

/// Mascara credenciais em URL, tokens conhecidos, Bearer/Basic, flags, JSON/YAML e `CHAVE=valor` sensíveis.
pub fn redact(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    let mut prev_key = false;
    for w in line.split(' ') {
        if w.is_empty() {
            out.push(String::new());
            continue;
        }
        if mask_next && !matches!(w.to_lowercase().as_str(), "bearer" | "basic") {
            out.push("***".into());
            mask_next = false;
            prev_key = false;
            continue;
        }
        if w == "=" && prev_key {
            out.push(w.into());
            mask_next = true;
            prev_key = false;
            continue;
        }
        prev_key = !w.contains(['=', ':']) && is_sensitive_key(w);
        let (r, next) = redact_word(w);
        out.push(r);
        mask_next = next;
    }
    out.join(" ")
}

fn copy_redacted<R: Read + Send + 'static>(reader: R, mut log: File) {
    std::thread::spawn(move || {
        for chunk in BufReader::new(reader).split(b'\n') {
            let Ok(bytes) = chunk else { break };
            let line = format!("{}\n", redact(&String::from_utf8_lossy(&bytes)));
            let _ = log.write_all(line.as_bytes());
        }
    });
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Succeeded,
    Failed { step: String, reason: String },
}

struct Running {
    step: String,
    child: Child,
    started: Instant,
}

pub struct SetupJob {
    pub key: PrKey,
    pub generation: u64,
    pub target_sha: String,
    steps: Vec<String>,
    next: usize,
    current: Option<Running>,
    finished: Option<JobStatus>,
    cwd: PathBuf,
    log_path: PathBuf,
    timeout: Duration,
    wrap: Wrap,
}

impl SetupJob {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        key: PrKey,
        generation: u64,
        target_sha: &str,
        steps: &[String],
        cwd: &Path,
        log_path: &Path,
        timeout: Duration,
        wrap: Wrap,
    ) -> Result<Self> {
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(log_path)
            .context("não consegui criar o log do setup")?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            key,
            generation,
            target_sha: target_sha.into(),
            steps: steps.to_vec(),
            next: 0,
            current: None,
            finished: None,
            cwd: cwd.into(),
            log_path: log_path.into(),
            timeout,
            wrap,
        })
    }

    pub fn current_pid(&self) -> Option<u32> {
        self.current.as_ref().map(|r| r.child.id())
    }

    /// Avança o setup sem bloquear: inicia o próximo passo quando o atual termina.
    pub fn poll(&mut self) -> JobStatus {
        if let Some(done) = &self.finished {
            return done.clone();
        }
        let st = self.advance();
        if st != JobStatus::Running {
            self.next = self.steps.len();
            self.finished = Some(st.clone());
        }
        st
    }

    fn advance(&mut self) -> JobStatus {
        loop {
            if let Some(run) = self.current.as_mut() {
                match run.child.try_wait() {
                    Ok(None) if run.started.elapsed() < self.timeout => return JobStatus::Running,
                    Ok(None) => {
                        terminate_group(&mut run.child);
                        let step = run.step.clone();
                        self.current = None;
                        return JobStatus::Failed { step, reason: format!("excedeu {}s", self.timeout.as_secs()) };
                    }
                    Ok(Some(st)) => {
                        kill_group(run.child.id()); // sobras em background não continuam escrevendo
                        let step = run.step.clone();
                        self.current = None;
                        if !st.success() {
                            let code = st.code().map_or("sinal".to_string(), |c| c.to_string());
                            return JobStatus::Failed {
                                step,
                                reason: format!("saiu com {code}; veja {}", self.log_path.display()),
                            };
                        }
                    }
                    Err(e) => {
                        let step = run.step.clone();
                        self.current = None;
                        return JobStatus::Failed { step, reason: e.to_string() };
                    }
                }
            }
            if self.next >= self.steps.len() {
                return JobStatus::Succeeded;
            }
            let step = self.steps[self.next].clone();
            self.next += 1;
            if let Err(e) = self.spawn(&step) {
                return JobStatus::Failed { step: redact(&step), reason: format!("{e:#}") };
            }
        }
    }

    fn spawn(&mut self, step: &str) -> Result<()> {
        let mut header = OpenOptions::new().append(true).open(&self.log_path)?;
        writeln!(header, "\n$ {}", redact(step))?;
        let argv = (self.wrap)(step);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .with_context(|| format!("falha ao iniciar: {}", redact(step)))?;
        copy_redacted(child.stdout.take().expect("stdout piped"), OpenOptions::new().append(true).open(&self.log_path)?);
        copy_redacted(child.stderr.take().expect("stderr piped"), OpenOptions::new().append(true).open(&self.log_path)?);
        self.current = Some(Running { step: redact(step), child, started: Instant::now() });
        Ok(())
    }

    pub fn cancel(&mut self) {
        if let Some(run) = self.current.as_mut() {
            terminate_group(&mut run.child);
        }
        self.current = None;
    }
}

impl Drop for SetupJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(step: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), step.into()]
    }

    fn run_to_end(job: &mut SetupJob) -> JobStatus {
        let start = Instant::now();
        loop {
            let st = job.poll();
            if st != JobStatus::Running {
                std::thread::sleep(Duration::from_millis(150)); // deixa as threads de log terminarem
                return st;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "setup não terminou");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn job(steps: &[&str], timeout: Duration) -> (tempfile::TempDir, SetupJob) {
        let dir = tempfile::tempdir().unwrap();
        let steps: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
        let j = SetupJob::start(PrKey::new("o/r", 1), 3, "abc", &steps, dir.path(), &dir.path().join("logs/s.log"), timeout, sh).unwrap();
        (dir, j)
    }

    fn log(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("logs/s.log")).unwrap()
    }

    fn read_pid(path: &Path) -> i32 {
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

    #[test]
    fn runs_all_steps_in_order() {
        let (dir, mut j) = job(&["echo um > a.txt", "echo dois >> a.txt"], Duration::from_secs(5));
        assert_eq!(run_to_end(&mut j), JobStatus::Succeeded);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "um\ndois\n");
        assert!(log(&dir).contains("$ echo um > a.txt"));
        assert_eq!((j.generation, j.target_sha.as_str()), (3, "abc"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("logs/s.log")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn stops_at_first_failure_without_copying_output_to_reason() {
        let (dir, mut j) = job(&["true", "echo segredo-na-saida; exit 3", "touch nunca"], Duration::from_secs(5));
        match run_to_end(&mut j) {
            JobStatus::Failed { step, reason } => {
                assert_eq!(step, "echo segredo-na-saida; exit 3");
                assert!(reason.contains("3") && reason.contains("s.log"));
                assert!(!reason.contains("segredo-na-saida"));
            }
            other => panic!("esperava falha, veio {other:?}"),
        }
        assert!(!dir.path().join("nunca").exists());
        assert!(log(&dir).contains("segredo-na-saida"));
        let again = j.poll();
        assert!(matches!(&again, JobStatus::Failed { step, .. } if step == "echo segredo-na-saida; exit 3"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!dir.path().join("nunca").exists());
    }

    #[test]
    fn step_timeout_fails() {
        let (_dir, mut j) = job(&["sleep 5"], Duration::from_millis(200));
        assert!(matches!(run_to_end(&mut j), JobStatus::Failed { reason, .. } if reason.contains("excedeu")));
    }

    #[test]
    fn cancel_kills_grandchildren() {
        let (dir, mut j) = job(&["sleep 30 & echo $! > pid; wait"], Duration::from_secs(60));
        assert_eq!(j.poll(), JobStatus::Running);
        assert!(j.current_pid().is_some());
        let pid = read_pid(&dir.path().join("pid"));
        j.cancel();
        std::thread::sleep(Duration::from_millis(300));
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "neto do setup ainda vivo");
    }

    #[test]
    fn leftovers_are_killed_when_step_ends() {
        let (dir, mut j) = job(&["sleep 30 & echo $! > pid"], Duration::from_secs(60));
        assert_eq!(run_to_end(&mut j), JobStatus::Succeeded);
        let pid = read_pid(&dir.path().join("pid"));
        std::thread::sleep(Duration::from_millis(300));
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "sobra do passo ainda viva");
    }

    #[test]
    fn credentials_never_reach_the_log() {
        let (dir, mut j) = job(
            &["echo 'fetching https://user:s3cr3t@gems.example.com/x'; echo GITHUB_TOKEN=abc123 >&2; echo BUNDLE_GEMS__AVO__APP=u:p4ss"],
            Duration::from_secs(5),
        );
        assert_eq!(run_to_end(&mut j), JobStatus::Succeeded);
        let text = log(&dir);
        for secret in ["s3cr3t", "abc123", "p4ss"] {
            assert!(!text.contains(secret), "{secret} vazou: {text}");
        }
        assert!(text.contains("gems.example.com"));
    }

    #[test]
    fn redact_rules() {
        assert_eq!(redact("url https://u:p@host/x ok"), "url https://***@host/x ok");
        assert_eq!(redact("token ghp_abcdef"), "token ***");
        assert_eq!(redact("API_KEY=xyz"), "API_KEY=***");
        assert_eq!(redact("BUNDLE_GEMS__X=user:pass"), "BUNDLE_GEMS__X=***");
        assert_eq!(redact("Authorization: Bearer abc.def"), "Authorization: Bearer ***");
        assert_eq!(redact("curl -H basic dXNlcjpwdw=="), "curl -H basic ***");
        assert_eq!(redact("run --password hunter2 now"), "run --password *** now");
        assert_eq!(redact("run --api-token=hunter2"), "run --api-token=***");
        assert!(!redact(r#"{"password": "hunter2"}"#).contains("hunter2"));
        assert!(!redact(r#"{"password":"hunter2"}"#).contains("hunter2"));
        assert_eq!(redact("secret: hunter2"), "secret: ***");
        assert_eq!(redact("API_KEY = hunter2"), "API_KEY = ***");
        for t in ["npm_abc123", "AKIAABCDEF123", "ASIAABCDEF123", "sk-abc123"] {
            assert_eq!(redact(&format!("v {t}")), "v ***");
        }
        assert_eq!(redact("task-1 done"), "task-1 done");
        assert_eq!(redact("Installing rails 7.1"), "Installing rails 7.1");
        assert_eq!(redact("see https://github.com/a/b"), "see https://github.com/a/b");
    }

    #[test]
    fn mise_wrap_rules() {
        assert_eq!(mise_wrap("mise trust"), vec!["sh", "-c", "mise trust"]);
        assert_eq!(mise_wrap("bundle install"), vec!["mise", "exec", "--", "sh", "-c", "bundle install"]);
    }
}
