use crate::requests::RequestKind;
use crate::state::{Ownership, Phase, PrKey, PrRecord, Remote, State};
use crate::status::phase_label;
use crate::today::{ago, local_hm, valid_success};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Pending,
    NoSync,
    Leaving,
    Adopted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Eligibility {
    pub retry: bool,
    pub adopt: bool,
    pub release: bool,
    pub open: bool,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub key: PrKey,
    /// "#n" com um repo configurado; "repo#n" com mais de um.
    pub label: String,
    pub section: Section,
    pub author: String,
    pub title: String,
    pub icon: char,
    pub status: String,
    pub url: String,
    pub eligible: Eligibility,
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
    pub header: Header,
    pub alerts: Vec<String>,
    pub request_line: Option<String>,
    pub empty_reason: Option<String>,
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
    pub selected: Option<PrKey>,
    /// Ordem das linhas da última visão (para `reselect`).
    pub order: Vec<PrKey>,
    /// Confirmação pendente (tecla `y`/`n`).
    pub confirm: Option<(RequestKind, PrKey)>,
    pub sent: Vec<Sent>,
    /// Resultados e avisos; independente de `confirm`.
    pub message: Option<String>,
    pub scroll: usize,
}

impl UiState {
    pub fn selected_row<'v>(&self, view: &'v View) -> Option<&'v Row> {
        self.selected.as_ref().and_then(|k| view.row(k)).or_else(|| view.rows.first())
    }
}

/// Escolhe a seleção após uma nova visão: a mesma linha se ainda existe; senão o próximo
/// sobrevivente da ordem anterior, ou o anterior se era o último; senão a primeira linha.
pub fn reselect(prev_order: &[PrKey], selected: Option<&PrKey>, view: &View) -> Option<PrKey> {
    let first = || view.rows.first().map(|r| r.key.clone());
    let Some(sel) = selected else { return first() };
    if view.row(sel).is_some() {
        return Some(sel.clone());
    }
    let Some(idx) = prev_order.iter().position(|k| k == sel) else { return first() };
    let alive = |k: &&PrKey| view.row(k).is_some();
    prev_order[idx + 1..]
        .iter()
        .find(alive)
        .or_else(|| prev_order[..idx].iter().rev().find(alive))
        .cloned()
        .or_else(first)
}

impl View {
    pub fn row(&self, key: &PrKey) -> Option<&Row> {
        self.rows.iter().find(|r| &r.key == key)
    }

    /// Ordem atual das linhas, para guardar em `UiState.order`.
    pub fn order(&self) -> Vec<PrKey> {
        self.rows.iter().map(|r| r.key.clone()).collect()
    }

    pub fn build(state: Option<&State>, ui: &UiState, cfg: &ViewConfig, facts: &UiFacts, now: DateTime<Utc>) -> View {
        let Some(state) = state else {
            return View {
                pending: "–".into(),
                today: "–".into(),
                empty_reason: Some("sem snapshot ainda — o daemon não gravou o estado".into()),
                header: Header { sync: "nunca".into(), stale: true, errors: vec![] },
                ..Default::default()
            };
        };
        let mut rows: Vec<(Section, u8, DateTime<Utc>, Row)> = state
            .prs
            .values()
            .map(|r| {
                let section = section_of(r);
                (section, phase_rank(&r.phase), r.first_seen_at, row_of(r, section, state, cfg, facts, now))
            })
            .collect();
        rows.sort_by(|a, b| (a.0, a.1, a.2, &a.3.key).cmp(&(b.0, b.1, b.2, &b.3.key)));

        let pending_n = state.prs.values().filter(|r| r.remote == Remote::Pending).count();
        let any_unsynced = cfg.repos.iter().any(|name| !state.repos.get(name).is_some_and(|s| s.ok));
        let valid = valid_success(&state.reviews_today, now, &cfg.repos, None);
        View {
            rows: rows.into_iter().map(|t| t.3).collect(),
            pending: format!("{pending_n}{}", if any_unsynced { "?" } else { "" }),
            today: valid.map_or("–".into(), |s| s.count.to_string()),
            today_detail: today_detail(state, valid.map(|s| s.as_of), now),
            header: header(state, cfg, now),
            alerts: state.alerts.iter().rev().take(3).map(|a| format!("{}  {}", local_hm(a.at), a.message)).collect(),
            request_line: request_line(state, ui, now),
            empty_reason: None,
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
        Ownership::Managed => Section::Pending,
    }
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

fn row_of(r: &PrRecord, section: Section, state: &State, cfg: &ViewConfig, facts: &UiFacts, now: DateTime<Utc>) -> Row {
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
    let label = if cfg.repos.len() > 1 { r.key.to_string() } else { format!("#{}", r.key.number) };
    Row {
        key: r.key.clone(),
        label,
        section,
        author: r.author.clone(),
        title: r.title.clone(),
        icon: icon(r),
        status,
        url: r.url.clone(),
        eligible: Eligibility {
            retry: managed && (matches!(r.phase, Phase::Failed { .. }) || (matches!(r.phase, Phase::Blocked { .. }) && missing)),
            adopt: managed,
            release: !managed,
            open: !matches!(r.phase, Phase::Removing | Phase::Creating),
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
        let order: Vec<(u64, Section)> = v.rows.iter().map(|r| (r.key.number, r.section)).collect();
        assert_eq!(order, vec![
            (2, Section::Pending), (3, Section::Pending), (1, Section::Pending),
            (6, Section::NoSync), (8, Section::NoSync), (4, Section::Leaving), (7, Section::Leaving), (5, Section::Adopted)]);
        assert!(v.row(&PrKey::new("o/r", 4)).unwrap().status.contains("sai em 12min"));
        let paused = &v.row(&PrKey::new("o/r", 8)).unwrap().status;
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
        let e = |n| v.row(&PrKey::new("o/r", n)).unwrap().eligible;
        assert!(!e(1).retry && e(1).adopt && !e(1).release && e(1).open);
        assert!(e(2).retry);
        assert!(!e(5).adopt && e(5).release && !e(5).retry);
        assert!(!e(3).open);
    }

    #[test]
    fn retry_for_blocked_only_when_worktree_missing() {
        let now = Utc::now();
        let blocked = |n| rec(n, Phase::Blocked { reason: "x".into() });
        let s = state_with(vec![
            rec(1, Phase::Failed { step: "a".into(), reason: "b".into() }), blocked(2), blocked(3)], now);
        let facts = UiFacts { missing_worktrees: [PrKey::new("o/r", 3)].into() };
        let v = View::build(Some(&s), &UiState::default(), &cfg(), &facts, now);
        let retry = |n| v.row(&PrKey::new("o/r", n)).unwrap().eligible.retry;
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
        assert_eq!(build(Some(&s), &UiState::default(), &c, now).rows[0].label, "o/r#1");
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
        let ui = UiState { selected: Some(PrKey::new("o/r", 2)), ..Default::default() };
        let v = build(Some(&s), &ui, &cfg(), now);
        assert_eq!(ui.selected_row(&v).unwrap().key.number, 2);
        let ui = UiState { selected: Some(PrKey::new("o/r", 99)), ..Default::default() };
        assert_eq!(ui.selected_row(&v).unwrap().key.number, 1); // sumiu → primeiro
    }

    #[test]
    fn reselect_picks_next_survivor_or_previous_at_end() {
        let now = Utc::now();
        let k = |n| PrKey::new("o/r", n);
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
        assert_eq!(reselect(&prev, None, &v), Some(v.rows[0].key.clone()));
        assert_eq!(reselect(&prev, Some(&k(2)), &View::default()), None);
    }
}
