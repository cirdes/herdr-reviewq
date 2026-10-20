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
    /// CI do head remoto do PR (metadado; nunca afeta o ciclo de reviews).
    #[serde(default)]
    pub ci: Option<crate::ci::Ci>,
    /// Primeira coleta em que um agente (Claude/Codex) foi visto neste worktree.
    #[serde(default)]
    pub work_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub work_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MyPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub draft: bool,
    pub created_at: DateTime<Utc>,
    pub head_sha: String,
    #[serde(default)]
    pub ci: Option<crate::ci::Ci>,
}

/// Último resultado bom de meus PRs num repo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MyPrsRepo {
    pub prs: Vec<MyPr>,
    pub opened_24h: u64,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MyPrs {
    /// Conta dos dados abaixo; outra conta zera tudo antes de aplicar.
    #[serde(default)]
    pub viewer: Option<String>,
    #[serde(default)]
    pub repos: BTreeMap<String, MyPrsRepo>,
    /// Erro da última tentativa por repo.
    #[serde(default)]
    pub errors: BTreeMap<String, String>,
}

/// PR aberto do repo, só o que monta pilhas (metadado; nunca afeta o ciclo de reviews).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenPr {
    pub number: u64,
    pub head_ref: String,
    pub base_ref: String,
}

/// Última coleta boa dos PRs abertos de um repo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenPrsRepo {
    pub prs: Vec<OpenPr>,
    /// Branch padrão do repo: PR com essa head (release `main` → `production`) não abre pilha.
    #[serde(default)]
    pub default_branch: Option<String>,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenPrs {
    #[serde(default)]
    pub repos: BTreeMap<String, OpenPrsRepo>,
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
    #[serde(default)]
    pub my_prs: MyPrs,
    #[serde(default)]
    pub open_prs: OpenPrs,
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

impl PrRecord {
    /// Commit contra o qual o CI é conferido. Na fila, o head que a busca de reviews viu (head novo
    /// sem CI novo vira `?`). Fora dela (Saindo/Adotado) `observed_head_sha` para de atualizar, e o
    /// CI coletado, que é sempre do head remoto atual, é o dado mais novo que existe.
    pub fn ci_head(&self) -> &str {
        match (&self.remote, &self.ci) {
            (Remote::Pending, _) | (_, None) => &self.observed_head_sha,
            (_, Some(c)) => &c.sha,
        }
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
            ci: None,
            work_started_at: None,
            work_agent: None,
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

    fn sample_ci() -> crate::ci::Ci {
        crate::ci::Ci {
            sha: "abc".into(),
            state: crate::ci::CiState::Failure,
            failed_check: Some("rspec".into()),
            finished_at: Some("2026-10-13T14:00:00Z".parse().unwrap()),
            observed_at: "2026-10-13T14:01:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn old_state_without_new_fields_loads() {
        let mut s = State::default();
        s.insert(PrRecord::fixture("o/r", 1, "feat/x", "abc"));
        let mut v = serde_json::to_value(&s).unwrap();
        let rec = v["prs"]["o/r#1"].as_object_mut().unwrap();
        for k in ["ci", "work_started_at", "work_agent"] {
            assert!(rec.remove(k).is_some(), "campo {k} deveria existir");
        }
        v.as_object_mut().unwrap().remove("my_prs").expect("my_prs deveria existir");
        v.as_object_mut().unwrap().remove("open_prs").expect("open_prs deveria existir");
        let loaded: State = serde_json::from_value(v).unwrap();
        let r = loaded.get(&PrKey::new("o/r", 1)).unwrap();
        assert_eq!((r.ci.clone(), r.work_started_at, r.work_agent.clone()), (None, None, None));
        assert_eq!(loaded.my_prs, MyPrs::default());
        assert_eq!(loaded.open_prs, OpenPrs::default());
    }

    #[test]
    fn new_fields_roundtrip() {
        let mut s = State::default();
        let mut rec = PrRecord::fixture("o/r", 1, "feat/x", "abc");
        rec.ci = Some(sample_ci());
        rec.work_started_at = Some("2026-10-13T14:32:00Z".parse().unwrap());
        rec.work_agent = Some("claude".into());
        s.insert(rec);
        s.my_prs.viewer = Some("cirdes".into());
        s.my_prs.errors.insert("o/x".into(), "boom".into());
        s.my_prs.repos.insert(
            "o/r".into(),
            MyPrsRepo {
                prs: vec![MyPr {
                    number: 9,
                    title: "Meu".into(),
                    url: "https://github.com/o/r/pull/9".into(),
                    draft: true,
                    created_at: "2026-10-13T10:00:00Z".parse().unwrap(),
                    head_sha: "abc".into(),
                    ci: Some(sample_ci()),
                }],
                opened_24h: 2,
                as_of: "2026-10-13T14:00:00Z".parse().unwrap(),
            },
        );
        s.open_prs.repos.insert(
            "o/r".into(),
            OpenPrsRepo {
                prs: vec![OpenPr { number: 9, head_ref: "feat/b".into(), base_ref: "feat/a".into() }],
                default_branch: Some("main".into()),
                as_of: "2026-10-13T14:00:00Z".parse().unwrap(),
            },
        );
        let back: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn unknown_ci_state_does_not_isolate_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::default();
        let mut rec = PrRecord::fixture("o/r", 1, "feat/x", "abc");
        rec.ci = Some(sample_ci());
        s.insert(rec);
        let text = serde_json::to_string(&s).unwrap().replace(r#""state":"failure""#, r#""state":"novo""#);
        std::fs::write(&path, text).unwrap();
        let (loaded, corrupt) = State::load(&path).unwrap();
        assert!(corrupt.is_none());
        assert_eq!(loaded.get(&PrKey::new("o/r", 1)).unwrap().ci.as_ref().unwrap().state, crate::ci::CiState::Unknown);
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
