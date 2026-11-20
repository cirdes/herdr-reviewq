use crate::config::{grace_duration, DEFAULT_REMOVE_GRACE_SECS};
use crate::state::{Ownership, Phase, Remote, State};
use chrono::{DateTime, Duration, Utc};

pub fn phase_label(p: &Phase) -> String {
    match p {
        Phase::Creating => "creating".into(),
        Phase::Preparing => "preparing".into(),
        Phase::Ready => "ready".into(),
        Phase::Failed { step, .. } => format!("failed · {step}"),
        Phase::Removing => "removing".into(),
        Phase::Blocked { reason } => format!("blocked · {reason}"),
    }
}

pub fn render(state: &State) -> String {
    render_at(state, Utc::now(), grace_duration(DEFAULT_REMOVE_GRACE_SECS), &[], 60)
}

/// `remove_grace` é a carência da config; mostra quanto falta para a remoção.
/// `repos` vazio (config ilegível): meus PRs de todos os repos guardados.
pub fn render_at(state: &State, now: DateTime<Utc>, remove_grace: Duration, repos: &[String], poll_secs: u64) -> String {
    let mut out = String::new();
    let pending = state.prs.values().filter(|r| r.remote == Remote::Pending).count();
    out.push_str(&format!("Pending: {pending}\n"));
    let today = match crate::today::valid_success(&state.reviews_today, now, repos, None) {
        Some(s) => format!("{} ({} ago)", s.count, crate::today::ago(now - s.as_of)),
        None => "–".to_string(),
    };
    out.push_str(&format!("Done today: {today}\n"));
    let mine_repos: Vec<String> = if repos.is_empty() { state.my_prs.repos.keys().cloned().collect() } else { repos.to_vec() };
    out.push_str(&format!("Opened 24h: {}\n", crate::today::opened_24h_label(&state.my_prs, &mine_repos, now, poll_secs)));
    if let Some(a) = state.reviews_today.last_attempt.as_ref().filter(|a| !a.ok) {
        out.push_str(&format!("! done today: {} ({})\n", a.error.as_deref().unwrap_or("error"), a.failed_repos.join(", ")));
    }
    for (repo, e) in &state.my_prs.errors {
        if repos.is_empty() || repos.contains(repo) {
            out.push_str(&format!("! my PRs · {repo}: {e}\n"));
        }
    }
    for (repo, st) in &state.repos {
        if !st.ok {
            out.push_str(&format!("! {repo}: {}\n", st.error.as_deref().unwrap_or("error")));
        }
        if !st.forks.is_empty() {
            out.push_str(&format!("! {repo}: fork PRs ignored {:?}\n", st.forks));
        }
    }
    let stacks = crate::stacks::labels(state);
    for r in state.prs.values() {
        let stack = match stacks.get(&r.key).map(String::as_str) {
            Some(s) if s != crate::stacks::NONE => format!(" · stack {s}"),
            _ => String::new(),
        };
        let owner = match &r.ownership {
            Ownership::Managed => String::new(),
            Ownership::Adopted { reason, .. } => format!(" [adopted: {reason}]"),
        };
        let warn = r.warning.as_deref().map(|w| format!(" ⚠ {w}")).unwrap_or_default();
        let leaving = match (&r.ownership, r.not_pending_since) {
            (Ownership::Managed, Some(since)) if r.phase != Phase::Removing => {
                let left = (since + remove_grace - now).num_seconds().max(0);
                format!(" · leaves in {}min", (left + 59) / 60)
            }
            _ => String::new(),
        };
        let ci = match crate::ci::cell(r.ci.as_ref(), r.ci_head(), now) {
            c if c.is_empty() => String::new(),
            c => format!(" · CI {c}"),
        };
        let started = match (r.work_started_at, &r.work_agent) {
            (Some(t), agent) => format!(" · started {} {}", crate::today::start_label(t, now), agent.as_deref().unwrap_or("agent")),
            (None, _) => String::new(),
        };
        out.push_str(&format!("{}  {}  {}  {}{leaving}{ci}{started}{stack}{owner}{warn}\n", r.key, r.author, r.title, phase_label(&r.phase)));
    }
    let mine: Vec<(&String, &crate::state::MyPr)> =
        mine_repos.iter().filter_map(|repo| state.my_prs.repos.get_key_value(repo)).flat_map(|(repo, m)| m.prs.iter().map(move |p| (repo, p))).collect();
    if !mine.is_empty() {
        out.push_str("My PRs\n");
        for (repo, p) in mine {
            let ci = match crate::ci::cell(p.ci.as_ref(), &p.head_sha, now) {
                c if c.is_empty() => String::new(),
                c => format!(" · CI {c}"),
            };
            let kind = if p.draft { "draft" } else { "open" };
            out.push_str(&format!("  {repo}#{}  {}  {kind}{ci}\n", p.number, p.title));
        }
    }
    for a in state.alerts.iter().rev().take(5) {
        out.push_str(&format!("alert {}: {}\n", a.at.format("%b %d %H:%M"), a.message));
    }
    for q in state.last_requests.iter().rev().take(3) {
        out.push_str(&format!("request {} {}: {}\n", q.kind, if q.ok { "ok" } else { "failed" }, q.message));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PrRecord;

    #[test]
    fn renders_counts_phases_adoption_and_alerts() {
        let mut s = State::default();
        s.insert(PrRecord::fixture("o/r", 1, "a", "s"));
        let mut adopted = PrRecord::fixture("o/r", 2, "b", "s");
        adopted.remote = Remote::NotPending;
        adopted.ownership = Ownership::Adopted { reason: "arquivos novos: x".into(), at: chrono::Utc::now() };
        s.insert(adopted);
        s.push_alert("state.json corrompido".into());
        let out = render(&s);
        assert!(out.starts_with("Pending: 1\n"));
        assert!(out.contains("o/r#1  ana  PR 1  ready"));
        assert!(out.contains("[adopted: arquivos novos: x]"));
        assert!(out.contains("state.json corrompido"));
    }

    #[test]
    fn shows_stack_position_only_when_stacked() {
        let mut s = State::default();
        s.insert(PrRecord::fixture("o/r", 1, "feat/a", "s"));
        let mut top = PrRecord::fixture("o/r", 2, "feat/b", "s");
        top.base_ref = "feat/a".into();
        s.insert(top);
        s.insert(PrRecord::fixture("o/r", 3, "feat/c", "s"));
        let out = render(&s);
        assert!(out.contains("o/r#1  ana  PR 1  ready · stack #1 1/2"), "{out}");
        assert!(out.contains("o/r#2  ana  PR 2  ready · stack #1 2/2"), "{out}");
        assert!(out.contains("o/r#3  ana  PR 3  ready\n"), "{out}");
    }

    #[test]
    fn shows_time_left_before_removal() {
        let now: DateTime<Utc> = "2026-10-07T12:00:00Z".parse().unwrap();
        let mut s = State::default();
        let mut rec = PrRecord::fixture("o/r", 3, "c", "s");
        rec.remote = Remote::NotPending;
        rec.not_pending_since = Some(now - Duration::minutes(5));
        s.insert(rec);
        let out = render_at(&s, now, Duration::minutes(15), &[], 60);
        assert!(out.contains("o/r#3  ana  PR 3  ready · leaves in 10min"), "{out}");
    }

    #[test]
    fn shows_reviews_today_or_dash() {
        let now = chrono::Utc::now();
        let repos = vec!["o/r".to_string()];
        let mut s = State::default();
        assert!(render_at(&s, now, Duration::minutes(15), &repos, 60).contains("Done today: –"));
        s.reviews_today.last_success = Some(crate::state::ReviewsTodaySuccess {
            day: crate::today::local_day(now),
            count: 4,
            per_repo: Default::default(),
            as_of: now - Duration::minutes(2),
            viewer: "c".into(),
            repos: repos.clone(),
        });
        assert!(render_at(&s, now, Duration::minutes(15), &repos, 60).contains("Done today: 4 (2min ago)"));
        // troca de repos (ou config ilegível): a contagem antiga não vale
        assert!(render_at(&s, now, Duration::minutes(15), &["x/y".to_string()], 60).contains("Done today: –"));
        assert!(render_at(&s, now, Duration::minutes(15), &[], 60).contains("Done today: –"));
    }

    #[test]
    fn status_shows_ci_and_start() {
        let now: DateTime<Utc> = "2026-10-13T15:00:00Z".parse().unwrap();
        let mut s = State::default();
        let mut rec = PrRecord::fixture("o/r", 1, "a", "s");
        rec.ci = Some(crate::ci::Ci {
            sha: "s".into(),
            state: crate::ci::CiState::Failure,
            failed_check: Some("rspec".into()),
            finished_at: None,
            observed_at: now,
        });
        rec.work_started_at = Some("2026-10-13T14:32:00Z".parse().unwrap());
        rec.work_agent = Some("claude".into());
        s.insert(rec);
        s.insert(PrRecord::fixture("o/r", 2, "b", "s"));
        let out = render_at(&s, now, Duration::minutes(15), &[], 60);
        assert!(out.contains("o/r#1  ana  PR 1  ready · CI ✗ rspec · started 11:32 claude"), "{out}");
        assert!(out.contains("o/r#2  ana  PR 2  ready\n"), "{out}");
    }

    #[test]
    fn status_ci_of_pr_out_of_queue_follows_remote_head() {
        let now: DateTime<Utc> = "2026-10-13T15:00:00Z".parse().unwrap();
        let mut s = State::default();
        let mut rec = PrRecord::fixture("o/r", 1, "a", "antigo");
        rec.remote = Remote::NotPending;
        rec.ci = Some(crate::ci::Ci { sha: "novo".into(), state: crate::ci::CiState::Pending, failed_check: None, finished_at: None, observed_at: now });
        s.insert(rec);
        assert!(render_at(&s, now, Duration::minutes(15), &[], 60).contains(" · CI ◐"));
    }

    #[test]
    fn status_shows_opened_24h_and_my_prs() {
        let now: DateTime<Utc> = "2026-10-13T15:00:00Z".parse().unwrap();
        let repos = vec!["o/r".to_string(), "o/x".to_string()];
        let mut s = State::default();
        assert!(render_at(&s, now, Duration::minutes(15), &repos, 60).contains("Opened 24h: –\n"));
        s.my_prs.repos.insert(
            "o/r".into(),
            crate::state::MyPrsRepo {
                prs: vec![crate::state::MyPr {
                    number: 9,
                    title: "Meu".into(),
                    url: String::new(),
                    draft: true,
                    created_at: now,
                    head_sha: "m".into(),
                    ci: None,
                }],
                opened_24h: 2,
                as_of: now,
            },
        );
        s.my_prs.errors.insert("o/x".into(), "boom".into());
        let out = render_at(&s, now, Duration::minutes(15), &repos, 60);
        assert!(out.contains("Done today: –\nOpened 24h: 2?\n"), "{out}");
        assert!(out.contains("! my PRs · o/x: boom"), "{out}");
        assert!(out.contains("My PRs\n  o/r#9  Meu  draft\n"), "{out}");
    }
}
