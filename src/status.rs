use crate::config::{grace_duration, DEFAULT_REMOVE_GRACE_SECS};
use crate::state::{Ownership, Phase, Remote, State};
use chrono::{DateTime, Duration, Utc};

pub fn phase_label(p: &Phase) -> String {
    match p {
        Phase::Creating => "criando".into(),
        Phase::Preparing => "preparando".into(),
        Phase::Ready => "pronto".into(),
        Phase::Failed { step, .. } => format!("falhou · {step}"),
        Phase::Removing => "removendo".into(),
        Phase::Blocked { reason } => format!("bloqueado · {reason}"),
    }
}

pub fn render(state: &State) -> String {
    render_at(state, Utc::now(), grace_duration(DEFAULT_REMOVE_GRACE_SECS), &[])
}

/// `remove_grace` é a carência da config; mostra quanto falta para a remoção.
pub fn render_at(state: &State, now: DateTime<Utc>, remove_grace: Duration, repos: &[String]) -> String {
    let mut out = String::new();
    let pending = state.prs.values().filter(|r| r.remote == Remote::Pending).count();
    out.push_str(&format!("Pendentes: {pending}\n"));
    let today = match crate::today::valid_success(&state.reviews_today, now, repos, None) {
        Some(s) => format!("{} (há {})", s.count, crate::today::ago(now - s.as_of)),
        None => "–".to_string(),
    };
    out.push_str(&format!("Feitas hoje: {today}\n"));
    if let Some(a) = state.reviews_today.last_attempt.as_ref().filter(|a| !a.ok) {
        out.push_str(&format!("! feitas hoje: {} ({})\n", a.error.as_deref().unwrap_or("erro"), a.failed_repos.join(", ")));
    }
    for (repo, st) in &state.repos {
        if !st.ok {
            out.push_str(&format!("! {repo}: {}\n", st.error.as_deref().unwrap_or("erro")));
        }
        if !st.forks.is_empty() {
            out.push_str(&format!("! {repo}: PRs de fork ignorados {:?}\n", st.forks));
        }
    }
    for r in state.prs.values() {
        let owner = match &r.ownership {
            Ownership::Managed => String::new(),
            Ownership::Adopted { reason, .. } => format!(" [adotado: {reason}]"),
        };
        let warn = r.warning.as_deref().map(|w| format!(" ⚠ {w}")).unwrap_or_default();
        let leaving = match (&r.ownership, r.not_pending_since) {
            (Ownership::Managed, Some(since)) if r.phase != Phase::Removing => {
                let left = (since + remove_grace - now).num_seconds().max(0);
                format!(" · sai em {}min", (left + 59) / 60)
            }
            _ => String::new(),
        };
        out.push_str(&format!("{}  {}  {}  {}{leaving}{owner}{warn}\n", r.key, r.author, r.title, phase_label(&r.phase)));
    }
    for a in state.alerts.iter().rev().take(5) {
        out.push_str(&format!("alerta {}: {}\n", a.at.format("%d/%m %H:%M"), a.message));
    }
    for q in state.last_requests.iter().rev().take(3) {
        out.push_str(&format!("pedido {} {}: {}\n", q.kind, if q.ok { "ok" } else { "falhou" }, q.message));
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
        assert!(out.starts_with("Pendentes: 1\n"));
        assert!(out.contains("o/r#1  ana  PR 1  pronto"));
        assert!(out.contains("[adotado: arquivos novos: x]"));
        assert!(out.contains("state.json corrompido"));
    }

    #[test]
    fn shows_time_left_before_removal() {
        let now: DateTime<Utc> = "2026-10-07T12:00:00Z".parse().unwrap();
        let mut s = State::default();
        let mut rec = PrRecord::fixture("o/r", 3, "c", "s");
        rec.remote = Remote::NotPending;
        rec.not_pending_since = Some(now - Duration::minutes(5));
        s.insert(rec);
        let out = render_at(&s, now, Duration::minutes(15), &[]);
        assert!(out.contains("o/r#3  ana  PR 3  pronto · sai em 10min"), "{out}");
    }

    #[test]
    fn shows_reviews_today_or_dash() {
        let now = chrono::Utc::now();
        let repos = vec!["o/r".to_string()];
        let mut s = State::default();
        assert!(render_at(&s, now, Duration::minutes(15), &repos).contains("Feitas hoje: –"));
        s.reviews_today.last_success = Some(crate::state::ReviewsTodaySuccess {
            day: crate::today::local_day(now),
            count: 4,
            per_repo: Default::default(),
            as_of: now - Duration::minutes(2),
            viewer: "c".into(),
            repos: repos.clone(),
        });
        assert!(render_at(&s, now, Duration::minutes(15), &repos).contains("Feitas hoje: 4 (há 2min)"));
        // troca de repos (ou config ilegível): a contagem antiga não vale
        assert!(render_at(&s, now, Duration::minutes(15), &["x/y".to_string()]).contains("Feitas hoje: –"));
        assert!(render_at(&s, now, Duration::minutes(15), &[]).contains("Feitas hoje: –"));
    }
}
