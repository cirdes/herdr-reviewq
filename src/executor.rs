use crate::config::{Config, RepoConfig};
use crate::facts::{self, divergence, Divergence};
use crate::git::Git;
use crate::herdr::{CreateReq, Herdr};
use crate::reconcile::Op;
use crate::runner::Runner;
use crate::state::{Ownership, Phase, PrKey, PrRecord, Remote, State, Suppression};
use anyhow::{bail, Context, Result};
use std::path::Path;
use chrono::{DateTime, Utc};

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub runner: &'a dyn Runner,
    pub herdr: &'a dyn Herdr,
    pub now: DateTime<Utc>,
}

impl<'a> Ctx<'a> {
    fn git(&self) -> Git<'a> {
        Git::new(self.runner)
    }
    fn repo(&self, key: &PrKey) -> Result<&'a RepoConfig> {
        self.cfg.repo(&key.repo).with_context(|| format!("repo {} fora da config", key.repo))
    }
}

fn record<'s>(state: &'s mut State, key: &PrKey) -> Result<&'s mut PrRecord> {
    state.get_mut(key).with_context(|| format!("{key} não está no estado"))
}

pub fn create(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<()> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let rec = record(state, key)?;
    if rec.phase != Phase::Creating {
        return Ok(());
    }
    let sha = git.fetch_branch(&repo.path, &rec.head_ref)?;
    if sha != rec.observed_head_sha {
        return Ok(()); // GitHub e remoto ainda não concordam; o próximo ciclo resolve
    }
    if rec.path.exists() {
        // Sem prova de que fomos nós que criamos: nunca assume.
        rec.phase = Phase::Blocked {
            reason: format!("{} já existe e o daemon não tem registro de tê-lo criado; confira e remova à mão", rec.path.display()),
        };
        return Ok(());
    }
    if git.ref_sha(&repo.path, &format!("refs/heads/{}", rec.head_ref))?.is_some() {
        rec.phase = Phase::Blocked { reason: format!("branch {} já existe no clone", rec.head_ref) };
        return Ok(());
    }
    if let Some(parent) = rec.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let created = ctx.herdr.create_worktree(&CreateReq {
        repo: repo.path.clone(),
        branch: rec.head_ref.clone(),
        base: format!("origin/{}", rec.head_ref),
        path: rec.path.clone(),
        label: format!("#{} {}", key.number, rec.title),
    })?;
    rec.workspace_id = Some(created.workspace_id);
    rec.created_branch = true;
    rec.managed_sha = Some(sha);
    rec.phase = Phase::Preparing;
    rec.warning = None;
    if let Err(e) = git.set_upstream(&rec.path, &rec.head_ref) {
        rec.warning = Some(format!("upstream não configurado: {e:#}"));
    }
    Ok(())
}

pub fn update(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<()> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let (head_ref, path, observed, managed) = {
        let r = state.get(key).with_context(|| format!("{key} não está no estado"))?;
        if r.ownership != Ownership::Managed {
            return Ok(());
        }
        (r.head_ref.clone(), r.path.clone(), r.observed_head_sha.clone(), r.managed_sha.clone())
    };
    let sha = git.fetch_branch(&repo.path, &head_ref)?;
    if sha != observed || managed.as_deref() == Some(sha.as_str()) {
        return Ok(());
    }
    // Checagem logo antes do efeito, depois do fetch.
    let f = facts::collect(ctx.runner, &path, &repo.disposable_ignored)?;
    let rec = record(state, key)?;
    match divergence(rec, &f) {
        Some(Divergence::Missing) => {
            rec.phase = Phase::Blocked { reason: "worktree sumiu do disco; use retry para recriar".into() };
            return Ok(());
        }
        Some(Divergence::Diverged(reason)) => {
            rec.ownership = Ownership::Adopted { reason, at: ctx.now };
            rec.warning = Some("autor atualizou o PR".into());
            return Ok(());
        }
        None => {}
    }
    if let Some(head) = &f.head {
        git.backup(&repo.path, key.number, head, ctx.now)?;
    }
    if let Err(e) = git.reset_keep(&path, &sha) {
        if !would_overwrite(&format!("{e:#}")) {
            // ex.: index.lock de um git do usuário; segue gerenciado e o próximo ciclo tenta de novo
            return Err(e.context("reset --keep falhou"));
        }
        rec.ownership = Ownership::Adopted { reason: format!("atualização abortada para preservar alterações: {e:#}"), at: ctx.now };
        return Ok(());
    }
    rec.managed_sha = Some(sha);
    rec.phase = Phase::Preparing;
    rec.notify_pending = false;
    rec.warning = None;
    Ok(())
}

/// O `reset --keep` recusou porque sobrescreveria alteração local (e não por outro motivo).
pub(crate) fn would_overwrite(err: &str) -> bool {
    ["would be overwritten", "not uptodate", "Entry '"].iter().any(|n| err.contains(n))
}

fn discard_worktree(ctx: &Ctx, repo: &Path, path: &Path, workspace: Option<String>) -> Result<()> {
    match workspace {
        Some(id) => ctx.herdr.remove_worktree(&id),
        None => ctx.git().worktree_remove(repo, path),
    }
}

/// Apaga a branch do PR só se o daemon a criou, se ela ainda está no sha que ele
/// colocou e se não está aberta em outro worktree. Devolve uma nota quando preserva.
pub fn delete_owned_branch(git: &Git, repo: &Path, rec: &PrRecord) -> Result<Option<String>> {
    if !rec.created_branch {
        return Ok(None);
    }
    let Some(expected) = &rec.managed_sha else { return Ok(None) };
    let Some(current) = git.ref_sha(repo, &format!("refs/heads/{}", rec.head_ref))? else { return Ok(None) };
    if &current != expected {
        return Ok(Some(format!("branch {} preservada: mudou desde que o daemon a criou", rec.head_ref)));
    }
    if git.branch_checked_out(repo, &rec.head_ref)? {
        return Ok(Some(format!("branch {} preservada: está aberta em outro worktree", rec.head_ref)));
    }
    if !git.delete_branch_if_at(repo, &rec.head_ref, expected)? {
        return Ok(Some(format!("branch {} preservada: mudou durante a remoção", rec.head_ref)));
    }
    Ok(None)
}

/// Remove um worktree gerenciado e impecável. Qualquer alteração local → adota.
pub fn remove(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<Option<String>> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let rec = state.get(key).cloned().with_context(|| format!("{key} não está no estado"))?;
    if rec.ownership != Ownership::Managed {
        return Ok(None);
    }
    // IPC antes da última checagem, para a janela entre checar e remover ser mínima.
    let workspace = if rec.path.exists() { ctx.herdr.find_open_workspace(&repo.path, &rec.path)? } else { None };
    let f = facts::collect(ctx.runner, &rec.path, &repo.disposable_ignored)?;
    if let Some(Divergence::Diverged(reason)) = divergence(&rec, &f) {
        let r = record(state, key)?;
        r.ownership = Ownership::Adopted { reason: reason.clone(), at: ctx.now };
        r.phase = Phase::Blocked { reason: format!("remoção cancelada: {reason}") };
        return Ok(Some(format!("remoção cancelada: {reason}")));
    }
    if f.exists {
        if let Some(head) = &f.head {
            git.backup(&repo.path, key.number, head, ctx.now)?;
        }
        discard_worktree(ctx, &repo.path, &rec.path, workspace)?;
    }
    git.worktree_prune(&repo.path)?;
    let note = delete_owned_branch(&git, &repo.path, &rec)?;
    state.remove(key);
    if let Some(n) = &note {
        state.push_alert(format!("{key}: {n}"));
    }
    if ctx.cfg.notify.on_removed {
        let _ = ctx.herdr.notify(&format!("#{} removido", key.number), &rec.title, &ctx.cfg.notify.sound);
    }
    Ok(note)
}

/// Libera um worktree adotado, só se nada local puder se perder.
pub fn release(ctx: &Ctx, state: &mut State, key: &PrKey) -> Result<String> {
    let repo = ctx.repo(key)?;
    let git = ctx.git();
    let rec = state.get(key).cloned().with_context(|| format!("{key} não está sendo acompanhado"))?;
    if rec.ownership == Ownership::Managed {
        bail!("{key} não está adotado; ele é removido sozinho quando sair da sua lista");
    }
    let workspace = if rec.path.exists() { ctx.herdr.find_open_workspace(&repo.path, &rec.path)? } else { None };
    git.fetch_origin(&repo.path).context("não consegui atualizar o origin para confirmar que nada se perde")?;
    // Depois de squash merge a branch some do origin; o head do PR continua em refs/pull/<n>/head.
    if let Err(e) = git.fetch_pull_head(&repo.path, key.number) {
        crate::applog::info(&format!("{key}: {e:#}; a checagem segue só com as branches do origin"));
    }
    let f = facts::collect(ctx.runner, &rec.path, &repo.disposable_ignored)?;
    if f.exists {
        if let Some(dirt) = f.dirt() {
            bail!("{key} não pode ser liberado: {dirt}. Faça commit e push, ou descarte, e tente de novo");
        }
        if let Some(head) = &f.head {
            if !git.is_reachable_from_origin(&repo.path, head)? {
                bail!("{key} não pode ser liberado: o HEAD {} tem commits que não estão no origin", &head[..head.len().min(8)]);
            }
        }
    }
    if f.exists && f.head.is_none() {
        bail!("{key} não pode ser liberado: não consegui ler o HEAD");
    }
    let stashes = git.stash_count(&repo.path)?;
    if f.exists {
        if let Some(head) = &f.head {
            git.backup(&repo.path, key.number, head, ctx.now)?;
        }
        discard_worktree(ctx, &repo.path, &rec.path, workspace)?;
    }
    git.worktree_prune(&repo.path)?;
    let note = delete_owned_branch(&git, &repo.path, &rec)?;
    if matches!(rec.remote, Remote::Pending | Remote::Unknown) {
        state.suppressions.insert(key.to_string(), Suppression { after: rec.last_request_event_at });
    }
    state.remove(key);
    let mut msg = format!("{key} liberado");
    if let Some(n) = note {
        state.push_alert(format!("{key}: {n}"));
        msg.push_str(&format!("; {n}"));
    }
    if stashes > 0 {
        msg.push_str(&format!("; atenção: há {stashes} stash(es) no repositório"));
    }
    Ok(msg)
}

pub fn run_op(ctx: &Ctx, state: &mut State, op: &Op) -> Result<Option<String>> {
    match op {
        Op::Create(k) => create(ctx, state, k).map(|_| None),
        Op::Update(k) => update(ctx, state, k).map(|_| None),
        Op::Remove(k) => remove(ctx, state, k),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;
    use crate::reconcile::worktree_path;
    use crate::runner::RealRunner;
    use crate::testutil::{config, fixture, git, Fixture};

    const BRANCH: &str = "feat/x";

    fn creating(fx: &Fixture, cfg: &Config) -> (State, PrKey) {
        let mut rec = PrRecord::fixture("o/r", 7, BRANCH, &fx.remote_sha(BRANCH));
        rec.phase = Phase::Creating;
        rec.created_branch = false;
        rec.managed_sha = None;
        rec.prepared_sha = None;
        rec.workspace_id = None;
        rec.path = worktree_path(&cfg.worktrees_dir, &rec.key);
        let key = rec.key.clone();
        let mut s = State::default();
        s.insert(rec);
        (s, key)
    }

    /// Cria e marca como pronto.
    fn ready(ctx: &Ctx, state: &mut State, key: &PrKey) -> std::path::PathBuf {
        create(ctx, state, key).unwrap();
        let rec = state.get_mut(key).unwrap();
        rec.phase = Phase::Ready;
        rec.path.clone()
    }

    fn adopt(s: &mut State, key: &PrKey) {
        s.get_mut(key).unwrap().ownership = Ownership::Adopted { reason: "teste".into(), at: Utc::now() };
    }

    fn branch_exists(fx: &Fixture, name: &str) -> bool {
        crate::git::Git::new(&RealRunner).ref_sha(&fx.clone, &format!("refs/heads/{name}")).unwrap().is_some()
    }

    #[test]
    fn create_builds_worktree_with_upstream() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        create(&ctx, &mut s, &key).unwrap();
        let rec = s.get(&key).unwrap();
        assert_eq!(rec.phase, Phase::Preparing);
        assert!(rec.created_branch);
        assert_eq!(rec.managed_sha.as_deref(), Some(fx.remote_sha(BRANCH).as_str()));
        assert_eq!(rec.workspace_id.as_deref(), Some("w1"));
        assert_eq!(git(&rec.path, &["rev-parse", "--abbrev-ref", "@{u}"]), "origin/feat/x");
        assert_eq!(git(&rec.path, &["rev-parse", "--abbrev-ref", "HEAD"]), BRANCH);
    }

    #[test]
    fn create_waits_when_github_and_remote_disagree() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        s.get_mut(&key).unwrap().observed_head_sha = "outro-sha".into();
        create(&ctx, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().phase, Phase::Creating);
        assert!(!s.get(&key).unwrap().path.exists());
    }

    #[test]
    fn create_blocks_when_branch_already_exists_locally() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        git(&fx.clone, &["fetch", "-q", "origin"]);
        git(&fx.clone, &["branch", BRANCH, "origin/feat/x"]);
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        create(&ctx, &mut s, &key).unwrap();
        assert!(matches!(&s.get(&key).unwrap().phase, Phase::Blocked { reason } if reason.contains("já existe")));
        assert!(!s.get(&key).unwrap().created_branch);
    }

    #[test]
    fn create_never_claims_an_existing_path() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let before = s.clone();
        create(&ctx, &mut s, &key).unwrap(); // simula: criou, mas o estado não foi salvo
        let path = s.get(&key).unwrap().path.clone();
        std::fs::write(path.join("notas.txt"), "trabalho").unwrap();
        let mut restarted = before;
        create(&ctx, &mut restarted, &key).unwrap();
        let rec = restarted.get(&key).unwrap();
        assert!(matches!(&rec.phase, Phase::Blocked { reason } if reason.contains("já existe")));
        assert!(!rec.created_branch);
        assert!(path.join("notas.txt").exists());
    }

    #[test]
    fn update_moves_to_new_sha_and_keeps_backup() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let old = s.get(&key).unwrap().managed_sha.clone().unwrap();
        let new = fx.author_push(BRANCH, "f2.txt", "x");
        s.get_mut(&key).unwrap().observed_head_sha = new.clone();
        update(&ctx, &mut s, &key).unwrap();
        let rec = s.get(&key).unwrap();
        assert_eq!(rec.phase, Phase::Preparing);
        assert_eq!(rec.managed_sha.as_deref(), Some(new.as_str()));
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), new);
        let backups = git(&fx.clone, &["for-each-ref", "--format=%(objectname)", "refs/reviewq/backup/pr-7"]);
        assert!(backups.contains(&old));
    }

    #[test]
    fn update_handles_force_push() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let new = fx.author_force_push(BRANCH);
        s.get_mut(&key).unwrap().observed_head_sha = new.clone();
        update(&ctx, &mut s, &key).unwrap();
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), new);
    }

    #[test]
    fn update_adopts_instead_of_touching_local_commit() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "meu"]);
        let mine = git(&path, &["rev-parse", "HEAD"]);
        s.get_mut(&key).unwrap().observed_head_sha = fx.author_push(BRANCH, "f2.txt", "x");
        update(&ctx, &mut s, &key).unwrap();
        assert!(matches!(s.get(&key).unwrap().ownership, Ownership::Adopted { .. }));
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), mine);
    }

    #[test]
    fn update_adopts_when_user_created_a_file() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::write(path.join("notas.txt"), "x").unwrap();
        let old = git(&path, &["rev-parse", "HEAD"]);
        s.get_mut(&key).unwrap().observed_head_sha = fx.author_push(BRANCH, "f2.txt", "x");
        update(&ctx, &mut s, &key).unwrap();
        assert!(matches!(&s.get(&key).unwrap().ownership, Ownership::Adopted { reason, .. } if reason.contains("notas.txt")));
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), old);
    }

    #[test]
    fn update_with_git_lock_fails_and_retries_without_adopting() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let new = fx.author_push(BRANCH, "f2.txt", "x");
        s.get_mut(&key).unwrap().observed_head_sha = new.clone();
        let lock = std::path::PathBuf::from(git(&path, &["rev-parse", "--absolute-git-dir"])).join("index.lock");
        std::fs::write(&lock, "").unwrap();
        assert!(update(&ctx, &mut s, &key).is_err());
        assert_eq!(s.get(&key).unwrap().ownership, Ownership::Managed);
        assert_ne!(s.get(&key).unwrap().managed_sha.as_deref(), Some(new.as_str()));
        std::fs::remove_file(&lock).unwrap();
        update(&ctx, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().ownership, Ownership::Managed);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), new);
    }

    #[test]
    fn would_overwrite_matches_only_overwrite_errors() {
        assert!(would_overwrite("error: Entry 'feature.txt' not uptodate. Cannot merge."));
        assert!(would_overwrite("Your local changes to the following files would be overwritten by merge"));
        assert!(!would_overwrite("fatal: Unable to create '/x/index.lock': File exists."));
    }

    #[test]
    fn update_waits_when_fetch_disagrees_with_github() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let old = git(&path, &["rev-parse", "HEAD"]);
        s.get_mut(&key).unwrap().observed_head_sha = "sha-que-o-remoto-ainda-nao-tem".into();
        update(&ctx, &mut s, &key).unwrap();
        assert_eq!(s.get(&key).unwrap().phase, Phase::Ready);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), old);
    }

    #[test]
    fn remove_cleans_worktree_branch_and_record() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::create_dir_all(path.join("node_modules/pkg")).unwrap(); // descartável
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(s.get(&key).is_none());
        assert!(!path.exists());
        assert!(!branch_exists(&fx, BRANCH));
        assert!(!git(&fx.clone, &["for-each-ref", "refs/reviewq/backup/pr-7"]).is_empty());
    }

    #[test]
    fn remove_backs_off_on_any_local_change() {
        for (file, content) in [(".env", "SEGREDO=1"), ("feature.txt", "lockfile reescrito\n"), ("notas.txt", "x")] {
            let fx = fixture(BRANCH);
            let cfg = config(&fx, "true");
            let herdr = FakeHerdr::new();
            let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
            let (mut s, key) = creating(&fx, &cfg);
            let path = ready(&ctx, &mut s, &key);
            std::fs::write(path.join(file), content).unwrap();
            s.get_mut(&key).unwrap().phase = Phase::Removing;
            let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
            assert!(note.contains("cancelada"), "{file}: {note}");
            assert!(matches!(s.get(&key).unwrap().ownership, Ownership::Adopted { .. }));
            assert_eq!(std::fs::read_to_string(path.join(file)).unwrap(), content);
            assert!(branch_exists(&fx, BRANCH));
        }
    }

    #[test]
    fn remove_preserves_branch_that_moved_after_worktree_is_gone() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&fx.clone, &["worktree", "remove", path.to_str().unwrap()]);
        let main = git(&fx.clone, &["rev-parse", "main"]);
        git(&fx.clone, &["update-ref", "refs/heads/feat/x", &main]);
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("preservada"));
        assert!(branch_exists(&fx, BRANCH));
        assert!(s.get(&key).is_none());
        assert!(s.alerts.iter().any(|a| a.message.contains("preservada")));
    }

    #[test]
    fn remove_preserves_branch_open_in_another_worktree() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&fx.clone, &["worktree", "remove", path.to_str().unwrap()]);
        let other = fx.root.join("outro-checkout");
        git(&fx.clone, &["worktree", "add", "-q", other.to_str().unwrap(), BRANCH]);
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        let note = remove(&ctx, &mut s, &key).unwrap().unwrap();
        assert!(note.contains("outro worktree"));
        assert!(branch_exists(&fx, BRANCH));
    }

    #[test]
    fn remove_of_record_without_worktree_just_forgets() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        s.get_mut(&key).unwrap().phase = Phase::Removing;
        remove(&ctx, &mut s, &key).unwrap();
        assert!(s.get(&key).is_none());
    }

    /// herdr de verdade (FakeRunner) e git de verdade.
    struct HerdrOnFake<'a> {
        herdr: &'a crate::runner::FakeRunner,
    }

    impl crate::runner::Runner for HerdrOnFake<'_> {
        fn run(&self, c: &crate::runner::Cmd) -> Result<crate::runner::Output> {
            if c.program == "herdr" { self.herdr.run(c) } else { RealRunner.run(c) }
        }
    }

    #[test]
    fn remove_never_reopens_a_closed_workspace() {
        use crate::runner::{FakeRunner, Output};
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let fake_herdr = FakeHerdr::new();
        let setup_ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &fake_herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&setup_ctx, &mut s, &key);
        s.get_mut(&key).unwrap().phase = Phase::Removing;

        let herdr_runner = FakeRunner::new();
        let list = format!(r#"{{"result":{{"worktrees":[{{"path":"{}","open_workspace_id":null}}]}}}}"#, path.display());
        herdr_runner.on("worktree list", Output::ok(&list));
        let runner = HerdrOnFake { herdr: &herdr_runner };
        let cli = crate::herdr::HerdrCli::new(&runner, None);
        let ctx = Ctx { cfg: &cfg, runner: &runner, herdr: &cli, now: Utc::now() };
        assert_eq!(remove(&ctx, &mut s, &key).unwrap(), None);
        assert!(!path.exists());
        assert!(s.get(&key).is_none());
        let lines = herdr_runner.lines();
        assert!(lines.iter().all(|l| !l.contains("worktree open") && !l.contains("worktree remove")), "{lines:?}");
    }

    #[test]
    fn release_refuses_unpushed_commit_then_accepts_after_push() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&path, &["checkout", "-q", "-b", "stack/minha"]);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "empilhado"]);
        adopt(&mut s, &key);
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains("origin"), "{err}");
        assert!(path.exists());

        git(&path, &["push", "-q", "origin", "stack/minha"]);
        let msg = release(&ctx, &mut s, &key).unwrap();
        assert!(msg.contains("liberado"));
        assert!(!path.exists());
        assert!(s.get(&key).is_none());
        assert!(branch_exists(&fx, "stack/minha"), "branch empilhada é do usuário");
        assert!(!branch_exists(&fx, BRANCH), "branch do PR foi criada pelo daemon");
    }

    #[test]
    fn release_after_squash_merge_uses_pull_head() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        let head = fx.remote_sha(BRANCH);
        git(&fx.remote, &["update-ref", "refs/pull/7/head", &head]);
        git(&fx.remote, &["update-ref", "-d", "refs/heads/feat/x"]); // squash merge + delete branch
        adopt(&mut s, &key);
        let msg = release(&ctx, &mut s, &key).unwrap();
        assert!(msg.contains("liberado"), "{msg}");
        assert!(!path.exists());
        assert!(s.get(&key).is_none());
    }

    #[test]
    fn release_without_pull_head_stays_conservative() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&fx.remote, &["update-ref", "-d", "refs/heads/feat/x"]);
        adopt(&mut s, &key);
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains("origin"), "{err}");
        assert!(path.exists());
    }

    #[test]
    fn release_ignores_stale_refs_of_other_remotes() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        git(&path, &["checkout", "-q", "-b", "stack/minha"]);
        git(&path, &["commit", "-q", "--allow-empty", "-m", "empilhado"]);
        let local = git(&path, &["rev-parse", "HEAD"]);
        git(&fx.clone, &["update-ref", "refs/remotes/backup/velha", &local]);
        adopt(&mut s, &key);
        assert!(release(&ctx, &mut s, &key).is_err());
        assert!(path.exists());
    }

    #[test]
    fn release_refuses_local_changes() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::write(path.join("feature.txt"), "mexi\n").unwrap();
        adopt(&mut s, &key);
        let err = release(&ctx, &mut s, &key).unwrap_err().to_string();
        assert!(err.contains("feature.txt"), "{err}");
        assert!(path.exists());
    }

    #[test]
    fn release_refuses_ignored_env_file() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        let path = ready(&ctx, &mut s, &key);
        std::fs::write(path.join(".env"), "SEGREDO=1").unwrap();
        adopt(&mut s, &key);
        assert!(release(&ctx, &mut s, &key).is_err());
        assert!(path.join(".env").exists());
    }

    #[test]
    fn release_suppresses_when_pending_or_unknown() {
        for remote in [Remote::Pending, Remote::Unknown] {
            let fx = fixture(BRANCH);
            let cfg = config(&fx, "true");
            let herdr = FakeHerdr::new();
            let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
            let (mut s, key) = creating(&fx, &cfg);
            ready(&ctx, &mut s, &key);
            let when: DateTime<Utc> = "2026-10-07T10:00:00Z".parse().unwrap();
            let rec = s.get_mut(&key).unwrap();
            rec.last_request_event_at = Some(when);
            rec.remote = remote.clone();
            adopt(&mut s, &key);
            release(&ctx, &mut s, &key).unwrap();
            assert_eq!(s.suppressions[&key.to_string()].after, Some(when), "{remote:?}");
        }
    }

    #[test]
    fn release_rejects_managed() {
        let fx = fixture(BRANCH);
        let cfg = config(&fx, "true");
        let herdr = FakeHerdr::new();
        let ctx = Ctx { cfg: &cfg, runner: &RealRunner, herdr: &herdr, now: Utc::now() };
        let (mut s, key) = creating(&fx, &cfg);
        ready(&ctx, &mut s, &key);
        assert!(release(&ctx, &mut s, &key).is_err());
    }
}
