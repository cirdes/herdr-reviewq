use crate::git::Git;
use crate::runner::Runner;
use crate::state::PrRecord;
use anyhow::{anyhow, Result};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub exists: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub op_in_progress: bool,
    /// Arquivos rastreados diferentes do HEAD (index ou working tree).
    pub tracked: Vec<String>,
    /// Arquivos não rastreados e não ignorados.
    pub untracked: Vec<String>,
    /// Arquivos rastreados marcados com skip-worktree/assume-unchanged (edições invisíveis ao diff).
    pub flagged: Vec<String>,
    /// Arquivos/pastas ignorados que não estão na lista de descartáveis.
    pub foreign_ignored: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Divergence {
    Missing,
    Diverged(String),
}

fn split_z(s: &str) -> Vec<String> {
    s.split('\0').filter(|x| !x.is_empty()).map(String::from).collect()
}

fn sample(list: &[String]) -> String {
    let mut s = list.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if list.len() > 3 {
        s.push_str(&format!(" (+{})", list.len() - 3));
    }
    s
}

impl Facts {
    pub fn missing() -> Self {
        Self {
            exists: false,
            branch: None,
            head: None,
            op_in_progress: false,
            tracked: vec![],
            untracked: vec![],
            flagged: vec![],
            foreign_ignored: vec![],
        }
    }

    /// Por que o worktree não está impecável (sem olhar branch/HEAD). `None` = impecável.
    pub fn dirt(&self) -> Option<String> {
        if self.op_in_progress {
            return Some("operação git em andamento".into());
        }
        if !self.tracked.is_empty() {
            return Some(format!("arquivos alterados: {}", sample(&self.tracked)));
        }
        if !self.flagged.is_empty() {
            return Some(format!("arquivos com skip-worktree/assume-unchanged: {}", sample(&self.flagged)));
        }
        if !self.untracked.is_empty() {
            return Some(format!("arquivos novos: {}", sample(&self.untracked)));
        }
        if !self.foreign_ignored.is_empty() {
            return Some(format!("ignorados fora da lista de descartáveis: {}", sample(&self.foreign_ignored)));
        }
        None
    }
}

/// `disposable` são caminhos relativos à raiz do worktree, como o git os lista.
pub fn is_disposable(path: &str, disposable: &[String]) -> bool {
    let p = path.trim_end_matches('/');
    disposable.iter().any(|d| {
        let d = d.trim_matches('/');
        !d.is_empty() && (p == d || p.starts_with(&format!("{d}/")))
    })
}

pub fn collect(runner: &dyn Runner, path: &Path, disposable: &[String]) -> Result<Facts> {
    if !path.exists() {
        return Ok(Facts::missing());
    }
    if !path.join(".git").exists() {
        return Err(anyhow!("{} existe mas não é um worktree git", path.display()));
    }
    let git = Git::read_only(runner);
    let branch = {
        let o = git.raw(path, &["symbolic-ref", "-q", "--short", "HEAD"], 60)?;
        if o.success() { Some(o.stdout.trim().to_string()) } else { None }
    };
    let head = git.ref_sha(path, "HEAD")?;
    let git_dir = std::path::PathBuf::from(git.ok(path, &["rev-parse", "--absolute-git-dir"])?);
    let op_in_progress = ["MERGE_HEAD", "sequencer", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG", "rebase-merge", "rebase-apply"]
        .iter()
        .any(|f| git_dir.join(f).exists());
    let tracked = split_z(&git.ok(path, &["diff", "HEAD", "--name-only", "-z"])?);
    let flagged = split_z(&git.ok(path, &["ls-files", "-v", "-z"])?)
        .into_iter()
        .filter(|e| e.chars().next().is_some_and(|c| c == 'S' || c.is_ascii_lowercase()))
        .map(|e| e.chars().skip(2).collect::<String>())
        .collect();
    let untracked = split_z(&git.ok(path, &["ls-files", "--others", "--exclude-standard", "-z"])?);
    let foreign_ignored = split_z(&git.ok(path, &["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"])?)
        .into_iter()
        .filter(|p| !is_disposable(p, disposable))
        .collect();
    Ok(Facts { exists: true, branch, head, op_in_progress, tracked, untracked, flagged, foreign_ignored })
}

/// Compara o worktree com o que o daemon colocou. `None` = impecável e intocado.
pub fn divergence(rec: &PrRecord, f: &Facts) -> Option<Divergence> {
    if !f.exists {
        return Some(Divergence::Missing);
    }
    if f.branch.as_deref() != Some(rec.head_ref.as_str()) {
        return Some(Divergence::Diverged(format!(
            "trocou para {}",
            f.branch.as_deref().unwrap_or("HEAD destacado")
        )));
    }
    if rec.managed_sha.is_none() {
        return Some(Divergence::Diverged("sha gerenciado desconhecido".into()));
    }
    if f.head != rec.managed_sha {
        return Some(Divergence::Diverged("HEAD mudou (commit ou reset local)".into()));
    }
    f.dirt().map(Divergence::Diverged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Git;
    use crate::runner::RealRunner;
    use crate::testutil::{fixture, git, Fixture};
    use std::path::PathBuf;

    fn disposable() -> Vec<String> {
        vec!["node_modules".into(), "vendor/bundle".into()]
    }

    fn worktree(fx: &Fixture) -> PathBuf {
        Git::new(&RealRunner).fetch_branch(&fx.clone, "feat/x").unwrap();
        let wt = fx.root.join("wt-facts");
        git(&fx.clone, &["worktree", "add", "-q", "-b", "feat/x", wt.to_str().unwrap(), "origin/feat/x"]);
        wt
    }

    fn facts(wt: &Path) -> Facts {
        collect(&RealRunner, wt, &disposable()).unwrap()
    }

    struct Recording(std::cell::RefCell<Vec<crate::runner::Cmd>>);

    impl Runner for Recording {
        fn run(&self, c: &crate::runner::Cmd) -> Result<crate::runner::Output> {
            self.0.borrow_mut().push(c.clone());
            RealRunner.run(c)
        }
    }

    #[test]
    fn collect_never_takes_optional_locks() {
        let fx = fixture("feat/x");
        let wt = worktree(&fx);
        let rec = Recording(Default::default());
        collect(&rec, &wt, &disposable()).unwrap();
        let calls = rec.0.borrow();
        assert!(!calls.is_empty());
        for c in calls.iter() {
            assert_eq!(c.program, "git");
            assert_eq!(c.args[0], "--no-optional-locks", "{}", c.line());
            assert_eq!(c.args[1], "-C");
        }
    }

    #[test]
    fn fresh_worktree_is_pristine() {
        let fx = fixture("feat/x");
        let wt = worktree(&fx);
        let f = facts(&wt);
        assert!(f.exists);
        assert_eq!(f.branch.as_deref(), Some("feat/x"));
        assert_eq!(f.head.as_deref(), Some(fx.remote_sha("feat/x").as_str()));
        assert_eq!(f.dirt(), None);
    }

    #[test]
    fn each_kind_of_local_change_is_dirt() {
        let fx = fixture("feat/x");
        let wt = worktree(&fx);

        std::fs::create_dir_all(wt.join("node_modules/pkg")).unwrap();
        std::fs::write(wt.join("node_modules/pkg/index.js"), "x").unwrap();
        assert_eq!(facts(&wt).dirt(), None, "node_modules é descartável");

        std::fs::write(wt.join(".env"), "SEGREDO=1").unwrap();
        assert!(facts(&wt).dirt().unwrap().contains(".env"));
        std::fs::remove_file(wt.join(".env")).unwrap();

        std::fs::create_dir_all(wt.join("private")).unwrap();
        std::fs::write(wt.join("private/notas.txt"), "x").unwrap();
        assert!(facts(&wt).dirt().unwrap().contains("private"));
        std::fs::remove_dir_all(wt.join("private")).unwrap();

        std::fs::write(wt.join("anotacoes.txt"), "x").unwrap();
        assert!(facts(&wt).dirt().unwrap().contains("anotacoes.txt"));
        std::fs::remove_file(wt.join("anotacoes.txt")).unwrap();

        std::fs::write(wt.join("feature.txt"), "editado\n").unwrap();
        assert!(facts(&wt).dirt().unwrap().contains("feature.txt"));
    }

    #[test]
    fn skip_worktree_and_assume_unchanged_edits_are_dirt() {
        let fx = fixture("feat/x");
        let wt = worktree(&fx);
        git(&wt, &["update-index", "--skip-worktree", "feature.txt"]);
        std::fs::write(wt.join("feature.txt"), "editado\n").unwrap();
        assert!(facts(&wt).dirt().unwrap().contains("feature.txt"));
        git(&wt, &["update-index", "--no-skip-worktree", "feature.txt"]);
        git(&wt, &["checkout", "--", "feature.txt"]);
        git(&wt, &["update-index", "--assume-unchanged", "README"]);
        std::fs::write(wt.join("README"), "editado\n").unwrap();
        assert!(facts(&wt).dirt().unwrap().contains("README"));
    }

    #[test]
    fn existing_non_git_dir_is_error() {
        let d = tempfile::TempDir::new().unwrap();
        assert!(collect(&RealRunner, d.path(), &[]).is_err());
    }

    #[test]
    fn unknown_managed_sha_is_divergence() {
        let fx = fixture("feat/x");
        let wt = worktree(&fx);
        let f = facts(&wt);
        let mut rec = crate::state::PrRecord::fixture("o/r", 1, "feat/x", f.head.as_deref().unwrap());
        rec.managed_sha = None;
        assert!(matches!(divergence(&rec, &f), Some(Divergence::Diverged(r)) if r.contains("desconhecido")));
    }

    #[test]
    fn missing_path() {
        assert!(!collect(&RealRunner, Path::new("/nao/existe/pr-1"), &[]).unwrap().exists);
    }

    #[test]
    fn divergence_rules() {
        let fx = fixture("feat/x");
        let wt = worktree(&fx);
        let f = facts(&wt);
        let rec = crate::state::PrRecord::fixture("o/r", 1, "feat/x", f.head.as_deref().unwrap());
        assert_eq!(divergence(&rec, &f), None);

        std::fs::write(wt.join("anotacoes.txt"), "x").unwrap();
        assert!(matches!(divergence(&rec, &facts(&wt)), Some(Divergence::Diverged(r)) if r.contains("anotacoes")));
        std::fs::remove_file(wt.join("anotacoes.txt")).unwrap();

        git(&wt, &["commit", "-q", "--allow-empty", "-m", "meu commit"]);
        assert!(matches!(divergence(&rec, &facts(&wt)), Some(Divergence::Diverged(r)) if r.contains("HEAD")));

        git(&wt, &["checkout", "-q", "-b", "stack"]);
        assert!(matches!(divergence(&rec, &facts(&wt)), Some(Divergence::Diverged(r)) if r.contains("stack")));

        assert_eq!(divergence(&rec, &Facts::missing()), Some(Divergence::Missing));
    }

    #[test]
    fn disposable_paths_are_relative_to_root() {
        let d = vec!["node_modules".to_string(), "vendor/bundle".to_string(), "tmp".to_string()];
        assert!(is_disposable("node_modules/", &d));
        assert!(is_disposable("vendor/bundle/", &d));
        assert!(is_disposable("tmp/cache/", &d));
        assert!(!is_disposable("app/frontend/node_modules/", &d));
        assert!(!is_disposable("private/tmp/", &d));
        assert!(!is_disposable("tmpfile", &d));
        assert!(!is_disposable("vendor/", &d));
        assert!(!is_disposable(".env", &d));
    }
}
