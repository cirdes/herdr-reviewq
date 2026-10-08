use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    pub worktrees_dir: PathBuf,
    #[serde(default)]
    pub herdr_session: Option<String>,
    #[serde(default = "default_step_timeout")]
    pub step_timeout_secs: u64,
    /// Espera depois que o PR sai da lista antes de remover o worktree.
    #[serde(default = "default_remove_grace")]
    pub remove_grace_secs: u64,
    /// Cadência da coleta "feitas hoje" (mínimo 60).
    #[serde(default = "default_reviews_today_every")]
    pub reviews_today_every_secs: u64,
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    pub path: PathBuf,
    pub setup: Vec<String>,
    #[serde(default = "default_disposable")]
    pub disposable_ignored: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    #[serde(default = "default_true")]
    pub on_ready: bool,
    #[serde(default)]
    pub on_removed: bool,
    #[serde(default = "default_sound")]
    pub sound: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { on_ready: true, on_removed: false, sound: default_sound() }
    }
}

fn default_poll_interval() -> u64 { 60 }
fn default_step_timeout() -> u64 { 900 }
pub const DEFAULT_REMOVE_GRACE_SECS: u64 = 900;
fn default_remove_grace() -> u64 { DEFAULT_REMOVE_GRACE_SECS }
fn default_reviews_today_every() -> u64 { 300 }

/// Limita a um valor que o chrono representa sem pânico.
pub fn grace_duration(secs: u64) -> chrono::Duration {
    chrono::Duration::seconds(secs.min(i32::MAX as u64) as i64)
}
fn default_true() -> bool { true }
fn default_sound() -> String { "request".into() }
fn default_disposable() -> Vec<String> {
    ["node_modules", "vendor/bundle", "tmp", "log", ".bundle"].iter().map(|s| s.to_string()).collect()
}

pub fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

impl Config {
    pub fn parse(text: &str, home: &Path) -> Result<Config> {
        let mut cfg: Config = toml::from_str(text).context("config.toml inválido")?;
        cfg.worktrees_dir = expand_tilde(&cfg.worktrees_dir, home);
        for repo in &mut cfg.repos {
            repo.path = expand_tilde(&repo.path, home);
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path, home: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("não consegui ler {}", path.display()))?;
        Self::parse(&text, home)
    }

    pub fn remove_grace(&self) -> chrono::Duration {
        grace_duration(self.remove_grace_secs)
    }

    pub fn repo(&self, name: &str) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| r.name == name)
    }

    fn validate(&self) -> Result<()> {
        if self.repos.is_empty() {
            bail!("a config precisa de ao menos um [[repos]]");
        }
        for repo in &self.repos {
            let parts: Vec<&str> = repo.name.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
                bail!("repo inválido: {:?} (use owner/name)", repo.name);
            }
        }
        if !["none", "done", "request"].contains(&self.notify.sound.as_str()) {
            bail!("notify.sound deve ser none, done ou request");
        }
        if self.poll_interval_secs < 15 || self.poll_interval_secs > 3600 {
            bail!("poll_interval_secs deve estar entre 15 e 3600");
        }
        if self.reviews_today_every_secs < 60 || self.reviews_today_every_secs > 86400 {
            bail!("reviews_today_every_secs deve estar entre 60 e 86400");
        }
        if self.remove_grace_secs > 604800 {
            bail!("remove_grace_secs deve estar entre 0 e 604800");
        }
        if self.step_timeout_secs < 1 || self.step_timeout_secs > 86400 {
            bail!("step_timeout_secs deve estar entre 1 e 86400");
        }
        let mut seen = std::collections::BTreeSet::new();
        for repo in &self.repos {
            if !seen.insert(repo.name.as_str()) {
                bail!("repo duplicado na config: {}", repo.name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FULL: &str = r#"
poll_interval_secs = 30
worktrees_dir = "~/Workspaces/.reviews"
[[repos]]
name = "acme/app"
path = "~/Workspaces/app"
setup = ["mise trust", "bundle install"]
[notify]
sound = "done"
"#;

    #[test]
    fn parses_and_expands_tilde() {
        let cfg = Config::parse(FULL, Path::new("/Users/x")).unwrap();
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.worktrees_dir, Path::new("/Users/x/Workspaces/.reviews"));
        assert_eq!(cfg.repos[0].path, Path::new("/Users/x/Workspaces/app"));
        assert_eq!(cfg.repos[0].disposable_ignored, vec!["node_modules", "vendor/bundle", "tmp", "log", ".bundle"]);
        assert!(cfg.notify.on_ready);
        assert!(!cfg.notify.on_removed);
        assert_eq!(cfg.notify.sound, "done");
        assert_eq!(cfg.step_timeout_secs, 900);
        assert_eq!(cfg.remove_grace_secs, 900);
        assert!(cfg.herdr_session.is_none());
        assert!(cfg.repo("acme/app").is_some());
        assert!(cfg.repo("outro/repo").is_none());
    }

    #[test]
    fn rejects_empty_repos() {
        let err = Config::parse("worktrees_dir = \"/w\"\nrepos = []\n", Path::new("/h")).unwrap_err();
        assert!(err.to_string().contains("[[repos]]"));
    }

    #[test]
    fn rejects_bad_repo_name() {
        let text = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"app\"\npath = \"/r\"\nsetup = []\n";
        assert!(Config::parse(text, Path::new("/h")).is_err());
    }

    #[test]
    fn rejects_bad_sound_and_short_poll() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        assert!(Config::parse(&format!("{base}[notify]\nsound = \"bip\"\n"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("poll_interval_secs = 5\n{base}"), Path::new("/h")).is_err());
    }

    #[test]
    fn expand_tilde_leaves_absolute_paths() {
        assert_eq!(expand_tilde(Path::new("/abs/p"), Path::new("/h")), Path::new("/abs/p"));
        assert_eq!(expand_tilde(Path::new("~/p"), Path::new("/h")), Path::new("/h/p"));
    }

    #[test]
    fn rejects_unknown_keys() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        let err = Config::parse(&format!("{base}disposable_ignore = [\"tmp\"]\n"), Path::new("/h")).unwrap_err();
        assert!(format!("{err:#}").contains("disposable_ignore"), "{err:#}");
        assert!(Config::parse(&format!("poll_interval = 30\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("{base}[notify]\non_redy = true\n"), Path::new("/h")).is_err());
    }

    #[test]
    fn example_config_parses() {
        let cfg = Config::parse(include_str!("../config.example.toml"), Path::new("/Users/x")).unwrap();
        assert_eq!(cfg.repos[0].name, "owner/repo");
        assert_eq!(cfg.remove_grace_secs, 900);
    }

    #[test]
    fn reviews_today_cadence_and_duplicates() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        let cfg = Config::parse(base, Path::new("/h")).unwrap();
        assert_eq!(cfg.reviews_today_every_secs, 300);
        assert!(Config::parse(&format!("reviews_today_every_secs = 30\n{base}"), Path::new("/h")).is_err());
        let dup = format!("{base}[[repos]]\nname = \"a/b\"\npath = \"/r2\"\nsetup = []\n");
        assert!(Config::parse(&dup, Path::new("/h")).unwrap_err().to_string().contains("duplicado"));
    }

    #[test]
    fn boundary_values_valid() {
        let base = "worktrees_dir = \"/w\"\n[[repos]]\nname = \"a/b\"\npath = \"/r\"\nsetup = []\n";
        // poll_interval_secs: 15–3600
        assert!(Config::parse(&format!("poll_interval_secs = 15\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("poll_interval_secs = 3600\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("poll_interval_secs = 14\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("poll_interval_secs = 3601\n{base}"), Path::new("/h")).is_err());

        // reviews_today_every_secs: 60–86400
        assert!(Config::parse(&format!("reviews_today_every_secs = 60\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("reviews_today_every_secs = 86400\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("reviews_today_every_secs = 59\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("reviews_today_every_secs = 86401\n{base}"), Path::new("/h")).is_err());

        // remove_grace_secs: 0–604800
        assert!(Config::parse(&format!("remove_grace_secs = 0\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("remove_grace_secs = 604800\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("remove_grace_secs = 604801\n{base}"), Path::new("/h")).is_err());

        // step_timeout_secs: 1–86400
        assert!(Config::parse(&format!("step_timeout_secs = 1\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("step_timeout_secs = 86400\n{base}"), Path::new("/h")).is_ok());
        assert!(Config::parse(&format!("step_timeout_secs = 0\n{base}"), Path::new("/h")).is_err());
        assert!(Config::parse(&format!("step_timeout_secs = 86401\n{base}"), Path::new("/h")).is_err());
    }
}
