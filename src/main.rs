use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use herdr_reviewq::config::{grace_duration, Config, DEFAULT_REMOVE_GRACE_SECS};
use herdr_reviewq::herdr::{Herdr, HerdrCli, UiRouting};
use herdr_reviewq::paths::Paths;
use herdr_reviewq::requests::{self, RequestKind};
use herdr_reviewq::runner::RealRunner;
use herdr_reviewq::state::State;
use herdr_reviewq::{daemon, service, status, ui};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "herdr-reviewq", version, about = "Worktrees prontos para os reviews pedidos a você")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Roda o reconciliador (usado pelo launchd)
    Daemon,
    /// Mostra o estado atual
    Status,
    /// Envia um pedido ao daemon
    Request {
        kind: Kind,
        /// PR no formato owner/repo#n
        #[arg(long)]
        pr: Option<String>,
    },
    /// Gerencia o LaunchAgent
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Painel interativo (pane do plugin herdr)
    Tui,
    /// Workspace do painel no herdr
    Ui {
        #[command(subcommand)]
        action: UiAction,
    },
    /// Foca um PR no herdr
    Focus {
        #[command(subcommand)]
        target: FocusTarget,
    },
}

#[derive(Subcommand)]
enum UiAction {
    /// Garante o workspace reviewq com o TUI (sem foco)
    Ensure,
    /// Garante e foca o workspace reviewq
    Open,
}

#[derive(Subcommand)]
enum FocusTarget {
    /// PR pronto há mais tempo
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
            let (grace, repos) = match Config::load(&paths.config_file, &paths.home) {
                Ok(c) => (c.remove_grace_secs, c.repos.iter().map(|r| r.name.clone()).collect::<Vec<_>>()),
                Err(_) => (DEFAULT_REMOVE_GRACE_SECS, Vec::new()),
            };
            print!("{}", status::render_at(&state, chrono::Utc::now(), grace_duration(grace), &repos));
            Ok(())
        }
        Command::Request { kind, pr } => {
            let kind = match kind {
                Kind::Sync => RequestKind::Sync,
                Kind::Retry => RequestKind::Retry,
                Kind::Release => RequestKind::Release,
                Kind::Adopt => RequestKind::Adopt,
            };
            let id = requests::write(&paths.requests_dir(), kind, pr)?;
            println!("pedido {id} enviado; veja o resultado com `herdr-reviewq status`");
            Ok(())
        }
        Command::Service { action } => match action {
            ServiceAction::Install => service::install(&paths),
            ServiceAction::Uninstall => service::uninstall(&paths),
            ServiceAction::Restart => service::restart(),
        },
        Command::Tui => herdr_reviewq::tui::run::run_logged(paths.clone(), || load_cfg(&paths)),
        Command::Ui { action } => {
            let cfg = load_cfg(&paths);
            let runner = RealRunner;
            let routing = UiRouting::from_process_env(cfg.as_ref().ok().and_then(|c| c.herdr_session.clone()));
            let herdr = HerdrCli::routed(&runner, &routing).with_timeout(ui::UI_CALL_TIMEOUT);
            let deadline = Instant::now() + ui::ENSURE_TOTAL;
            let r = cfg.and_then(|_| match action {
                UiAction::Ensure => ui::ensure(&herdr, &paths.ui_lock_file(), &paths.home, deadline, ui::LOCK_WAIT_CLI).map(|_| ()),
                UiAction::Open => ui::open(&herdr, &paths.ui_lock_file(), &paths.home, deadline, ui::LOCK_WAIT_CLI),
            });
            // `ui open` é a action do atalho: o stderr não aparece para ninguém
            match action {
                UiAction::Open => ui::notify_error(&herdr, r),
                UiAction::Ensure => r,
            }
        }
        Command::Focus { target: FocusTarget::FirstReady } => {
            let cfg = load_cfg(&paths);
            let runner = RealRunner;
            let routing = UiRouting::from_process_env(cfg.as_ref().ok().and_then(|c| c.herdr_session.clone()));
            let herdr = HerdrCli::routed(&runner, &routing).with_timeout(ui::UI_CALL_TIMEOUT);
            let r = cfg.and_then(|cfg| match ui::focus_first_ready(&herdr, &cfg, || State::read(&paths.state_file()))? {
                Some(_) => Ok(()),
                None => herdr.notify("reviewq", "nenhum PR pronto", "none"),
            });
            ui::notify_error(&herdr, r)
        }
    }
}
