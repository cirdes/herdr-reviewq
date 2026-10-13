//! Coletas opcionais do GitHub: CI dos PRs de review e meus PRs. Nada aqui alimenta o ciclo de
//! reviews; falhas ficam no dado coletado, nunca no snapshot.

use crate::ci::{parse_pr_ci, Ci, CI_FRAGMENT};
use crate::github::{arr_at, bool_at, gh_json, graphql_data, other, str_at, FetchError, MAX_PAGES, SEARCH_CEILING};
use crate::runner::{Cmd, Runner};
use crate::state::MyPr;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// PRs por query de CI dos reviews.
const CI_BATCH: usize = 20;

fn review_ci_query(numbers: &[u64]) -> String {
    let fields: String = numbers.iter().map(|n| format!("p{n}: pullRequest(number: {n}) {{ {CI_FRAGMENT} }}\n")).collect();
    format!("query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{\n{fields}}} }}")
}

/// CI dos PRs `numbers` do repo, em lotes. Lote com erro é pulado; PR nulo ou sem checks fica
/// fora do mapa (quem chama mantém o valor anterior).
pub fn fetch_review_ci(
    runner: &dyn Runner,
    repo: &str,
    numbers: &[u64],
    now: DateTime<Utc>,
    deadline: Instant,
    cancel: &dyn Fn() -> bool,
) -> BTreeMap<u64, Ci> {
    let mut out = BTreeMap::new();
    let Some((owner, name)) = repo.split_once('/') else { return out };
    for chunk in numbers.chunks(CI_BATCH) {
        if cancel() {
            break;
        }
        let cmd = Cmd::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={}", review_ci_query(chunk)))
            .args(["-f".to_string(), format!("owner={owner}"), "-f".to_string(), format!("name={name}")]);
        let data = match gh_json(runner, cmd, deadline).and_then(|o| graphql_data(&o)) {
            Ok(d) => d,
            Err(e) => {
                crate::applog::info(&format!("CI de {repo}: lote falhou: {}", e.message));
                continue;
            }
        };
        for n in chunk {
            if let Some(ci) = data.pointer(&format!("/repository/p{n}")).filter(|v| v.is_object()).and_then(|v| parse_pr_ci(v, now)) {
                out.insert(*n, ci);
            }
        }
    }
    out
}

pub const MY_PRS_QUERY: &str = r#"query($q: String!, $owner: String!, $name: String!, $after: String) {
  repository(owner: $owner, name: $name) { id }
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes { __typename ... on PullRequest { number title url isDraft createdAt %CI% } }
  }
}"#;

pub const MY_PRS_COUNT_QUERY: &str = r#"query($q: String!) { search(query: $q, type: ISSUE, first: 1) { issueCount } }"#;

#[derive(Debug, Clone, PartialEq)]
pub struct MyPrsResult {
    pub prs: Vec<MyPr>,
    pub opened_24h: u64,
}

fn parse_my_pr(node: &Value, now: DateTime<Utc>) -> Result<MyPr, FetchError> {
    if node.get("__typename").and_then(Value::as_str) != Some("PullRequest") {
        return Err(other("resultado da busca não é um PR"));
    }
    let raw = str_at(node, "/createdAt")?;
    let created_at = DateTime::parse_from_rfc3339(&raw).map_err(|e| other(format!("createdAt inválido {raw}: {e}")))?.with_timezone(&Utc);
    Ok(MyPr {
        number: node.get("number").and_then(Value::as_u64).ok_or_else(|| other("PR sem number"))?,
        title: str_at(node, "/title")?,
        url: str_at(node, "/url")?,
        draft: bool_at(node, "/isDraft")?,
        created_at,
        head_sha: str_at(node, "/headRefOid")?,
        ci: parse_pr_ci(node, now),
    })
}

/// PRs abertos de `viewer` no repo e quantos ele criou desde `since`. Tudo ou nada: qualquer falha
/// (inclusive no contador) falha o repo.
pub fn fetch_my_prs(
    runner: &dyn Runner,
    repo: &str,
    viewer: &str,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    deadline: Instant,
    cancel: &dyn Fn() -> bool,
) -> Result<MyPrsResult, FetchError> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| other(format!("repo inválido {repo}")))?;
    let query = MY_PRS_QUERY.replace("%CI%", CI_FRAGMENT);
    let q = format!("repo:{repo} is:pr is:open author:{viewer}");
    let mut prs = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_cursors = BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        if cancel() {
            return Err(other("coleta cancelada (daemon parando)"));
        }
        let mut cmd = Cmd::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={query}"))
            .args(["-f".to_string(), format!("q={q}"), "-f".to_string(), format!("owner={owner}")])
            .args(["-f".to_string(), format!("name={name}")]);
        if let Some(c) = &cursor {
            cmd = cmd.arg("-f").arg(format!("after={c}"));
        }
        let data = graphql_data(&gh_json(runner, cmd, deadline)?)?;
        if data.get("repository").is_none_or(Value::is_null) {
            return Err(other("sem acesso ao repositório"));
        }
        let search = data.get("search").ok_or_else(|| other("resposta sem search"))?;
        let total = search.get("issueCount").and_then(Value::as_u64).ok_or_else(|| other("search sem issueCount"))?;
        if total > SEARCH_CEILING {
            return Err(other(format!("busca acima do teto do GitHub ({total} PRs)")));
        }
        for node in arr_at(search, "/nodes")? {
            let pr = parse_my_pr(node, now)?;
            if seen.insert(pr.number) {
                prs.push(pr);
            }
        }
        match bool_at(search, "/pageInfo/hasNextPage")? {
            false => {
                complete = true;
                break;
            }
            true => {
                let c = str_at(search, "/pageInfo/endCursor")?;
                if !seen_cursors.insert(c.clone()) {
                    return Err(other("cursor repetido na paginação"));
                }
                cursor = Some(c);
            }
        }
    }
    if !complete {
        return Err(other("paginação excedeu o limite"));
    }
    if cancel() {
        return Err(other("coleta cancelada (daemon parando)"));
    }
    let cq = format!("repo:{repo} is:pr author:{viewer} created:>={}", since.to_rfc3339_opts(SecondsFormat::Secs, true));
    let cmd = Cmd::new("gh").args(["api", "graphql", "-f"]).arg(format!("query={MY_PRS_COUNT_QUERY}")).arg("-f").arg(format!("q={cq}"));
    let data = graphql_data(&gh_json(runner, cmd, deadline)?)?;
    let opened_24h = data.pointer("/search/issueCount").and_then(Value::as_u64).ok_or_else(|| other("contador sem issueCount"))?;
    Ok(MyPrsResult { prs, opened_24h })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::CiState;
    use crate::runner::{FakeRunner, Output};
    use serde_json::{json, Value};
    use std::time::{Duration, Instant};

    fn now() -> DateTime<Utc> {
        "2026-10-13T15:00:00Z".parse().unwrap()
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn never() -> bool {
        false
    }

    fn ci_node(sha: &str, state: &str) -> Value {
        json!({"headRefOid": sha, "commits": {"nodes": [{"commit": {"oid": sha,
            "statusCheckRollup": {"state": state, "contexts": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}}]}})
    }

    fn batch(nums: &[u64], state: &str) -> String {
        let mut repo = serde_json::Map::new();
        for n in nums {
            repo.insert(format!("p{n}"), ci_node(&format!("s{n}"), state));
        }
        json!({"data": {"repository": repo}}).to_string()
    }

    #[test]
    fn review_ci_batches_by_twenty_and_uses_aliases() {
        let fake = FakeRunner::new();
        let nums: Vec<u64> = (1..=25).collect();
        fake.on("p21: pullRequest(number: 21)", Output::ok(&batch(&nums[20..], "PENDING")));
        fake.on("p1: pullRequest(number: 1)", Output::ok(&batch(&nums[..20], "SUCCESS")));
        let got = fetch_review_ci(&fake, "o/r", &nums, now(), far(), &never);
        assert_eq!(fake.lines().len(), 2);
        assert!(fake.lines().iter().all(|l| l.contains("owner=o") && l.contains("name=r")));
        assert!(!fake.lines()[0].contains("p21:"), "lote 1 só com 20 PRs");
        assert_eq!(got.len(), 25);
        assert_eq!(got[&1].state, CiState::Success);
        assert_eq!(got[&1].sha, "s1");
        assert_eq!(got[&25].state, CiState::Pending);
    }

    #[test]
    fn review_ci_skips_failed_batch_and_null_pr() {
        let fake = FakeRunner::new();
        let nums: Vec<u64> = (1..=21).collect();
        fake.on("p21:", Output::fail(1, "boom"));
        let mut ok: Value = serde_json::from_str(&batch(&nums[..20], "SUCCESS")).unwrap();
        ok["data"]["repository"]["p2"] = Value::Null;
        ok["data"]["repository"]["p3"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = Value::Null;
        fake.on("p1:", Output::ok(&ok.to_string()));
        let got = fetch_review_ci(&fake, "o/r", &nums, now(), far(), &never);
        assert!(got.contains_key(&1));
        assert!(!got.contains_key(&2), "PR nulo fica fora");
        assert!(!got.contains_key(&3), "PR sem checks fica fora");
        assert!(!got.contains_key(&21), "lote com erro fica fora");

        let errs = FakeRunner::new();
        errs.on("p1:", Output::ok(r#"{"errors":[{"message":"x"}],"data":null}"#));
        assert!(fetch_review_ci(&errs, "o/r", &[1], now(), far(), &never).is_empty());
    }

    #[test]
    fn review_ci_stops_when_cancelled() {
        let fake = FakeRunner::new();
        assert!(fetch_review_ci(&fake, "o/r", &[1, 2], now(), far(), &|| true).is_empty());
        assert!(fake.lines().is_empty());
    }

    fn mine_node(n: u64, draft: bool) -> Value {
        let mut v = ci_node(&format!("h{n}"), "FAILURE");
        let o = v.as_object_mut().unwrap();
        o.insert("__typename".into(), json!("PullRequest"));
        o.insert("number".into(), json!(n));
        o.insert("title".into(), json!(format!("Meu {n}")));
        o.insert("url".into(), json!(format!("https://github.com/o/r/pull/{n}")));
        o.insert("isDraft".into(), json!(draft));
        o.insert("createdAt".into(), json!("2026-10-13T10:00:00Z"));
        v
    }

    fn page(nodes: Vec<Value>, next: Option<&str>, count: u64) -> String {
        json!({"data": {"repository": {"id": "R"}, "search": {"issueCount": count,
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}, "nodes": nodes}}})
        .to_string()
    }

    fn count(n: u64) -> String {
        json!({"data": {"search": {"issueCount": n}}}).to_string()
    }

    fn since() -> DateTime<Utc> {
        now() - chrono::Duration::hours(24)
    }

    #[test]
    fn my_prs_lists_pages_and_counts() {
        let fake = FakeRunner::new();
        fake.on("after=C1", Output::ok(&page(vec![mine_node(2, false)], None, 2)));
        fake.on("is:open author:cirdes", Output::ok(&page(vec![mine_node(1, true)], Some("C1"), 2)));
        fake.on("created:>=2026-10-12T15:00:00Z", Output::ok(&count(3)));
        let r = fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).unwrap();
        assert_eq!(r.opened_24h, 3);
        assert_eq!(r.prs.iter().map(|p| p.number).collect::<Vec<_>>(), vec![1, 2]);
        assert!(r.prs[0].draft);
        assert!(!r.prs[1].draft);
        assert_eq!(r.prs[0].head_sha, "h1");
        assert_eq!(r.prs[0].title, "Meu 1");
        assert_eq!(r.prs[0].ci.as_ref().unwrap().state, CiState::Failure);
        assert!(fake.lines()[0].contains("q=repo:o/r is:pr is:open author:cirdes"));
        assert!(fake.lines()[2].contains("q=repo:o/r is:pr author:cirdes created:>=2026-10-12T15:00:00Z"));
    }

    #[test]
    fn my_prs_repeated_cursor_fails() {
        let fake = FakeRunner::new();
        fake.on("after=C1", Output::ok(&page(vec![], Some("C1"), 1)));
        fake.on("is:open", Output::ok(&page(vec![mine_node(1, false)], Some("C1"), 1)));
        let e = fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).unwrap_err();
        assert!(e.message.contains("cursor repetido"), "{}", e.message);
    }

    #[test]
    fn my_prs_over_search_ceiling_fails() {
        let fake = FakeRunner::new();
        fake.on("is:open", Output::ok(&page(vec![], None, 1001)));
        assert!(fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).unwrap_err().message.contains("teto"));
    }

    #[test]
    fn my_prs_without_repo_access_fails() {
        let fake = FakeRunner::new();
        let mut v: Value = serde_json::from_str(&page(vec![], None, 0)).unwrap();
        v["data"]["repository"] = Value::Null;
        fake.on("is:open", Output::ok(&v.to_string()));
        assert!(fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).unwrap_err().message.contains("sem acesso"));
    }

    #[test]
    fn my_prs_dedups_by_number() {
        let fake = FakeRunner::new();
        fake.on("after=C1", Output::ok(&page(vec![mine_node(1, false)], None, 1)));
        fake.on("is:open", Output::ok(&page(vec![mine_node(1, false)], Some("C1"), 1)));
        fake.on("created:>=", Output::ok(&count(1)));
        assert_eq!(fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).unwrap().prs.len(), 1);
    }

    #[test]
    fn my_prs_count_failure_fails_repo() {
        let fake = FakeRunner::new();
        fake.on("is:open", Output::ok(&page(vec![mine_node(1, false)], None, 1)));
        fake.on("created:>=", Output::fail(1, "boom"));
        assert!(fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).is_err());
    }

    #[test]
    fn my_prs_stops_when_cancelled() {
        let fake = FakeRunner::new();
        assert!(fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &|| true).is_err());
        assert!(fake.lines().is_empty());
    }

    #[test]
    fn my_prs_rejects_non_pr_node() {
        let fake = FakeRunner::new();
        fake.on("is:open", Output::ok(&page(vec![json!({"__typename": "Issue"})], None, 1)));
        assert!(fetch_my_prs(&fake, "o/r", "cirdes", since(), now(), far(), &never).is_err());
    }
}

#[cfg(test)]
mod dump {
    /// Grava as queries em `$REVIEWQ_DUMP` para validar contra o GitHub real (manual).
    #[test]
    #[ignore]
    fn dump_queries() {
        let dir = std::path::PathBuf::from(std::env::var("REVIEWQ_DUMP").unwrap());
        std::fs::write(dir.join("ci.graphql"), super::review_ci_query(&[1, 2])).unwrap();
        std::fs::write(dir.join("mine.graphql"), super::MY_PRS_QUERY.replace("%CI%", crate::ci::CI_FRAGMENT)).unwrap();
        std::fs::write(dir.join("count.graphql"), super::MY_PRS_COUNT_QUERY).unwrap();
    }
}
