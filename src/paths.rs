use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn from_home(home: PathBuf) -> Self {
        Self {
            config_file: home.join(".config/herdr-reviewq/config.toml"),
            state_dir: home.join(".local/state/herdr-reviewq"),
            home,
        }
    }

    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME não definido")?;
        Ok(Self::from_home(PathBuf::from(home)))
    }

    pub fn state_file(&self) -> PathBuf { self.state_dir.join("state.json") }
    pub fn lock_file(&self) -> PathBuf { self.state_dir.join("daemon.lock") }
    pub fn log_file(&self) -> PathBuf { self.state_dir.join("daemon.log") }
    pub fn setup_logs_dir(&self) -> PathBuf { self.state_dir.join("logs") }
    /// Log do TUI (motivo de saída forçada, pânicos fora da thread da UI).
    pub fn tui_log_file(&self) -> PathBuf { self.setup_logs_dir().join("tui.log") }
    /// Log do setup de um PR; única convenção do nome, usada pelo daemon (grava) e pelo TUI (`l`).
    pub fn setup_log(&self, key: &crate::state::PrKey) -> PathBuf {
        self.setup_logs_dir().join(format!("{}-pr-{}.log", key.repo.replace('/', "-"), key.number))
    }
    pub fn requests_dir(&self) -> PathBuf { self.state_dir.join("requests") }
    pub fn ui_lock_file(&self) -> PathBuf { self.state_dir.join("ui.lock") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PrKey;

    #[test]
    fn setup_log_names_repo_and_pr() {
        let p = Paths::from_home(PathBuf::from("/h"));
        assert_eq!(p.setup_log(&PrKey::new("o/r", 7)), PathBuf::from("/h/.local/state/herdr-reviewq/logs/o-r-pr-7.log"));
    }
}
