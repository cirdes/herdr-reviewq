//! Início do trabalho: agentes (Claude/Codex) que o herdr vê dentro de um worktree de review.

use crate::herdr::AgentInfo;
use crate::state::{PrKey, State};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

const KNOWN: &[&str] = &["claude", "codex"];

fn abs(v: &Value, k: &str) -> Option<PathBuf> {
    let p = PathBuf::from(v.get(k)?.as_str()?);
    p.is_absolute().then_some(p)
}

/// `result` do `herdr agent list`. Entradas sem nome ou sem nenhum caminho absoluto são ignoradas.
pub fn parse_agents(result: &Value) -> Vec<AgentInfo> {
    result
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let agent = a.get("agent")?.as_str()?.to_string();
            let (cwd, foreground_cwd) = (abs(a, "cwd"), abs(a, "foreground_cwd"));
            (cwd.is_some() || foreground_cwd.is_some()).then_some(AgentInfo { agent, cwd, foreground_cwd })
        })
        .collect()
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Marca `work_started_at`/`work_agent` nos registros ainda sem início que têm um agente conhecido
/// na raiz do worktree ou abaixo dela. Uma vez marcado, não muda. Devolve os marcados agora.
pub fn apply_agents(state: &mut State, agents: &[AgentInfo], now: DateTime<Utc>) -> Vec<PrKey> {
    let mut known: Vec<(&str, Vec<PathBuf>)> = agents
        .iter()
        .filter(|a| KNOWN.contains(&a.agent.as_str()))
        .map(|a| (a.agent.as_str(), [&a.cwd, &a.foreground_cwd].into_iter().flatten().map(|p| canon(p)).collect()))
        .collect();
    // desempate determinístico quando há mais de um agente no mesmo worktree
    known.sort_by(|a, b| a.0.cmp(b.0));
    let mut marked = vec![];
    for rec in state.prs.values_mut().filter(|r| r.work_started_at.is_none()) {
        let root = canon(&rec.path);
        if let Some((agent, _)) = known.iter().find(|(_, ps)| ps.iter().any(|p| p.starts_with(&root))) {
            rec.work_started_at = Some(now);
            rec.work_agent = Some(agent.to_string());
            marked.push(rec.key.clone());
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentInfo;
    use crate::state::{PrKey, PrRecord, State};
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::path::PathBuf;

    fn now() -> DateTime<Utc> {
        "2026-10-13T14:32:00Z".parse().unwrap()
    }

    fn agent(name: &str, cwd: &str) -> AgentInfo {
        AgentInfo { agent: name.into(), cwd: Some(PathBuf::from(cwd)), foreground_cwd: None }
    }

    fn state(recs: &[(u64, &str)]) -> State {
        let mut s = State::default();
        for (n, path) in recs {
            let mut r = PrRecord::fixture("o/r", *n, "h", "s");
            r.path = PathBuf::from(path);
            s.insert(r);
        }
        s
    }

    fn started(s: &State, n: u64) -> (Option<DateTime<Utc>>, Option<String>) {
        let r = s.get(&PrKey::new("o/r", n)).unwrap();
        (r.work_started_at, r.work_agent.clone())
    }

    #[test]
    fn parses_real_agent_list() {
        let result = json!({"agents": [
            {"agent": "claude", "agent_status": "done", "cwd": "/w/pr-15791", "foreground_cwd": "/w/pr-15791/src",
             "pane_id": "w26:p1", "workspace_id": "w26"},
            {"agent": "codex", "agent_status": "working", "foreground_cwd": "/w/pr-2"},
            {"agent": "claude", "agent_status": "idle"},
            {"agent": "claude", "cwd": "relativo/pr-3"},
            {"cwd": "/w/pr-4"}
        ], "type": "agent_list"});
        let agents = parse_agents(&result);
        assert_eq!(
            agents,
            vec![
                AgentInfo {
                    agent: "claude".into(),
                    cwd: Some("/w/pr-15791".into()),
                    foreground_cwd: Some("/w/pr-15791/src".into())
                },
                AgentInfo { agent: "codex".into(), cwd: None, foreground_cwd: Some("/w/pr-2".into()) },
            ]
        );
        assert!(parse_agents(&json!({"nada": 1})).is_empty());
    }

    #[test]
    fn marks_worktree_with_agent_in_root_or_subdir() {
        let mut s = state(&[(1, "/w/pr-1"), (2, "/w/pr-2"), (3, "/w/pr-3")]);
        let marked = apply_agents(&mut s, &[agent("claude", "/w/pr-1"), agent("codex", "/w/pr-2/app/models")], now());
        assert_eq!(marked, vec![PrKey::new("o/r", 1), PrKey::new("o/r", 2)]);
        assert_eq!(started(&s, 1), (Some(now()), Some("claude".into())));
        assert_eq!(started(&s, 2), (Some(now()), Some("codex".into())));
        assert_eq!(started(&s, 3), (None, None));
    }

    #[test]
    fn uses_foreground_cwd_too() {
        let mut s = state(&[(1, "/w/pr-1")]);
        let a = AgentInfo { agent: "claude".into(), cwd: Some("/home".into()), foreground_cwd: Some("/w/pr-1/lib".into()) };
        apply_agents(&mut s, &[a], now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
    }

    #[test]
    fn similar_prefix_does_not_match() {
        let mut s = state(&[(157, "/w/pr-157")]);
        assert!(apply_agents(&mut s, &[agent("claude", "/w/pr-1579")], now()).is_empty());
        assert_eq!(started(&s, 157), (None, None));
    }

    #[test]
    fn matches_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("pr-1")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut s = state(&[(1, link.join("pr-1").to_str().unwrap())]);
        apply_agents(&mut s, &[agent("claude", real.join("pr-1").to_str().unwrap())], now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
    }

    #[test]
    fn ignores_agents_outside_allowlist() {
        let mut s = state(&[(1, "/w/pr-1")]);
        assert!(apply_agents(&mut s, &[agent("pi", "/w/pr-1"), agent("gemini", "/w/pr-1")], now()).is_empty());
    }

    #[test]
    fn first_detection_is_stable() {
        let mut s = state(&[(1, "/w/pr-1")]);
        apply_agents(&mut s, &[agent("codex", "/w/pr-1")], now());
        let later = now() + chrono::Duration::hours(1);
        assert!(apply_agents(&mut s, &[agent("claude", "/w/pr-1")], later).is_empty());
        assert_eq!(started(&s, 1), (Some(now()), Some("codex".into())));
    }

    #[test]
    fn picks_alphabetical_agent_on_tie() {
        let mut s = state(&[(1, "/w/pr-1")]);
        apply_agents(&mut s, &[agent("codex", "/w/pr-1"), agent("claude", "/w/pr-1/x")], now());
        assert_eq!(started(&s, 1).1.as_deref(), Some("claude"));
    }
}
