use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use herdr_reviewq::config::{grace_duration, Config, DEFAULT_REMOVE_GRACE_SECS};
use herdr_reviewq::herdr::{self, Herdr, HerdrCli, UiRouting};
use herdr_reviewq::paths::Paths;
use herdr_reviewq::requests::{self, RequestKind};
use herdr_reviewq::runner::RealRunner;
use herdr_reviewq::state::State;
use herdr_reviewq::{daemon, service, status, ui};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "herdr-reviewq", version, about = "Ready-to-use worktrees for the reviews requested from you")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the reconciler (used by launchd)
    Daemon,
    /// Shows the current state
    Status,
    /// Sends a request to the daemon
    Request {
        kind: Kind,
        /// PR as owner/repo#n
        #[arg(long)]
        pr: Option<String>,
    },
    /// Manages the LaunchAgent
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Interactive panel (herdr plugin pane)
    Tui,
    /// The panel workspace in herdr
    Ui {
        #[command(subcommand)]
        action: UiAction,
    },
    /// Focuses a PR in herdr
    Focus {
        #[command(subcommand)]
        target: FocusTarget,
    },
}

#[derive(Subcommand)]
enum UiAction {
    /// Ensures the reviewq workspace with the TUI (without focus)
    Ensure,
    /// Ensures and focuses the reviewq workspace
    Open,
}

#[derive(Subcommand)]
enum FocusTarget {
    /// The PR that has been ready the longest
    FirstReady,
}

fn load_cfg(p: &Paths) -> Result<Config> {
    Config::load(&p.config_file, &p.home)
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Sync,
    Retry,
    Release,
    Adopt,
    ReleaseAll,
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Restart,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::from_env()?;
    match cli.cmd {
        Command::Daemon => daemon::run(paths),
        Command::Status => {
            let state = State::read(&paths.state_file())?;
            let (grace, repos, poll) = match Config::load(&paths.config_file, &paths.home) {
                Ok(c) => (c.remove_grace_secs, c.repos.iter().map(|r| r.name.clone()).collect::<Vec<_>>(), c.poll_interval_secs),
                Err(_) => (DEFAULT_REMOVE_GRACE_SECS, Vec::new(), 60),
            };
            print!("{}", status::render_at(&state, chrono::Utc::now(), grace_duration(grace), &repos, poll));
            Ok(())
        }
        Command::Request { kind, pr } => {
            let kind = match kind {
                Kind::Sync => RequestKind::Sync,
                Kind::Retry => RequestKind::Retry,
                Kind::Release => RequestKind::Release,
                Kind::Adopt => RequestKind::Adopt,
                Kind::ReleaseAll => RequestKind::ReleaseAll,
            };
            let id = requests::write(&paths.requests_dir(), kind, pr)?;
            println!("request {id} sent; see the result with `herdr-reviewq status`");
            Ok(())
        }
        Command::Service { action } => match action {
            ServiceAction::Install => service::install(&paths),
            ServiceAction::Uninstall => service::uninstall(&paths),
            ServiceAction::Restart => service::restart(),
        },
        Command::Tui => herdr_reviewq::tui::run::run_logged(paths.clone(), || load_cfg(&paths)),
        Command::Ui { action } => {
            let runner = RealRunner;
            let mut cfg = load_cfg(&paths);
            let routing = UiRouting::from_process_env(cfg.as_ref().ok().and_then(|c| c.herdr_session.clone()));
            if let Ok(c) = cfg.as_mut() {
                herdr::resolve_backend(c, &runner, &routing, &paths.home, true);
            }
            let client: Box<dyn Herdr> = match cfg.as_ref() {
                Ok(c) => herdr::client(c, &runner, &routing, &paths.home, ui::UI_CALL_TIMEOUT),
                Err(_) => Box::new(HerdrCli::routed(&runner, &routing).with_timeout(ui::UI_CALL_TIMEOUT)),
            };
            let herdr = client.as_ref();
            let deadline = Instant::now() + ui::ENSURE_TOTAL;
            let r = cfg.and_then(|_| match action {
                UiAction::Ensure => ui::ensure(herdr, &paths.ui_lock_file(), &paths.home, deadline, ui::LOCK_WAIT_CLI).map(|_| ()),
                UiAction::Open => ui::open(herdr, &paths.ui_lock_file(), &paths.home, deadline, ui::LOCK_WAIT_CLI),
            });
            // `ui open` é a action do atalho: o stderr não aparece para ninguém
            match action {
                UiAction::Open => ui::notify_error(herdr, r),
                UiAction::Ensure => r,
            }
        }
        Command::Focus { target: FocusTarget::FirstReady } => {
            let runner = RealRunner;
            let mut cfg = load_cfg(&paths);
            let routing = UiRouting::from_process_env(cfg.as_ref().ok().and_then(|c| c.herdr_session.clone()));
            if let Ok(c) = cfg.as_mut() {
                herdr::resolve_backend(c, &runner, &routing, &paths.home, true);
            }
            let client: Box<dyn Herdr> = match cfg.as_ref() {
                Ok(c) => herdr::client(c, &runner, &routing, &paths.home, ui::UI_CALL_TIMEOUT),
                Err(_) => Box::new(HerdrCli::routed(&runner, &routing).with_timeout(ui::UI_CALL_TIMEOUT)),
            };
            let herdr = client.as_ref();
            let r = cfg.and_then(|cfg| match ui::focus_first_ready(herdr, &cfg, || State::read(&paths.state_file()))? {
                Some(_) => Ok(()),
                None => herdr.notify("reviewq", "no PR ready", "none"),
            });
            ui::notify_error(herdr, r)
        }
    }
}
