use crate::facts::{divergence, Divergence, Facts};
use crate::github::{PendingPr, RepoSnapshot, Snapshot};
use crate::state::{Ownership, Phase, PrKey, PrRecord, Remote, State};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Create(PrKey),
    Update(PrKey),
    Remove(PrKey),
}

pub struct Input<'a> {
    pub snapshot: &'a Snapshot,
    pub facts: &'a BTreeMap<String, Facts>,
    pub now: DateTime<Utc>,
    pub worktrees_dir: &'a Path,
    /// Carência entre o PR sair da lista e a remoção do worktree gerenciado.
    pub remove_grace: chrono::Duration,
}

pub fn worktree_path(dir: &Path, key: &PrKey) -> PathBuf {
    dir.join(&key.repo).join(format!("pr-{}", key.number))
}

pub fn reconcile(state: &State, input: &Input) -> (State, Vec<Op>) {
    let mut s = state.clone();
    let mut ops = Vec::new();
    apply_facts(&mut s, input);
    for (repo, snap) in &input.snapshot.repos {
        match snap {
            RepoSnapshot::Failed(e) => {
                for rec in s.prs.values_mut().filter(|r| &r.key.repo == repo) {
                    rec.remote = Remote::Unknown;
                }
                let st = s.repos.entry(repo.clone()).or_default();
                if st.ok || st.since.is_none() {
                    st.since = Some(input.now);
                }
                st.ok = false;
                st.error = Some(e.message.clone());
            }
            RepoSnapshot::Complete(prs) => reconcile_repo(&mut s, &mut ops, repo, prs, input),
        }
    }
    (s, ops)
}

fn apply_facts(s: &mut State, input: &Input) {
    for (k, f) in input.facts {
        let Some(rec) = s.prs.get_mut(k) else { continue };
        if rec.ownership != Ownership::Managed {
            continue;
        }
        match rec.phase.clone() {
            Phase::Ready | Phase::Failed { .. } => {
                match divergence(rec, f) {
                    Some(Divergence::Missing) => {
                        rec.phase = Phase::Blocked { reason: "worktree sumiu do disco; use retry para recriar".into() }
                    }
                    Some(Divergence::Diverged(reason)) => rec.ownership = Ownership::Adopted { reason, at: input.now },
                    None => {}
                }
            }
            Phase::Removing => {
                if let Some(Divergence::Diverged(reason)) = divergence(rec, f) {
                    rec.ownership = Ownership::Adopted { reason: format!("remoção cancelada: {reason}"), at: input.now };
                    rec.phase = Phase::Blocked { reason: format!("remoção cancelada: {reason}") };
                }
            }
            _ => {}
        }
    }
}

fn reconcile_repo(s: &mut State, ops: &mut Vec<Op>, repo: &str, prs: &[PendingPr], input: &Input) {
    let st = s.repos.entry(repo.to_string()).or_default();
    st.ok = true;
    st.error = None;
    st.since = None;
    st.last_sync = Some(input.now);
    st.forks = prs.iter().filter(|p| p.is_fork).map(|p| p.number).collect();

    let pending: BTreeMap<String, &PendingPr> = prs
        .iter()
        .filter(|p| !p.is_fork)
        .map(|p| (PrKey::new(repo, p.number).to_string(), p))
        .collect();

    for (k, p) in &pending {
        let key = PrKey::new(repo, p.number);
        if let Some(rec) = s.prs.get_mut(k) {
            rec.remote = Remote::Pending;
            rec.not_pending_since = None;
            rec.title = p.title.clone();
            rec.last_request_event_at = p.last_request_event_at;
            if p.head_sha != rec.observed_head_sha {
                rec.observed_head_sha = p.head_sha.clone();
                match rec.ownership {
                    Ownership::Managed => rec.generation += 1,
                    Ownership::Adopted { .. } => rec.warning = Some("autor atualizou o PR".into()),
                }
            }
            if rec.ownership == Ownership::Managed {
                let applied = rec.managed_sha.as_deref() == Some(rec.observed_head_sha.as_str());
                match rec.phase.clone() {
                    Phase::Ready | Phase::Failed { .. } | Phase::Preparing if !applied => ops.push(Op::Update(key)),
                    Phase::Creating => ops.push(Op::Create(key)),
                    Phase::Removing => {
                        match input.facts.get(k) {
                            Some(f) if f.exists => {
                                rec.generation += 1;
                                rec.phase = Phase::Preparing;
                            }
                            Some(_) => ops.push(Op::Remove(key)),
                            None => {}
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        if let Some(sup) = s.suppressions.get(k) {
            let newer = match (p.last_request_event_at, sup.after) {
                (Some(a), Some(b)) => a > b,
                (Some(_), None) => true,
                _ => false,
            };
            if !newer {
                continue;
            }
            s.suppressions.remove(k);
        }
        s.insert(new_record(p, input));
        ops.push(Op::Create(key));
    }

    let prefix = format!("{repo}#");
    s.suppressions.retain(|k, _| !k.starts_with(&prefix) || pending.contains_key(k));

    let gone: Vec<String> = s
        .prs
        .iter()
        .filter(|(k, r)| r.key.repo == repo && !pending.contains_key(*k))
        .map(|(k, _)| k.clone())
        .collect();
    for k in gone {
        let rec = s.prs.get_mut(&k).expect("chave existe");
        rec.remote = Remote::NotPending;
        if rec.ownership != Ownership::Managed {
            continue;
        }
        if matches!(rec.phase, Phase::Blocked { .. }) && !rec.created_branch {
            s.prs.remove(&k);
            continue;
        }
        if rec.phase != Phase::Removing {
            let since = *rec.not_pending_since.get_or_insert(input.now);
            if input.now - since < input.remove_grace {
                continue;
            }
            rec.generation += 1;
            rec.phase = Phase::Removing;
        }
        ops.push(Op::Remove(rec.key.clone()));
    }
}

fn new_record(p: &PendingPr, input: &Input) -> PrRecord {
    let key = PrKey::new(&p.repo, p.number);
    PrRecord {
        path: worktree_path(input.worktrees_dir, &key),
        key,
        title: p.title.clone(),
        author: p.author.clone(),
        url: p.url.clone(),
        head_ref: p.head_ref.clone(),
        remote: Remote::Pending,
        ownership: Ownership::Managed,
        phase: Phase::Creating,
        created_branch: false,
        workspace_id: None,
        observed_head_sha: p.head_sha.clone(),
        managed_sha: None,
        prepared_sha: None,
        generation: 0,
        first_seen_at: input.now,
        last_request_event_at: p.last_request_event_at,
        notify_pending: false,
        warning: None,
        not_pending_since: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{ErrorKind, FetchError};
    use crate::state::Suppression;

    const REPO: &str = "o/r";

    fn pr(n: u64, sha: &str) -> PendingPr {
        PendingPr {
            repo: REPO.into(),
            number: n,
            title: format!("PR {n}"),
            author: "ana".into(),
            url: "u".into(),
            head_ref: "feat/x".into(),
            head_sha: sha.into(),
            is_fork: false,
            last_request_event_at: Some("2026-10-07T10:00:00Z".parse().unwrap()),
        }
    }

    fn snap(prs: Vec<PendingPr>) -> Snapshot {
        let mut s = Snapshot::default();
        s.repos.insert(REPO.into(), RepoSnapshot::Complete(prs));
        s
    }

    fn failed() -> Snapshot {
        let mut s = Snapshot::default();
        s.repos.insert(
            REPO.into(),
            RepoSnapshot::Failed(FetchError {
                kind: ErrorKind::Other,
                message: "rede".into(),
            }),
        );
        s
    }

    /// Sem carência: os testes de remoção em si.
    fn run(state: &State, snapshot: &Snapshot, facts: &BTreeMap<String, Facts>) -> (State, Vec<Op>) {
        run_at(state, snapshot, facts, Utc::now(), chrono::Duration::zero())
    }

    fn run_at(
        state: &State,
        snapshot: &Snapshot,
        facts: &BTreeMap<String, Facts>,
        now: DateTime<Utc>,
        remove_grace: chrono::Duration,
    ) -> (State, Vec<Op>) {
        reconcile(state, &Input { snapshot, facts, now, worktrees_dir: Path::new("/w"), remove_grace })
    }

    fn with(rec: PrRecord) -> State {
        let mut s = State::default();
        s.insert(rec);
        s
    }

    fn clean(rec: &PrRecord) -> Facts {
        Facts {
            exists: true,
            branch: Some(rec.head_ref.clone()),
            head: rec.managed_sha.clone(),
            op_in_progress: false,
            tracked: vec![],
            flagged: vec![],
            untracked: vec![],
            foreign_ignored: vec![],
        }
    }

    fn key(n: u64) -> PrKey {
        PrKey::new(REPO, n)
    }

    #[test]
    fn new_pending_pr_is_tracked_and_created() {
        let (s, ops) = run(&State::default(), &snap(vec![pr(7, "a1")]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Create(key(7))]);
        let rec = s.get(&key(7)).unwrap();
        assert_eq!(rec.phase, Phase::Creating);
        assert_eq!(rec.ownership, Ownership::Managed);
        assert_eq!(rec.path, Path::new("/w/o/r/pr-7"));
        assert_eq!(rec.observed_head_sha, "a1");
    }

    #[test]
    fn forks_are_listed_not_tracked() {
        let mut p = pr(8, "f");
        p.is_fork = true;
        let (s, ops) = run(&State::default(), &snap(vec![p]), &BTreeMap::new());
        assert!(ops.is_empty());
        assert!(s.get(&key(8)).is_none());
        assert_eq!(s.repos[REPO].forks, vec![8]);
    }

    #[test]
    fn failed_snapshot_never_removes() {
        let state = with(PrRecord::fixture(REPO, 1, "feat/x", "a1"));
        let (s, ops) = run(&state, &failed(), &BTreeMap::new());
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().remote, Remote::Unknown);
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Ready);
        assert!(!s.repos[REPO].ok);
        assert!(s.repos[REPO].since.is_some());
    }

    #[test]
    fn failure_in_one_repo_does_not_touch_another() {
        let mut state = with(PrRecord::fixture(REPO, 1, "feat/x", "a1"));
        state.insert(PrRecord::fixture("o/outro", 2, "b", "b1"));
        let mut snapshot = failed();
        snapshot.repos.insert("o/outro".into(), RepoSnapshot::Complete(vec![]));
        let (s, ops) = run(&state, &snapshot, &BTreeMap::new());
        assert_eq!(ops, vec![Op::Remove(PrKey::new("o/outro", 2))]);
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Ready);
    }

    #[test]
    fn managed_not_pending_is_removed() {
        let state = with(PrRecord::fixture(REPO, 1, "feat/x", "a1"));
        let (s, ops) = run(&state, &snap(vec![]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Remove(key(1))]);
        let rec = s.get(&key(1)).unwrap();
        assert_eq!(rec.phase, Phase::Removing);
        assert_eq!(rec.remote, Remote::NotPending);
        assert_eq!(rec.generation, 1);
    }

    #[test]
    fn adopted_not_pending_is_kept() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.ownership = Ownership::Adopted {
            reason: "x".into(),
            at: Utc::now(),
        };
        let (s, ops) = run(&with(rec), &snap(vec![]), &BTreeMap::new());
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().remote, Remote::NotPending);
    }

    #[test]
    fn author_push_updates_managed() {
        let state = with(PrRecord::fixture(REPO, 1, "feat/x", "a1"));
        let (s, ops) = run(&state, &snap(vec![pr(1, "a2")]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Update(key(1))]);
        assert_eq!(s.get(&key(1)).unwrap().observed_head_sha, "a2");
        assert_eq!(s.get(&key(1)).unwrap().generation, 1);
    }

    #[test]
    fn update_is_retried_until_applied() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.observed_head_sha = "a2".into(); // ciclo anterior viu a2, mas o update falhou
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a2")]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Update(key(1))]);
        assert_eq!(s.get(&key(1)).unwrap().generation, 0);
    }

    #[test]
    fn author_push_only_warns_adopted() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.ownership = Ownership::Adopted {
            reason: "x".into(),
            at: Utc::now(),
        };
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a2")]), &BTreeMap::new());
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().warning.as_deref(), Some("autor atualizou o PR"));
    }

    #[test]
    fn diverged_facts_adopt_and_block_removal() {
        let rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        let mut f = clean(&rec);
        f.untracked = vec!["notas.txt".into()];
        let facts = BTreeMap::from([(key(1).to_string(), f)]);
        let (s, ops) = run(&with(rec), &snap(vec![]), &facts);
        assert!(ops.is_empty());
        let final_rec = s.get(&key(1)).unwrap();
        assert!(matches!(
            &final_rec.ownership,
            Ownership::Adopted { reason, .. } if reason.contains("notas.txt")
        ));
        assert!(matches!(final_rec.phase, Phase::Ready));
    }

    #[test]
    fn missing_worktree_blocks() {
        let rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        let facts = BTreeMap::from([(key(1).to_string(), Facts::missing())]);
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &facts);
        assert!(ops.is_empty());
        assert!(matches!(
            s.get(&key(1)).unwrap().phase,
            Phase::Blocked { .. }
        ));
    }

    #[test]
    fn clean_facts_change_nothing() {
        let rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        let facts = BTreeMap::from([(key(1).to_string(), clean(&rec))]);
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &facts);
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().ownership, Ownership::Managed);
    }

    #[test]
    fn rerequest_during_removal_with_worktree_cancels_removal() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Removing;
        let facts = BTreeMap::from([(key(1).to_string(), clean(&rec))]);
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &facts);
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Preparing);
        assert_eq!(s.get(&key(1)).unwrap().generation, 1);
    }

    #[test]
    fn rerequest_during_removal_without_worktree_finishes_removal() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Removing;
        let facts = BTreeMap::from([(key(1).to_string(), Facts::missing())]);
        let (_, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &facts);
        assert_eq!(ops, vec![Op::Remove(key(1))]);
    }

    #[test]
    fn suppression_waits_for_newer_request() {
        let mut state = State::default();
        state.suppressions.insert(
            key(1).to_string(),
            Suppression {
                after: Some("2026-10-07T10:00:00Z".parse().unwrap()),
            },
        );
        let (s, ops) = run(&state, &snap(vec![pr(1, "a1")]), &BTreeMap::new());
        assert!(ops.is_empty());
        assert!(s.suppressions.contains_key(&key(1).to_string()));

        let mut newer = pr(1, "a1");
        newer.last_request_event_at = Some("2026-10-07T11:00:00Z".parse().unwrap());
        let (s, ops) = run(&state, &snap(vec![newer]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Create(key(1))]);
        assert!(s.suppressions.is_empty());
    }

    #[test]
    fn suppression_dropped_when_pr_no_longer_pending() {
        let mut state = State::default();
        state
            .suppressions
            .insert(key(1).to_string(), Suppression { after: None });
        let (s, _) = run(&state, &snap(vec![]), &BTreeMap::new());
        assert!(s.suppressions.is_empty());
    }

    #[test]
    fn blocked_without_branch_is_forgotten_when_not_pending() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Blocked {
            reason: "branch já existe".into(),
        };
        rec.created_branch = false;
        let (s, ops) = run(&with(rec), &snap(vec![]), &BTreeMap::new());
        assert!(ops.is_empty());
        assert!(s.get(&key(1)).is_none());
    }

    #[test]
    fn creating_is_resumed() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Creating;
        let (_, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Create(key(1))]);
    }

    #[test]
    fn removing_with_diverged_facts_adopts_and_blocks() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Removing;
        let mut f = clean(&rec);
        f.untracked = vec!["notas.txt".into()];
        let facts = BTreeMap::from([(key(1).to_string(), f)]);
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &facts);
        assert!(ops.is_empty());
        let final_rec = s.get(&key(1)).unwrap();
        assert!(matches!(
            &final_rec.ownership,
            Ownership::Adopted { reason, .. } if reason.contains("remoção cancelada") && reason.contains("notas.txt")
        ));
        assert!(matches!(
            &final_rec.phase,
            Phase::Blocked { reason } if reason.contains("remoção cancelada")
        ));
    }

    #[test]
    fn preparing_with_unapplied_update() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Preparing;
        rec.managed_sha = Some("a1".into());
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a2")]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Update(key(1))]);
        assert_eq!(s.get(&key(1)).unwrap().observed_head_sha, "a2");
    }

    #[test]
    fn not_pending_creating_is_removed() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Creating;
        let (s, ops) = run(&with(rec), &snap(vec![]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Remove(key(1))]);
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Removing);
    }

    #[test]
    fn not_pending_preparing_is_removed() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Preparing;
        let (s, ops) = run(&with(rec), &snap(vec![]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Remove(key(1))]);
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Removing);
    }

    #[test]
    fn not_pending_blocked_with_created_branch_is_removed() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Blocked {
            reason: "x".into(),
        };
        rec.created_branch = true;
        let (s, ops) = run(&with(rec), &snap(vec![]), &BTreeMap::new());
        assert_eq!(ops, vec![Op::Remove(key(1))]);
        assert!(s.get(&key(1)).is_some());
    }

    #[test]
    fn removing_pending_without_facts_no_op() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.phase = Phase::Removing;
        let (s, ops) = run(&with(rec), &snap(vec![pr(1, "a1")]), &BTreeMap::new());
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Removing);
    }

    fn t0() -> DateTime<Utc> {
        "2026-10-07T12:00:00Z".parse().unwrap()
    }

    fn grace() -> chrono::Duration {
        chrono::Duration::minutes(15)
    }

    #[test]
    fn not_pending_within_grace_waits() {
        let state = with(PrRecord::fixture(REPO, 1, "feat/x", "a1"));
        let (s, ops) = run_at(&state, &snap(vec![]), &BTreeMap::new(), t0(), grace());
        assert!(ops.is_empty());
        let rec = s.get(&key(1)).unwrap();
        assert_eq!(rec.phase, Phase::Ready);
        assert_eq!(rec.remote, Remote::NotPending);
        assert_eq!(rec.not_pending_since, Some(t0()));
        assert_eq!(rec.generation, 0);

        // ciclo seguinte, ainda dentro da carência: não reinicia o relógio
        let later = t0() + chrono::Duration::minutes(14);
        let (s, ops) = run_at(&s, &snap(vec![]), &BTreeMap::new(), later, grace());
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().not_pending_since, Some(t0()));
    }

    #[test]
    fn not_pending_after_grace_is_removed() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.remote = Remote::NotPending;
        rec.not_pending_since = Some(t0());
        let (s, ops) = run_at(&with(rec), &snap(vec![]), &BTreeMap::new(), t0() + grace(), grace());
        assert_eq!(ops, vec![Op::Remove(key(1))]);
        assert_eq!(s.get(&key(1)).unwrap().phase, Phase::Removing);
        assert_eq!(s.get(&key(1)).unwrap().generation, 1);
    }

    #[test]
    fn pending_again_within_grace_clears_timer() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.remote = Remote::NotPending;
        rec.not_pending_since = Some(t0());
        let later = t0() + chrono::Duration::minutes(5);
        let (s, ops) = run_at(&with(rec), &snap(vec![pr(1, "a1")]), &BTreeMap::new(), later, grace());
        assert!(ops.is_empty());
        let rec = s.get(&key(1)).unwrap();
        assert_eq!(rec.not_pending_since, None);
        assert_eq!(rec.remote, Remote::Pending);
        assert_eq!(rec.phase, Phase::Ready);
    }

    #[test]
    fn failed_snapshot_keeps_grace_timer() {
        let mut rec = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        rec.remote = Remote::NotPending;
        rec.not_pending_since = Some(t0());
        let (s, ops) = run_at(&with(rec.clone()), &failed(), &BTreeMap::new(), t0() + grace(), grace());
        assert!(ops.is_empty());
        assert_eq!(s.get(&key(1)).unwrap().not_pending_since, Some(t0()));

        rec.not_pending_since = None;
        let (s, _) = run_at(&with(rec), &failed(), &BTreeMap::new(), t0(), grace());
        assert_eq!(s.get(&key(1)).unwrap().not_pending_since, None);
    }

    #[test]
    fn grace_does_not_delay_forgetting_blocked_without_branch_nor_resuming_removal() {
        let mut blocked = PrRecord::fixture(REPO, 1, "feat/x", "a1");
        blocked.phase = Phase::Blocked { reason: "x".into() };
        blocked.created_branch = false;
        let mut removing = PrRecord::fixture(REPO, 2, "feat/y", "b1");
        removing.phase = Phase::Removing;
        let mut state = with(blocked);
        state.insert(removing);
        let (s, ops) = run_at(&state, &snap(vec![]), &BTreeMap::new(), t0(), grace());
        assert!(s.get(&key(1)).is_none());
        assert_eq!(ops, vec![Op::Remove(key(2))]);
    }
}
