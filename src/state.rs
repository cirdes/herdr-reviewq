use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewsTodaySuccess {
    pub day: chrono::NaiveDate,
    pub count: u32,
    pub per_repo: BTreeMap<String, u32>,
    pub as_of: DateTime<Utc>,
    pub viewer: String,
    pub repos: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewsTodayAttempt {
    pub at: DateTime<Utc>,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub failed_repos: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewsToday {
    #[serde(default)]
    pub last_success: Option<ReviewsTodaySuccess>,
    #[serde(default)]
    pub last_attempt: Option<ReviewsTodayAttempt>,
    #[serde(default)]
    pub backoff_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failures: u32,
    /// Último dia (SP) em que a virada do dia já forçou uma tentativa imediata.
    #[serde(default)]
    pub last_day_forced: Option<chrono::NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrKey {
    pub repo: String,
    pub number: u64,
}

impl PrKey {
    pub fn new(repo: &str, number: u64) -> Self {
        Self { repo: repo.into(), number }
    }
    pub fn parse(s: &str) -> Option<Self> {
        let (repo, n) = s.rsplit_once('#')?;
        Some(Self::new(repo, n.parse().ok()?))
    }
}

impl fmt::Display for PrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Remote {
    Pending,
    NotPending,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ownership {
    Managed,
    Adopted { reason: String, at: DateTime<Utc> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    Creating,
    Preparing,
    Ready,
    Failed { step: String, reason: String },
    Removing,
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrRecord {
    pub key: PrKey,
    pub title: String,
    pub author: String,
    pub url: String,
    pub head_ref: String,
    /// Branch alvo do PR segundo o GitHub (vazio em estado de antes deste campo).
    #[serde(default)]
    pub base_ref: String,
    /// Base que o daemon já buscou e gravou como escolha do reviewr no worktree.
    #[serde(default)]
    pub synced_base: Option<String>,
    pub remote: Remote,
    pub ownership: Ownership,
    pub phase: Phase,
    pub path: PathBuf,
    #[serde(default)]
    pub created_branch: bool,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// SHA do head do PR segundo o GitHub.
    pub observed_head_sha: String,
    /// SHA que o daemon colocou no worktree.
    #[serde(default)]
    pub managed_sha: Option<String>,
    /// SHA para o qual o setup terminou com sucesso.
    #[serde(default)]
    pub prepared_sha: Option<String>,
    #[serde(default)]
    pub generation: u64,
    pub first_seen_at: DateTime<Utc>,
    #[serde(default)]
    pub last_request_event_at: Option<DateTime<Utc>>,
    /// Notificação de "pronto" ainda não entregue.
    #[serde(default)]
    pub notify_pending: bool,
    #[serde(default)]
    pub warning: Option<String>,
    /// Desde quando o PR não está mais pendente (carência antes da remoção).
    #[serde(default)]
    pub not_pending_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suppression {
    /// Suprimido até surgir um ReviewRequestedEvent mais novo que isto.
    #[serde(default)]
    pub after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoStatus {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub forks: Vec<u64>,
    #[serde(default)]
    pub last_sync: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestResult {
    pub id: String,
    pub kind: String,
    pub pr: Option<String>,
    pub ok: bool,
    pub message: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub at: DateTime<Utc>,
    pub message: String,
}

/// Grupo de processos do setup em andamento, para encerrar órfãos depois de uma queda.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupProcess {
    pub key: PrKey,
    pub pgid: i32,
    /// Hora de início do líder segundo `ps -o lstart=`; protege contra pid reutilizado.
    pub started: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub prs: BTreeMap<String, PrRecord>,
    #[serde(default)]
    pub suppressions: BTreeMap<String, Suppression>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoStatus>,
    #[serde(default)]
    pub last_requests: Vec<RequestResult>,
    #[serde(default)]
    pub alerts: Vec<Alert>,
    #[serde(default)]
    pub setup_process: Option<SetupProcess>,
    #[serde(default)]
    pub auth_error_notified: bool,
    #[serde(default)]
    pub reviews_today: ReviewsToday,
}

fn cap<T>(v: &mut Vec<T>, max: usize) {
    if v.len() > max {
        let extra = v.len() - max;
        v.drain(0..extra);
    }
}

impl State {
    pub fn get(&self, key: &PrKey) -> Option<&PrRecord> {
        self.prs.get(&key.to_string())
    }
    pub fn get_mut(&mut self, key: &PrKey) -> Option<&mut PrRecord> {
        self.prs.get_mut(&key.to_string())
    }
    pub fn insert(&mut self, rec: PrRecord) {
        self.prs.insert(rec.key.to_string(), rec);
    }
    pub fn remove(&mut self, key: &PrKey) -> Option<PrRecord> {
        self.prs.remove(&key.to_string())
    }
    pub fn push_request_result(&mut self, r: RequestResult) {
        self.last_requests.push(r);
        cap(&mut self.last_requests, 20);
    }
    pub fn request_seen(&self, id: &str) -> bool {
        self.last_requests.iter().any(|r| r.id == id)
    }
    pub fn push_alert(&mut self, message: String) {
        self.alerts.push(Alert { at: Utc::now(), message });
        cap(&mut self.alerts, 20);
    }

    /// Carrega o estado. Se o arquivo estiver corrompido, isola-o e começa vazio.
    pub fn load(path: &Path) -> Result<(State, Option<PathBuf>)> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((State::default(), None)),
            Err(e) => return Err(e).context("falha ao ler state.json"),
        };
        match serde_json::from_str(&text) {
            Ok(s) => Ok((s, None)),
            Err(_) => {
                let bak = path.with_extension(format!("json.corrupt-{}", Utc::now().format("%Y%m%d%H%M%S")));
                std::fs::rename(path, &bak).context("falha ao isolar state.json corrompido")?;
                Ok((State::default(), Some(bak)))
            }
        }
    }

    /// Leitura sem efeitos colaterais (para `status` e para o TUI).
    pub fn read(path: &Path) -> Result<State> {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).context("state.json inválido"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).context("falha ao ler state.json"),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("caminho de estado sem diretório")?;
        std::fs::create_dir_all(dir).context("falha ao criar o diretório de estado")?;
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp).context("falha ao salvar state.json")?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).context("falha ao salvar state.json")?;
        Ok(())
    }
}

#[cfg(test)]
impl PrRecord {
    pub fn fixture(repo: &str, number: u64, head_ref: &str, sha: &str) -> Self {
        Self {
            key: PrKey::new(repo, number),
            title: format!("PR {number}"),
            author: "ana".into(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            head_ref: head_ref.into(),
            base_ref: "main".into(),
            synced_base: Some("main".into()),
            remote: Remote::Pending,
            ownership: Ownership::Managed,
            phase: Phase::Ready,
            path: PathBuf::from(format!("/tmp/reviewq-test/pr-{number}")),
            created_branch: true,
            workspace_id: Some("w9".into()),
            observed_head_sha: sha.into(),
            managed_sha: Some(sha.into()),
            prepared_sha: Some(sha.into()),
            generation: 0,
            first_seen_at: DateTime::<Utc>::UNIX_EPOCH,
            last_request_event_at: None,
            notify_pending: false,
            warning: None,
            not_pending_since: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let k = PrKey::new("acme/app", 42);
        assert_eq!(k.to_string(), "acme/app#42");
        assert_eq!(PrKey::parse("acme/app#42"), Some(k));
        assert_eq!(PrKey::parse("sem-numero"), None);
        assert_eq!(PrKey::parse("a/b#x"), None);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::default();
        s.insert(PrRecord::fixture("o/r", 1, "feat/x", "abc"));
        s.get_mut(&PrKey::new("o/r", 1)).unwrap().ownership =
            Ownership::Adopted { reason: "arquivos novos: a".into(), at: Utc::now() };
        s.setup_process = Some(SetupProcess { key: PrKey::new("o/r", 1), pgid: 123, started: "Wed Oct  7 10:00:00 2026".into() });
        s.push_alert("branch preservada".into());
        s.save(&path).unwrap();
        let (loaded, corrupt) = State::load(&path).unwrap();
        assert_eq!(loaded, s);
        assert!(corrupt.is_none());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn missing_file_is_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let (s, corrupt) = State::load(&dir.path().join("nao-existe.json")).unwrap();
        assert_eq!(s, State::default());
        assert!(corrupt.is_none());
    }

    #[test]
    fn corrupt_file_is_isolated_and_state_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ isso não é json").unwrap();
        let (s, corrupt) = State::load(&path).unwrap();
        assert_eq!(s, State::default());
        let bak = corrupt.expect("deveria isolar o arquivo");
        assert!(bak.exists());
        assert!(!path.exists());
        assert!(State::read(&bak).is_err());
    }

    #[test]
    fn old_record_without_optional_fields_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"prs":{"o/r#3":{
            "key":{"repo":"o/r","number":3},"title":"t","author":"a","url":"u","head_ref":"h",
            "remote":"pending","ownership":{"kind":"managed"},"phase":{"kind":"ready"},
            "path":"/w/pr-3","observed_head_sha":"s","first_seen_at":"2026-10-07T10:00:00Z",
            "baseline":"campo-antigo-ignorado"}}}"#).unwrap();
        let (s, corrupt) = State::load(&path).unwrap();
        assert!(corrupt.is_none());
        let rec = s.get(&PrKey::new("o/r", 3)).unwrap();
        assert!(!rec.created_branch);
        assert_eq!(rec.generation, 0);
        assert!(!rec.notify_pending);
    }

    #[test]
    fn histories_are_capped_at_20() {
        let mut s = State::default();
        for i in 0..25 {
            s.push_request_result(RequestResult {
                id: i.to_string(), kind: "sync".into(), pr: None, ok: true, message: String::new(), at: Utc::now(),
            });
            s.push_alert(format!("alerta {i}"));
        }
        assert_eq!(s.last_requests.len(), 20);
        assert_eq!(s.alerts.len(), 20);
        assert!(s.request_seen("24"));
        assert!(!s.request_seen("0"));
        assert_eq!(s.alerts.last().unwrap().message, "alerta 24");
    }

    #[test]
    fn reviews_today_roundtrip_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::default();
        assert!(s.reviews_today.last_success.is_none());
        s.reviews_today.last_success = Some(ReviewsTodaySuccess {
            day: chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            count: 3,
            per_repo: BTreeMap::from([("o/r".to_string(), 3)]),
            as_of: Utc::now(),
            viewer: "cirdes".into(),
            repos: vec!["o/r".into()],
        });
        s.reviews_today.last_attempt = Some(ReviewsTodayAttempt { at: Utc::now(), ok: false, error: Some("x".into()), failed_repos: vec!["o/r".into()] });
        s.save(&path).unwrap();
        assert_eq!(State::read(&path).unwrap(), s);
        // estado antigo sem o campo carrega
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(State::read(&path).unwrap().reviews_today, ReviewsToday::default());
    }
}
