use crate::config::Config;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Roda git com identidade fixa; entra em pânico se falhar.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {:?} falhou: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Remoto bare + clone base (o repo principal) + clone do autor do PR.
pub struct Fixture {
    pub _tmp: TempDir,
    pub root: PathBuf,
    pub remote: PathBuf,
    pub clone: PathBuf,
    pub author: PathBuf,
}

pub fn fixture(branch: &str) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let remote = root.join("remote.git");
    let author = root.join("author");
    let clone = root.join("clone");
    git(&root, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    git(&root, &["clone", "-q", remote.to_str().unwrap(), author.to_str().unwrap()]);
    std::fs::write(author.join("README"), "v1\n").unwrap();
    std::fs::write(author.join(".gitignore"), "node_modules/\n.env\nprivate/\n").unwrap();
    git(&author, &["add", "."]);
    git(&author, &["commit", "-qm", "init"]);
    git(&author, &["push", "-q", "origin", "HEAD:main"]);
    git(&author, &["checkout", "-qb", branch]);
    std::fs::write(author.join("feature.txt"), "a\n").unwrap();
    git(&author, &["add", "."]);
    git(&author, &["commit", "-qm", "feat"]);
    git(&author, &["push", "-q", "origin", branch]);
    git(&root, &["clone", "-q", remote.to_str().unwrap(), clone.to_str().unwrap()]);
    Fixture { _tmp: tmp, root, remote, clone, author }
}

impl Fixture {
    /// Autor faz um novo commit e push; devolve o novo sha.
    pub fn author_push(&self, branch: &str, file: &str, content: &str) -> String {
        git(&self.author, &["checkout", "-q", branch]);
        std::fs::write(self.author.join(file), content).unwrap();
        git(&self.author, &["add", "."]);
        git(&self.author, &["commit", "-qm", "mais"]);
        git(&self.author, &["push", "-q", "origin", branch]);
        git(&self.author, &["rev-parse", "HEAD"])
    }

    /// Autor reescreve o último commit e faz force-push; devolve o novo sha.
    pub fn author_force_push(&self, branch: &str) -> String {
        git(&self.author, &["checkout", "-q", branch]);
        git(&self.author, &["commit", "-q", "--amend", "-m", "reescrito"]);
        git(&self.author, &["push", "-q", "-f", "origin", branch]);
        git(&self.author, &["rev-parse", "HEAD"])
    }

    pub fn remote_sha(&self, branch: &str) -> String {
        git(&self.remote, &["rev-parse", branch])
    }

    pub fn wt_dir(&self) -> PathBuf {
        self.root.join("wt")
    }
}

/// Config de teste com um repo `o/r` apontando para o clone do fixture.
pub fn config(fx: &Fixture, setup: &str) -> Config {
    let text = format!(
        "worktrees_dir = \"{}\"\n[[repos]]\nname = \"o/r\"\npath = \"{}\"\nsetup = [\"{}\"]\n",
        fx.wt_dir().display(),
        fx.clone.display(),
        setup
    );
    Config::parse(&text, &fx.root).unwrap()
}
