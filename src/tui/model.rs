use crate::requests::RequestKind;
use crate::state::{Ownership, Phase, PrKey, PrRecord, Remote, State};
use crate::status::phase_label;
use crate::today::{ago, local_hm, opened_24h_label, start_label, valid_success};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct ViewConfig {
    pub repos: Vec<String>,
    pub poll_interval_secs: u64,
    pub remove_grace: chrono::Duration,
}

/// Fatos do disco que o estado não carrega (calculados no loop do TUI).
#[derive(Debug, Clone, Default)]
pub struct UiFacts {
    /// PRs cujo worktree (`path`) não existe mais.
    pub missing_worktrees: BTreeSet<PrKey>,
}

/// Identidade de uma linha: o mesmo PR pode aparecer como review e como meu PR.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowId {
    Review(PrKey),
    Mine(PrKey),
}

impl RowId {
    pub fn key(&self) -> &PrKey {
        match self {
            RowId::Review(k) | RowId::Mine(k) => k,
        }
    }

    /// Só linhas de review aceitam pedidos ao daemon, foco e log.
    pub fn review(&self) -> Option<&PrKey> {
        match self {
            RowId::Review(k) => Some(k),
            RowId::Mine(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Started,
    Pending,
    Mine,
    Adopted,
    NoSync,
    Leaving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Eligibility {
    pub retry: bool,
    pub adopt: bool,
    pub release: bool,
    /// Focar o workspace do worktree.
    pub focus: bool,
    pub log: bool,
}

/// Célula de Início sem início registrado.
const NO_START: &str = "–";

#[derive(Debug, Clone)]
pub struct Row {
    pub id: RowId,
    /// "#n" com um repo configurado; "repo#n" (sem owner, salvo nome ambíguo) com mais de um.
    pub label: String,
    pub section: Section,
    pub author: String,
    pub title: String,
    pub icon: char,
    pub status: String,
    /// Coluna CI (vazia sem dado).
    pub ci: String,
    /// Coluna Início: `HH:MM` de hoje ou `dd/mm` (São Paulo); `–` sem início, para a célula
    /// vazia não parecer falha.
    pub started: String,
    /// Coluna Empilhado: "Não", ou "#raiz i/n" com a posição na pilha; vazia sem dado.
    pub stack: String,
    pub url: String,
    pub eligible: Eligibility,
    /// Texto normalizado (`fold`) onde a busca procura: `repo#n`, `#n`, autor, título e branch,
    /// separados por `FIELD` para um termo nunca casar letras de campos diferentes.
    pub search: String,
}

#[derive(Debug, Clone, Default)]
pub struct Header {
    pub sync: String,
    pub stale: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct View {
    pub rows: Vec<Row>,
    pub pending: String,
    pub today: String,
    pub today_detail: String,
    pub opened_24h: String,
    pub header: Header,
    pub alerts: Vec<String>,
    pub request_line: Option<String>,
    pub empty_reason: Option<String>,
    /// Com filtro ativo: (linhas que casam, total de linhas sem filtro).
    pub filtered: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct Sent {
    pub id: String,
    pub kind: RequestKind,
    pub pr: Option<PrKey>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub selected: Option<RowId>,
    /// Ordem das linhas da última visão (para `reselect`).
    pub order: Vec<RowId>,
    /// Confirmação pendente (tecla `y`/`n`).
    pub confirm: Option<(RequestKind, PrKey)>,
    pub sent: Vec<Sent>,
    /// Resultados e avisos; independente de `confirm`.
    pub message: Option<String>,
    pub scroll: usize,
    /// Texto da busca (`/`); vazio é sem filtro. Sobrevive às releituras do estado até `esc`.
    pub filter: String,
    /// Digitando a busca: as teclas vão para `filter` em vez de virar ações.
    pub typing: bool,
}

impl UiState {
    pub fn selected_row<'v>(&self, view: &'v View) -> Option<&'v Row> {
        self.selected.as_ref().and_then(|k| view.row(k)).or_else(|| view.rows.first())
    }
}

/// Escolhe a seleção após uma nova visão: a mesma linha se ainda existe; senão o próximo
/// sobrevivente da ordem anterior, ou o anterior se era o último; senão a primeira linha.
pub fn reselect(prev_order: &[RowId], selected: Option<&RowId>, view: &View) -> Option<RowId> {
    let first = || view.rows.first().map(|r| r.id.clone());
    let Some(sel) = selected else { return first() };
    if view.row(sel).is_some() {
        return Some(sel.clone());
    }
    let Some(idx) = prev_order.iter().position(|k| k == sel) else { return first() };
    let alive = |k: &&RowId| view.row(k).is_some();
    prev_order[idx + 1..]
        .iter()
        .find(alive)
        .or_else(|| prev_order[..idx].iter().rev().find(alive))
        .cloned()
        .or_else(first)
}

impl View {
    pub fn row(&self, id: &RowId) -> Option<&Row> {
        self.rows.iter().find(|r| &r.id == id)
    }

    /// Ordem atual das linhas, para guardar em `UiState.order`.
    pub fn order(&self) -> Vec<RowId> {
        self.rows.iter().map(|r| r.id.clone()).collect()
    }

    pub fn build(state: Option<&State>, ui: &UiState, cfg: &ViewConfig, facts: &UiFacts, now: DateTime<Utc>) -> View {
        let Some(state) = state else {
            return View {
                pending: "–".into(),
                today: "–".into(),
                opened_24h: "–".into(),
                empty_reason: Some("sem snapshot ainda — o daemon não gravou o estado".into()),
                header: Header { sync: "nunca".into(), stale: true, errors: vec![] },
                ..Default::default()
            };
        };
        let stacks = crate::stacks::labels(state);
        // Meus PRs não guardam a branch; ela vem dos PRs abertos que o daemon coleta para as pilhas.
        let open_branches: std::collections::HashMap<PrKey, String> = state
            .open_prs
            .repos
            .iter()
            .flat_map(|(repo, o)| o.prs.iter().map(move |p| (PrKey::new(repo, p.number), p.head_ref.clone())))
            .collect();
        // (seção, fase, desempate, linha): Iniciados pelo início mais recente, Meus PRs pela criação
        // mais recente, as demais pela ordem de chegada
        let mut rows: Vec<(Section, u8, i64, Row)> = state
            .prs
            .values()
            .map(|r| {
                let section = section_of(r);
                let (rank, tie) = match (section, r.work_started_at) {
                    (Section::Started, Some(t)) => (0, -t.timestamp()),
                    _ => (phase_rank(&r.phase), r.first_seen_at.timestamp()),
                };
                let stack = stacks.get(&r.key).cloned().unwrap_or_default();
                (section, rank, tie, row_of(r, section, stack, state, cfg, facts, now))
            })
            .collect();
        for repo in &cfg.repos {
            for p in state.my_prs.repos.get(repo).map(|m| m.prs.as_slice()).unwrap_or_default() {
                let stack = stacks.get(&PrKey::new(repo, p.number)).cloned().unwrap_or_default();
                let branch = open_branches.get(&PrKey::new(repo, p.number)).map(String::as_str).unwrap_or_default();
                rows.push((Section::Mine, 0, -p.created_at.timestamp(), mine_row(repo, p, stack, branch, cfg, now)));
            }
        }
        rows.sort_by(|a, b| (a.0, a.1, a.2, &a.3.id).cmp(&(b.0, b.1, b.2, &b.3.id)));
        let total = rows.len();
        let terms = terms(&ui.filter);
        // A linha em confirmação (y/n) fica mesmo sem casar: uma releitura que mude o título não
        // pode cancelar o pedido como se o PR tivesse mudado de estado.
        let confirming = ui.confirm.as_ref().map(|(_, k)| RowId::Review(k.clone()));
        let mut scored: Vec<(i64, (Section, u8, i64, Row))> = rows
            .into_iter()
            .filter_map(|t| {
                let held = confirming.as_ref() == Some(&t.3.id);
                score(&t.3.search, &terms).or(held.then_some(0)).map(|n| (if held { i64::MAX } else { n }, t))
            })
            .collect();
        // Com busca, o que casa melhor sobe dentro da própria seção; empate mantém a ordem de sempre.
        if !terms.is_empty() {
            scored.sort_by_key(|(n, t)| (t.0, std::cmp::Reverse(*n)));
        }
        let rows: Vec<(Section, u8, i64, Row)> = scored.into_iter().map(|(_, t)| t).collect();
        let filtered = (!terms.is_empty()).then_some((rows.len(), total));
        let empty_reason = match filtered {
            Some((0, _)) => Some(format!("nenhum PR casa com \"{}\" (esc limpa a busca)", ui.filter.trim())),
            _ => None,
        };

        let pending_n = state.prs.values().filter(|r| r.remote == Remote::Pending).count();
        let any_unsynced = cfg.repos.iter().any(|name| !state.repos.get(name).is_some_and(|s| s.ok));
        let valid = valid_success(&state.reviews_today, now, &cfg.repos, None);
        View {
            rows: rows.into_iter().map(|t| t.3).collect(),
            pending: format!("{pending_n}{}", if any_unsynced { "?" } else { "" }),
            today: valid.map_or("–".into(), |s| s.count.to_string()),
            today_detail: today_detail(state, valid.map(|s| s.as_of), now),
            opened_24h: opened_24h_label(&state.my_prs, &cfg.repos, now, cfg.poll_interval_secs),
            header: header(state, cfg, now),
            alerts: state.alerts.iter().rev().take(3).map(|a| format!("{}  {}", local_hm(a.at), a.message)).collect(),
            request_line: request_line(state, ui, now),
            empty_reason,
            filtered,
        }
    }
}

fn section_of(r: &PrRecord) -> Section {
    match r.ownership {
        Ownership::Adopted { .. } => Section::Adopted,
        Ownership::Managed if r.remote == Remote::Unknown => Section::NoSync,
        Ownership::Managed if r.phase == Phase::Removing || r.not_pending_since.is_some() || r.remote == Remote::NotPending => {
            Section::Leaving
        }
        Ownership::Managed if r.work_started_at.is_some() => Section::Started,
        Ownership::Managed => Section::Pending,
    }
}

/// "#n" com um repo configurado. Com mais de um, "repo#n" sem o owner (todos os repos costumam
/// ser da mesma org, e o owner sozinho enchia a coluna); se dois repos configurados têm o mesmo
/// nome curto, volta para "owner/repo#n".
fn label(key: &PrKey, cfg: &ViewConfig) -> String {
    if cfg.repos.len() <= 1 {
        return format!("#{}", key.number);
    }
    let short = |r: &str| r.rsplit('/').next().unwrap_or(r).to_string();
    let name = short(&key.repo);
    if cfg.repos.iter().filter(|r| short(r) == name).count() > 1 { key.to_string() } else { format!("{name}#{}", key.number) }
}

fn mine_row(repo: &str, p: &crate::state::MyPr, stack: String, branch: &str, cfg: &ViewConfig, now: DateTime<Utc>) -> Row {
    let key = PrKey::new(repo, p.number);
    Row {
        search: haystack(&key, "você", &p.title, branch),
        label: label(&key, cfg),
        id: RowId::Mine(key),
        section: Section::Mine,
        author: "você".into(),
        title: p.title.clone(),
        icon: crate::ci::icon(p.ci.as_ref()),
        status: if p.draft { "rascunho" } else { "aberto" }.into(),
        ci: crate::ci::cell(p.ci.as_ref(), &p.head_sha, now),
        started: NO_START.into(),
        stack,
        url: p.url.clone(),
        eligible: Eligibility::default(),
    }
}

/// Minúsculas e sem acento, para a busca não depender de como o texto foi digitado.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

/// Separa os campos do texto de busca; não aparece em nenhum campo.
const FIELD: char = '\u{1f}';

fn haystack(key: &PrKey, author: &str, title: &str, branch: &str) -> String {
    let fields = [key.to_string(), format!("#{}", key.number), author.into(), title.into(), branch.into()];
    fold(&fields.join(&FIELD.to_string()))
}

/// Termos da busca separados por espaço; todos precisam casar.
fn terms(filter: &str) -> Vec<String> {
    fold(filter).split_whitespace().map(str::to_string).collect()
}

/// Pontuação da linha para a busca, ou `None` se algum termo não casa. Cada termo vale o melhor
/// campo em que casa; os termos somam.
fn score(search: &str, terms: &[String]) -> Option<i64> {
    terms.iter().map(|t| search.split(FIELD).filter_map(|f| fuzzy(t, f)).max()).sum()
}

/// Casa `term` com `field` de forma aproximada: as letras na ordem, com buracos. Trecho contínuo
/// vale bem mais, e começo de palavra e letras seguidas somam, para `imp` preferir
/// "importação" a um título que só espalha essas letras.
fn fuzzy(term: &str, field: &str) -> Option<i64> {
    let word_start = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    if let Some(at) = field.find(term) {
        let bonus = if word_start(field[..at].chars().next_back()) { 200 } else { 0 };
        return Some(1000 + 10 * term.chars().count() as i64 - (at.min(500) / 10) as i64 + bonus);
    }
    let chars: Vec<char> = field.chars().collect();
    let (mut total, mut from, mut last) = (0, 0, None);
    for c in term.chars() {
        let i = (from..chars.len()).find(|&i| chars[i] == c)?;
        total += if last.is_some_and(|l: usize| l + 1 == i) { 5 } else { 1 };
        if word_start(i.checked_sub(1).map(|j| chars[j])) {
            total += 3;
        }
        last = Some(i);
        from = i + 1;
    }
    Some(total)
}

fn phase_rank(p: &Phase) -> u8 {
    match p {
        Phase::Failed { .. } => 0,
        Phase::Blocked { .. } => 1,
        Phase::Preparing => 2,
        Phase::Creating => 3,
        Phase::Ready => 4,
        Phase::Removing => 5,
    }
}

fn icon(r: &PrRecord) -> char {
    if matches!(r.ownership, Ownership::Adopted { .. }) {
        return '◆';
    }
    match r.phase {
        Phase::Ready => '●',
        Phase::Preparing | Phase::Creating => '◐',
        Phase::Failed { .. } => '✗',
        Phase::Blocked { .. } => '⊘',
        Phase::Removing => '○',
    }
}

fn row_of(r: &PrRecord, section: Section, stack: String, state: &State, cfg: &ViewConfig, facts: &UiFacts, now: DateTime<Utc>) -> Row {
    let managed = r.ownership == Ownership::Managed;
    let status = match (&r.ownership, section) {
        (Ownership::Adopted { reason, .. }, _) => {
            let still = if r.remote == Remote::Pending { " · ainda pendente" } else { "" };
            format!("seu · {reason}{still}")
        }
        (_, Section::NoSync) => {
            let since = state
                .repos
                .get(&r.key.repo)
                .and_then(|s| s.since)
                .map(|t| format!(" desde {}", local_hm(t)))
                .unwrap_or_default();
            let paused = if r.not_pending_since.is_some() { "carência pausada: sem sync · " } else { "" };
            format!("{} · {paused}GitHub sem sync{since}", phase_label(&r.phase))
        }
        (_, Section::Leaving) if r.phase == Phase::Removing => "removendo".into(),
        (_, Section::Leaving) => {
            let left = r.not_pending_since.map(|t| cfg.remove_grace - (now - t)).unwrap_or_else(chrono::Duration::zero);
            let mins = (left.num_seconds().max(0) + 59) / 60;
            format!("{} · sai em {mins}min", phase_label(&r.phase))
        }
        _ => phase_label(&r.phase),
    };
    let missing = facts.missing_worktrees.contains(&r.key);
    Row {
        search: haystack(&r.key, &r.author, &r.title, &r.head_ref),
        id: RowId::Review(r.key.clone()),
        label: label(&r.key, cfg),
        section,
        author: r.author.clone(),
        title: r.title.clone(),
        icon: icon(r),
        status,
        ci: crate::ci::cell(r.ci.as_ref(), r.ci_head(), now),
        started: r.work_started_at.map(|t| start_label(t, now)).unwrap_or_else(|| NO_START.into()),
        stack,
        url: r.url.clone(),
        eligible: Eligibility {
            retry: managed && (matches!(r.phase, Phase::Failed { .. }) || (matches!(r.phase, Phase::Blocked { .. }) && missing)),
            adopt: managed,
            release: !managed,
            focus: !matches!(r.phase, Phase::Removing | Phase::Creating),
            log: true,
        },
    }
}

fn today_detail(state: &State, valid_as_of: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let mut parts = vec![];
    if let Some(as_of) = valid_as_of {
        parts.push(format!("atualizado há {}", ago(now - as_of)));
    }
    if let Some(a) = state.reviews_today.last_attempt.as_ref().filter(|a| !a.ok) {
        parts.push(format!("erro: {} ({})", a.error.as_deref().unwrap_or("?"), a.failed_repos.join(", ")));
    }
    parts.join(" · ")
}

fn header(state: &State, cfg: &ViewConfig, now: DateTime<Utc>) -> Header {
    let mut worst: Option<chrono::Duration> = None;
    let mut never = false;
    let mut errors = vec![];
    for name in &cfg.repos {
        match state.repos.get(name) {
            Some(st) => {
                match st.last_sync {
                    Some(t) => worst = Some(worst.map_or(now - t, |w: chrono::Duration| w.max(now - t))),
                    None => never = true,
                }
                if !st.ok {
                    let since = st.since.map(|t| format!(" desde {}", local_hm(t))).unwrap_or_default();
                    errors.push(format!("{name}: {}{since}", st.error.as_deref().unwrap_or("erro")));
                }
            }
            None => {
                never = true;
                errors.push(format!("{name}: ainda não sincronizado"));
            }
        }
    }
    for name in &cfg.repos {
        if let Some(e) = state.my_prs.errors.get(name) {
            errors.push(format!("meus PRs · {name}: {e}"));
        }
    }
    let limit = chrono::Duration::seconds(3 * cfg.poll_interval_secs as i64);
    match (never, worst) {
        (true, _) | (false, None) => Header { sync: "sync nunca · atrasado".into(), stale: true, errors },
        (false, Some(w)) => {
            let stale = w > limit;
            let late = if stale { " · atrasado" } else { "" };
            Header { sync: format!("sync {}{late}", ago(w)), stale, errors }
        }
    }
}

/// Acima do pior tick do daemon (~105 s: contador até 90 s + `ensure` até 15 s).
const NO_REPLY_SECS: i64 = 150;

fn request_line(state: &State, ui: &UiState, now: DateTime<Utc>) -> Option<String> {
    let sent = ui.sent.last()?;
    let what = match &sent.pr {
        Some(k) => format!("pedido {} {k}", kind_label(sent.kind)),
        None => format!("pedido {}", kind_label(sent.kind)),
    };
    if let Some(res) = state.last_requests.iter().find(|r| r.id == sent.id) {
        let verdict = if res.ok { "ok" } else { "falhou" };
        return Some(format!("{what}: {verdict} — {}", res.message));
    }
    if now - sent.at > chrono::Duration::seconds(NO_REPLY_SECS) {
        Some(format!("{what}: sem resposta do daemon"))
    } else {
        Some(format!("{what}: enviado…"))
    }
}

pub fn kind_label(k: RequestKind) -> &'static str {
    match k {
        RequestKind::Sync => "sync",
        RequestKind::Retry => "tentar de novo",
        RequestKind::Release => "liberar",
        RequestKind::Adopt => "adotar",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Ownership, Phase, PrRecord, Remote, RepoStatus, RequestResult, State};

    fn cfg() -> ViewConfig {
        ViewConfig { repos: vec!["o/r".into()], poll_interval_secs: 60, remove_grace: chrono::Duration::minutes(15) }
    }

    fn build(s: Option<&State>, ui: &UiState, c: &ViewConfig, now: DateTime<Utc>) -> View {
        View::build(s, ui, c, &UiFacts::default(), now)
    }

    fn state_with(recs: Vec<PrRecord>, now: DateTime<Utc>) -> State {
        let mut s = State::default();
        s.repos.insert("o/r".into(), RepoStatus { ok: true, last_sync: Some(now - chrono::Duration::seconds(12)), ..Default::default() });
        for r in recs {
            s.insert(r);
        }
        s
    }

    fn rid(n: u64) -> RowId {
        RowId::Review(PrKey::new("o/r", n))
    }

    fn mid(n: u64) -> RowId {
        RowId::Mine(PrKey::new("o/r", n))
    }

    fn rec(n: u64, phase: Phase) -> PrRecord {
        let mut r = PrRecord::fixture("o/r", n, "feat/x", "s");
        r.phase = phase;
        r
    }

    #[test]
    fn partition_and_order() {
        let now = Utc::now();
        let mut adopted = rec(5, Phase::Ready);
        adopted.ownership = Ownership::Adopted { reason: "trocou para stack/x".into(), at: now };
        let mut leaving = rec(4, Phase::Ready);
        leaving.not_pending_since = Some(now - chrono::Duration::minutes(3));
        leaving.remote = Remote::NotPending;
        let mut unknown = rec(6, Phase::Ready);
        unknown.remote = Remote::Unknown;
        // em carência E sem sync: Sem sync vence Saindo (M18)
        let mut paused = rec(8, Phase::Ready);
        paused.remote = Remote::Unknown;
        paused.not_pending_since = Some(now - chrono::Duration::minutes(3));
        let mut s = state_with(vec![rec(1, Phase::Ready), rec(2, Phase::Failed { step: "pnpm install".into(), reason: "x".into() }),
                                rec(3, Phase::Preparing), leaving, adopted, unknown, paused, rec(7, Phase::Removing)], now);
        s.repos.get_mut("o/r").unwrap().since = Some(now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        let order: Vec<(u64, Section)> = v.rows.iter().map(|r| (r.id.key().number, r.section)).collect();
        assert_eq!(order, vec![
            (2, Section::Pending), (3, Section::Pending), (1, Section::Pending),
            (5, Section::Adopted), (6, Section::NoSync), (8, Section::NoSync), (4, Section::Leaving), (7, Section::Leaving)]);
        assert!(v.row(&rid(4)).unwrap().status.contains("sai em 12min"));
        let paused = &v.row(&rid(8)).unwrap().status;
        assert!(paused.contains("carência pausada: sem sync") && paused.contains("GitHub sem sync desde"), "{paused}");
        // remote = pending: 1, 2, 3, 5 (adotado) e 7 (removing com remote pending)
        assert_eq!(v.pending, "5");
    }

    #[test]
    fn pending_counter_marks_unknown_repo() {
        let now = Utc::now();
        let mut s = state_with(vec![rec(1, Phase::Ready)], now);
        s.repos.get_mut("o/r").unwrap().ok = false;
        s.repos.get_mut("o/r").unwrap().error = Some("HTTP 502".into());
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.pending, "1?");
        assert!(v.header.errors[0].contains("HTTP 502"));
    }

    #[test]
    fn header_uses_worst_age_and_marks_stale() {
        let now = Utc::now();
        let mut s = state_with(vec![], now);
        s.repos.get_mut("o/r").unwrap().last_sync = Some(now - chrono::Duration::minutes(10));
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert!(v.header.sync.contains("10min") && v.header.stale && v.header.sync.contains("atrasado"));
        let c = ViewConfig { repos: vec!["o/r".into(), "o/novo".into()], ..cfg() };
        let v = build(Some(&s), &UiState::default(), &c, now);
        assert!(v.header.sync.contains("nunca"));
        assert!(v.header.errors.iter().any(|e| e.contains("o/novo") && e.contains("ainda não sincronizado")));
    }

    #[test]
    fn no_snapshot_is_not_zero() {
        let v = build(None, &UiState::default(), &cfg(), Utc::now());
        assert_eq!(v.pending, "–");
        assert!(v.empty_reason.as_deref().unwrap().contains("sem snapshot"));
    }

    #[test]
    fn today_counter_valid_only_same_day_and_repos() {
        let now = Utc::now();
        let mut s = state_with(vec![], now);
        s.reviews_today.last_success = Some(crate::state::ReviewsTodaySuccess {
            day: crate::today::local_day(now), count: 3, per_repo: Default::default(),
            as_of: now - chrono::Duration::minutes(2), viewer: "c".into(), repos: vec!["o/r".into()] });
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.today, "3");
        assert!(v.today_detail.contains("2min"));
        let c = ViewConfig { repos: vec!["o/r".into(), "o/novo".into()], ..cfg() };
        let v = build(Some(&s), &UiState::default(), &c, now);
        assert_eq!(v.today, "–");
        assert!(!v.today_detail.contains("atualizado"));
        s.reviews_today.last_success.as_mut().unwrap().day = crate::today::local_day(now).pred_opt().unwrap();
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).today, "–");
    }

    #[test]
    fn eligibility() {
        let now = Utc::now();
        let mut adopted = rec(5, Phase::Ready);
        adopted.ownership = Ownership::Adopted { reason: "x".into(), at: now };
        let s = state_with(vec![rec(1, Phase::Ready), rec(2, Phase::Failed { step: "a".into(), reason: "b".into() }), adopted, rec(3, Phase::Creating)], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        let e = |n| v.row(&rid(n)).unwrap().eligible;
        assert!(!e(1).retry && e(1).adopt && !e(1).release && e(1).focus && e(1).log);
        assert!(e(2).retry);
        assert!(!e(5).adopt && e(5).release && !e(5).retry);
        assert!(!e(3).focus);
    }

    #[test]
    fn retry_for_blocked_only_when_worktree_missing() {
        let now = Utc::now();
        let blocked = |n| rec(n, Phase::Blocked { reason: "x".into() });
        let s = state_with(vec![
            rec(1, Phase::Failed { step: "a".into(), reason: "b".into() }), blocked(2), blocked(3)], now);
        let facts = UiFacts { missing_worktrees: [PrKey::new("o/r", 3)].into() };
        let v = View::build(Some(&s), &UiState::default(), &cfg(), &facts, now);
        let retry = |n| v.row(&rid(n)).unwrap().eligible.retry;
        assert!(retry(1), "failed com worktree");
        assert!(!retry(2), "blocked com worktree");
        assert!(retry(3), "blocked sem worktree");
    }

    #[test]
    fn label_shows_repo_only_with_several_repos() {
        let now = Utc::now();
        let s = state_with(vec![rec(1, Phase::Ready)], now);
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).rows[0].label, "#1");
        let c = ViewConfig { repos: vec!["o/r".into(), "o/x".into()], ..cfg() };
        assert_eq!(build(Some(&s), &UiState::default(), &c, now).rows[0].label, "r#1");
        let c = ViewConfig { repos: vec!["o/r".into(), "p/r".into()], ..cfg() };
        assert_eq!(build(Some(&s), &UiState::default(), &c, now).rows[0].label, "o/r#1", "nome curto ambíguo");
    }

    #[test]
    fn request_line_states() {
        let now = Utc::now();
        let mut s = state_with(vec![], now);
        let ui = UiState { sent: vec![Sent { id: "a1".into(), kind: RequestKind::Release, pr: Some(PrKey::new("o/r", 5)), at: now - chrono::Duration::seconds(5) }], ..Default::default() };
        let l = build(Some(&s), &ui, &cfg(), now).request_line.unwrap();
        assert!(l.contains("enviado") && l.contains("o/r#5"), "{l}");
        // um tick ocupado chega a ~105 s (contador 90 s + ensure 15 s): ainda não é "sem resposta"
        let busy = now + chrono::Duration::seconds(120);
        assert!(!build(Some(&s), &ui, &cfg(), busy).request_line.unwrap().contains("sem resposta do daemon"));
        let late = now + chrono::Duration::seconds(160);
        assert!(build(Some(&s), &ui, &cfg(), late).request_line.unwrap().contains("sem resposta do daemon"));
        s.push_request_result(RequestResult { id: "a1".into(), kind: "release".into(), pr: Some("o/r#5".into()), ok: false, message: "há alterações locais".into(), at: now });
        let line = build(Some(&s), &ui, &cfg(), now).request_line.unwrap();
        assert!(line.contains("falhou") && line.contains("alterações locais"));
    }

    #[test]
    fn selection_survives_reorder() {
        let now = Utc::now();
        let s = state_with(vec![rec(1, Phase::Ready), rec(2, Phase::Ready)], now);
        let ui = UiState { selected: Some(rid(2)), ..Default::default() };
        let v = build(Some(&s), &ui, &cfg(), now);
        assert_eq!(ui.selected_row(&v).unwrap().id.key().number, 2);
        let ui = UiState { selected: Some(rid(99)), ..Default::default() };
        assert_eq!(ui.selected_row(&v).unwrap().id.key().number, 1); // sumiu → primeiro
    }

    #[test]
    fn stack_column_covers_queue_and_my_prs() {
        let now = at("2026-10-13T15:00:00Z");
        let mut bottom = PrRecord::fixture("o/r", 1, "feat/a", "s");
        bottom.base_ref = "main".into();
        let mut s = state_with(vec![bottom], now);
        with_mine(&mut s, vec![mine(2, false, "2026-10-13T10:00:00Z")], now, 1);
        // sem coleta: a fila sozinha não vê o PR 2 e o meu PR fica sem dado
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.row(&rid(1)).unwrap().stack, "Não");
        assert_eq!(v.row(&mid(2)).unwrap().stack, "");
        // com a coleta, o meu PR 2 está em cima do 1
        s.open_prs.repos.insert(
            "o/r".into(),
            crate::state::OpenPrsRepo {
                prs: vec![
                    crate::state::OpenPr { number: 1, head_ref: "feat/a".into(), base_ref: "main".into() },
                    crate::state::OpenPr { number: 2, head_ref: "feat/b".into(), base_ref: "feat/a".into() },
                ],
                default_branch: Some("main".into()),
                as_of: now,
            },
        );
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.row(&rid(1)).unwrap().stack, "#1 1/2");
        assert_eq!(v.row(&mid(2)).unwrap().stack, "#1 2/2");
    }

    #[test]
    fn reselect_picks_next_survivor_or_previous_at_end() {
        let now = Utc::now();
        let k = rid;
        let prev = vec![k(1), k(2), k(3)];
        // remoção no meio: 2 some → 3
        let s = state_with(vec![rec(1, Phase::Ready), rec(3, Phase::Ready)], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(reselect(&prev, Some(&k(2)), &v), Some(k(3)));
        // remoção no fim: 3 some → 2
        let s = state_with(vec![rec(1, Phase::Ready), rec(2, Phase::Ready)], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(reselect(&prev, Some(&k(3)), &v), Some(k(2)));
        // ainda existe / sem seleção / vazio
        assert_eq!(reselect(&prev, Some(&k(1)), &v), Some(k(1)));
        assert_eq!(reselect(&prev, None, &v), Some(v.rows[0].id.clone()));
        assert_eq!(reselect(&prev, Some(&k(2)), &View::default()), None);
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn mine(n: u64, draft: bool, created: &str) -> crate::state::MyPr {
        crate::state::MyPr {
            number: n,
            title: format!("Meu {n}"),
            url: format!("https://github.com/o/r/pull/{n}"),
            draft,
            created_at: at(created),
            head_sha: "m".into(),
            ci: None,
        }
    }

    fn with_mine(s: &mut State, prs: Vec<crate::state::MyPr>, as_of: DateTime<Utc>, opened: u64) {
        s.my_prs.viewer = Some("cirdes".into());
        s.my_prs.repos.insert("o/r".into(), crate::state::MyPrsRepo { prs, opened_24h: opened, as_of });
    }

    fn started(n: u64, when: &str) -> PrRecord {
        let mut r = rec(n, Phase::Ready);
        r.work_started_at = Some(at(when));
        r.work_agent = Some("claude".into());
        r
    }

    #[test]
    fn sections_in_order_with_started_first() {
        let now = at("2026-10-13T15:00:00Z");
        let mut adopted = started(5, "2026-10-13T12:00:00Z");
        adopted.ownership = Ownership::Adopted { reason: "x".into(), at: now };
        let mut s = state_with(vec![rec(1, Phase::Ready), started(2, "2026-10-13T14:00:00Z"), adopted], now);
        with_mine(&mut s, vec![mine(9, false, "2026-10-13T10:00:00Z")], now, 1);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        let order: Vec<(RowId, Section)> = v.rows.iter().map(|r| (r.id.clone(), r.section)).collect();
        assert_eq!(order, vec![
            (rid(2), Section::Started), (rid(1), Section::Pending), (mid(9), Section::Mine), (rid(5), Section::Adopted)]);
    }

    #[test]
    fn started_sorted_by_most_recent_start() {
        let now = at("2026-10-13T15:00:00Z");
        let s = state_with(vec![started(1, "2026-10-13T09:00:00Z"), started(2, "2026-10-13T14:00:00Z"), started(3, "2026-10-13T11:00:00Z")], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.order(), vec![rid(2), rid(3), rid(1)]);
    }

    #[test]
    fn started_that_leaves_goes_to_leaving_and_keeps_start_column() {
        let now = at("2026-10-13T15:00:00Z");
        let mut r = started(4, "2026-10-13T14:32:00Z");
        r.remote = Remote::NotPending;
        r.not_pending_since = Some(now - chrono::Duration::minutes(3));
        let s = state_with(vec![r], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        let row = v.row(&rid(4)).unwrap();
        assert_eq!(row.section, Section::Leaving);
        assert_eq!(row.started, "11:32"); // São Paulo
        assert!(row.status.contains("sai em 12min"));
    }

    #[test]
    fn same_pr_in_both_lists_has_distinct_row_ids() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![rec(9, Phase::Ready)], now);
        with_mine(&mut s, vec![mine(9, false, "2026-10-13T10:00:00Z")], now, 1);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.row(&rid(9)).unwrap().section, Section::Pending);
        assert_eq!(v.row(&mid(9)).unwrap().section, Section::Mine);
        assert!(v.row(&rid(9)).unwrap().eligible.adopt);
        assert_eq!(v.row(&mid(9)).unwrap().eligible, Eligibility::default());
    }

    #[test]
    fn reselect_with_row_ids() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![rec(1, Phase::Ready)], now);
        with_mine(&mut s, vec![mine(9, false, "2026-10-13T10:00:00Z")], now, 1);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        let prev = vec![rid(1), rid(2), mid(9)];
        assert_eq!(reselect(&prev, Some(&rid(2)), &v), Some(mid(9)));
        assert_eq!(reselect(&prev, Some(&mid(9)), &v), Some(mid(9)));
    }

    #[test]
    fn mine_rows_fields() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![], now);
        with_mine(&mut s, vec![mine(9, true, "2026-10-13T10:00:00Z"), mine(8, false, "2026-10-13T09:00:00Z")], now, 2);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        let r9 = v.row(&mid(9)).unwrap();
        assert_eq!((r9.status.as_str(), r9.author.as_str(), r9.started.as_str(), r9.stack.as_str()), ("rascunho", "você", "–", ""));
        assert_eq!(r9.icon, '·');
        assert_eq!(r9.url, "https://github.com/o/r/pull/9");
        assert_eq!(v.row(&mid(8)).unwrap().status, "aberto");
    }

    #[test]
    fn mine_sorted_by_created_desc() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![], now);
        with_mine(&mut s, vec![mine(1, false, "2026-10-13T08:00:00Z"), mine(2, false, "2026-10-13T12:00:00Z")], now, 2);
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).order(), vec![mid(2), mid(1)]);
    }

    #[test]
    fn mine_filtered_by_config_repos() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![], now);
        s.my_prs.repos.insert("x/y".into(), crate::state::MyPrsRepo { prs: vec![mine(1, false, "2026-10-13T08:00:00Z")], opened_24h: 4, as_of: now });
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert!(v.rows.is_empty());
        assert_eq!(v.opened_24h, "–");
    }

    #[test]
    fn started_column_today_and_other_day() {
        let now = at("2026-10-13T15:00:00Z");
        let s = state_with(vec![started(1, "2026-10-13T14:32:00Z"), started(2, "2026-10-12T14:32:00Z"), rec(3, Phase::Ready)], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.row(&rid(1)).unwrap().started, "11:32");
        assert_eq!(v.row(&rid(2)).unwrap().started, "12/10");
        assert_eq!(v.row(&rid(3)).unwrap().started, "–");
    }

    #[test]
    fn opened_24h_counter_dash_partial_and_full() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![], now);
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).opened_24h, "–");
        with_mine(&mut s, vec![], now - chrono::Duration::seconds(30), 2);
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).opened_24h, "2");
        s.my_prs.errors.insert("o/r".into(), "boom".into());
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).opened_24h, "2?");
        s.my_prs.errors.clear();
        s.my_prs.repos.get_mut("o/r").unwrap().as_of = now - chrono::Duration::minutes(4);
        assert_eq!(build(Some(&s), &UiState::default(), &cfg(), now).opened_24h, "2?", "velho: mais de 3 polls");
        s.my_prs.repos.get_mut("o/r").unwrap().as_of = now;
        let two = ViewConfig { repos: vec!["o/r".into(), "o/x".into()], ..cfg() };
        assert_eq!(build(Some(&s), &UiState::default(), &two, now).opened_24h, "2?", "repo sem resultado");
    }

    #[test]
    fn my_prs_errors_go_to_header_errors() {
        let now = at("2026-10-13T15:00:00Z");
        let mut s = state_with(vec![], now);
        s.my_prs.errors.insert("o/r".into(), "boom".into());
        s.my_prs.errors.insert("x/y".into(), "fora da config".into());
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert!(v.header.errors.contains(&"meus PRs · o/r: boom".to_string()), "{:?}", v.header.errors);
        assert!(!v.header.errors.iter().any(|e| e.contains("x/y")));
    }

    #[test]
    fn ci_of_pr_out_of_queue_follows_remote_head() {
        // fora da fila (Saindo/Adotado), observed_head_sha para de atualizar; o CI coletado é do head
        // remoto atual e é o dado mais novo que existe
        let now = at("2026-10-13T15:00:00Z");
        let mut r = rec(1, Phase::Ready);
        r.ownership = Ownership::Adopted { reason: "x".into(), at: now };
        r.remote = Remote::NotPending;
        r.observed_head_sha = "antigo".into();
        r.ci = Some(crate::ci::Ci { sha: "novo".into(), state: crate::ci::CiState::Failure, failed_check: Some("lint".into()), finished_at: None, observed_at: now });
        let s = state_with(vec![r], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.row(&rid(1)).unwrap().ci, "✗ lint");
    }

    #[test]
    fn ci_cell_hides_ci_of_old_head() {
        let now = at("2026-10-13T15:00:00Z");
        let mut r = rec(1, Phase::Ready);
        r.observed_head_sha = "novo".into();
        r.ci = Some(crate::ci::Ci { sha: "velho".into(), state: crate::ci::CiState::Success, failed_check: None, finished_at: None, observed_at: now });
        let mut ok = rec(2, Phase::Ready);
        ok.ci = Some(crate::ci::Ci { sha: "s".into(), state: crate::ci::CiState::Failure, failed_check: Some("rspec".into()), finished_at: None, observed_at: now });
        let s = state_with(vec![r, ok], now);
        let v = build(Some(&s), &UiState::default(), &cfg(), now);
        assert_eq!(v.row(&rid(1)).unwrap().ci, "?");
        assert_eq!(v.row(&rid(2)).unwrap().ci, "✗ rspec");
    }

    fn search_state(now: DateTime<Utc>) -> State {
        let mut a = PrRecord::fixture("o/r", 101, "alice-eng-4321-fix", "s");
        a.title = "Corrige importação".into();
        a.author = "alice".into();
        let mut b = PrRecord::fixture("o/r", 102, "feat/outra", "s");
        b.title = "Nova tela".into();
        let mut s = state_with(vec![a, b], now);
        with_mine(&mut s, vec![mine(9, false, "2026-01-01T00:00:00Z")], now, 1);
        s.open_prs.repos.insert(
            "o/r".into(),
            crate::state::OpenPrsRepo {
                prs: vec![crate::state::OpenPr { number: 9, head_ref: "dev/eng-7000-busca".into(), base_ref: "main".into() }],
                default_branch: Some("main".into()),
                as_of: now,
            },
        );
        s
    }

    fn ids_for(filter: &str) -> (Vec<RowId>, Option<(usize, usize)>) {
        let now = Utc::now();
        let s = search_state(now);
        let v = build(Some(&s), &UiState { filter: filter.into(), ..Default::default() }, &cfg(), now);
        (v.order(), v.filtered)
    }

    #[test]
    fn filter_matches_number_branch_title_author() {
        assert_eq!(ids_for("").0.len(), 3);
        assert_eq!(ids_for("").1, None);
        assert_eq!(ids_for("#101"), (vec![rid(101)], Some((1, 3))));
        assert_eq!(ids_for("ENG-4321").0, vec![rid(101)], "branch, sem diferenciar maiúscula");
        assert_eq!(ids_for("4321").0, vec![rid(101)]);
        assert_eq!(ids_for("importacao").0, vec![rid(101)], "sem acento casa com acento");
        assert_eq!(ids_for("TELA").0, vec![rid(102)]);
        assert_eq!(ids_for("alice").0, vec![rid(101)]);
    }

    #[test]
    fn filter_is_fuzzy_inside_one_field() {
        assert_eq!(ids_for("crgimp").0, vec![rid(101)], "letras na ordem, com buracos");
        assert_eq!(ids_for("eng4321").0, vec![rid(101)], "sem o hífen da branch");
        assert_eq!(ids_for("101alice").0, Vec::<RowId>::new(), "não junta número e autor");
    }

    #[test]
    fn filter_ranks_contiguous_match_first_inside_a_section() {
        assert_eq!(ids_for("").0[..2], [rid(101), rid(102)], "sem busca, ordem de chegada");
        assert_eq!(ids_for("ra").0[..2], [rid(102), rid(101)], "\"outra\" na branch vence letras espalhadas no título");
    }

    #[test]
    fn fuzzy_scores() {
        assert!(fuzzy("imp", "importacao").unwrap() > fuzzy("imp", "i am pronto").unwrap());
        assert!(fuzzy("tela", "nova tela").unwrap() > fuzzy("tela", "outela").unwrap(), "começo de palavra soma");
        assert_eq!(fuzzy("zx", "importacao"), None);
    }

    #[test]
    fn filter_terms_are_and() {
        assert_eq!(ids_for("alice corrige").0, vec![rid(101)]);
        assert_eq!(ids_for("alice tela").0, Vec::<RowId>::new());
    }

    #[test]
    fn filter_finds_my_pr_by_branch_from_open_prs() {
        assert_eq!(ids_for("eng-7000").0, vec![mid(9)]);
        assert_eq!(ids_for("você").0, vec![mid(9)]);
    }

    #[test]
    fn filter_without_match_explains_empty_list() {
        let now = Utc::now();
        let s = search_state(now);
        let v = build(Some(&s), &UiState { filter: "zzz".into(), ..Default::default() }, &cfg(), now);
        assert!(v.rows.is_empty());
        assert_eq!(v.filtered, Some((0, 3)));
        assert!(v.empty_reason.as_deref().unwrap().contains("zzz"));
    }

    #[test]
    fn filter_keeps_row_under_confirmation() {
        let now = Utc::now();
        let s = search_state(now);
        let ui = UiState { filter: "tela".into(), confirm: Some((RequestKind::Adopt, PrKey::new("o/r", 101))), ..Default::default() };
        let v = build(Some(&s), &ui, &cfg(), now);
        assert_eq!(v.order(), vec![rid(101), rid(102)]);
    }

    #[test]
    fn filter_does_not_change_counters() {
        let now = Utc::now();
        let s = search_state(now);
        let v = build(Some(&s), &UiState { filter: "#101".into(), ..Default::default() }, &cfg(), now);
        assert_eq!(v.pending, "2");
    }
}
