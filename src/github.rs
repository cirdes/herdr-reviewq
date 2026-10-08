use crate::runner::{Cmd, Runner};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const PENDING_QUERY: &str = r#"query($q: String!, $owner: String!, $name: String!, $after: String) {
  viewer { login }
  repository(owner: $owner, name: $name) { id }
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      __typename
      ... on PullRequest {
        number title url isCrossRepository headRefName headRefOid
        author { login }
        reviewRequests(first: 100) {
          pageInfo { hasNextPage }
          nodes { requestedReviewer { __typename ... on User { login } ... on Team { name } } }
        }
        timelineItems(itemTypes: [REVIEW_REQUESTED_EVENT], last: 100) {
          nodes { ... on ReviewRequestedEvent { createdAt requestedReviewer { ... on User { login } } } }
        }
      }
    }
  }
}"#;

const MAX_PAGES: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct PendingPr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub url: String,
    pub head_ref: String,
    pub head_sha: String,
    pub is_fork: bool,
    pub last_request_event_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ErrorKind {
    Auth,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RepoSnapshot {
    Complete(Vec<PendingPr>),
    Failed(FetchError),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub repos: BTreeMap<String, RepoSnapshot>,
}

#[derive(Debug)]
pub struct Page {
    pub prs: Vec<PendingPr>,
    pub next_cursor: Option<String>,
}

fn other(m: impl Into<String>) -> FetchError {
    FetchError { kind: ErrorKind::Other, message: m.into() }
}

fn classify(text: &str) -> FetchError {
    let low = text.to_lowercase();
    let kind = if ["auth login", "401", "bad credentials", "not logged"].iter().any(|n| low.contains(n)) {
        ErrorKind::Auth
    } else {
        ErrorKind::Other
    };
    FetchError { kind, message: text.trim().to_string() }
}

fn str_at(v: &Value, ptr: &str) -> Result<String, FetchError> {
    v.pointer(ptr).and_then(Value::as_str).map(String::from).ok_or_else(|| other(format!("campo ausente: {ptr}")))
}

fn arr_at<'v>(v: &'v Value, ptr: &str) -> Result<&'v Vec<Value>, FetchError> {
    v.pointer(ptr).and_then(Value::as_array).ok_or_else(|| other(format!("campo ausente: {ptr}")))
}

fn bool_at(v: &Value, ptr: &str) -> Result<bool, FetchError> {
    v.pointer(ptr).and_then(Value::as_bool).ok_or_else(|| other(format!("campo ausente: {ptr}")))
}

pub fn parse_page(json: &str, repo: &str) -> Result<Page, FetchError> {
    let v: Value = serde_json::from_str(json).map_err(|e| other(format!("JSON inválido do gh: {e}")))?;
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        if !errs.is_empty() {
            let msg = errs.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect::<Vec<_>>().join("; ");
            return Err(other(format!("GraphQL: {msg}")));
        }
    }
    let data = v.get("data").filter(|d| !d.is_null()).ok_or_else(|| other("resposta sem data"))?;
    let viewer = str_at(data, "/viewer/login")?;
    if data.get("repository").is_none_or(Value::is_null) {
        return Err(other(format!("sem acesso ao repositório {repo}")));
    }
    let search = data.get("search").ok_or_else(|| other("resposta sem search"))?;
    let mut prs = Vec::new();
    for node in arr_at(search, "/nodes")? {
        if node.get("__typename").and_then(Value::as_str) != Some("PullRequest") {
            return Err(other("resultado da busca não é um PR"));
        }
        let number = node.get("number").and_then(Value::as_u64).ok_or_else(|| other("PR sem number"))?;
        if bool_at(node, "/reviewRequests/pageInfo/hasNextPage")? {
            return Err(other(format!("reviewRequests truncado no PR #{number}")));
        }
        let direct = arr_at(node, "/reviewRequests/nodes")?
            .iter()
            .any(|n| n.pointer("/requestedReviewer/login").and_then(Value::as_str) == Some(viewer.as_str()));
        let mut last_request_event_at: Option<DateTime<Utc>> = None;
        for ev in arr_at(node, "/timelineItems/nodes")? {
            if ev.pointer("/requestedReviewer/login").and_then(Value::as_str) != Some(viewer.as_str()) {
                continue;
            }
            let raw = str_at(ev, "/createdAt")?;
            let at = DateTime::parse_from_rfc3339(&raw)
                .map_err(|e| other(format!("createdAt inválido {raw}: {e}")))?
                .with_timezone(&Utc);
            last_request_event_at = last_request_event_at.max(Some(at));
        }
        if !direct {
            continue;
        }
        prs.push(PendingPr {
            repo: repo.to_string(),
            number,
            title: str_at(node, "/title")?,
            author: node.pointer("/author/login").and_then(Value::as_str).unwrap_or("ghost").to_string(),
            url: str_at(node, "/url")?,
            head_ref: str_at(node, "/headRefName")?,
            head_sha: str_at(node, "/headRefOid")?,
            is_fork: bool_at(node, "/isCrossRepository")?,
            last_request_event_at,
        });
    }
    let next_cursor = if bool_at(search, "/pageInfo/hasNextPage")? {
        Some(str_at(search, "/pageInfo/endCursor")?)
    } else {
        None
    };
    Ok(Page { prs, next_cursor })
}

/// Busca todas as páginas. Qualquer falha vira `Failed`: nunca devolve lista parcial.
pub fn fetch_pending(runner: &dyn Runner, repo: &str) -> RepoSnapshot {
    let Some((owner, name)) = repo.split_once('/') else {
        return RepoSnapshot::Failed(other(format!("repo inválido {repo}")));
    };
    let search = format!("repo:{repo} is:pr is:open review-requested:@me");
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut cmd = Cmd::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={PENDING_QUERY}"))
            .arg("-f")
            .arg(format!("q={search}"))
            .arg("-f")
            .arg(format!("owner={owner}"))
            .arg("-f")
            .arg(format!("name={name}"))
            .timeout(Duration::from_secs(60));
        if let Some(c) = &cursor {
            cmd = cmd.arg("-f").arg(format!("after={c}"));
        }
        let out = match runner.run(&cmd) {
            Ok(o) => o,
            Err(e) => return RepoSnapshot::Failed(other(e.to_string())),
        };
        if out.timed_out {
            return RepoSnapshot::Failed(other("gh excedeu o tempo"));
        }
        if !out.success() {
            return RepoSnapshot::Failed(classify(&format!("{} {}", out.stderr, out.stdout)));
        }
        match parse_page(&out.stdout, repo) {
            Ok(page) => {
                all.extend(page.prs);
                match page.next_cursor {
                    Some(c) => cursor = Some(c),
                    None => return RepoSnapshot::Complete(all),
                }
            }
            Err(e) => return RepoSnapshot::Failed(e),
        }
    }
    RepoSnapshot::Failed(other("paginação excedeu o limite"))
}

pub const REVIEWED_QUERY: &str = r#"query($q: String!, $owner: String!, $name: String!, $author: String!, $after: String) {
  repository(owner: $owner, name: $name) { id }
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      __typename
      ... on PullRequest {
        number
        reviews(author: $author, last: 50) {
          pageInfo { hasPreviousPage startCursor }
          nodes { submittedAt state }
        }
      }
    }
  }
}"#;

pub const OLDER_REVIEWS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $author: String!, $before: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviews(author: $author, last: 50, before: $before) {
        pageInfo { hasPreviousPage startCursor }
        nodes { submittedAt state }
      }
    }
  }
}"#;

const SEARCH_CEILING: u64 = 1000;
/// Páginas de reviews por PR, contando a página inicial da busca.
const MAX_REVIEW_PAGES: usize = 10;
const GH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start: DateTime<Utc>,
    pub as_of: DateTime<Utc>,
    pub search_since: chrono::NaiveDate,
}

#[derive(Debug)]
pub struct ReviewedPr {
    pub number: u64,
    pub counted: bool,
    /// Cursor para buscar reviews mais antigos, só quando ainda não contou e a página
    /// atual começa dentro da janela.
    pub older_cursor: Option<String>,
}

#[derive(Debug)]
pub struct ReviewedPage {
    pub prs: Vec<ReviewedPr>,
    pub next_cursor: Option<String>,
}

/// Envelope GraphQL compartilhado: rejeita JSON inválido, `errors` não vazio e `data` nulo.
fn graphql_data(json: &str) -> Result<Value, FetchError> {
    let mut v: Value = serde_json::from_str(json).map_err(|e| other(format!("JSON inválido do gh: {e}")))?;
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        if !errs.is_empty() {
            let msg = errs.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect::<Vec<_>>().join("; ");
            return Err(other(format!("GraphQL: {msg}")));
        }
    }
    match v.get_mut("data").map(Value::take) {
        Some(d) if !d.is_null() => Ok(d),
        _ => Err(other("resposta sem data")),
    }
}

/// Um review entra no contador?
pub fn review_counts(state: &str, submitted: Option<DateTime<Utc>>, w: &Window) -> Result<bool, FetchError> {
    let counted_state = match state {
        "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED" => true,
        "PENDING" => false,
        other_state => return Err(other(format!("estado de review desconhecido: {other_state}"))),
    };
    Ok(counted_state && submitted.is_some_and(|t| t >= w.start && t <= w.as_of))
}

/// Avalia uma página de reviews: (contou?, cursor para mais antigos se necessário).
fn eval_reviews(reviews: &Value, w: &Window) -> Result<(bool, Option<String>), FetchError> {
    let nodes = arr_at(reviews, "/nodes")?;
    let mut counted = false;
    let mut earliest: Option<DateTime<Utc>> = None;
    for n in nodes {
        let state = str_at(n, "/state")?;
        let submitted = match n.get("submittedAt") {
            None => return Err(other("review sem submittedAt")),
            Some(Value::Null) if state == "PENDING" => None,
            Some(Value::Null) => return Err(other(format!("review {state} sem submittedAt"))),
            Some(Value::String(raw)) => Some(
                DateTime::parse_from_rfc3339(raw)
                    .map_err(|e| other(format!("submittedAt inválido {raw}: {e}")))?
                    .with_timezone(&Utc),
            ),
            Some(_) => return Err(other("submittedAt de tipo inválido")),
        };
        if let Some(t) = submitted {
            earliest = Some(earliest.map_or(t, |e: DateTime<Utc>| e.min(t)));
        }
        counted |= review_counts(&state, submitted, w)?;
    }
    let has_prev = bool_at(reviews, "/pageInfo/hasPreviousPage")?;
    let needs_older = !counted && has_prev && earliest.is_none_or(|e| e >= w.start);
    let cursor = if needs_older { Some(str_at(reviews, "/pageInfo/startCursor")?) } else { None };
    Ok((counted, cursor))
}

pub fn parse_reviewed_page(json: &str, w: &Window) -> Result<ReviewedPage, FetchError> {
    let data = graphql_data(json)?;
    if data.get("repository").is_none_or(Value::is_null) {
        return Err(other("sem acesso ao repositório"));
    }
    let search = data.get("search").ok_or_else(|| other("resposta sem search"))?;
    let count = search.get("issueCount").and_then(Value::as_u64).ok_or_else(|| other("search sem issueCount"))?;
    if count > SEARCH_CEILING {
        return Err(other(format!("busca acima do teto do GitHub ({count} PRs)")));
    }
    let mut prs = Vec::new();
    for node in arr_at(search, "/nodes")? {
        if node.get("__typename").and_then(Value::as_str) != Some("PullRequest") {
            return Err(other("resultado da busca não é um PR"));
        }
        let number = node.get("number").and_then(Value::as_u64).ok_or_else(|| other("PR sem number"))?;
        let reviews = node.get("reviews").ok_or_else(|| other(format!("PR #{number} sem reviews")))?;
        let (counted, older_cursor) = eval_reviews(reviews, w)?;
        prs.push(ReviewedPr { number, counted, older_cursor });
    }
    let next_cursor = if bool_at(search, "/pageInfo/hasNextPage")? { Some(str_at(search, "/pageInfo/endCursor")?) } else { None };
    Ok(ReviewedPage { prs, next_cursor })
}

/// Uma chamada `gh` sob o prazo compartilhado: `timeout = min(30 s, restante)`;
/// prazo checado antes e depois da chamada (resposta tardia = falha).
fn gh_json(runner: &dyn Runner, cmd: Cmd, deadline: Instant) -> Result<String, FetchError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(other("coleta excedeu o orçamento"));
    }
    let out = runner.run(&cmd.timeout(GH_TIMEOUT.min(remaining))).map_err(|e| other(e.to_string()))?;
    if Instant::now() >= deadline {
        return Err(other("coleta excedeu o orçamento"));
    }
    if out.timed_out {
        return Err(other("gh excedeu o tempo"));
    }
    if !out.success() {
        return Err(classify(&format!("{} {}", out.stderr, out.stdout)));
    }
    Ok(out.stdout)
}

pub fn fetch_viewer(runner: &dyn Runner, deadline: Instant) -> Result<String, FetchError> {
    let out = gh_json(runner, Cmd::new("gh").args(["api", "graphql", "-f", "query={ viewer { login } }"]), deadline)?;
    str_at(&graphql_data(&out)?, "/viewer/login")
}

#[allow(clippy::too_many_arguments)]
fn older_counts(
    runner: &dyn Runner,
    repo: &str,
    number: u64,
    viewer: &str,
    first: String,
    w: &Window,
    deadline: Instant,
    cancel: &dyn Fn() -> bool,
) -> Result<bool, FetchError> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| other(format!("repo inválido {repo}")))?;
    let mut seen = std::collections::BTreeSet::from([first.clone()]);
    let mut cursor = first;
    // a página inicial da busca já é a 1ª das MAX_REVIEW_PAGES
    for _ in 1..MAX_REVIEW_PAGES {
        if cancel() {
            return Err(other("coleta cancelada (daemon parando)"));
        }
        let cmd = Cmd::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={OLDER_REVIEWS_QUERY}"))
            .args(["-f".to_string(), format!("owner={owner}"), "-f".to_string(), format!("name={name}")])
            .args(["-F".to_string(), format!("number={number}"), "-f".to_string(), format!("author={viewer}")])
            .args(["-f".to_string(), format!("before={cursor}")]);
        let data = graphql_data(&gh_json(runner, cmd, deadline)?)?;
        let repository = data.get("repository").filter(|r| !r.is_null()).ok_or_else(|| other("sem acesso ao repositório"))?;
        let pr = repository.get("pullRequest").filter(|p| !p.is_null()).ok_or_else(|| other(format!("PR #{number} não encontrado")))?;
        let reviews = pr.get("reviews").ok_or_else(|| other("resposta sem reviews"))?;
        let (counted, next) = eval_reviews(reviews, w)?;
        if counted {
            return Ok(true);
        }
        match next {
            Some(c) if !seen.insert(c.clone()) => return Err(other(format!("cursor repetido nos reviews de {repo}#{number}"))),
            Some(c) => cursor = c,
            None => return Ok(false),
        }
    }
    Err(other(format!("histórico truncado em {repo}#{number}")))
}

/// Número de PRs distintos do repo com review submetido pelo viewer na janela.
pub fn count_reviews_today(
    runner: &dyn Runner,
    repo: &str,
    viewer: &str,
    w: &Window,
    deadline: Instant,
    cancel: &dyn Fn() -> bool,
) -> Result<u32, FetchError> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| other(format!("repo inválido {repo}")))?;
    let q = format!("repo:{repo} is:pr reviewed-by:{viewer} updated:>={}", w.search_since.format("%Y-%m-%d"));
    let mut counted: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut seen_cursors = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        if cancel() {
            return Err(other("coleta cancelada (daemon parando)"));
        }
        let mut cmd = Cmd::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={REVIEWED_QUERY}"))
            .args(["-f".to_string(), format!("q={q}"), "-f".to_string(), format!("owner={owner}")])
            .args(["-f".to_string(), format!("name={name}"), "-f".to_string(), format!("author={viewer}")]);
        if let Some(c) = &cursor {
            cmd = cmd.arg("-f").arg(format!("after={c}"));
        }
        let page = parse_reviewed_page(&gh_json(runner, cmd, deadline)?, w)?;
        for pr in page.prs {
            if counted.contains(&pr.number) {
                continue;
            }
            let yes = pr.counted
                || match pr.older_cursor {
                    Some(c) => older_counts(runner, repo, pr.number, viewer, c, w, deadline, cancel)?,
                    None => false,
                };
            if yes {
                counted.insert(pr.number);
            }
        }
        match page.next_cursor {
            Some(c) if !seen_cursors.insert(c.clone()) => return Err(other("cursor repetido na paginação")),
            Some(c) => cursor = Some(c),
            None => return Ok(counted.len() as u32),
        }
    }
    Err(other("paginação excedeu o limite"))
}

#[cfg(test)]
pub(crate) fn pr_node(n: u64, head: &str, sha: &str, requested: &str, fork: bool) -> Value {
    serde_json::json!({
        "__typename": "PullRequest",
        "number": n, "title": format!("PR {n}"), "url": format!("https://github.com/o/r/pull/{n}"),
        "isCrossRepository": fork, "headRefName": head, "headRefOid": sha, "author": {"login": "ana"},
        "reviewRequests": {"pageInfo": {"hasNextPage": false},
            "nodes": [{"requestedReviewer": {"__typename": "User", "login": requested}}]},
        "timelineItems": {"nodes": [{"createdAt": "2026-10-07T10:00:00Z", "requestedReviewer": {"login": requested}}]}
    })
}

#[cfg(test)]
pub(crate) fn page_json(viewer: &str, prs: &[(u64, &str, &str)], next: Option<&str>) -> String {
    let nodes: Vec<Value> = prs.iter().map(|(n, head, sha)| pr_node(*n, head, sha, viewer, false)).collect();
    serde_json::json!({"data": {"viewer": {"login": viewer}, "repository": {"id": "R1"}, "search": {
        "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}, "nodes": nodes}}})
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{FakeRunner, Output};
    use serde_json::json;

    fn body(nodes: Vec<Value>) -> String {
        json!({"data": {"viewer": {"login": "cirdes"}, "repository": {"id": "R1"},
            "search": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": nodes}}})
        .to_string()
    }

    #[test]
    fn keeps_only_direct_requests_and_flags_forks() {
        let mut team = pr_node(2, "h", "s", "x", false);
        team["reviewRequests"]["nodes"] = json!([{"requestedReviewer": {"__typename": "Team", "name": "core"}}]);
        let page = parse_page(&body(vec![pr_node(1, "feat/x", "aaa", "cirdes", false), team, pr_node(3, "f", "ccc", "cirdes", true)]), "o/r").unwrap();
        assert_eq!(page.prs.len(), 2);
        assert_eq!(page.prs[0].number, 1);
        assert_eq!(page.prs[0].head_ref, "feat/x");
        assert!(!page.prs[0].is_fork);
        assert!(page.prs[1].is_fork);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn last_request_event_is_latest_for_viewer_only() {
        let mut node = pr_node(1, "h", "s", "cirdes", false);
        node["timelineItems"]["nodes"] = json!([
            {"createdAt": "2026-10-07T10:00:00Z", "requestedReviewer": {"login": "cirdes"}},
            {"createdAt": "2026-10-07T12:00:00Z", "requestedReviewer": {"login": "cirdes"}},
            {"createdAt": "2026-10-07T13:00:00Z", "requestedReviewer": {"login": "outra"}}]);
        let page = parse_page(&body(vec![node]), "o/r").unwrap();
        assert_eq!(page.prs[0].last_request_event_at.unwrap().to_rfc3339(), "2026-10-07T12:00:00+00:00");
    }

    #[test]
    fn graphql_errors_fail_even_with_data() {
        let mut v: Value = serde_json::from_str(&body(vec![])).unwrap();
        v["errors"] = json!([{"message": "timeout"}]);
        let err = parse_page(&v.to_string(), "o/r").unwrap_err();
        assert!(err.message.contains("timeout"));
    }

    #[test]
    fn incomplete_responses_fail() {
        assert!(parse_page(r#"{"data": null}"#, "o/r").is_err());

        let mut v: Value = serde_json::from_str(&body(vec![])).unwrap();
        v["data"]["repository"] = Value::Null;
        assert!(parse_page(&v.to_string(), "o/r").unwrap_err().message.contains("sem acesso"));

        let mut truncated = pr_node(1, "h", "s", "cirdes", false);
        truncated["reviewRequests"]["pageInfo"]["hasNextPage"] = json!(true);
        assert!(parse_page(&body(vec![truncated]), "o/r").is_err());

        let mut no_requests = pr_node(1, "h", "s", "cirdes", false);
        no_requests.as_object_mut().unwrap().remove("reviewRequests");
        assert!(parse_page(&body(vec![no_requests]), "o/r").is_err());

        let mut not_pr = pr_node(1, "h", "s", "cirdes", false);
        not_pr["__typename"] = json!("Issue");
        assert!(parse_page(&body(vec![not_pr]), "o/r").is_err());

        let mut bad_time = pr_node(1, "h", "s", "cirdes", false);
        bad_time["timelineItems"]["nodes"][0]["createdAt"] = json!("ontem");
        assert!(parse_page(&body(vec![bad_time]), "o/r").is_err());

        let mut no_page_info: Value = serde_json::from_str(&body(vec![])).unwrap();
        no_page_info["data"]["search"].as_object_mut().unwrap().remove("pageInfo");
        assert!(parse_page(&no_page_info.to_string(), "o/r").is_err());
    }

    #[test]
    fn fetch_follows_pagination_and_passes_repo() {
        let fake = FakeRunner::new();
        fake.on("after=CUR1", Output::ok(&page_json("cirdes", &[(2, "b", "s2")], None)));
        fake.on("q=repo:o/r", Output::ok(&page_json("cirdes", &[(1, "a", "s1")], Some("CUR1"))));
        match fetch_pending(&fake, "o/r") {
            RepoSnapshot::Complete(prs) => assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), vec![1, 2]),
            other => panic!("esperava Complete, veio {other:?}"),
        }
        let first = fake.lines()[0].clone();
        assert!(first.contains("owner=o") && first.contains("name=r"));
        assert_eq!(fake.calls.borrow().len(), 2);
    }

    #[test]
    fn auth_failure_is_classified() {
        let fake = FakeRunner::new();
        fake.on("graphql", Output::fail(1, "To get started with GitHub CLI, please run:  gh auth login"));
        match fetch_pending(&fake, "o/r") {
            RepoSnapshot::Failed(e) => assert_eq!(e.kind, ErrorKind::Auth),
            other => panic!("esperava Failed, veio {other:?}"),
        }
    }

    #[test]
    fn non_zero_exit_never_becomes_empty_list() {
        let fake = FakeRunner::new();
        fake.on("graphql", Output { status: Some(1), stdout: page_json("cirdes", &[], None), ..Default::default() });
        assert!(matches!(fetch_pending(&fake, "o/r"), RepoSnapshot::Failed(_)));
    }

    use chrono::TimeZone;

    fn window() -> Window {
        Window {
            start: Utc.with_ymd_and_hms(2026, 10, 7, 3, 0, 0).unwrap(),
            as_of: Utc.with_ymd_and_hms(2026, 10, 7, 20, 0, 0).unwrap(),
            search_since: chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap(),
        }
    }

    fn far() -> std::time::Instant {
        std::time::Instant::now() + Duration::from_secs(30)
    }

    fn reviewed_node(n: u64, reviews: Value, has_prev: bool) -> Value {
        json!({"__typename": "PullRequest", "number": n,
               "reviews": {"pageInfo": {"hasPreviousPage": has_prev, "startCursor": if has_prev { json!("C0") } else { Value::Null }},
                           "nodes": reviews}})
    }

    fn reviewed_body(count: u64, nodes: Vec<Value>, next: Option<&str>) -> String {
        json!({"data": {"repository": {"id": "R1"}, "search": {"issueCount": count,
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}, "nodes": nodes}}}).to_string()
    }

    fn older_body(reviews: Value, has_prev: bool, start: Option<&str>) -> String {
        json!({"data": {"repository": {"pullRequest": {"reviews": {
            "pageInfo": {"hasPreviousPage": has_prev, "startCursor": start}, "nodes": reviews}}}}}).to_string()
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn review_states_and_window() {
        let w = window();
        let at = |h: u32, m: u32, s: u32| Some(Utc.with_ymd_and_hms(2026, 10, 7, h, m, s).unwrap());
        assert!(review_counts("APPROVED", at(3, 0, 0), &w).unwrap()); // 00:00:00 SP conta
        assert!(!review_counts("APPROVED", at(2, 59, 59), &w).unwrap()); // 23:59:59 de ontem não
        assert!(review_counts("DISMISSED", at(12, 0, 0), &w).unwrap());
        assert!(review_counts("COMMENTED", at(12, 0, 0), &w).unwrap());
        assert!(review_counts("CHANGES_REQUESTED", at(12, 0, 0), &w).unwrap());
        assert!(!review_counts("PENDING", at(12, 0, 0), &w).unwrap());
        assert!(!review_counts("APPROVED", None, &w).unwrap());
        assert!(!review_counts("APPROVED", at(21, 0, 0), &w).unwrap()); // depois do as_of
        assert!(review_counts("WHATEVER", at(12, 0, 0), &w).is_err());
    }

    #[test]
    fn reviewed_page_parses_and_dedupes_later() {
        let nodes = vec![
            reviewed_node(1, json!([{"submittedAt": "2026-10-07T12:00:00Z", "state": "APPROVED"},
                                    {"submittedAt": "2026-10-07T13:00:00Z", "state": "COMMENTED"}]), false),
            reviewed_node(2, json!([{"submittedAt": "2026-10-06T12:00:00Z", "state": "APPROVED"}]), false),
        ];
        let page = parse_reviewed_page(&reviewed_body(2, nodes, None), &window()).unwrap();
        assert_eq!(page.prs.len(), 2);
        assert!(page.prs[0].counted && !page.prs[1].counted);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn reviewed_page_limits() {
        assert!(parse_reviewed_page(&reviewed_body(1001, vec![], None), &window()).unwrap_err().message.contains("teto"));
        let mut v: Value = serde_json::from_str(&reviewed_body(0, vec![], None)).unwrap();
        v["data"]["repository"] = Value::Null;
        assert!(parse_reviewed_page(&v.to_string(), &window()).is_err());
    }

    #[test]
    fn submitted_at_is_strict() {
        let w = window();
        let page = |review: Value| parse_reviewed_page(&reviewed_body(1, vec![reviewed_node(1, json!([review]), false)], None), &w);
        // ausente
        assert!(page(json!({"state": "APPROVED"})).is_err());
        // tipo errado
        assert!(page(json!({"submittedAt": 123, "state": "APPROVED"})).is_err());
        // null com estado submetido
        assert!(page(json!({"submittedAt": null, "state": "APPROVED"})).is_err());
        // null com PENDING é aceito e não conta
        assert!(!page(json!({"submittedAt": null, "state": "PENDING"})).unwrap().prs[0].counted);
        // CHANGES_REQUESTED conta
        assert!(page(json!({"submittedAt": "2026-10-07T12:00:00Z", "state": "CHANGES_REQUESTED"})).unwrap().prs[0].counted);
    }

    #[test]
    fn needs_older_reviews_only_when_page_starts_inside_window() {
        let n = reviewed_node(5, json!([{"submittedAt": "2026-10-07T10:00:00Z", "state": "COMMENTED"}]), true);
        let page = parse_reviewed_page(&reviewed_body(1, vec![n], None), &window()).unwrap();
        // já contou com o que tem: não precisa de mais
        assert!(page.prs[0].counted && page.prs[0].older_cursor.is_none());
        let n = reviewed_node(6, json!([{"submittedAt": "2026-10-07T10:00:00Z", "state": "PENDING"}]), true);
        let page = parse_reviewed_page(&reviewed_body(1, vec![n], None), &window()).unwrap();
        // não contou e a página começa dentro da janela: precisa buscar mais antigos
        assert_eq!(page.prs[0].older_cursor.as_deref(), Some("C0"));
    }

    #[test]
    fn count_follows_pages_and_dedupes() {
        let fake = FakeRunner::new();
        let n1 = reviewed_node(1, json!([{"submittedAt": "2026-10-07T12:00:00Z", "state": "APPROVED"}]), false);
        let n1_again = n1.clone();
        let n2 = reviewed_node(2, json!([{"submittedAt": "2026-10-07T12:00:00Z", "state": "DISMISSED"}]), false);
        fake.on("after=P2", Output::ok(&reviewed_body(2, vec![n1_again, n2], None)));
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(2, vec![n1], Some("P2"))));
        assert_eq!(count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never).unwrap(), 2);
        assert!(fake.lines()[0].contains("updated:>=2026-10-06"));
    }

    #[test]
    fn count_fetches_older_reviews_of_one_pr() {
        let fake = FakeRunner::new();
        let n = reviewed_node(9, json!([{"submittedAt": "2026-10-07T10:00:00Z", "state": "PENDING"}]), true);
        fake.on("before=C0", Output::ok(&older_body(json!([{"submittedAt": "2026-10-07T09:00:00Z", "state": "APPROVED"}]), false, None)));
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(1, vec![n], None)));
        assert_eq!(count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never).unwrap(), 1);
    }

    /// Cadeia de `older` páginas: as `last` primeiras só PENDING (dentro da janela, com mais antigas),
    /// a última com APPROVED. Devolve o runner pronto e o PR inicial.
    fn older_chain(older_pages: usize) -> (FakeRunner, Value) {
        let fake = FakeRunner::new();
        let pending = json!([{"submittedAt": "2026-10-07T10:00:00Z", "state": "PENDING"}]);
        for i in 0..older_pages {
            let cur = format!("K{i}z");
            let last = i + 1 == older_pages;
            let body = if last {
                older_body(json!([{"submittedAt": "2026-10-07T09:00:00Z", "state": "APPROVED"}]), false, None)
            } else {
                older_body(pending.clone(), true, Some(&format!("K{}z", i + 1)))
            };
            fake.on(&format!("before={cur}"), Output::ok(&body));
        }
        let mut node = reviewed_node(9, pending, true);
        node["reviews"]["pageInfo"]["startCursor"] = json!("K0z");
        (fake, node)
    }

    #[test]
    fn review_pages_per_pr_boundary() {
        // página inicial + 9 mais antigas = 10 páginas: ok
        let (fake, node) = older_chain(9);
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(1, vec![node], None)));
        assert_eq!(count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never).unwrap(), 1);
        // página inicial + 10 mais antigas = 11: truncado
        let (fake, node) = older_chain(10);
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(1, vec![node], None)));
        let err = count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never).unwrap_err();
        assert!(err.message.contains("histórico truncado"), "{}", err.message);
    }

    #[test]
    fn older_cursor_repeat_fails() {
        let fake = FakeRunner::new();
        let pending = json!([{"submittedAt": "2026-10-07T10:00:00Z", "state": "PENDING"}]);
        // a página mais antiga devolve de novo o mesmo cursor C0
        fake.on("before=C0", Output::ok(&older_body(pending.clone(), true, Some("C0"))));
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(1, vec![reviewed_node(9, pending, true)], None)));
        let err = count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never).unwrap_err();
        assert!(err.message.contains("cursor"), "{}", err.message);
    }

    #[test]
    fn older_page_rejects_graphql_errors_and_null_nodes() {
        let pending = json!([{"submittedAt": "2026-10-07T10:00:00Z", "state": "PENDING"}]);
        let node = reviewed_node(9, pending.clone(), true);
        let run = |body: String| {
            let fake = FakeRunner::new();
            fake.on("before=C0", Output::ok(&body));
            fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(1, vec![node.clone()], None)));
            count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never)
        };
        let mut with_errors: Value = serde_json::from_str(&older_body(pending, false, None)).unwrap();
        with_errors["errors"] = json!([{"message": "boom"}]);
        assert!(run(with_errors.to_string()).unwrap_err().message.contains("boom"));
        assert!(run(json!({"data": {"repository": null}}).to_string()).is_err());
        assert!(run(json!({"data": {"repository": {"pullRequest": null}}}).to_string()).is_err());
    }

    #[test]
    fn count_respects_deadline_and_repeated_cursor() {
        let fake = FakeRunner::new();
        let past = std::time::Instant::now() - Duration::from_secs(1);
        assert!(count_reviews_today(&fake, "o/r", "cirdes", &window(), past, &never).unwrap_err().message.contains("orçamento"));
        let fake = FakeRunner::new();
        fake.on("after=P1", Output::ok(&reviewed_body(2, vec![], Some("P1"))));
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(2, vec![], Some("P1"))));
        assert!(count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &never).unwrap_err().message.contains("cursor"));
    }

    struct SlowRunner(Duration, String);

    impl Runner for SlowRunner {
        fn run(&self, _cmd: &Cmd) -> anyhow::Result<Output> {
            std::thread::sleep(self.0);
            Ok(Output::ok(&self.1))
        }
    }

    #[test]
    fn response_after_deadline_fails_and_timeout_is_capped() {
        let slow = SlowRunner(Duration::from_millis(80), r#"{"data":{"viewer":{"login":"cirdes"}}}"#.into());
        let deadline = std::time::Instant::now() + Duration::from_millis(30);
        let err = fetch_viewer(&slow, deadline).unwrap_err();
        assert!(err.message.contains("orçamento"), "{}", err.message);

        let fake = FakeRunner::new();
        fake.on("viewer", Output::ok(r#"{"data":{"viewer":{"login":"cirdes"}}}"#));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        fetch_viewer(&fake, deadline).unwrap();
        let t = fake.calls.borrow()[0].timeout;
        assert!(t <= Duration::from_secs(5) && t > Duration::from_secs(4), "{t:?}");
        let fake = FakeRunner::new();
        fake.on("viewer", Output::ok(r#"{"data":{"viewer":{"login":"cirdes"}}}"#));
        fetch_viewer(&fake, std::time::Instant::now() + Duration::from_secs(90)).unwrap();
        assert_eq!(fake.calls.borrow()[0].timeout, Duration::from_secs(30));
    }

    #[test]
    fn cancel_on_second_page_fails() {
        let fake = FakeRunner::new();
        let n1 = reviewed_node(1, json!([{"submittedAt": "2026-10-07T12:00:00Z", "state": "APPROVED"}]), false);
        fake.on("reviewed-by:cirdes", Output::ok(&reviewed_body(2, vec![n1], Some("P2"))));
        let checks = std::cell::Cell::new(0);
        let cancel = || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        };
        let err = count_reviews_today(&fake, "o/r", "cirdes", &window(), far(), &cancel).unwrap_err();
        assert!(err.message.contains("cancelada"), "{}", err.message);
        assert_eq!(fake.calls.borrow().len(), 1);
    }

    #[test]
    fn viewer_is_fetched() {
        let fake = FakeRunner::new();
        fake.on("viewer", Output::ok(r#"{"data":{"viewer":{"login":"cirdes"}}}"#));
        assert_eq!(fetch_viewer(&fake, far()).unwrap(), "cirdes");
    }

    #[test]
    fn viewer_rejects_errors_with_data_and_null_data() {
        let fake = FakeRunner::new();
        fake.on("viewer", Output::ok(r#"{"errors":[{"message":"rate"}],"data":{"viewer":{"login":"cirdes"}}}"#));
        assert!(fetch_viewer(&fake, far()).unwrap_err().message.contains("rate"));
        let fake = FakeRunner::new();
        fake.on("viewer", Output::ok(r#"{"data":null}"#));
        assert!(fetch_viewer(&fake, far()).is_err());
    }
}
