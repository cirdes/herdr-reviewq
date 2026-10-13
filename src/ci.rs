//! CI de um PR a partir do `statusCheckRollup` do último commit. Tudo aqui é metadado opcional:
//! resposta estranha vira `None` (ou detalhe vazio), nunca erro que derrube uma coleta.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Trecho GraphQL do PR usado nas coletas de CI (reviews e meus PRs).
pub const CI_FRAGMENT: &str = r#"headRefOid
commits(last: 1) { nodes { commit { oid statusCheckRollup {
  state
  contexts(first: 50) {
    pageInfo { hasNextPage }
    nodes {
      __typename
      ... on CheckRun { name status conclusion completedAt }
      ... on StatusContext { context state createdAt }
    }
  }
} } } }"#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    Pending,
    Success,
    Failure,
    /// Valor que esta versão não conhece (estado gravado por uma versão futura).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ci {
    /// Commit do qual o rollup veio (head remoto do PR).
    pub sha: String,
    pub state: CiState,
    #[serde(default)]
    pub failed_check: Option<String>,
    /// Só com o conjunto de checks completo e terminado.
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}

const FAILED_CONCLUSIONS: &[&str] = &["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"];

fn time(v: &Value, k: &str) -> Option<DateTime<Utc>> {
    v.get(k)?.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc))
}

/// `pr` é o nó do PR com o `CI_FRAGMENT`. Rollup nulo (PR sem checks) ou estado desconhecido: `None`.
pub fn parse_pr_ci(pr: &Value, now: DateTime<Utc>) -> Option<Ci> {
    let commit = pr.pointer("/commits/nodes/0/commit")?;
    let sha = commit.get("oid")?.as_str()?.to_string();
    let rollup = commit.get("statusCheckRollup").filter(|r| !r.is_null())?;
    let state = match rollup.get("state")?.as_str()? {
        "SUCCESS" => CiState::Success,
        "FAILURE" | "ERROR" => CiState::Failure,
        "PENDING" | "EXPECTED" => CiState::Pending,
        _ => return None,
    };
    let empty = vec![];
    let nodes = rollup.pointer("/contexts/nodes").and_then(Value::as_array).unwrap_or(&empty);
    // sem pageInfo não dá para saber se o conjunto está completo
    let truncated = rollup.pointer("/contexts/pageInfo/hasNextPage").and_then(Value::as_bool).unwrap_or(true);
    let mut failed_check = None;
    let mut latest: Option<DateTime<Utc>> = None;
    let mut running = false;
    for n in nodes {
        let (name, failed, at) = match n.get("__typename").and_then(Value::as_str) {
            Some("CheckRun") => {
                running |= n.get("status").and_then(Value::as_str) != Some("COMPLETED");
                let c = n.get("conclusion").and_then(Value::as_str).unwrap_or("");
                (n.get("name").and_then(Value::as_str), FAILED_CONCLUSIONS.contains(&c), time(n, "completedAt"))
            }
            Some("StatusContext") => {
                let s = n.get("state").and_then(Value::as_str).unwrap_or("");
                (n.get("context").and_then(Value::as_str), matches!(s, "FAILURE" | "ERROR"), time(n, "createdAt"))
            }
            _ => continue,
        };
        if failed && failed_check.is_none() {
            failed_check = name.map(String::from);
        }
        latest = latest.max(at);
    }
    let finished_at = if state != CiState::Pending && !truncated && !running { latest } else { None };
    Some(Ci { sha, state, failed_check, finished_at, observed_at: now })
}

pub fn icon(ci: Option<&Ci>) -> char {
    match ci.map(|c| &c.state) {
        Some(CiState::Pending) => '◐',
        Some(CiState::Success) => '✓',
        Some(CiState::Failure) => '✗',
        Some(CiState::Unknown) => '?',
        None => '·',
    }
}

/// Texto da coluna CI. CI de outro commit que não o head atual vira `?`: head novo nunca herda o
/// resultado do anterior.
pub fn cell(ci: Option<&Ci>, head_sha: &str, now: DateTime<Utc>) -> String {
    let Some(c) = ci else { return String::new() };
    if c.sha != head_sha {
        return "?".into();
    }
    match (&c.state, &c.failed_check, c.finished_at) {
        (CiState::Success, _, Some(t)) => format!("✓ {}", crate::today::ago(now - t)),
        (CiState::Failure, Some(n), _) => format!("✗ {n}"),
        _ => icon(Some(c)).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use serde_json::{json, Value};

    fn now() -> DateTime<Utc> {
        "2026-10-13T15:00:00Z".parse().unwrap()
    }

    fn pr(rollup: Value) -> Value {
        json!({"headRefOid": "abc", "commits": {"nodes": [{"commit": {"oid": "abc", "statusCheckRollup": rollup}}]}})
    }

    fn rollup(state: &str, nodes: Vec<Value>, more: bool) -> Value {
        json!({"state": state, "contexts": {"pageInfo": {"hasNextPage": more}, "nodes": nodes}})
    }

    fn run(name: &str, status: &str, concl: Option<&str>, at: Option<&str>) -> Value {
        json!({"__typename": "CheckRun", "name": name, "status": status, "conclusion": concl, "completedAt": at})
    }

    fn status(ctx: &str, state: &str, at: &str) -> Value {
        json!({"__typename": "StatusContext", "context": ctx, "state": state, "createdAt": at})
    }

    fn parse(r: Value) -> Option<Ci> {
        parse_pr_ci(&pr(r), now())
    }

    #[test]
    fn maps_five_rollup_states() {
        for (raw, want) in [
            ("SUCCESS", CiState::Success),
            ("FAILURE", CiState::Failure),
            ("ERROR", CiState::Failure),
            ("PENDING", CiState::Pending),
            ("EXPECTED", CiState::Pending),
        ] {
            assert_eq!(parse(rollup(raw, vec![], false)).unwrap().state, want, "{raw}");
        }
    }

    #[test]
    fn null_rollup_or_unknown_state_is_none() {
        assert!(parse(Value::Null).is_none());
        assert!(parse(rollup("WAT", vec![], false)).is_none());
        assert!(parse_pr_ci(&json!({"headRefOid": "abc", "commits": {"nodes": []}}), now()).is_none());
    }

    #[test]
    fn failed_check_from_check_run_and_status_context() {
        for c in ["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"] {
            let ci = parse(rollup("FAILURE", vec![run("rspec", "COMPLETED", Some(c), None)], false)).unwrap();
            assert_eq!(ci.failed_check.as_deref(), Some("rspec"), "{c}");
        }
        for c in [Some("SUCCESS"), Some("NEUTRAL"), Some("SKIPPED"), None] {
            let ci = parse(rollup("SUCCESS", vec![run("lint", "COMPLETED", c, None)], false)).unwrap();
            assert_eq!(ci.failed_check, None, "{c:?}");
        }
        for s in ["FAILURE", "ERROR"] {
            let ci = parse(rollup("FAILURE", vec![status("ci/circle", s, "2026-10-13T14:00:00Z")], false)).unwrap();
            assert_eq!(ci.failed_check.as_deref(), Some("ci/circle"), "{s}");
        }
        let first = parse(rollup(
            "FAILURE",
            vec![
                run("ok", "COMPLETED", Some("SUCCESS"), None),
                run("primeiro", "COMPLETED", Some("FAILURE"), None),
                run("segundo", "COMPLETED", Some("FAILURE"), None),
            ],
            false,
        ))
        .unwrap();
        assert_eq!(first.failed_check.as_deref(), Some("primeiro"));
    }

    #[test]
    fn finished_at_is_max_of_runs_and_statuses_when_complete() {
        let ci = parse(rollup(
            "SUCCESS",
            vec![
                run("a", "COMPLETED", Some("SUCCESS"), Some("2026-10-13T14:10:00Z")),
                status("b", "SUCCESS", "2026-10-13T14:20:00Z"),
                run("c", "COMPLETED", Some("SUCCESS"), Some("2026-10-13T14:05:00Z")),
            ],
            false,
        ))
        .unwrap();
        assert_eq!(ci.finished_at, Some("2026-10-13T14:20:00Z".parse().unwrap()));
    }

    #[test]
    fn no_finished_at_when_truncated_or_run_in_progress_or_pending() {
        let done = run("a", "COMPLETED", Some("SUCCESS"), Some("2026-10-13T14:10:00Z"));
        assert_eq!(parse(rollup("SUCCESS", vec![done.clone()], true)).unwrap().finished_at, None);
        let running = run("b", "IN_PROGRESS", None, None);
        assert_eq!(parse(rollup("FAILURE", vec![done.clone(), running], false)).unwrap().finished_at, None);
        assert_eq!(parse(rollup("PENDING", vec![done], false)).unwrap().finished_at, None);
    }

    #[test]
    fn malformed_detail_keeps_state() {
        let ci = parse(rollup("FAILURE", vec![json!({"foo": 1}), json!({"__typename": "Outro"}), json!("x")], false)).unwrap();
        assert_eq!(ci.state, CiState::Failure);
        assert_eq!(ci.failed_check, None);
        let no_contexts = parse(json!({"state": "SUCCESS"})).unwrap();
        assert_eq!(no_contexts.state, CiState::Success);
        assert_eq!(no_contexts.finished_at, None);
    }

    #[test]
    fn sha_comes_from_commit_oid_and_observed_at_is_now() {
        let mut node = pr(rollup("SUCCESS", vec![], false));
        node["commits"]["nodes"][0]["commit"]["oid"] = json!("def");
        let ci = parse_pr_ci(&node, now()).unwrap();
        assert_eq!(ci.sha, "def");
        assert_eq!(ci.observed_at, now());
    }

    fn ci(state: CiState, check: Option<&str>, finished: Option<&str>) -> Ci {
        Ci {
            sha: "abc".into(),
            state,
            failed_check: check.map(String::from),
            finished_at: finished.map(|t| t.parse().unwrap()),
            observed_at: now(),
        }
    }

    #[test]
    fn cell_texts() {
        assert_eq!(cell(Some(&ci(CiState::Pending, None, None)), "abc", now()), "◐");
        assert_eq!(cell(Some(&ci(CiState::Success, None, Some("2026-10-13T14:48:00Z"))), "abc", now()), "✓ 12min");
        assert_eq!(cell(Some(&ci(CiState::Success, None, None)), "abc", now()), "✓");
        assert_eq!(cell(Some(&ci(CiState::Failure, Some("rspec"), None)), "abc", now()), "✗ rspec");
        assert_eq!(cell(Some(&ci(CiState::Failure, None, None)), "abc", now()), "✗");
        assert_eq!(cell(Some(&ci(CiState::Unknown, None, None)), "abc", now()), "?");
        assert_eq!(cell(None, "abc", now()), "");
    }

    #[test]
    fn ci_cell_hides_ci_of_old_head() {
        assert_eq!(cell(Some(&ci(CiState::Success, None, None)), "novo", now()), "?");
    }

    #[test]
    fn icons() {
        assert_eq!(icon(Some(&ci(CiState::Pending, None, None))), '◐');
        assert_eq!(icon(Some(&ci(CiState::Success, None, None))), '✓');
        assert_eq!(icon(Some(&ci(CiState::Failure, None, None))), '✗');
        assert_eq!(icon(None), '·');
    }

    #[test]
    fn unknown_state_deserializes() {
        let c: Ci = serde_json::from_str(
            r#"{"sha":"a","state":"weird","failed_check":null,"finished_at":null,"observed_at":"2026-10-13T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(c.state, CiState::Unknown);
    }
}
