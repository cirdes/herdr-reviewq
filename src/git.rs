use crate::runner::{Cmd, Output, Runner};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::Duration;

pub struct Git<'a> {
    runner: &'a dyn Runner,
    no_optional_locks: bool,
}

impl<'a> Git<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        Self { runner, no_optional_locks: false }
    }

    /// Para leituras: `git --no-optional-locks`, que não disputa o index.lock com o usuário.
    pub fn read_only(runner: &'a dyn Runner) -> Self {
        Self { runner, no_optional_locks: true }
    }

    pub fn raw(&self, dir: &Path, args: &[&str], timeout_secs: u64) -> Result<Output> {
        let base = if self.no_optional_locks { Cmd::new("git").arg("--no-optional-locks") } else { Cmd::new("git") };
        self.runner.run(
            &base
                .arg("-C")
                .arg(dir.to_string_lossy())
                .args(args.iter().copied())
                .timeout(Duration::from_secs(timeout_secs)),
        )
    }

    pub fn ok(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self.raw(dir, args, 120)?;
        if !out.success() {
            bail!("git {} falhou: {}", args.join(" "), out.stderr.trim());
        }
        Ok(out.stdout.trim_end().to_string())
    }

    /// Busca só a branch do PR para `refs/remotes/origin/<branch>` e devolve o sha obtido.
    pub fn fetch_branch(&self, repo: &Path, branch: &str) -> Result<String> {
        let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        let out = self.raw(repo, &["fetch", "--no-tags", "origin", &refspec], 600)?;
        if !out.success() {
            bail!("git fetch {branch} falhou: {}", out.stderr.trim());
        }
        self.ok(repo, &["rev-parse", &format!("refs/remotes/origin/{branch}")])
    }

    pub fn fetch_origin(&self, repo: &Path) -> Result<()> {
        let out = self.raw(repo, &["fetch", "--no-tags", "--prune", "origin"], 600)?;
        if !out.success() {
            bail!("git fetch origin falhou: {}", out.stderr.trim());
        }
        Ok(())
    }

    /// Busca o head publicado pelo GitHub em `refs/pull/<n>/head` (sobrevive ao squash merge).
    pub fn fetch_pull_head(&self, repo: &Path, number: u64) -> Result<()> {
        let refspec = format!("+refs/pull/{number}/head:refs/remotes/origin/pr/{number}");
        let out = self.raw(repo, &["fetch", "--no-tags", "origin", &refspec], 600)?;
        if !out.success() {
            bail!("git fetch pull/{number}/head falhou: {}", out.stderr.trim());
        }
        Ok(())
    }

    pub fn ref_sha(&self, dir: &Path, refname: &str) -> Result<Option<String>> {
        let out = self.raw(dir, &["rev-parse", "--verify", "-q", &format!("{refname}^{{commit}}")], 60)?;
        Ok(if out.success() { Some(out.stdout.trim().to_string()) } else { None })
    }

    pub fn set_upstream(&self, wt: &Path, branch: &str) -> Result<()> {
        self.ok(wt, &["branch", &format!("--set-upstream-to=origin/{branch}"), branch]).map(|_| ())
    }

    /// Move o checkout para `target`; aborta se fosse sobrescrever alteração local.
    pub fn reset_keep(&self, wt: &Path, target: &str) -> Result<()> {
        self.ok(wt, &["reset", "--keep", target]).map(|_| ())
    }

    pub fn backup(&self, repo: &Path, number: u64, sha: &str, now: DateTime<Utc>) -> Result<()> {
        let prefix = format!("refs/reviewq/backup/pr-{number}/");
        let mut ts = now.timestamp();
        // Outro sha no mesmo segundo não pode sobrescrever o ref: avança até achar nome livre.
        while self.ref_sha(repo, &format!("{prefix}{ts}"))?.is_some_and(|other| other != sha) {
            ts += 1;
        }
        let name = format!("{prefix}{ts}");
        let existing = self.ok(repo, &["for-each-ref", "--points-at", sha, "--format=%(refname)", &prefix])?;
        // Renova a retenção: cria o ref novo e apaga os antigos do mesmo sha.
        self.ok(repo, &["update-ref", &name, sha])?;
        for old in existing.lines().filter(|l| !l.is_empty() && *l != name) {
            self.ok(repo, &["update-ref", "-d", old])?;
        }
        Ok(())
    }

    /// Apaga a branch só se ela ainda aponta para `expected`. Devolve se apagou.
    pub fn delete_branch_if_at(&self, repo: &Path, branch: &str, expected: &str) -> Result<bool> {
        let out = self.raw(repo, &["update-ref", "-d", &format!("refs/heads/{branch}"), expected], 60)?;
        Ok(out.success())
    }

    pub fn branch_checked_out(&self, repo: &Path, branch: &str) -> Result<bool> {
        let want = format!("branch refs/heads/{branch}");
        Ok(self.ok(repo, &["worktree", "list", "--porcelain"])?.lines().any(|l| l == want))
    }

    /// Só refs de `origin` contam como prova de que o commit está publicado.
    pub fn is_reachable_from_origin(&self, repo: &Path, sha: &str) -> Result<bool> {
        let out = self.ok(repo, &["for-each-ref", "--contains", sha, "--format=%(refname)", "refs/remotes/origin"])?;
        Ok(!out.trim().is_empty())
    }

    pub fn stash_count(&self, repo: &Path) -> Result<usize> {
        Ok(self.ok(repo, &["stash", "list"])?.lines().filter(|l| !l.is_empty()).count())
    }

    /// Sem --force: o git recusa worktree com alteração ou arquivo não rastreado.
    pub fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<()> {
        self.ok(repo, &["worktree", "remove", &path.to_string_lossy()]).map(|_| ())
    }

    pub fn worktree_prune(&self, repo: &Path) -> Result<()> {
        self.ok(repo, &["worktree", "prune"]).map(|_| ())
    }

    pub fn prune_backups(&self, repo: &Path, now: DateTime<Utc>, max_age_days: i64) -> Result<usize> {
        let list = self.ok(repo, &["for-each-ref", "--format=%(refname)", "refs/reviewq/backup"])?;
        let mut removed = 0;
        for name in list.lines() {
            let Some(ts) = name.rsplit('/').next().and_then(|s| s.parse::<i64>().ok()) else { continue };
            if now.timestamp() - ts > max_age_days * 86_400 {
                self.ok(repo, &["update-ref", "-d", name])?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RealRunner;
    use crate::testutil::{fixture, git};

    #[test]
    fn fetch_branch_with_slash_returns_remote_sha() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        assert_eq!(g.fetch_branch(&fx.clone, "feat/x").unwrap(), fx.remote_sha("feat/x"));
    }

    #[test]
    fn delete_branch_only_when_at_expected_sha() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        let sha = g.fetch_branch(&fx.clone, "feat/x").unwrap();
        git(&fx.clone, &["branch", "feat/x", "origin/feat/x"]);
        let main = git(&fx.clone, &["rev-parse", "main"]);
        assert!(!g.delete_branch_if_at(&fx.clone, "feat/x", &main).unwrap());
        assert!(g.ref_sha(&fx.clone, "refs/heads/feat/x").unwrap().is_some());
        assert!(g.delete_branch_if_at(&fx.clone, "feat/x", &sha).unwrap());
        assert!(g.ref_sha(&fx.clone, "refs/heads/feat/x").unwrap().is_none());
    }

    #[test]
    fn backups_are_created_and_pruned_by_age() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        let sha = g.fetch_branch(&fx.clone, "feat/x").unwrap();
        let main = git(&fx.clone, &["rev-parse", "main"]);
        g.backup(&fx.clone, 7, &sha, Utc::now() - chrono::Duration::days(20)).unwrap();
        g.backup(&fx.clone, 7, &main, Utc::now()).unwrap();
        assert_eq!(g.prune_backups(&fx.clone, Utc::now(), 14).unwrap(), 1);
        let left = g.ok(&fx.clone, &["for-each-ref", "--format=%(refname)", "refs/reviewq/backup"]).unwrap();
        assert_eq!(left.lines().count(), 1);
    }

    #[test]
    fn backup_is_idempotent_for_same_sha() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        let sha = g.fetch_branch(&fx.clone, "feat/x").unwrap();
        let count = |g: &Git| g.ok(&fx.clone, &["for-each-ref", "--format=%(refname)", "refs/reviewq/backup/pr-7/"]).unwrap().lines().count();
        g.backup(&fx.clone, 7, &sha, Utc::now() - chrono::Duration::seconds(100)).unwrap();
        g.backup(&fx.clone, 7, &sha, Utc::now()).unwrap();
        assert_eq!(count(&g), 1);
        let main = git(&fx.clone, &["rev-parse", "main"]);
        g.backup(&fx.clone, 7, &main, Utc::now()).unwrap();
        assert_eq!(count(&g), 2);
    }

    #[test]
    fn rebackup_refreshes_retention() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        let sha = g.fetch_branch(&fx.clone, "feat/x").unwrap();
        let now = Utc::now();
        g.backup(&fx.clone, 7, &sha, now - chrono::Duration::days(13)).unwrap();
        g.backup(&fx.clone, 7, &sha, now).unwrap();
        g.prune_backups(&fx.clone, now + chrono::Duration::days(2), 14).unwrap();
        let left = g.ok(&fx.clone, &["for-each-ref", "--format=%(refname)", "refs/reviewq/backup/pr-7/"]).unwrap();
        assert_eq!(left.lines().count(), 1);
    }

    #[test]
    fn reachability_counts_only_origin() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        let sha = g.fetch_branch(&fx.clone, "feat/x").unwrap();
        assert!(g.is_reachable_from_origin(&fx.clone, &sha).unwrap());
        git(&fx.clone, &["checkout", "-q", "-b", "local"]);
        git(&fx.clone, &["commit", "-q", "--allow-empty", "-m", "só local"]);
        let local = git(&fx.clone, &["rev-parse", "HEAD"]);
        git(&fx.clone, &["update-ref", "refs/remotes/backup/velha", &local]);
        assert!(!g.is_reachable_from_origin(&fx.clone, &local).unwrap());
        git(&fx.clone, &["push", "-q", "origin", "local"]);
        g.fetch_origin(&fx.clone).unwrap();
        assert!(g.is_reachable_from_origin(&fx.clone, &local).unwrap());
    }

    #[test]
    fn reset_keep_refuses_to_overwrite_local_change() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        g.fetch_branch(&fx.clone, "feat/x").unwrap();
        let wt = fx.root.join("wt-keep");
        git(&fx.clone, &["worktree", "add", "-q", "-b", "feat/x", wt.to_str().unwrap(), "origin/feat/x"]);
        std::fs::write(wt.join("feature.txt"), "minha edição\n").unwrap();
        let new = fx.author_push("feat/x", "feature.txt", "do autor\n");
        g.fetch_branch(&fx.clone, "feat/x").unwrap();
        let err = g.reset_keep(&wt, &new).unwrap_err();
        assert!(crate::executor::would_overwrite(&format!("{err:#}")), "{err:#}");
        assert_eq!(std::fs::read_to_string(wt.join("feature.txt")).unwrap(), "minha edição\n");
    }

    #[test]
    fn branch_checked_out_detects_other_worktrees() {
        let fx = fixture("feat/x");
        let g = Git::new(&RealRunner);
        g.fetch_branch(&fx.clone, "feat/x").unwrap();
        assert!(!g.branch_checked_out(&fx.clone, "feat/x").unwrap());
        let wt = fx.root.join("wt-other");
        git(&fx.clone, &["worktree", "add", "-q", "-b", "feat/x", wt.to_str().unwrap(), "origin/feat/x"]);
        assert!(g.branch_checked_out(&fx.clone, "feat/x").unwrap());
        assert!(g.branch_checked_out(&fx.clone, "main").unwrap());
    }
}
