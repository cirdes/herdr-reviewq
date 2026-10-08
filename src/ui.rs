use crate::config::Config;
use crate::herdr::Herdr;
use crate::state::{Ownership, Phase, PrKey, Remote, State};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

pub const UI_LABEL: &str = "reviewq";
/// Timeout de cada chamada herdr do cliente da UI (`HerdrCli::with_timeout`).
pub const UI_CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Prazo total de um `ensure` (lock, listagens e espera do TUI).
pub const ENSURE_TOTAL: Duration = Duration::from_secs(15);
/// Espera pelo `ui.lock` quando quem chama é o daemon.
pub const LOCK_WAIT_DAEMON: Duration = Duration::from_secs(2);
/// Espera pelo `ui.lock` quando quem chama é a CLI ou a action do plugin.
pub const LOCK_WAIT_CLI: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub enum UiOutcome {
    AlreadyOpen,
    Opened,
}

/// Lock exclusivo (flock) em `path`, esperando até `wait`. Solto quando o `File` cai.
/// Erro de uma action (`ui open`, `focus first-ready`) também vira notificação do herdr: quem
/// apertou o atalho não vê o stderr. Se a notificação falhar, o erro original segue igual.
pub fn notify_error<T>(herdr: &dyn Herdr, r: Result<T>) -> Result<T> {
    if let Err(e) = &r {
        let _ = herdr.notify(UI_LABEL, &format!("{e:#}"), "none");
    }
    r
}

pub fn lock(path: &Path, wait: Duration) -> Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("não consegui abrir {}", path.display()))?;
    let start = Instant::now();
    loop {
        if f.try_lock_exclusive().is_ok() {
            return Ok(f);
        }
        let elapsed = start.elapsed();
        if elapsed >= wait {
            bail!("outro processo está preparando o painel (ui.lock)");
        }
        std::thread::sleep(Duration::from_millis(50).min(wait - elapsed));
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        bail!("prazo para preparar o painel do reviewq esgotado");
    }
    Ok(left)
}

/// Garante o workspace `reviewq` com o TUI rodando. Não muda o foco: o herdr rouba o foco
/// ao abrir o pane do plugin, e aqui ele é devolvido ao workspace que estava focado antes.
pub fn ensure(herdr: &dyn Herdr, lock_path: &Path, home: &Path, deadline: Instant, lock_wait: Duration) -> Result<(String, UiOutcome)> {
    ensure_inner(herdr, lock_path, home, deadline, lock_wait, true)
}

fn ensure_inner(
    herdr: &dyn Herdr,
    lock_path: &Path,
    home: &Path,
    deadline: Instant,
    lock_wait: Duration,
    restore_focus: bool,
) -> Result<(String, UiOutcome)> {
    let _guard = lock(lock_path, lock_wait.min(remaining(deadline)?))?;
    // a lista é lida depois do lock: outro processo pode ter acabado de criar o painel
    let all = herdr.list_workspaces(deadline)?;
    let previous_focus = all.iter().find(|w| w.focused).map(|w| w.id.clone());
    let mine: Vec<_> = all.into_iter().filter(|w| w.label == UI_LABEL).collect();
    if mine.len() > 1 {
        bail!("workspaces {UI_LABEL} duplicados no herdr; feche um deles");
    }
    let (ws, root) = match mine.into_iter().next() {
        Some(w) => (w.id, None),
        None => herdr.create_workspace(UI_LABEL, home, deadline)?,
    };
    let panes = herdr.list_panes(&ws, deadline)?;
    if panes.iter().any(|p| p.is_reviewq_tui) {
        return Ok((ws, UiOutcome::AlreadyOpen));
    }
    let target = root
        .or_else(|| panes.first().map(|p| p.id.clone()))
        .context("workspace do painel sem pane para ancorar")?;
    herdr.open_plugin_pane(&ws, &target, deadline)?;
    if restore_focus {
        give_focus_back(herdr, &ws, previous_focus.as_deref(), deadline)?;
    }
    let dead = "o painel do reviewq não ficou de pé (confira `herdr plugin log` e se o binário instalado tem `tui`)";
    loop {
        if Instant::now() >= deadline {
            bail!(dead);
        }
        let panes = herdr.list_panes(&ws, deadline)?;
        if panes.iter().any(|p| p.is_reviewq_tui) {
            return Ok((ws, UiOutcome::Opened));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!(dead);
        }
        std::thread::sleep(Duration::from_millis(100).min(left));
    }
}

/// Só devolve o foco se ele foi parar no painel e antes estava em outro workspace conhecido.
fn give_focus_back(herdr: &dyn Herdr, panel: &str, previous: Option<&str>, deadline: Instant) -> Result<()> {
    let Some(previous) = previous.filter(|p| *p != panel) else { return Ok(()) };
    let now = herdr.list_workspaces(deadline)?.into_iter().find(|w| w.focused).map(|w| w.id);
    if now.as_deref() == Some(panel) {
        herdr.focus_workspace(previous, deadline)?;
    }
    Ok(())
}

/// Garante o painel e foca nele.
pub fn open(herdr: &dyn Herdr, lock_path: &Path, home: &Path, deadline: Instant, lock_wait: Duration) -> Result<()> {
    let (ws, _) = ensure_inner(herdr, lock_path, home, deadline, lock_wait, false)?;
    herdr.focus_workspace(&ws, deadline)
}

fn is_first_ready_candidate(r: &crate::state::PrRecord) -> bool {
    r.remote == Remote::Pending && r.ownership == Ownership::Managed && r.phase == Phase::Ready
}

pub fn pick_first_ready(state: &State) -> Option<PrKey> {
    state
        .prs
        .values()
        .filter(|r| is_first_ready_candidate(r))
        .min_by(|a, b| (a.first_seen_at, &a.key).cmp(&(b.first_seen_at, &b.key)))
        .map(|r| r.key.clone())
}

/// `focus first-ready`: escolhe no estado lido e, logo antes de focar, relê o estado e confere
/// que o escolhido ainda é gerenciado, pendente e pronto. Se deixou de ser, escolhe de novo (uma
/// vez) no estado relido. `Ok(None)` quando nenhum está pronto.
pub fn focus_first_ready(herdr: &dyn Herdr, cfg: &Config, read_state: impl Fn() -> Result<State>) -> Result<Option<PrKey>> {
    let Some(key) = pick_first_ready(&read_state()?) else { return Ok(None) };
    let fresh = read_state()?;
    let key = if fresh.get(&key).is_some_and(is_first_ready_candidate) {
        key
    } else {
        match pick_first_ready(&fresh) {
            Some(k) => k,
            None => return Ok(None),
        }
    };
    focus_pr(herdr, cfg, &fresh, &key)?;
    Ok(Some(key))
}

/// Foca o workspace de um PR, reabrindo se fechado. Nunca muta estado.
pub fn focus_pr(herdr: &dyn Herdr, cfg: &Config, state: &State, key: &PrKey) -> Result<()> {
    let rec = state.get(key).with_context(|| format!("{key} não está mais no painel"))?;
    if matches!(rec.phase, Phase::Removing | Phase::Creating) {
        bail!("{key} está sendo criado ou removido");
    }
    if !rec.path.exists() {
        bail!("o worktree de {key} não existe mais");
    }
    let repo = cfg.repo(&key.repo).with_context(|| format!("repo {} fora da config", key.repo))?;
    let ws = herdr
        .find_workspace(&repo.path, &rec.path)?
        .with_context(|| format!("o herdr não encontrou o worktree de {key}"))?;
    herdr.focus_workspace(&ws, Instant::now() + UI_CALL_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{FakeHerdr, PaneInfo, WorkspaceInfo};
    use crate::state::{Phase, PrRecord, State};
    use std::time::{Duration, Instant};

    fn lock_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("state/ui.lock")
    }

    fn soon(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn notify_error_shows_failure_and_keeps_it() {
        let h = FakeHerdr::new();
        assert!(notify_error(&h, Ok(())).is_ok());
        assert!(h.notifications.borrow().is_empty(), "sucesso não notifica");
        let r = notify_error::<()>(&h, Err(anyhow::anyhow!("herdr lento")).context("abrir o painel"));
        assert_eq!(format!("{:#}", r.unwrap_err()), "abrir o painel: herdr lento");
        assert_eq!(*h.notifications.borrow(), vec!["reviewq | abrir o painel: herdr lento".to_string()]);
        // herdr fora do ar: a notificação falha calada e o erro original volta
        h.fail_notify.set(true);
        assert!(notify_error::<()>(&h, Err(anyhow::anyhow!("x"))).is_err());
    }

    /// Fake com o workspace `~` (w1) focado, como numa sessão real.
    fn fake_with_focus() -> FakeHerdr {
        let h = FakeHerdr::new();
        h.workspaces.borrow_mut().push(WorkspaceInfo { id: "w1".into(), label: "~".into(), focused: false });
        *h.focused.borrow_mut() = Some("w1".into());
        h
    }

    fn ui_count(h: &FakeHerdr) -> usize {
        h.workspaces.borrow().iter().filter(|w| w.label == UI_LABEL).count()
    }

    #[test]
    fn ensure_creates_once_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_with_focus();
        let (ws, out) = ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert_eq!(out, UiOutcome::Opened);
        assert_eq!(ui_count(&h), 1);
        let (ws2, out2) = ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert_eq!((ws2, out2), (ws, UiOutcome::AlreadyOpen));
        assert_eq!(h.created_cwds.borrow().as_slice(), &[dir.path().to_path_buf()]);
    }

    #[test]
    fn ensure_restores_focus_stolen_by_plugin_pane() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_with_focus();
        let (ws, _) = ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert_eq!(h.focused.borrow().as_deref(), Some("w1"), "ensure devolve o foco ao workspace anterior");
        assert_eq!(h.focus_calls.borrow().as_slice(), &["w1".to_string()]);
        assert_ne!(ws, "w1");
        // já aberto: nenhuma chamada de foco
        ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert_eq!(h.focus_calls.borrow().len(), 1);
    }

    #[test]
    fn ensure_never_focuses_when_focus_was_not_stolen_or_unknown() {
        let dir = tempfile::tempdir().unwrap();
        // nenhum workspace focado antes: não há para onde devolver
        let h = FakeHerdr::new();
        ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert!(h.focus_calls.borrow().is_empty());
        // o pane não roubou o foco: nada a fazer
        let h = fake_with_focus();
        h.open_steals_focus.set(false);
        ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert!(h.focus_calls.borrow().is_empty());
        assert_eq!(h.focused.borrow().as_deref(), Some("w1"));
    }

    #[test]
    fn ensure_reopens_missing_pane_and_rejects_dead_tui() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_with_focus();
        h.workspaces.borrow_mut().push(WorkspaceInfo { id: "w5".into(), label: UI_LABEL.into(), focused: false });
        h.panes.borrow_mut().insert("w5".into(), vec![PaneInfo { id: "w5:p1".into(), is_reviewq_tui: false }]);
        let (ws, out) = ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        assert_eq!((ws.as_str(), out), ("w5", UiOutcome::Opened));
        assert_eq!(h.opened_on.borrow().as_slice(), &[("w5".to_string(), "w5:p1".to_string())]);
        assert_eq!(ui_count(&h), 1, "reusa o workspace existente");

        let h2 = fake_with_focus();
        h2.tui_starts_dead.set(true);
        let err = ensure(&h2, &lock_path(&dir), dir.path(), soon(50), Duration::from_millis(50)).unwrap_err().to_string();
        assert!(err.contains("não ficou de pé"), "{err}");
    }

    #[test]
    fn ensure_respects_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_with_focus();
        let err = ensure(&h, &lock_path(&dir), dir.path(), Instant::now(), Duration::from_millis(50)).unwrap_err().to_string();
        assert!(err.contains("prazo"), "{err}");
        assert_eq!(ui_count(&h), 0);
    }

    #[test]
    fn ensure_wait_loop_checks_deadline_before_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_with_focus();
        h.tui_starts_dead.set(true);
        h.pane_scan_delay.set(Duration::from_millis(40));
        let start = Instant::now();
        let err = ensure(&h, &lock_path(&dir), dir.path(), soon(150), Duration::from_millis(50)).unwrap_err().to_string();
        assert!(err.contains("prazo") || err.contains("não ficou de pé"), "{err}");
        assert!(start.elapsed() < Duration::from_millis(260), "ensure estourou o prazo: {:?}", start.elapsed());
    }

    /// herdr lento: cada chamada demora `delay`, mas respeita o timeout do `Cmd` como o `RealRunner`
    /// (estoura e devolve `timed_out`). Registra (início, timeout) de cada chamada.
    struct SlowHerdrRunner {
        delay: Duration,
        opened: std::cell::Cell<bool>,
        calls: std::cell::RefCell<Vec<(String, Instant, Duration)>>,
    }

    impl crate::runner::Runner for SlowHerdrRunner {
        fn run(&self, cmd: &crate::runner::Cmd) -> Result<crate::runner::Output> {
            use crate::runner::Output;
            let line = cmd.line();
            self.calls.borrow_mut().push((line.clone(), Instant::now(), cmd.timeout));
            if self.delay > cmd.timeout {
                std::thread::sleep(cmd.timeout);
                return Ok(Output { timed_out: true, ..Default::default() });
            }
            std::thread::sleep(self.delay);
            let focused = if self.opened.get() { "w10" } else { "w1" };
            let body = if line.contains("workspace list") {
                let mut ws = vec![serde_json::json!({"workspace_id":"w1","label":"~","focused": focused == "w1"})];
                if self.calls.borrow().iter().any(|(l, ..)| l.contains("workspace create")) {
                    ws.push(serde_json::json!({"workspace_id":"w10","label":UI_LABEL,"focused": focused == "w10"}));
                }
                serde_json::json!({"result":{"workspaces":ws}})
            } else if line.contains("workspace create") {
                serde_json::json!({"result":{"workspace":{"workspace_id":"w10"},"root_pane":{"pane_id":"w10:p1"}}})
            } else if line.contains("pane list") {
                serde_json::json!({"result":{"panes":[{"pane_id":"w10:p1","workspace_id":"w10"}]}})
            } else if line.contains("process-info") {
                // o TUI nunca sobe: o ensure fica esperando até o prazo
                serde_json::json!({"result":{"process_info":{"foreground_processes":[{"argv":["-zsh"]}]}}})
            } else if line.contains("plugin pane open") {
                self.opened.set(true);
                serde_json::json!({"result":{}})
            } else {
                serde_json::json!({"result":{}})
            };
            Ok(Output::ok(&body.to_string()))
        }
    }

    #[test]
    fn ensure_total_deadline_bounds_every_herdr_call() {
        let dir = tempfile::tempdir().unwrap();
        for delay in [Duration::from_millis(80), Duration::from_secs(3)] {
            let slow = SlowHerdrRunner { delay, opened: Default::default(), calls: Default::default() };
            let h = crate::herdr::HerdrCli::new(&slow, None).with_timeout(UI_CALL_TIMEOUT);
            let start = Instant::now();
            let deadline = start + Duration::from_millis(700);
            assert!(ensure(&h, &lock_path(&dir), dir.path(), deadline, Duration::from_millis(50)).is_err());
            assert!(start.elapsed() < Duration::from_millis(850), "delay {delay:?}: ensure estourou o prazo: {:?}", start.elapsed());
            let calls = slow.calls.borrow();
            assert!(!calls.is_empty());
            for (line, at, timeout) in calls.iter() {
                assert!(*at + *timeout <= deadline + Duration::from_millis(5), "delay {delay:?}: `{line}` com timeout {timeout:?} passa do prazo");
            }
            if delay < Duration::from_millis(200) {
                // passou por todas as etapas (inclusive a devolução do foco) dentro do prazo
                for step in ["workspace create", "plugin pane open", "workspace focus w1"] {
                    assert!(calls.iter().any(|(l, ..)| l.contains(step)), "sem `{step}`: {:?}", calls.iter().map(|c| &c.0).collect::<Vec<_>>());
                }
            }
        }
    }

    #[test]
    fn ensure_refuses_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let h = FakeHerdr::new();
        for id in ["w1", "w2"] {
            h.workspaces.borrow_mut().push(WorkspaceInfo { id: id.into(), label: UI_LABEL.into(), focused: false });
        }
        let err = ensure(&h, &lock_path(&dir), dir.path(), soon(500), Duration::ZERO).unwrap_err().to_string();
        assert!(err.contains("duplicados"), "{err}");
        assert!(h.opened_on.borrow().is_empty());
    }

    #[test]
    fn ensure_propagates_each_herdr_failure() {
        let dir = tempfile::tempdir().unwrap();
        let run = |h: &FakeHerdr| ensure(h, &lock_path(&dir), dir.path(), soon(500), Duration::ZERO);

        let h = fake_with_focus();
        h.fail_herdr.set(true);
        assert!(run(&h).is_err());

        let h = fake_with_focus();
        h.fail_list_workspaces.set(true);
        assert!(run(&h).unwrap_err().to_string().contains("workspace list"));
        assert_eq!(ui_count(&h), 0);

        let h = fake_with_focus();
        h.fail_list_panes.set(true);
        assert!(run(&h).unwrap_err().to_string().contains("pane list"));
        assert!(h.opened_on.borrow().is_empty());

        let h = fake_with_focus();
        h.fail_open_pane.set(true);
        assert!(run(&h).unwrap_err().to_string().contains("plugin pane open"));
        assert!(h.focus_calls.borrow().is_empty());
    }

    #[test]
    fn lock_held_elsewhere_blocks_ensure_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let path = path.clone();
            std::thread::spawn(move || {
                let guard = lock(&path, Duration::ZERO).unwrap();
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                drop(guard);
            })
        };
        locked_rx.recv().unwrap();

        let h = fake_with_focus();
        let err = ensure(&h, &path, dir.path(), soon(2000), Duration::from_millis(150)).unwrap_err().to_string();
        assert!(err.contains("outro processo"), "{err}");
        assert_eq!(ui_count(&h), 0, "sem o lock, nada é criado");

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        ensure(&h, &path, dir.path(), soon(2000), Duration::from_millis(150)).unwrap();
        ensure(&h, &path, dir.path(), soon(2000), Duration::from_millis(150)).unwrap();
        assert_eq!(ui_count(&h), 1);
    }

    #[test]
    fn open_focuses_the_panel() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_with_focus();
        open(&h, &lock_path(&dir), dir.path(), soon(500), Duration::from_millis(50)).unwrap();
        let ui = h.workspaces.borrow().iter().find(|w| w.label == UI_LABEL).unwrap().id.clone();
        assert_eq!(h.focused.borrow().as_deref(), Some(ui.as_str()));
        assert!(!h.focus_calls.borrow().contains(&"w1".to_string()), "open não devolve o foco antes de focar o painel");
    }

    #[test]
    fn first_ready_is_oldest_managed_pending_ready() {
        let mut s = State::default();
        let mut a = PrRecord::fixture("o/r", 2, "x", "s");
        a.first_seen_at = chrono::Utc::now();
        let b = PrRecord::fixture("o/r", 3, "x", "s"); // epoch: mais antigo
        let mut c = PrRecord::fixture("o/r", 1, "x", "s");
        c.phase = Phase::Preparing;
        for r in [a, b, c] {
            s.insert(r);
        }
        assert_eq!(pick_first_ready(&s), Some(crate::state::PrKey::new("o/r", 3)));
        assert_eq!(pick_first_ready(&State::default()), None);
    }

    #[test]
    fn focus_first_ready_revalidates_on_fresh_state() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let mut rec = PrRecord::fixture("o/r", 7, "x", "s");
        rec.path = dir.path().to_path_buf();
        let mut ready = State::default();
        ready.insert(rec.clone());
        // nenhum pronto
        let h = FakeHerdr::new();
        assert_eq!(focus_first_ready(&h, &c, || Ok(State::default())).unwrap(), None);

        // escolhido no primeiro estado, mas deixou de estar pronto no estado relido
        let h = FakeHerdr::new();
        h.open.borrow_mut().insert("w42".into(), ("/repo".into(), rec.path.clone()));
        let mut gone = State::default();
        let mut adopted = rec.clone();
        adopted.ownership = crate::state::Ownership::Adopted { reason: "x".into(), at: chrono::Utc::now() };
        gone.insert(adopted);
        let reads = std::cell::RefCell::new(vec![gone.clone(), ready.clone()]);
        assert_eq!(focus_first_ready(&h, &c, || Ok(reads.borrow_mut().pop().unwrap())).unwrap(), None, "nenhum pronto no estado relido");
        assert!(h.focused.borrow().is_none());

        // deixou de estar pronto, mas outro ficou pronto: escolhe de novo no estado relido
        let mut other = PrRecord::fixture("o/r", 9, "x", "s");
        other.path = dir.path().join("wt9");
        std::fs::create_dir_all(&other.path).unwrap();
        h.open.borrow_mut().insert("w99".into(), ("/repo".into(), other.path.clone()));
        let mut regrown = gone;
        regrown.insert(other);
        let reads = std::cell::RefCell::new(vec![regrown, ready.clone()]);
        let got = focus_first_ready(&h, &c, || Ok(reads.borrow_mut().pop().unwrap())).unwrap();
        assert_eq!(got, Some(crate::state::PrKey::new("o/r", 9)));
        assert_eq!(h.focused.borrow().as_deref(), Some("w99"));
        *h.focused.borrow_mut() = None;

        // ainda pronto: foca
        let reads = std::cell::RefCell::new(vec![ready.clone(), ready]);
        let got = focus_first_ready(&h, &c, || Ok(reads.borrow_mut().pop().unwrap())).unwrap();
        assert_eq!(got, Some(crate::state::PrKey::new("o/r", 7)));
        assert_eq!(h.focused.borrow().as_deref(), Some("w42"));
    }

    fn cfg(home: &std::path::Path) -> Config {
        Config::parse("worktrees_dir = \"/w\"\n[[repos]]\nname = \"o/r\"\npath = \"/repo\"\nsetup = []\n", home).unwrap()
    }

    #[test]
    fn focus_pr_focuses_workspace_of_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let h = FakeHerdr::new();
        let mut rec = PrRecord::fixture("o/r", 7, "x", "s");
        rec.path = dir.path().to_path_buf();
        h.open.borrow_mut().insert("w42".into(), ("/repo".into(), rec.path.clone()));
        let mut s = State::default();
        s.insert(rec);
        let key = crate::state::PrKey::new("o/r", 7);
        focus_pr(&h, &cfg(dir.path()), &s, &key).unwrap();
        assert_eq!(h.focused.borrow().as_deref(), Some("w42"));
    }

    #[test]
    fn focus_pr_refuses_unusable_records() {
        let dir = tempfile::tempdir().unwrap();
        let h = FakeHerdr::new();
        let c = cfg(dir.path());
        let mut s = State::default();
        let key = crate::state::PrKey::new("o/r", 7);
        assert!(focus_pr(&h, &c, &s, &key).unwrap_err().to_string().contains("não está mais"));

        let mut rec = PrRecord::fixture("o/r", 7, "x", "s");
        rec.path = dir.path().to_path_buf();
        rec.phase = Phase::Removing;
        s.insert(rec.clone());
        assert!(focus_pr(&h, &c, &s, &key).unwrap_err().to_string().contains("removido"));

        rec.phase = Phase::Ready;
        rec.path = dir.path().join("sumiu");
        s.insert(rec.clone());
        assert!(focus_pr(&h, &c, &s, &key).unwrap_err().to_string().contains("não existe mais"));

        rec.path = dir.path().to_path_buf();
        s.insert(rec);
        assert!(focus_pr(&h, &c, &s, &key).unwrap_err().to_string().contains("não encontrou"));
        assert!(h.focused.borrow().is_none());
    }
}
