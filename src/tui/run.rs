use crate::config::Config;
use crate::herdr::{Herdr, HerdrCli, UiRouting};
use crate::paths::Paths;
use crate::requests;
use crate::runner::{Cmd, RealRunner, Runner};
use crate::state::{PrKey, State};
use crate::tui::keys::{handle, Action, Key};
use crate::tui::model::{reselect, Sent, UiFacts, UiState, View, ViewConfig};
use crate::ui::UI_CALL_TIMEOUT;
use anyhow::{bail, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

static STOP: AtomicBool = AtomicBool::new(false);
/// Último sinal recebido (para o motivo da saída no `tui.log`).
static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
    STOP.store(true, Ordering::SeqCst);
}

/// Por que o TUI saiu sem erro; vai para o `tui.log`.
#[derive(Debug, Clone, PartialEq)]
pub enum Exit {
    /// Tecla de saída (`q`, `Esc`, `Ctrl-C`).
    Key(String),
    /// SIGINT, SIGTERM ou SIGHUP.
    Signal(i32),
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Key(k) => write!(f, "tecla {k}"),
            Exit::Signal(libc::SIGINT) => f.write_str("sinal SIGINT"),
            Exit::Signal(libc::SIGTERM) => f.write_str("sinal SIGTERM"),
            Exit::Signal(libc::SIGHUP) => f.write_str("sinal SIGHUP"),
            Exit::Signal(n) => write!(f, "sinal {n}"),
        }
    }
}

fn key_name(k: &Key) -> String {
    match k {
        Key::Up => "↑".into(),
        Key::Down => "↓".into(),
        Key::Enter => "Enter".into(),
        Key::Esc => "Esc".into(),
        Key::CtrlC => "Ctrl-C".into(),
        Key::Char(c) => c.to_string(),
    }
}

/// Registra no `tui.log` como o TUI terminou. O pane fecha junto com o processo e leva o stderr,
/// então este é o único rastro de um erro do TUI.
pub fn log_outcome(log: &Path, r: &Result<Exit>) {
    let msg = match r {
        Ok(exit) => format!("tui encerrado: {exit}"),
        Err(e) => format!("tui encerrou com erro: {e:#}"),
    };
    let _ = crate::applog::append(log, &msg);
}

/// Braço `tui` do `main`: registra início e fim (ou erro) no `tui.log` e devolve o erro ao `main`.
pub fn run_logged(paths: Paths, load_cfg: impl FnOnce() -> Result<Config>) -> Result<()> {
    let log = paths.tui_log_file();
    let _ = crate::applog::append(&log, &format!("tui iniciado (pid {})", std::process::id()));
    let r = load_cfg().and_then(|cfg| run(paths, cfg));
    log_outcome(&log, &r);
    r.map(|_| ())
}

pub fn key_from_event(e: &Event) -> Option<Key> {
    // `Event::Paste` (bracketed paste) e qualquer outro evento que não seja tecla são ignorados
    let Event::Key(k) = e else { return None };
    // `Repeat` (com REPORT_EVENT_TYPES) só vale para rolar a lista segurando a tecla; ações nunca repetem
    let scroll = matches!(k.code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k'))
        && !k.modifiers.contains(KeyModifiers::CONTROL);
    match k.kind {
        KeyEventKind::Press => {}
        KeyEventKind::Repeat if scroll => {}
        _ => return None,
    }
    match k.code {
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlC),
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        _ => None,
    }
}

pub fn log_command(path: &Path) -> String {
    format!("less +F '{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

// Etapas do terminal já ativadas; `restore_terminal` desfaz só estas, na ordem inversa.
const RAW: u8 = 1;
const ALT: u8 = 2;
const HIDDEN: u8 = 4;
const PASTE: u8 = 8;
const KBD: u8 = 16;
static ACTIVE: AtomicU8 = AtomicU8::new(0);

fn mark(step: u8) {
    ACTIVE.fetch_or(step, Ordering::SeqCst);
}

/// Idempotente (o hook de pânico e o `Drop` podem chamar os dois).
fn restore_terminal() {
    let active = ACTIVE.swap(0, Ordering::SeqCst);
    let mut out = std::io::stdout();
    if active & KBD != 0 {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if active & PASTE != 0 {
        let _ = execute!(out, DisableBracketedPaste);
    }
    if active & HIDDEN != 0 {
        let _ = execute!(out, cursor::Show);
    }
    if active & ALT != 0 {
        let _ = execute!(out, LeaveAlternateScreen);
    }
    if active & RAW != 0 {
        let _ = disable_raw_mode();
    }
}

struct TerminalGuard;

impl TerminalGuard {
    /// O guard existe logo após o raw mode: se uma etapa seguinte falhar, o `Drop` desfaz as anteriores.
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        mark(RAW);
        let guard = TerminalGuard;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen)?;
        mark(ALT);
        execute!(out, cursor::Hide)?;
        mark(HIDDEN);
        execute!(out, EnableBracketedPaste)?;
        mark(PASTE);
        if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES))?;
            mark(KBD);
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn osc52(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64_lite::encode(text.as_bytes()));
    let _ = out.flush();
}

mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(b: &[u8]) -> String {
        let mut o = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    o.push('=');
                }
            }
        }
        o
    }
}

/// O que o worker precisa para executar uma ação.
struct WorkerCtx<'a> {
    herdr: &'a dyn Herdr,
    runner: &'a dyn Runner,
    cfg: &'a Config,
    paths: &'a Paths,
    routing: UiRouting,
    /// `HERDR_PANE_ID` do próprio TUI (onde abrir o split do log).
    pane: Option<String>,
}

/// Executa `f` e transforma um pânico em mensagem, para o worker sobreviver.
fn guarded(f: impl FnOnce() -> String) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(msg) => msg,
        Err(p) => {
            let why = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "pânico".into());
            format!("a ação falhou: {why}")
        }
    }
}

/// Uma ação do worker. Relê o estado do disco antes de agir (o snapshot do loop pode estar velho):
/// o PR tem de continuar no painel, e URL e log saem do registro relido.
fn perform(action: &Action, ctx: &WorkerCtx) -> String {
    let (Action::Focus(key) | Action::OpenUrl(key, _) | Action::ShowLog(key)) = action else { return String::new() };
    let state = match State::read(&ctx.paths.state_file()) {
        Ok(s) => s,
        Err(e) => return format!("não consegui ler o estado: {e:#}"),
    };
    if let Action::Focus(_) = action {
        return match crate::ui::focus_pr(ctx.herdr, ctx.cfg, &state, key) {
            Ok(()) => format!("abrindo {key}"),
            Err(e) => format!("{e:#}"),
        };
    }
    let Some(rec) = state.get(key) else { return format!("{key} não está mais no painel") };
    if let Action::OpenUrl(..) = action {
        let url = &rec.url;
        let opened = ctx
            .runner
            .run(&Cmd::new("open").arg(url.clone()).timeout(UI_CALL_TIMEOUT))
            .map(|o| o.success())
            .unwrap_or(false);
        let head = if opened { "abrindo no navegador desta máquina" } else { "não consegui abrir o navegador" };
        return format!("{head} · URL enviada ao clipboard do terminal: {url}");
    }
    let log = ctx.paths.setup_log(&rec.key);
    if !log.exists() {
        return "sem log ainda".into();
    }
    match &ctx.pane {
        None => format!("log em {}", log.display()),
        Some(pane) => show_log(ctx.runner, &ctx.routing, pane, &log),
    }
}

/// Comando herdr roteado como os demais da UI (bin e `--session` do `UiRouting`).
fn herdr_cmd(routing: &UiRouting) -> Cmd {
    let c = Cmd::new(&routing.bin).timeout(UI_CALL_TIMEOUT);
    match &routing.session {
        Some(s) => c.arg("--session").arg(s.clone()),
        None => c,
    }
}

fn show_log(runner: &dyn Runner, routing: &UiRouting, pane: &str, log: &Path) -> String {
    let split = runner.run(&herdr_cmd(routing).args(["pane", "split", pane, "--direction", "down"]));
    let new_pane = split
        .ok()
        .filter(|o| o.success())
        .and_then(|o| crate::herdr::parse_response(&o).ok())
        .and_then(|v| v.pointer("/pane/pane_id").and_then(|p| p.as_str()).map(String::from));
    let Some(new_pane) = new_pane else { return "não consegui abrir o split do log".into() };
    match runner.run(&herdr_cmd(routing).args(["pane", "run", &new_pane]).arg(log_command(log))) {
        Ok(o) if o.success() => "log aberto abaixo".into(),
        _ => "não consegui rodar o less no split".into(),
    }
}

/// Ações enviadas ao worker e ainda não terminadas (na fila ou rodando).
#[derive(Clone, Default)]
struct Pending(Arc<Mutex<Vec<Action>>>);

impl Pending {
    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Action>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `false` se uma ação idêntica já está pendente.
    fn claim(&self, action: &Action) -> bool {
        let mut list = self.list();
        if list.contains(action) {
            return false;
        }
        list.push(action.clone());
        true
    }

    fn done(&self, action: &Action) {
        let mut list = self.list();
        if let Some(i) = list.iter().position(|a| a == action) {
            list.remove(i);
        }
    }
}

/// Fila de ações do TUI para o worker, sem duplicatas.
struct ActionQueue {
    tx: mpsc::Sender<Action>,
    pending: Pending,
}

impl ActionQueue {
    fn new() -> (Self, mpsc::Receiver<Action>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx, pending: Pending::default() }, rx)
    }

    /// `Ok(false)`: descartada, porque uma idêntica ainda está pendente ou rodando.
    fn submit(&self, action: Action) -> Result<bool> {
        if !self.pending.claim(&action) {
            return Ok(false);
        }
        if self.tx.send(action.clone()).is_err() {
            self.pending.done(&action);
            bail!("o worker de ações do TUI parou");
        }
        Ok(true)
    }
}

fn spawn_worker(cfg: Config, paths: Paths) -> (ActionQueue, mpsc::Receiver<String>) {
    let (queue, rx) = ActionQueue::new();
    let pending = queue.pending.clone();
    let (back, msgs) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("tui-worker".into())
        .spawn(move || {
            let runner = RealRunner;
            let routing = UiRouting::from_process_env(cfg.herdr_session.clone());
            let herdr = HerdrCli::routed(&runner, &routing).with_timeout(UI_CALL_TIMEOUT);
            let ctx = WorkerCtx {
                herdr: &herdr,
                runner: &runner,
                cfg: &cfg,
                paths: &paths,
                routing: routing.clone(),
                pane: std::env::var("HERDR_PANE_ID").ok().filter(|p| !p.is_empty()),
            };
            for action in rx {
                let msg = guarded(|| perform(&action, &ctx));
                pending.done(&action);
                if back.send(msg).is_err() {
                    break;
                }
            }
        })
        .expect("não consegui criar a thread do worker do TUI");
    (queue, msgs)
}

/// Estado lido do disco pelo loop, relido quando o mtime muda ou quando a última leitura falhou.
#[derive(Default)]
struct StateWatch {
    state: Option<State>,
    mtime: Option<SystemTime>,
    read_error: Option<String>,
}

impl StateWatch {
    fn check(&mut self, file: &Path) {
        let m = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        // com erro de leitura, tenta de novo mesmo sem mudança de mtime (o conserto pode ter caído no mesmo instante)
        if m == self.mtime && self.state.is_some() && self.read_error.is_none() {
            return;
        }
        self.mtime = m;
        match State::read(file) {
            Ok(s) => {
                self.state = if m.is_some() { Some(s) } else { None };
                self.read_error = None;
            }
            Err(e) => self.read_error = Some(format!("estado ilegível: {e:#}")),
        }
    }
}

/// PRs cujo worktree sumiu do disco.
fn missing_worktrees(state: &State) -> BTreeSet<PrKey> {
    state.prs.values().filter(|r| !r.path.exists()).map(|r| r.key.clone()).collect()
}

/// Batida do loop principal (ms desde `EPOCH`), para o watchdog.
static BEAT_MS: AtomicU64 = AtomicU64::new(0);
/// Sem batida há tanto tempo, o loop está preso (o `event::poll` volta em 250 ms).
const STALL: Duration = Duration::from_secs(5);
/// Depois de um sinal, quanto o loop tem para sair sozinho.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// Acordar do watchdog com atraso maior que isso = o processo ficou suspenso (SIGSTOP) e voltou.
const OVERSLEEP: Duration = Duration::from_secs(1);

/// Decisão do watchdog: `Some((código, motivo))` quando o processo deve sair à força.
/// O crossterm 0.28 entra em laço dentro do `event::poll` quando o tty some (read devolve EOF/EIO
/// para sempre), então nem o `STOP` de um SIGHUP é visto pelo loop. Um loop parado sozinho não
/// basta (pode ser só lentidão): só mata quando `tty_alive` confirma que o terminal sumiu.
fn watchdog_verdict(since_beat: Duration, since_stop: Option<Duration>, tty_alive: &dyn Fn() -> bool) -> Option<(i32, &'static str)> {
    if since_stop.is_some_and(|t| t >= STOP_GRACE) {
        return Some((0, "sinal recebido e o loop não saiu em 1 s"));
    }
    if since_beat >= STALL && !tty_alive() {
        return Some((1, "loop parado e o terminal sumiu"));
    }
    None
}

/// Estado do watchdog entre um acordar e outro.
struct Watchdog {
    last_wake: Instant,
    stop_seen: Option<Instant>,
    /// Última retomada depois de suspensão: o tempo sem batida conta a partir daqui.
    resumed_at: Option<Instant>,
}

impl Watchdog {
    fn new(now: Instant) -> Self {
        Self { last_wake: now, stop_seen: None, resumed_at: None }
    }

    fn tick(&mut self, now: Instant, beat: Instant, stop: bool, tty_alive: &dyn Fn() -> bool) -> Option<(i32, &'static str)> {
        let overslept = now.saturating_duration_since(self.last_wake) > OVERSLEEP;
        self.last_wake = now;
        if overslept {
            // suspenso e retomado: dá ao loop a chance de bater antes de julgar
            self.resumed_at = Some(now);
            return None;
        }
        if stop {
            self.stop_seen.get_or_insert(now);
        }
        let since = self.resumed_at.map_or(beat, |r| r.max(beat));
        watchdog_verdict(now.saturating_duration_since(since), self.stop_seen.map(|t| now.saturating_duration_since(t)), tty_alive)
    }
}

/// O terminal em `fd` sumiu de verdade: `poll` com POLLHUP/POLLNVAL, ou `tcgetattr` falhando com
/// EIO/ENXIO/EBADF. Um fd que nunca foi tty (ENOTTY) não conta como sumido.
fn tty_gone(fd: libc::c_int) -> bool {
    // no macOS o POLLHUP de um pty sem mestre só vem quando se pede POLLIN; entrada pendente sozinha não conta
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    if n > 0 && p.revents & (libc::POLLHUP | libc::POLLNVAL) != 0 {
        return true;
    }
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error();
        return matches!(errno, Some(libc::EIO) | Some(libc::ENXIO) | Some(libc::EBADF));
    }
    false
}

const WATCHDOG_THREAD: &str = "tui-watchdog";

/// Registra o motivo da saída forçada: primeiro no `tui.log`, depois em `err` (o stderr, que é o
/// pty do pane). Nunca entra em pânico: com o pane fechado, escrever no pty dá EIO, e `eprintln!`
/// entraria em pânico antes do `exit`, deixando o processo girando.
fn report_exit(log: &Path, err: &mut dyn Write, why: &str) {
    let msg = format!("tui encerrado pelo watchdog: {why}");
    let _ = crate::applog::append(log, &msg);
    let _ = writeln!(err, "herdr-reviewq: {msg}");
    let _ = err.flush();
}

/// Saída forçada do watchdog. Nada aqui entra em pânico (`restore_terminal` só descarta erros).
fn force_exit(code: i32, log: &Path, why: &str) -> ! {
    restore_terminal();
    report_exit(log, &mut std::io::stderr(), why);
    std::process::exit(code)
}

fn spawn_watchdog(epoch: Instant, log: PathBuf) {
    let fallback_log = log.clone();
    let spawned = std::thread::Builder::new().name(WATCHDOG_THREAD.into()).spawn(move || {
        let mut dog = Watchdog::new(Instant::now());
        let tty_alive = || !tty_gone(libc::STDIN_FILENO);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let beat = epoch + Duration::from_millis(BEAT_MS.load(Ordering::SeqCst));
            if let Some((code, why)) = dog.tick(Instant::now(), beat, STOP.load(Ordering::SeqCst), &tty_alive) {
                force_exit(code, &log, why);
            }
        }
    });
    if let Err(e) = spawned {
        // sem watchdog o TUI ainda funciona; só perde a saída forçada com o tty morto
        let _ = crate::applog::append(&fallback_log, &format!("não consegui criar a thread do watchdog: {e}"));
    }
}

/// Pânico fora da thread da UI: o TUI segue de pé, então o texto vai para o `tui.log`.
fn record_panic(log: &Path, thread: Option<&str>, info: &dyn std::fmt::Display) {
    let _ = crate::applog::append(log, &format!("pânico na thread {}: {info}", thread.unwrap_or("sem nome")));
}

fn install_signals() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let prev = unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
        if prev == libc::SIG_ERR {
            bail!("não consegui instalar o tratador do sinal {sig}: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn run(paths: Paths, cfg: Config) -> Result<Exit> {
    install_signals()?;
    let default_hook = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    let tui_log = paths.tui_log_file();
    std::panic::set_hook(Box::new(move |info| {
        // pânico do worker é pego por `guarded` e vira mensagem no TUI, que segue de pé:
        // nem restaura o terminal nem escreve no stderr por cima da tela; o texto vai para o tui.log
        let current = std::thread::current();
        if current.id() == ui_thread {
            restore_terminal();
            // o pane fecha com o processo e leva o stderr: o pânico também fica no tui.log
            record_panic(&tui_log, Some("da UI"), info);
            default_hook(info);
        } else {
            record_panic(&tui_log, current.name(), info);
            if current.name() == Some(WATCHDOG_THREAD) {
                // o watchdog é quem tira o processo de um tty morto: se ele cair, sai aqui mesmo
                restore_terminal();
                std::process::exit(1);
            }
        }
    }));
    let _guard = TerminalGuard::enter()?;
    // o que foi digitado antes (ex.: durante o roubo de foco ao abrir o pane) não vira ação
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let view_cfg = ViewConfig {
        repos: cfg.repos.iter().map(|r| r.name.clone()).collect(),
        poll_interval_secs: cfg.poll_interval_secs,
        remove_grace: cfg.remove_grace(),
    };
    let (work, results) = spawn_worker(cfg, paths.clone());
    let mut ui = UiState::default();
    let mut watch = StateWatch::default();
    let mut facts = UiFacts::default();
    let mut last_check = Instant::now() - Duration::from_secs(1);
    let epoch = Instant::now();
    spawn_watchdog(epoch, paths.tui_log_file());
    loop {
        BEAT_MS.store(epoch.elapsed().as_millis() as u64, Ordering::SeqCst);
        if STOP.load(Ordering::SeqCst) {
            return Ok(Exit::Signal(SIGNAL.load(Ordering::SeqCst)));
        }
        if last_check.elapsed() >= Duration::from_millis(500) {
            last_check = Instant::now();
            watch.check(&paths.state_file());
            // a cada releitura (e também quando só o disco mudou: um worktree apagado não muda o estado)
            facts.missing_worktrees = watch.state.as_ref().map(missing_worktrees).unwrap_or_default();
        }
        loop {
            match results.try_recv() {
                Ok(msg) if !msg.is_empty() => ui.message = Some(msg),
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => bail!("o worker de ações do TUI parou"),
            }
        }
        let mut view = View::build(watch.state.as_ref(), &ui, &view_cfg, &facts, chrono::Utc::now());
        if let Some(e) = &watch.read_error {
            view.header.errors.push(e.clone());
        }
        let old_order = std::mem::replace(&mut ui.order, view.order());
        ui.selected = reselect(&old_order, ui.selected.as_ref(), &view);
        terminal.draw(|f| crate::tui::render::draw(f, &view, &ui))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let ev = event::read()?;
        let Some(key) = key_from_event(&ev) else { continue };
        let (next, action) = handle(key, &ui, &view);
        ui = next;
        match action {
            None => {}
            Some(Action::Quit) => return Ok(Exit::Key(key_name(&key))),
            Some(Action::Request(kind, pr)) => match requests::write(&paths.requests_dir(), kind, pr.as_ref().map(|k| k.to_string())) {
                Ok(id) => ui.sent.push(Sent { id, kind, pr, at: chrono::Utc::now() }),
                Err(e) => ui.message = Some(format!("não consegui enviar o pedido: {e:#}")),
            },
            Some(other) => {
                if let Action::OpenUrl(_, url) = &other {
                    osc52(url);
                }
                // `false`: idêntica ainda pendente ou rodando, descartada
                work.submit(other)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;
    use crate::runner::{FakeRunner, Output};
    use crate::state::{Phase, PrKey, PrRecord};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent { code, modifiers: mods, kind, state: KeyEventState::NONE })
    }

    #[test]
    fn presses_become_keys_and_repeat_only_scrolls() {
        assert_eq!(key_from_event(&ev(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Press)), Some(Key::Char('r')));
        assert_eq!(key_from_event(&ev(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Repeat)), None);
        // segurar ↑/↓/j/k rola a lista; nenhuma outra tecla repete (ações nunca)
        assert_eq!(key_from_event(&ev(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Down));
        assert_eq!(key_from_event(&ev(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Up));
        assert_eq!(key_from_event(&ev(KeyCode::Char('j'), KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Char('j')));
        assert_eq!(key_from_event(&ev(KeyCode::Char('k'), KeyModifiers::NONE, KeyEventKind::Repeat)), Some(Key::Char('k')));
        assert_eq!(key_from_event(&ev(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('r'), KeyModifiers::NONE, KeyEventKind::Release)), None);
        assert_eq!(key_from_event(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL, KeyEventKind::Press)), Some(Key::CtrlC));
        assert_eq!(key_from_event(&Event::Paste("y".into())), None);
        assert_eq!(key_from_event(&ev(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Press)), Some(Key::Up));
    }

    #[test]
    fn exit_outcome_goes_to_tui_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/tui.log");
        log_outcome(&log, &Ok(Exit::Key("q".into())));
        log_outcome(&log, &Ok(Exit::Signal(libc::SIGTERM)));
        log_outcome(&log, &Err(anyhow::anyhow!("pty sumiu").context("event::read")));
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("tui encerrado: tecla q"), "{text}");
        assert!(lines[1].ends_with("tui encerrado: sinal SIGTERM"), "{text}");
        assert!(lines[2].ends_with("tui encerrou com erro: event::read: pty sumiu"), "{text}");
    }

    #[test]
    fn exit_reason_names_signals() {
        assert_eq!(Exit::Signal(libc::SIGHUP).to_string(), "sinal SIGHUP");
        assert_eq!(Exit::Signal(libc::SIGINT).to_string(), "sinal SIGINT");
        assert_eq!(Exit::Signal(99).to_string(), "sinal 99");
    }

    #[test]
    fn log_command_quotes_path() {
        assert_eq!(log_command(Path::new("/a b/it's.log")), "less +F '/a b/it'\\''s.log'");
    }

    #[test]
    fn base64_matches_standard_encoding() {
        assert_eq!(base64_lite::encode(b"url"), "dXJs");
        assert_eq!(base64_lite::encode(b"ur"), "dXI=");
        assert_eq!(base64_lite::encode(b"u"), "dQ==");
        assert_eq!(base64_lite::encode(b""), "");
    }

    struct Fx {
        _dir: tempfile::TempDir,
        paths: Paths,
        cfg: Config,
        wt: std::path::PathBuf,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let cfg = Config::parse("worktrees_dir = \"/w\"\n[[repos]]\nname = \"o/r\"\npath = \"/repo\"\nsetup = []\n", dir.path()).unwrap();
        Fx { _dir: dir, paths, cfg, wt }
    }

    fn save(fx: &Fx, phase: Phase) {
        let mut rec = PrRecord::fixture("o/r", 7, "x", "s");
        rec.path = fx.wt.clone();
        rec.phase = phase;
        let mut s = State::default();
        s.insert(rec);
        s.save(&fx.paths.state_file()).unwrap();
    }

    fn routing() -> UiRouting {
        UiRouting { bin: "/opt/herdr".into(), session: Some("main".into()) }
    }

    #[test]
    fn focus_rereads_state_before_acting() {
        let fx = fx();
        let h = FakeHerdr::new();
        h.open.borrow_mut().insert("w42".into(), ("/repo".into(), fx.wt.clone()));
        let runner = FakeRunner::new();
        let key = PrKey::new("o/r", 7);
        let ctx = WorkerCtx { herdr: &h, runner: &runner, cfg: &fx.cfg, paths: &fx.paths, routing: routing(), pane: None };

        save(&fx, Phase::Removing);
        let msg = perform(&Action::Focus(key.clone()), &ctx);
        assert!(msg.contains("removido"), "{msg}");
        assert!(h.focused.borrow().is_none());

        save(&fx, Phase::Ready);
        let msg = perform(&Action::Focus(key.clone()), &ctx);
        assert_eq!(msg, "abrindo o/r#7");
        assert_eq!(h.focused.borrow().as_deref(), Some("w42"));

        std::fs::remove_file(fx.paths.state_file()).unwrap();
        std::fs::write(fx.paths.state_file(), "{lixo").unwrap();
        let msg = perform(&Action::Focus(key), &ctx);
        assert!(msg.contains("estado"), "{msg}");
    }

    #[test]
    fn open_url_message_always_has_the_url_and_never_claims_copy() {
        let fx = fx();
        save(&fx, Phase::Ready);
        let url = "https://github.com/o/r/pull/7";
        let h = FakeHerdr::new();
        let runner = FakeRunner::new();
        runner.on(&format!("open {url}"), Output::ok(""));
        runner.on(&format!("open {url}"), Output::fail(1, "sem navegador"));
        let ctx = WorkerCtx { herdr: &h, runner: &runner, cfg: &fx.cfg, paths: &fx.paths, routing: routing(), pane: None };
        // a URL vem do estado relido, não da que a tecla carregou
        let action = Action::OpenUrl(PrKey::new("o/r", 7), "https://velha".into());
        let ok = perform(&action, &ctx);
        let bad = perform(&action, &ctx);
        for m in [&ok, &bad] {
            assert!(m.contains(url), "{m}");
            assert!(!m.contains("copiada"), "{m}");
        }
        assert!(ok.contains("abrindo"), "{ok}");
        assert!(bad.contains("não consegui"), "{bad}");
    }

    #[test]
    fn open_url_and_show_log_reread_state_and_refuse_vanished_pr() {
        let fx = fx();
        save(&fx, Phase::Ready);
        let h = FakeHerdr::new();
        let runner = FakeRunner::new();
        let ctx = WorkerCtx { herdr: &h, runner: &runner, cfg: &fx.cfg, paths: &fx.paths, routing: routing(), pane: None };
        let gone = PrKey::new("o/r", 8);
        let log = fx.paths.setup_log(&gone);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "ok").unwrap();
        let m = perform(&Action::OpenUrl(gone.clone(), "https://github.com/o/r/pull/8".into()), &ctx);
        assert!(m.contains("não está mais no painel"), "{m}");
        let m = perform(&Action::ShowLog(gone), &ctx);
        assert!(m.contains("não está mais no painel"), "{m}");
        assert!(runner.calls.borrow().is_empty(), "PR sumido: nada roda");

        std::fs::write(fx.paths.state_file(), "{lixo").unwrap();
        let m = perform(&Action::OpenUrl(PrKey::new("o/r", 7), "x".into()), &ctx);
        assert!(m.contains("estado"), "{m}");
    }

    #[test]
    fn show_log_uses_ui_routing_and_falls_back_to_path() {
        let fx = fx();
        let h = FakeHerdr::new();
        let key = PrKey::new("o/r", 7);
        let runner = FakeRunner::new();
        save(&fx, Phase::Ready);
        let ctx = WorkerCtx { herdr: &h, runner: &runner, cfg: &fx.cfg, paths: &fx.paths, routing: routing(), pane: None };
        assert_eq!(perform(&Action::ShowLog(key.clone()), &ctx), "sem log ainda");

        let log = fx.paths.setup_logs_dir().join("o-r-pr-7.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "ok").unwrap();
        let msg = perform(&Action::ShowLog(key.clone()), &ctx);
        assert_eq!(msg, format!("log em {}", log.display()));

        runner.on("pane split", Output::ok(r#"{"result":{"pane":{"pane_id":"w1:p9"}}}"#));
        runner.on("pane run", Output::ok(r#"{"result":{}}"#));
        let ctx = WorkerCtx { pane: Some("w1:p2".into()), ..ctx };
        assert_eq!(perform(&Action::ShowLog(key), &ctx), "log aberto abaixo");
        let calls = runner.calls.borrow();
        assert_eq!(calls[0].line(), "/opt/herdr --session main pane split w1:p2 --direction down");
        assert_eq!(calls[1].program, "/opt/herdr");
        assert_eq!(&calls[1].args[..5], &["--session", "main", "pane", "run", "w1:p9"]);
        assert_eq!(calls[1].args[5], log_command(&log));
    }

    #[test]
    fn guarded_turns_panic_into_message() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let msg = guarded(|| panic!("boom"));
        std::panic::set_hook(prev);
        assert!(msg.contains("falhou") && msg.contains("boom"), "{msg}");
        assert_eq!(guarded(|| "ok".to_string()), "ok");
    }

    #[test]
    fn watchdog_kills_a_stall_only_when_the_tty_is_gone() {
        let ms = Duration::from_millis;
        let alive = || true;
        let dead = || false;
        let code = |v: Option<(i32, &str)>| v.map(|v| v.0);
        assert_eq!(code(watchdog_verdict(ms(300), None, &dead)), None);
        assert_eq!(code(watchdog_verdict(STALL * 10, None, &alive)), None, "stall com tty vivo: não mata");
        assert_eq!(code(watchdog_verdict(STALL, None, &dead)), Some(1), "stall com tty morto: mata");
        assert_eq!(code(watchdog_verdict(ms(100), Some(ms(500)), &alive)), None, "sinal: o loop ainda tem tempo de sair sozinho");
        assert_eq!(code(watchdog_verdict(ms(100), Some(STOP_GRACE), &alive)), Some(0), "sinal + 1 s: mata");
        // o tty só é consultado quando o loop está parado
        let probes = std::cell::Cell::new(0);
        let counting = || {
            probes.set(probes.get() + 1);
            true
        };
        watchdog_verdict(ms(300), None, &counting);
        assert_eq!(probes.get(), 0);
        watchdog_verdict(STALL, None, &counting);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn watchdog_counts_from_resume_after_oversleep() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let dead = || false;
        let mut w = Watchdog::new(t0);
        // processo parado (SIGSTOP) por 30 s e retomado: o tick da retomada não julga
        let resumed = t0 + Duration::from_secs(30);
        assert_eq!(w.tick(resumed, t0, false, &dead), None);
        // tick normal logo depois, o loop ainda não bateu: conta a partir da retomada, não da batida velha
        assert_eq!(w.tick(resumed + ms(200), t0, false, &dead), None);
        // o loop bate e segue normal
        assert_eq!(w.tick(resumed + ms(400), resumed + ms(300), false, &dead), None);
        // sem batida por STALL depois da retomada e com o tty morto: mata
        let mut now = resumed + ms(400);
        let mut verdict = None;
        while verdict.is_none() && now < resumed + STALL * 2 {
            now += ms(200);
            verdict = w.tick(now, resumed + ms(300), false, &dead);
        }
        assert_eq!(verdict.map(|v| v.0), Some(1));
        assert!(now >= resumed + ms(300) + STALL, "{:?}", now - resumed);
    }

    #[test]
    fn watchdog_kills_one_second_after_an_ignored_signal() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let alive = || true;
        let mut w = Watchdog::new(t0);
        let mut now = t0;
        let mut verdict = None;
        while verdict.is_none() && now < t0 + ms(3000) {
            now += ms(200);
            verdict = w.tick(now, now, true, &alive);
        }
        assert_eq!(verdict.map(|v| v.0), Some(0));
        assert!(now - t0 >= STOP_GRACE, "{:?}", now - t0);
    }

    #[test]
    fn tty_probe_sees_hangup_and_bad_fd() {
        let (mut master, mut slave) = (0, 0);
        let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        assert!(!tty_gone(slave), "pty aberto: vivo");
        unsafe { libc::close(master) };
        assert!(tty_gone(slave), "mestre fechado: o terminal sumiu");
        unsafe { libc::close(slave) };
        // fd que nunca existiu (um número baixo poderia ser reaberto por outro teste em paralelo)
        assert!(tty_gone(9_999), "EBADF/POLLNVAL");
    }

    #[test]
    fn state_watch_retries_after_read_error_even_with_same_mtime() {
        let fx = fx();
        let file = fx.paths.state_file();
        let mut w = StateWatch::default();
        save(&fx, Phase::Ready);
        w.check(&file);
        assert!(w.state.is_some() && w.read_error.is_none());

        std::fs::write(&file, "{lixo").unwrap();
        let bad_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        w.check(&file);
        assert!(w.read_error.is_some(), "estado ilegível vira erro");
        assert!(w.state.is_some(), "mantém o último estado bom");

        // conserta com o mesmo mtime (gravação no mesmo instante): relê mesmo assim
        save(&fx, Phase::Removing);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(bad_mtime).unwrap();
        w.check(&file);
        assert!(w.read_error.is_none(), "{:?}", w.read_error);
        assert_eq!(w.state.as_ref().unwrap().get(&PrKey::new("o/r", 7)).unwrap().phase, Phase::Removing);
    }

    #[test]
    fn duplicate_actions_are_dropped_while_pending_or_running() {
        let (q, rx) = ActionQueue::new();
        let focus = Action::Focus(PrKey::new("o/r", 7));
        assert!(q.submit(focus.clone()).unwrap());
        assert!(!q.submit(focus.clone()).unwrap(), "idêntica pendente: descarta");
        assert!(q.submit(Action::ShowLog(PrKey::new("o/r", 7))).unwrap(), "outra ação passa");
        assert!(q.submit(Action::Focus(PrKey::new("o/r", 8))).unwrap(), "mesma ação, outro PR, passa");
        let got = rx.recv().unwrap();
        assert_eq!(got, focus);
        assert!(!q.submit(focus.clone()).unwrap(), "rodando: ainda descarta");
        q.pending.done(&got);
        assert!(q.submit(focus.clone()).unwrap(), "terminou: aceita de novo");
        drop(rx);
        assert!(q.submit(Action::Focus(PrKey::new("o/r", 9))).is_err(), "worker parado");
    }

    /// Stderr de um pty cujo mestre fechou: toda escrita falha com EIO.
    struct EioWriter;

    impl std::io::Write for EioWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }
    }

    #[test]
    fn exit_report_survives_eio_on_stderr_and_still_logs() {
        let fx = fx();
        let log = fx.paths.tui_log_file();
        report_exit(&log, &mut EioWriter, "loop parado e o terminal sumiu");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("tui encerrado pelo watchdog: loop parado e o terminal sumiu"), "{text}");
    }

    #[test]
    fn exit_report_survives_a_hung_up_pty() {
        use std::os::fd::FromRawFd;
        let fx = fx();
        let log = fx.paths.tui_log_file();
        let (mut master, mut slave) = (0, 0);
        let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        unsafe { libc::close(master) };
        let mut tty = unsafe { std::fs::File::from_raw_fd(slave) };
        assert!(std::io::Write::write_all(&mut tty, b"x\n").is_err(), "pty sem mestre: a escrita falha");
        report_exit(&log, &mut tty, "sinal recebido e o loop não saiu em 1 s");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("sinal recebido"), "{text}");
    }

    #[test]
    fn worker_panic_is_logged_to_tui_log() {
        let fx = fx();
        record_panic(&fx.paths.tui_log_file(), Some("worker"), &"panicked at src/x.rs:1:1:\nboom");
        let text = std::fs::read_to_string(fx.paths.tui_log_file()).unwrap();
        assert!(text.contains("worker") && text.contains("boom"), "{text}");
    }

    #[test]
    fn missing_worktrees_lists_prs_without_path() {
        let fx = fx();
        let mut s = State::default();
        let mut here = PrRecord::fixture("o/r", 1, "x", "s");
        here.path = fx.wt.clone();
        let mut gone = PrRecord::fixture("o/r", 2, "x", "s");
        gone.path = fx.wt.join("sumiu");
        s.insert(here);
        s.insert(gone);
        assert_eq!(missing_worktrees(&s), [PrKey::new("o/r", 2)].into());
    }
}
