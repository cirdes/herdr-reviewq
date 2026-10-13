# Início do trabalho, CI e meus PRs: plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mostrar no painel e no `status` quando um agente abriu em cada worktree de review, o CI dos PRs de review e dos meus, e a lista dos meus PRs com o contador de 24h.

**Architecture:** Coletas novas ficam numa etapa própria do `tick` (`maybe_collect_extras`), depois do `poll_cycle`, com prazo de 30 s, e só escrevem metadados (`PrRecord.ci`, `work_started_at`, `work_agent`, `State.my_prs`). O parse de CI vive em `src/ci.rs`, as queries novas em `src/github_extras.rs`, a detecção de agentes em `src/agents.rs`. O TUI passa a identificar linhas por `RowId`.

**Tech Stack:** Rust 2021, serde/serde_json, chrono/chrono-tz, ratatui, `gh` CLI (GraphQL), `herdr` CLI.

**Spec:** `docs/superpowers/specs/2026-10-13-sugestoes-design.md`

## Global Constraints

- `PENDING_QUERY`, `parse_page` e `fetch_pending` não mudam.
- Coletas novas nunca escrevem `remote`, `phase`, `ownership`, `generation`, SHAs, `not_pending_since`, `notify_pending`, `State.repos`, `auth_error_notified`, alertas ou notificações.
- Prazo total da etapa de extras: 30 s. Cancelamento entre chamadas quando o daemon encerra; tentativa cancelada não grava.
- Todos os campos novos de estado com `#[serde(default)]`; `CiState` com `#[serde(other)] Unknown`.
- Agentes aceitos: `claude`, `codex`.
- Lotes de CI de review: até 20 PRs por query. Busca de meus PRs: 50 por página, até 20 páginas, `issueCount` > 1000 é erro.
- Textos de UI em português; código e identificadores em inglês; comentários seguem o estilo do arquivo (português).
- Cada task termina com `cargo test` verde e `cargo clippy --all-targets` sem warnings novos.

## Review Focus

1. Head do PR mudou e o CI novo ainda não veio: a linha mostra `?`, nunca o verde do commit anterior (teste em Task 6, `ci_cell_hides_ci_of_old_head`).
2. `herdr agent list` com cwd vindo como `/private/tmp/...` e worktree em `/tmp/...` (symlink do macOS): precisa casar (teste em Task 3, `matches_through_symlink`).
3. Troca de conta do `gh` com um repo falhando: os PRs da conta antiga não podem aparecer como "você" (teste em Task 5, `viewer_change_clears_before_apply_even_if_repo_fails`).
4. Ciclo em que tudo de extras falha (gh e herdr fora): operações, carência e notificações idênticas a um ciclo sem extras (teste em Task 5, `extras_failure_does_not_touch_review_flow`).
5. Painel em 60 colunas com `pronto · sai em 15min`, check com nome longo e início: o estado completo aparece (teste em Task 8, `narrow_keeps_status_and_grace`).

---

### Task 1: Tipos e parse de CI (`src/ci.rs`)

**Files:**
- Create: `src/ci.rs`
- Modify: `src/lib.rs` (adicionar `pub mod ci;`)

**Interfaces:**
- Produces:
  - `pub enum CiState { Pending, Success, Failure, Unknown }` (serde snake_case, `#[serde(other)]` em `Unknown`)
  - `pub struct Ci { pub sha: String, pub state: CiState, pub failed_check: Option<String>, pub finished_at: Option<DateTime<Utc>>, pub observed_at: DateTime<Utc> }`
  - `pub const CI_FRAGMENT: &str` (trecho GraphQL da spec, começando em `headRefOid`)
  - `pub fn parse_pr_ci(pr: &Value, now: DateTime<Utc>) -> Option<Ci>` (recebe o nó do PR com `commits`)
  - `pub fn cell(ci: Option<&Ci>, head_sha: &str, now: DateTime<Utc>) -> String` (texto da coluna CI)
  - `pub fn icon(ci: Option<&Ci>) -> char` (`◐ ✓ ✗ ·`)

- [ ] **Step 1: Testes que falham** em `src/ci.rs` (`#[cfg(test)] mod tests`):

```rust
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

#[test] fn maps_five_rollup_states() { /* SUCCESS→Success, FAILURE/ERROR→Failure, PENDING/EXPECTED→Pending */ }
#[test] fn null_rollup_or_unknown_state_is_none() { /* statusCheckRollup null; state "WAT" */ }
#[test] fn failed_check_from_check_run_and_status_context() {
    // conclusões FAILURE, TIMED_OUT, CANCELLED, ACTION_REQUIRED, STARTUP_FAILURE, STALE contam;
    // SUCCESS, NEUTRAL, SKIPPED, null não; StatusContext FAILURE/ERROR conta; vale o primeiro na ordem
}
#[test] fn finished_at_is_max_of_runs_and_statuses_when_complete() {}
#[test] fn no_finished_at_when_truncated_or_run_in_progress_or_pending() {}
#[test] fn malformed_detail_keeps_state() { /* nó sem __typename/nome é ignorado, state preservado */ }
#[test] fn sha_comes_from_commit_oid() {}
#[test] fn cell_texts() {
    // Pending→"◐", Success com finished_at 12 min atrás→"✓ 12min", Success sem finished_at→"✓",
    // Failure com check→"✗ rspec", Failure sem check→"✗", None→"", Unknown→"?"
}
#[test] fn ci_cell_hides_ci_of_old_head() { /* ci.sha "old", head "new" → "?" */ }
#[test] fn unknown_state_deserializes() {
    let c: Ci = serde_json::from_str(r#"{"sha":"a","state":"weird","failed_check":null,"finished_at":null,"observed_at":"2026-10-13T00:00:00Z"}"#).unwrap();
    assert_eq!(c.state, CiState::Unknown);
}
```

Cada teste deve ter os asserts concretos descritos no comentário.

- [ ] **Step 2:** `cargo test ci::` falha (módulo não existe).

- [ ] **Step 3: Implementação**

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub enum CiState { Pending, Success, Failure, #[serde(other)] Unknown }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ci {
    pub sha: String,
    pub state: CiState,
    #[serde(default)]
    pub failed_check: Option<String>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}

const FAILED_CONCLUSIONS: &[&str] = &["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"];

fn time(v: &Value, k: &str) -> Option<DateTime<Utc>> {
    v.get(k)?.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc))
}

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
    let nodes = rollup.pointer("/contexts/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
    let truncated = rollup.pointer("/contexts/pageInfo/hasNextPage").and_then(Value::as_bool).unwrap_or(true);
    let mut failed_check = None;
    let mut latest: Option<DateTime<Utc>> = None;
    let mut running = false;
    for n in &nodes {
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
    let finished_at = (state != CiState::Pending && !truncated && !running).then_some(latest).flatten();
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
```

Observação: `ci: None` com PR sem checks resulta em célula vazia; a spec distingue "sem dado" (`?`) de "sem checks" (vazio). Em PRs de review, um registro que nunca recebeu CI fica `None` → vazio. Isso é aceitável: a coleta roda a cada minuto.

- [ ] **Step 4:** `cargo test ci::` passa; `cargo test` inteiro passa.
- [ ] **Step 5:** Commit `feat(ci): tipos e parse do statusCheckRollup`.

---

### Task 2: Campos novos no estado

**Files:**
- Modify: `src/state.rs` (PrRecord, State, fixture, tipos MyPrs)
- Modify: `src/reconcile.rs` (`new_record` inicializa campos)
- Modify: qualquer construtor literal de `PrRecord` que o compilador apontar

**Interfaces:**
- Consumes: `crate::ci::Ci`
- Produces:
  - `PrRecord { ci: Option<Ci>, work_started_at: Option<DateTime<Utc>>, work_agent: Option<String>, .. }`
  - `pub struct MyPr { number: u64, title: String, url: String, draft: bool, created_at: DateTime<Utc>, head_sha: String, ci: Option<Ci> }`
  - `pub struct MyPrsRepo { prs: Vec<MyPr>, opened_24h: u64, as_of: DateTime<Utc> }`
  - `pub struct MyPrs { viewer: Option<String>, repos: BTreeMap<String, MyPrsRepo>, errors: BTreeMap<String, String> }` (Default)
  - `State.my_prs: MyPrs`

- [ ] **Step 1: Testes que falham** em `state.rs`:

```rust
#[test]
fn old_state_without_new_fields_loads() {
    // serializa um State com um PrRecord, remove "ci", "work_started_at", "work_agent" e "my_prs" do JSON,
    // desserializa e confere None/default
}
#[test]
fn new_fields_roundtrip() {
    // State com my_prs preenchido e PrRecord com ci/work_* → to_string → from_str → igual
}
#[test]
fn unknown_ci_state_does_not_isolate_file() {
    // grava JSON com "state":"novo" no ci de um PrRecord; State::load devolve (s, None) e s.prs[..].ci.state == Unknown
}
```

- [ ] **Step 2:** `cargo test state::` falha (campos não existem).
- [ ] **Step 3:** Adicionar os campos com `#[serde(default)]`, os tipos `MyPr`/`MyPrsRepo`/`MyPrs` (derive `Debug, Clone, PartialEq, Serialize, Deserialize`, `Default` em `MyPrs`), `ci: None, work_started_at: None, work_agent: None` em `fixture` e `new_record`.
- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(state): campos de CI, início do trabalho e meus PRs`.

---

### Task 3: Agentes do herdr (`src/agents.rs` + trait `Herdr`)

**Files:**
- Create: `src/agents.rs`
- Modify: `src/herdr.rs` (trait, `HerdrCli`, `FakeHerdr`)
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct AgentInfo { pub agent: String, pub cwd: Option<PathBuf>, pub foreground_cwd: Option<PathBuf> }` (em `herdr.rs`)
  - `fn list_agents(&self, deadline: Instant) -> Result<Vec<AgentInfo>>` na trait `Herdr`
  - `FakeHerdr.agents: RefCell<Vec<AgentInfo>>`, `FakeHerdr.fail_agents: Cell<bool>`
  - `pub fn parse_agents(result: &Value) -> Vec<AgentInfo>` (em `agents.rs`; entradas inválidas ignoradas)
  - `pub fn apply_agents(state: &mut State, agents: &[AgentInfo], now: DateTime<Utc>) -> Vec<PrKey>` (devolve os registros marcados agora)

- [ ] **Step 1: Testes que falham** em `agents.rs`:

```rust
#[test] fn parses_real_agent_list() {
    // JSON copiado de `herdr agent list` (result com agents: claude done em .../pr-15791 e claude working fora),
    // com uma entrada sem cwd e outra com cwd relativo → ignoradas
}
#[test] fn marks_worktree_with_agent_in_root_or_subdir() {}
#[test] fn uses_foreground_cwd_too() {}
#[test] fn similar_prefix_does_not_match() { /* agente em pr-1579, registro pr-157 */ }
#[test] fn matches_through_symlink() {
    // tempdir real: cria dir/pr-1 e um symlink link → dir; registro com path link/pr-1, agente com cwd dir/pr-1
}
#[test] fn ignores_agents_outside_allowlist() { /* "pi" */ }
#[test] fn first_detection_is_stable() { /* segunda chamada com outro agente e outro now não muda */ }
#[test] fn picks_alphabetical_agent_on_tie() { /* claude e codex no mesmo worktree → claude */ }
```

- [ ] **Step 2:** `cargo test agents::` falha.
- [ ] **Step 3: Implementação**

```rust
// agents.rs
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

pub fn parse_agents(result: &Value) -> Vec<AgentInfo> {
    result.get("agents").and_then(Value::as_array).into_iter().flatten().filter_map(|a| {
        let agent = a.get("agent")?.as_str()?.to_string();
        let (cwd, foreground_cwd) = (abs(a, "cwd"), abs(a, "foreground_cwd"));
        (cwd.is_some() || foreground_cwd.is_some()).then_some(AgentInfo { agent, cwd, foreground_cwd })
    }).collect()
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub fn apply_agents(state: &mut State, agents: &[AgentInfo], now: DateTime<Utc>) -> Vec<PrKey> {
    let mut known: Vec<(&str, Vec<PathBuf>)> = agents.iter()
        .filter(|a| KNOWN.contains(&a.agent.as_str()))
        .map(|a| (a.agent.as_str(), [&a.cwd, &a.foreground_cwd].into_iter().flatten().map(|p| canon(p)).collect()))
        .collect();
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
```

No `HerdrCli`:

```rust
fn list_agents(&self, deadline: Instant) -> Result<Vec<AgentInfo>> {
    let res = self.call(self.cmd().args(["agent", "list"]).timeout(self.until(deadline)?))?;
    Ok(crate::agents::parse_agents(&res))
}
```

No `FakeHerdr`: `Self::before(deadline)?; if self.fail_agents.get() { bail!(..) }; Ok(self.agents.borrow().clone())`.

- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(agents): detecta Claude e Codex nos worktrees`.

---

### Task 4: Queries novas do GitHub (`src/github_extras.rs`)

**Files:**
- Create: `src/github_extras.rs`
- Modify: `src/github.rs` (tornar `gh_json`, `graphql_data`, `other`, `classify` `pub(crate)`; nenhuma mudança de comportamento)
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `ci::{CI_FRAGMENT, parse_pr_ci, Ci}`, `github::{gh_json, graphql_data, FetchError, other}`, `state::{MyPr}`
- Produces:
  - `pub fn fetch_review_ci(runner: &dyn Runner, repo: &str, numbers: &[u64], now: DateTime<Utc>, deadline: Instant, cancel: &dyn Fn() -> bool) -> BTreeMap<u64, Ci>` (lotes de 20; lote com erro é pulado; PR nulo ou sem rollup fica fora do mapa)
  - `pub struct MyPrsResult { pub prs: Vec<MyPr>, pub opened_24h: u64 }`
  - `pub fn fetch_my_prs(runner: &dyn Runner, repo: &str, viewer: &str, since: DateTime<Utc>, now: DateTime<Utc>, deadline: Instant, cancel: &dyn Fn() -> bool) -> Result<MyPrsResult, FetchError>`

- [ ] **Step 1: Testes que falham**:

```rust
#[test] fn review_ci_batches_by_twenty_and_uses_aliases() {
    // 25 números; FakeRunner responde a "p1:" e a "p21:"; confere duas chamadas e mapa com os CIs
}
#[test] fn review_ci_skips_failed_batch_and_null_pr() {}
#[test] fn review_ci_stops_when_cancelled() {}
#[test] fn my_prs_lists_pages_and_counts() {
    // página 1 com 1 PR draft e cursor, página 2 com 1 PR; contador issueCount 3
    // confere query com "author:cirdes" e "created:>=<since>"
}
#[test] fn my_prs_repeated_cursor_fails() {}
#[test] fn my_prs_over_search_ceiling_fails() {}
#[test] fn my_prs_without_repo_access_fails() {}
#[test] fn my_prs_dedups_by_number() {}
#[test] fn my_prs_count_failure_fails_repo() {}
```

- [ ] **Step 2:** falham.
- [ ] **Step 3: Implementação**

Query de CI por lote, montada dinamicamente:

```rust
fn review_ci_query(numbers: &[u64]) -> String {
    let fields: String = numbers.iter()
        .map(|n| format!("p{n}: pullRequest(number: {n}) {{ {CI_FRAGMENT} }}\n"))
        .collect();
    format!("query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields} }} }}")
}
```

Para cada lote: `gh api graphql -f query=... -f owner=... -f name=...` via `gh_json`; `graphql_data` (erro → pula o lote, `continue`); para cada número, `data.repository["p{n}"]`, se objeto → `parse_pr_ci(node, now)`.

Meus PRs:

```rust
pub const MY_PRS_QUERY: &str = concat!(r#"query($q: String!, $owner: String!, $name: String!, $after: String) {
  repository(owner: $owner, name: $name) { id }
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes { __typename ... on PullRequest { number title url isDraft createdAt "#, "%CI%", r#" } }
  }
}"#);
pub const MY_PRS_COUNT_QUERY: &str = r#"query($q: String!) { search(query: $q, type: ISSUE, first: 1) { issueCount } }"#;
```

(`%CI%` é trocado por `CI_FRAGMENT` com `replace` em runtime, porque `concat!` não aceita `const`.) Laço de páginas igual ao `count_reviews_today`: `cancel()` antes de cada chamada, `seen_cursors`, `MAX_PAGES` (20), `issueCount > 1000` → erro, `repository` nulo → `"sem acesso ao repositório"`, nó que não é PR → erro, dedup por número com `BTreeSet`. Depois, contador com `q = repo:R is:pr author:V created:>=<since.to_rfc3339_opts(SecondsFormat::Secs, true)>`. `MyPr.head_sha` = `headRefOid`, `ci` = `parse_pr_ci`.

- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(github): CI dos reviews e busca dos meus PRs`.

---

### Task 5: Etapa de extras no daemon

**Files:**
- Modify: `src/daemon.rs` (`maybe_collect_extras`, `collect_extras_with`, `extras_requested`, `last_extras`; `tick`; `handle_request` limpa `work_*` no retry de recriação)

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces: `pub(crate) fn collect_extras_with(&mut self, now: DateTime<Utc>, stop: &dyn Fn() -> bool)`; campos `last_extras: Option<Instant>` e `pub(crate) extras_requested: bool` (sync seta junto com `counter_requested`).

- [ ] **Step 1: Testes que falham** (no `mod tests` de `daemon.rs`, reaproveitando `Routing`, `FakeHerdr`, `config`, `fixture`):

```rust
#[test] fn extras_mark_agent_ci_and_my_prs() {}
#[test] fn extras_failure_does_not_touch_review_flow() {
    // dois daemons idênticos rodando o mesmo poll_cycle (PR pendente + PR saindo);
    // num deles collect_extras_with com gh e herdr falhando; compara phase, ownership, generation,
    // not_pending_since, notify_pending, remote e State.repos dos dois
}
#[test] fn ci_change_does_not_bump_generation_or_update() {}
#[test] fn viewer_change_clears_before_apply_even_if_repo_fails() {}
#[test] fn failed_repo_keeps_previous_my_prs_and_records_error() {}
#[test] fn repo_removed_from_config_is_pruned() {}
#[test] fn cancelled_extras_write_nothing() {}
#[test] fn retry_recreate_clears_work_but_setup_retry_keeps() {}
#[test] fn sync_request_triggers_extras() {}
```

- [ ] **Step 2:** falham.
- [ ] **Step 3: Implementação**

```rust
const EXTRAS_BUDGET: Duration = Duration::from_secs(30);

fn maybe_collect_extras(&mut self) {
    let due = self.last_extras.is_none_or(|t| t.elapsed() >= Duration::from_secs(self.cfg.poll_interval_secs));
    if stopping() || !(due || self.extras_requested) {
        return;
    }
    self.extras_requested = false;
    self.last_extras = Some(Instant::now());
    self.collect_extras_with(Utc::now(), &stopping);
}

pub(crate) fn collect_extras_with(&mut self, now: DateTime<Utc>, stop: &dyn Fn() -> bool) {
    let deadline = Instant::now() + EXTRAS_BUDGET;
    let mut next = self.state.clone();
    // 1. agentes
    match self.herdr.list_agents(deadline) {
        Ok(agents) => for k in crate::agents::apply_agents(&mut next, &agents, now) { info(&format!("{k}: agente aberto no worktree")) },
        Err(e) => info(&format!("agentes do herdr: {e:#}")),
    }
    // 2. CI dos reviews
    for repo in self.cfg.repos.iter().map(|r| r.name.clone()).collect::<Vec<_>>() {
        if stop() { return; }
        let numbers: Vec<u64> = next.prs.values().filter(|r| r.key.repo == repo).map(|r| r.key.number).collect();
        if numbers.is_empty() { continue; }
        for (n, ci) in crate::github_extras::fetch_review_ci(self.runner, &repo, &numbers, now, deadline, stop) {
            if let Some(r) = next.get_mut(&PrKey::new(&repo, n)) { r.ci = Some(ci); }
        }
    }
    // 3. meus PRs
    if stop() { return; }
    let repos: Vec<String> = self.cfg.repos.iter().map(|r| r.name.clone()).collect();
    next.my_prs.repos.retain(|k, _| repos.contains(k));
    next.my_prs.errors.retain(|k, _| repos.contains(k));
    match github::fetch_viewer(self.runner, deadline) {
        Err(e) => info(&format!("meus PRs: viewer: {}", e.message)),
        Ok(viewer) => {
            if next.my_prs.viewer.as_deref() != Some(viewer.as_str()) {
                next.my_prs = crate::state::MyPrs { viewer: Some(viewer.clone()), ..Default::default() };
            }
            let since = now - chrono::Duration::hours(24);
            for repo in &repos {
                if stop() { return; }
                match crate::github_extras::fetch_my_prs(self.runner, repo, &viewer, since, now, deadline, stop) {
                    Ok(r) => {
                        next.my_prs.errors.remove(repo);
                        next.my_prs.repos.insert(repo.clone(), crate::state::MyPrsRepo { prs: r.prs, opened_24h: r.opened_24h, as_of: now });
                    }
                    Err(e) => { next.my_prs.errors.insert(repo.clone(), e.message); }
                }
            }
        }
    }
    if stop() { return; }
    // aplica só metadados sobre o estado atual (que pode ter mudado? não: single-thread, mas mantém a regra)
    for (k, rec) in self.state.prs.iter_mut() {
        if let Some(n) = next.prs.get(k) {
            rec.ci = n.ci.clone();
            rec.work_started_at = n.work_started_at;
            rec.work_agent = n.work_agent.clone();
        }
    }
    self.state.my_prs = next.my_prs;
    if let Err(e) = self.save() { info(&format!("{e:#}")); }
}
```

Trabalhar numa cópia e copiar só os metadados garante, por construção, que nenhum outro campo muda. `tick`: chamar `self.maybe_collect_extras()` logo após `self.maybe_collect_reviews_today()`. Em `process_requests`, onde o sync seta `counter_requested = true`, setar também `extras_requested = true`. Em `handle_request`, no ramo `Phase::Blocked { .. } if !rec.path.exists()`, acrescentar `rec.work_started_at = None; rec.work_agent = None;`.

- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(daemon): coleta de agentes, CI e meus PRs fora do ciclo de reviews`.

---

### Task 6: Modelo do TUI (`RowId`, seções, colunas, contador)

**Files:**
- Modify: `src/tui/model.rs`

**Interfaces:**
- Consumes: `ci::{cell, icon}`, `state::MyPrs`
- Produces:
  - `pub enum RowId { Review(PrKey), Mine(PrKey) }` (`Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash`) com `fn review(&self) -> Option<&PrKey>`
  - `Section { Started, Pending, NoSync, Leaving, Adopted, Mine }`
  - `Row { id: RowId, label, section, author, title, icon, status, ci: String, started: String, url, eligible }`
  - `Eligibility { retry, adopt, release, focus, log }`
  - `View.opened_24h: String`
  - `UiState.selected: Option<RowId>`, `UiState.order: Vec<RowId>`; `reselect(prev: &[RowId], selected: Option<&RowId>, view)`; `View::row(&RowId)`, `View::order() -> Vec<RowId>`

- [ ] **Step 1: Testes que falham**:

```rust
#[test] fn sections_in_order_with_started_first() {}
#[test] fn started_sorted_by_most_recent_start() {}
#[test] fn started_that_leaves_goes_to_leaving_and_keeps_start_column() {}
#[test] fn same_pr_in_both_lists_has_distinct_row_ids() {}
#[test] fn reselect_with_row_ids() {}
#[test] fn mine_rows_only_open_url() { /* eligibility toda falsa; status "rascunho"/"aberto"; author "você" */ }
#[test] fn mine_sorted_by_created_desc() {}
#[test] fn started_column_today_and_other_day() { /* "14:32" vs "12/10" em SP */ }
#[test] fn opened_24h_counter_dash_partial_and_full() {
    // sem repos → "–"; um repo com erro → "N?"; as_of velho → "N?"; todos ok → "N"
}
#[test] fn my_prs_errors_go_to_header_errors() {}
#[test] fn ci_cell_hides_ci_of_old_head() { /* via View: rec.ci.sha != observed_head_sha → row.ci == "?" */ }
#[test] fn mine_filtered_by_config_repos() {}
```

Atualizar os testes existentes que usam `row.key`/`selected: Some(PrKey)` para `RowId::Review(..)`.

- [ ] **Step 2:** falham.
- [ ] **Step 3: Implementação** (pontos principais)
  - `section_of`: depois de decidir `Section::Pending`, retornar `Section::Started` se `r.work_started_at.is_some()`.
  - Chave de ordenação: `(section, rank, tiebreak, id)`, com `rank = phase_rank` para review e `0` para Started/Mine, e `tiebreak: i64` = `-work_started_at.timestamp()` para Started, `-created_at.timestamp()` para Mine, `first_seen_at.timestamp()` nas demais.
  - `started` = `local_hm(t)` se `local_day(t) == local_day(now)`, senão `t.with_timezone(&Sao_Paulo).format("%d/%m")`.
  - `row.ci = crate::ci::cell(r.ci.as_ref(), &r.observed_head_sha, now)`.
  - Linhas Mine a partir de `state.my_prs.repos` filtrado por `cfg.repos`: `icon = ci::icon`, `status = if draft { "rascunho" } else { "aberto" }`, `author = "você"`, `ci = cell(p.ci.as_ref(), &p.head_sha, now)`, `started = ""`, `eligible = Eligibility::default()`.
  - `opened_24h`: criar `pub fn opened_24h_label(my: &MyPrs, repos: &[String], now: DateTime<Utc>, poll_secs: u64) -> String` em `src/today.rs` com a regra da spec (`as_of` velho = `now - as_of > 3 * poll_secs`) e usar aqui.
  - `header().errors` ganha `format!("meus PRs · {repo}: {err}")` para cada erro de repo do config.
  - Eligibility de review: `focus` = antigo `open`, `log = true`.
- [ ] **Step 4:** `cargo test` passa (inclui keys/render/run ajustados só o suficiente para compilar; os testes novos deles vêm nas Tasks 7 e 8).
- [ ] **Step 5:** Commit `feat(tui): seções Iniciados e Meus PRs, colunas de CI e início`.

---

### Task 7: Teclas e worker com `RowId`

**Files:**
- Modify: `src/tui/keys.rs`, `src/tui/run.rs`

**Interfaces:**
- Produces: `Action::OpenUrl(RowId, String)`; `Focus(PrKey)` e `ShowLog(PrKey)` inalterados.

- [ ] **Step 1: Testes que falham**:

```rust
// keys.rs
#[test] fn enter_and_o_on_mine_open_url() {}
#[test] fn log_retry_adopt_release_on_mine_do_nothing() { /* mensagem "só para PRs de review" e nenhuma ação */ }
#[test] fn navigation_crosses_into_mine() {}
// run.rs
#[test] fn open_url_mine_rereads_my_prs_and_refuses_vanished() {}
```

- [ ] **Step 2:** falham.
- [ ] **Step 3:** Em `handle`: navegação com `RowId`; para `RowId::Mine`, `Enter | 'o'` → `OpenUrl(row.id.clone(), row.url.clone())`, `'l' | 'R' | 'a' | 'r'` → `next.message = Some("só para PRs de review".into())`. Para review, `Enter` usa `eligible.focus`, `'l'` exige `eligible.log`. Confirmação: `view.row(&RowId::Review(target))`. Em `perform`: `OpenUrl(RowId::Review(k), _)` busca `state.get(k)`; `OpenUrl(RowId::Mine(k), _)` busca em `state.my_prs.repos.get(&k.repo)` o PR `k.number`; ausente → `format!("{k} não está mais no painel")`. `run()` que chama `reselect`/`order` passa a usar `RowId`.
- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(tui): ações por RowId e abrir meus PRs no navegador`.

---

### Task 8: Render (colunas e contadores)

**Files:**
- Modify: `src/tui/render.rs`

- [ ] **Step 1: Testes que falham**:

```rust
#[test] fn shows_pendentes_title_and_columns() {}
#[test] fn shows_opened_24h_counter() {}
#[test] fn narrow_keeps_status_and_grace() {
    // 60x20, registro saindo com "pronto · sai em 15min", ci "✗ rspec-system-very-long", início "14:32";
    // a tela contém "pronto · sai em 15min" e "14:32"
}
#[test] fn mine_section_rendered() {}
```

- [ ] **Step 2:** falham.
- [ ] **Step 3:** `section_title(Started) = "Iniciados"`, `Pending = "Pendentes"`, `Mine = "Meus PRs"`. Contadores: `Layout::horizontal([Length(14), Length(16), Length(16), Min(0)])`, terceiro bloco com título "Abertos 24h". Lista: colunas `marcador(2) rótulo(12) ícone(1) estado(sw) CI(10) Início(5) autor(aw) título(resto)`, com `sw = max_status.min(30)` sem depender de `rest * 3/5`; autor e título encolhem primeiro. Cabeçalho `"Estado" "CI" "Início" "Autor" "Título"`.
- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(tui): colunas CI e Início e contador Abertos 24h`.

---

### Task 9: `herdr-reviewq status`

**Files:**
- Modify: `src/status.rs`, chamador em `src/main.rs` se a assinatura mudar (passar `poll_interval_secs` para a regra do `?`)

- [ ] **Step 1: Testes que falham**:

```rust
#[test] fn status_shows_ci_and_start() { /* "o/r#1 ... pronto · CI ✗ rspec · iniciado 14:32 claude" */ }
#[test] fn status_shows_opened_24h_and_my_prs() { /* "Abertos 24h: 2", seção "Meus PRs", erro "! meus PRs · o/r: x" */ }
```

- [ ] **Step 2:** falham.
- [ ] **Step 3:** Usar `today::opened_24h_label` (Task 6) para a linha `Abertos 24h`. Acrescentar ` · CI {cell}` quando a célula não for vazia e ` · iniciado {HH:MM} {agente}`.
- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** Commit `feat(status): CI, início e meus PRs na CLI`.

---

### Task 10: README e verificação final

**Files:**
- Modify: `README.md`

- [ ] **Step 1:** Documentar em "How it works": detecção de agente, CI, meus PRs; em "The panel": seções novas, colunas e contador; nota de que o horário de início é o da primeira coleta que viu o agente.
- [ ] **Step 2:** `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build --release`.
- [ ] **Step 3:** Commit `docs: README com início do trabalho, CI e meus PRs`.
