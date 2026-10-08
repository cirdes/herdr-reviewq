use crate::applog::info;
use crate::config::Config;
use crate::executor::{self, Ctx};
use crate::facts::{self, divergence, Divergence};
use crate::git::Git;
use crate::github::{self, ErrorKind, FetchError, RepoSnapshot, Snapshot};
use crate::herdr::{Herdr, HerdrCli};
use crate::paths::Paths;
use crate::reconcile::{reconcile, Input, Op};
use crate::requests::{self, Entry, Request, RequestKind};
use crate::runner::{Cmd, RealRunner, Runner};
use crate::setup::{mise_wrap, JobStatus, SetupJob, Wrap};
use crate::state::{Ownership, Phase, PrKey, RequestResult, SetupProcess, State};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Hora de início de um processo segundo o `ps`; protege contra pid reutilizado.
fn process_start(runner: &dyn Runner, pid: u32) -> Option<String> {
    let out = runner
        .run(&Cmd::new("/usr/bin/env").args(["LC_ALL=C", "TZ=UTC", "ps", "-o", "lstart=", "-p"]).arg(pid.to_string()))
        .ok()?;
    let s = out.stdout.trim();
    (out.success() && !s.is_empty()).then(|| s.to_string())
}

pub(crate) fn backoff_secs(failures: u32) -> i64 {
    (300i64 << failures.saturating_sub(1).min(3)).min(1800)
}

/// Agenda do contador: sync força; virada do dia (SP) força uma tentativa mesmo em backoff;
/// após falha vale `backoff_until` (contado do fim da tentativa); após sucesso, fim + cadência.
pub(crate) fn counter_due(rt: &crate::state::ReviewsToday, now: DateTime<Utc>, every_secs: u64, requested: bool) -> bool {
    if requested || rt.last_day_forced != Some(crate::today::local_day(now)) {
        return true;
    }
    match (&rt.backoff_until, &rt.last_attempt) {
        (Some(b), _) => now >= *b,
        (None, None) => true,
        (None, Some(a)) => now - a.at >= chrono::Duration::seconds(every_secs as i64),
    }
}

pub struct Daemon<'a> {
    paths: Paths,
    cfg: Config,
    runner: &'a dyn Runner,
    herdr: &'a dyn Herdr,
    /// Cliente herdr do painel (timeout curto por chamada); por padrão o mesmo `herdr`.
    ui_herdr: &'a dyn Herdr,
    pub state: State,
    job: Option<SetupJob>,
    job_pid: Option<u32>,
    last_poll: Option<Instant>,
    last_prune: Option<Instant>,
    last_notify_try: Option<Instant>,
    pub(crate) notify_backoff: Duration,
    wrap: Wrap,
    pub(crate) counter_requested: bool,
    /// O painel já foi garantido nesta execução (depois disso o daemon não o recria).
    pub(crate) panel_ensured: bool,
    panel_alerted: bool,
    /// Falhas seguidas do `ensure` do painel (o alerta só sai a partir da 2ª).
    pub(crate) panel_failures: u32,
    pub(crate) panel_next_try: Option<Instant>,
    pub(crate) panel_backoff: Duration,
    /// Já rodou um tick nesta execução (no primeiro, o painel vem antes do poll).
    ticked: bool,
}

/// Linha do log quando o painel fica de pé; registra a recuperação depois de falhas.
fn panel_ok_message(failures: u32) -> String {
    match failures {
        0 => "painel reviewq garantido".into(),
        n => format!("painel reviewq garantido (recuperado depois de {n} falhas seguidas)"),
    }
}

impl<'a> Daemon<'a> {
    pub fn new(paths: Paths, cfg: Config, runner: &'a dyn Runner, herdr: &'a dyn Herdr, wrap: Wrap) -> Result<Self> {
        let (mut state, corrupt) = State::load(&paths.state_file())?;
        if let Some(p) = corrupt {
            let m = format!("state.json corrompido; isolado em {} e o estado recomeçou vazio", p.display());
            info(&m);
            state.push_alert(m);
        }
        let mut d = Self {
            paths,
            cfg,
            runner,
            herdr,
            ui_herdr: herdr,
            state,
            job: None,
            job_pid: None,
            last_poll: None,
            last_prune: None,
            last_notify_try: None,
            notify_backoff: Duration::from_secs(30),
            wrap,
            counter_requested: false,
            panel_ensured: false,
            panel_alerted: false,
            panel_failures: 0,
            panel_next_try: None,
            panel_backoff: Duration::from_secs(30),
            ticked: false,
        };
        d.recover_orphan_setup();
        if let Err(e) = d.save() {
            info(&format!("{e:#}"));
        }
        Ok(d)
    }

    /// Cliente herdr usado para garantir o painel (o daemon passa um com timeout curto).
    pub fn with_ui_herdr(mut self, ui_herdr: &'a dyn Herdr) -> Self {
        self.ui_herdr = ui_herdr;
        self
    }

    fn save(&self) -> Result<()> {
        self.state.save(&self.paths.state_file())
    }

    /// Tenta garantir o painel `reviewq`. Falha: nova tentativa com backoff (30 s dobrando até
    /// 300 s), o que também limita panes reabertos em sequência; um único alerta por execução, só a
    /// partir da 2ª falha seguida (uma falha isolada, como o herdr ainda subindo, não incomoda).
    pub(crate) fn ensure_panel_now(&mut self) {
        let deadline = Instant::now() + crate::ui::ENSURE_TOTAL;
        match crate::ui::ensure(self.ui_herdr, &self.paths.ui_lock_file(), &self.paths.home, deadline, crate::ui::LOCK_WAIT_DAEMON) {
            Ok(_) => {
                self.panel_ensured = true;
                self.panel_next_try = None;
                info(&panel_ok_message(self.panel_failures));
                self.panel_failures = 0;
            }
            Err(e) => {
                self.panel_failures += 1;
                info(&format!("painel reviewq (falha {} seguida): {e:#}", self.panel_failures));
                if self.panel_failures >= 2 && !self.panel_alerted {
                    self.state.push_alert(format!("painel reviewq não abriu: {e:#}"));
                    self.panel_alerted = true;
                    if let Err(e) = self.save() {
                        info(&format!("{e:#}"));
                    }
                }
                self.panel_next_try = Some(Instant::now() + self.panel_backoff);
                self.panel_backoff = (self.panel_backoff * 2).min(Duration::from_secs(300));
            }
        }
    }

    pub(crate) fn maybe_ensure_panel(&mut self) {
        if self.panel_ensured || stopping() || self.panel_next_try.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.ensure_panel_now();
    }

    fn recover_orphan_setup(&mut self) {
        let Some(sp) = self.state.setup_process.take() else { return };
        match process_start(self.runner, sp.pgid as u32) {
            Some(started) if started == sp.started => {
                unsafe {
                    libc::killpg(sp.pgid, libc::SIGTERM);
                }
                std::thread::sleep(Duration::from_millis(500));
                unsafe {
                    libc::killpg(sp.pgid, libc::SIGKILL);
                }
                let m = format!("setup órfão de {} encerrado (grupo {})", sp.key, sp.pgid);
                info(&m);
                self.state.push_alert(m);
            }
            Some(_) => {
                // pid reutilizado por outro processo: não mexe
                let m = format!("não confirmei que o grupo {} é o setup antigo de {}; nada foi encerrado", sp.pgid, sp.key);
                info(&m);
                self.state.push_alert(m);
            }
            None => self.state.push_alert(format!(
                "setup de {} pode ter deixado processos no grupo {}; confira com `pgrep -lg {}`",
                sp.key, sp.pgid, sp.pgid
            )),
        }
    }

    pub fn tick(&mut self) {
        let force = self.process_requests();
        if stopping() {
            return;
        }
        // ao subir, o painel não espera a criação de worktrees nem a coleta (depois do sucesso,
        // a chamada do fim do tick é barata: `panel_ensured` curto-circuita)
        if !std::mem::replace(&mut self.ticked, true) {
            self.maybe_ensure_panel();
        }
        let due = self.last_poll.is_none_or(|t| t.elapsed() >= Duration::from_secs(self.cfg.poll_interval_secs));
        if force || due {
            self.last_poll = Some(Instant::now());
            self.poll_cycle();
        }
        self.drive_setup();
        self.deliver_notifications();
        self.maybe_collect_reviews_today();
        self.maybe_ensure_panel();
    }

    fn snapshot(&self) -> Snapshot {
        let mut snap = Snapshot::default();
        for repo in &self.cfg.repos {
            let s = if !repo.path.join(".git").exists() {
                RepoSnapshot::Failed(FetchError {
                    kind: ErrorKind::Other,
                    message: format!("clone não encontrado em {}", repo.path.display()),
                })
            } else {
                github::fetch_pending(self.runner, &repo.name)
            };
            snap.repos.insert(repo.name.clone(), s);
        }
        snap
    }

    fn handle_auth(&mut self, snap: &Snapshot) {
        let auth_err = snap.repos.values().find_map(|s| match s {
            RepoSnapshot::Failed(e) if e.kind == ErrorKind::Auth => Some(e.message.clone()),
            _ => None,
        });
        match auth_err {
            Some(msg) if !self.state.auth_error_notified => {
                info(&format!("gh sem autenticação: {msg}"));
                if self.herdr.notify("reviewq: gh sem autenticação", "rode gh auth login nesta máquina", &self.cfg.notify.sound).is_ok() {
                    self.state.auth_error_notified = true;
                }
            }
            None => self.state.auth_error_notified = false,
            _ => {}
        }
    }

    pub fn poll_cycle(&mut self) {
        let snapshot = self.snapshot();
        self.handle_auth(&snapshot);
        let busy = self.job.as_ref().map(|j| j.key.to_string());
        let mut facts_map = BTreeMap::new();
        for (k, rec) in &self.state.prs {
            if Some(k) == busy.as_ref()
                || rec.ownership != Ownership::Managed
                || !matches!(rec.phase, Phase::Ready | Phase::Failed { .. } | Phase::Removing)
            {
                continue;
            }
            let Some(repo) = self.cfg.repo(&rec.key.repo) else { continue };
            match facts::collect(self.runner, &rec.path, &repo.disposable_ignored) {
                Ok(f) => {
                    facts_map.insert(k.clone(), f);
                }
                Err(e) => info(&format!("{k}: falha lendo o worktree: {e:#}")),
            }
        }
        let (next, ops) = reconcile(
            &self.state,
            &Input {
                snapshot: &snapshot,
                facts: &facts_map,
                now: Utc::now(),
                worktrees_dir: &self.cfg.worktrees_dir,
                remove_grace: self.cfg.remove_grace(),
            },
        );
        let previous = std::mem::replace(&mut self.state, next);
        if let Err(e) = self.save() {
            info(&format!("{e:#}; ciclo interrompido antes de qualquer efeito"));
            self.state = previous;
            return;
        }
        for op in ops {
            if stopping() {
                break;
            }
            let (key, touches_tree) = match &op {
                Op::Create(k) | Op::Update(k) | Op::Remove(k) => (k.clone(), true),
                Op::SyncBase(k) => (k.clone(), false),
            };
            if touches_tree && self.job.as_ref().is_some_and(|j| j.key == key) {
                self.job = None; // Drop cancela o setup e mata o grupo
                self.job_pid = None;
                self.state.setup_process = None;
                info(&format!("{key}: setup cancelado"));
            }
            info(&format!("{op:?}"));
            let ctx = Ctx { cfg: &self.cfg, runner: self.runner, herdr: self.herdr, now: Utc::now() };
            match executor::run_op(&ctx, &mut self.state, &op) {
                Ok(Some(note)) => info(&format!("{key}: {note}")),
                Ok(None) => {}
                Err(e) => {
                    info(&format!("{key}: {op:?} falhou: {e:#}"));
                    if let Some(r) = self.state.get_mut(&key) {
                        r.warning = Some(format!("{e:#}"));
                    }
                }
            }
            if let Err(e) = self.save() {
                info(&format!("{e:#}; operações restantes adiadas"));
                break;
            }
        }
        self.prune_backups();
    }

    pub(crate) fn collect_reviews_today_at(&mut self, as_of: DateTime<Utc>) {
        self.collect_reviews_today_timed(as_of, Duration::ZERO);
    }

    /// `extra` soma tempo ao fim da tentativa (permite testar coleta lenta sem dormir).
    pub(crate) fn collect_reviews_today_timed(&mut self, as_of: DateTime<Utc>, extra: Duration) {
        self.collect_reviews_today_with(as_of, extra, &stopping)
    }

    pub(crate) fn collect_reviews_today_with(&mut self, as_of: DateTime<Utc>, extra: Duration, stop: &dyn Fn() -> bool) {
        use crate::state::{ReviewsTodayAttempt, ReviewsTodaySuccess};
        let started = Instant::now();
        let day = crate::today::local_day(as_of);
        let window = github::Window {
            start: crate::today::day_start_utc(day),
            as_of,
            search_since: day.pred_opt().unwrap_or(day),
        };
        let deadline = started + Duration::from_secs(90);
        let repos: Vec<String> = self.cfg.repos.iter().map(|r| r.name.clone()).collect();
        let runner = self.runner;
        let result = github::fetch_viewer(runner, deadline).map_err(|e| (e.message, repos.clone())).and_then(|viewer| {
            let rt = &mut self.state.reviews_today;
            if rt.last_success.as_ref().is_some_and(|s| s.viewer != viewer) {
                rt.last_success = None;
            }
            let mut per_repo = BTreeMap::new();
            let mut failed = Vec::new();
            let mut first_err = None;
            for repo in &repos {
                if stop() {
                    failed.push(repo.clone());
                    first_err.get_or_insert_with(|| "daemon encerrando".to_string());
                    continue;
                }
                match github::count_reviews_today(runner, repo, &viewer, &window, deadline, stop) {
                    Ok(n) => {
                        per_repo.insert(repo.clone(), n);
                    }
                    Err(e) => {
                        failed.push(repo.clone());
                        first_err.get_or_insert(e.message);
                    }
                }
            }
            match first_err {
                Some(err) => Err((err, failed)),
                None => Ok((viewer, per_repo)),
            }
        });
        // SIGTERM no meio: a tentativa não terminou, então não é falha nem conta para o backoff
        if result.is_err() && stop() {
            info("feitas hoje: coleta interrompida pelo encerramento do daemon; tentativa descartada");
            return;
        }
        // o backoff conta do FIM da tentativa
        let ended = as_of + chrono::Duration::from_std(started.elapsed() + extra).unwrap_or_else(|_| chrono::Duration::zero());
        let rt = &mut self.state.reviews_today;
        rt.last_day_forced = Some(day);
        match result {
            Ok((viewer, per_repo)) => {
                rt.last_success = Some(ReviewsTodaySuccess { day, count: per_repo.values().sum(), per_repo, as_of, viewer, repos });
                rt.last_attempt = Some(ReviewsTodayAttempt { at: ended, ok: true, error: None, failed_repos: vec![] });
                rt.failures = 0;
                rt.backoff_until = None;
            }
            Err((err, failed)) => {
                info(&format!("feitas hoje: coleta falhou: {err}"));
                rt.failures = rt.failures.saturating_add(1);
                rt.backoff_until = Some(ended + chrono::Duration::seconds(backoff_secs(rt.failures)));
                rt.last_attempt = Some(ReviewsTodayAttempt { at: ended, ok: false, error: Some(err), failed_repos: failed });
            }
        }
        if let Err(e) = self.save() {
            info(&format!("{e:#}"));
        }
    }

    fn maybe_collect_reviews_today(&mut self) {
        let now = Utc::now();
        if stopping() || !counter_due(&self.state.reviews_today, now, self.cfg.reviews_today_every_secs, self.counter_requested) {
            return;
        }
        self.counter_requested = false;
        self.collect_reviews_today_at(now);
    }

    #[cfg(test)]
    pub(crate) fn process_requests_for_test(&mut self) {
        let _ = self.process_requests();
    }

    fn prune_backups(&mut self) {
        if self.last_prune.is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
            return;
        }
        if stopping() {
            return;
        }
        self.last_prune = Some(Instant::now());
        for repo in &self.cfg.repos {
            if repo.path.join(".git").exists() {
                if let Err(e) = Git::new(self.runner).prune_backups(&repo.path, Utc::now(), 14) {
                    info(&format!("{}: falha ao podar backups: {e:#}", repo.name));
                }
            }
        }
    }

    fn track_job_process(&mut self) {
        let pid = self.job.as_ref().and_then(|j| j.current_pid());
        if pid == self.job_pid {
            return;
        }
        self.job_pid = pid;
        self.state.setup_process = match (pid, self.job.as_ref()) {
            (Some(p), Some(j)) => process_start(self.runner, p).map(|started| SetupProcess { key: j.key.clone(), pgid: p as i32, started }),
            _ => None,
        };
        if let Err(e) = self.save() {
            info(&format!("{e:#}"));
        }
    }

    pub fn drive_setup(&mut self) {
        if let Some(status) = self.job.as_mut().map(|j| j.poll()) {
            self.track_job_process();
            if status == JobStatus::Running {
                return;
            }
            let job = self.job.take().expect("job existe");
            self.job_pid = None;
            self.state.setup_process = None;
            self.finish_job(&job, status);
            if let Err(e) = self.save() {
                info(&format!("{e:#}"));
            }
        }
        if stopping() {
            return;
        }
        let missing: Vec<PrKey> = self
            .state
            .prs
            .values()
            .filter(|r| r.phase == Phase::Preparing && self.cfg.repo(&r.key.repo).is_none())
            .map(|r| r.key.clone())
            .collect();
        for k in missing {
            info(&format!("{k}: repo fora da config; setup não iniciado"));
            if let Some(r) = self.state.get_mut(&k) {
                r.phase = Phase::Failed { step: "iniciar setup".into(), reason: "repo fora da config".into() };
            }
            if let Err(e) = self.save() {
                info(&format!("{e:#}"));
            }
        }
        let next = self
            .state
            .prs
            .values()
            .filter(|r| r.phase == Phase::Preparing && r.ownership == Ownership::Managed)
            .min_by_key(|r| r.first_seen_at)
            .map(|r| r.key.clone());
        let Some(key) = next else { return };
        let Some(rec) = self.state.get(&key).cloned() else { return };
        let Some(repo) = self.cfg.repo(&key.repo) else { return };
        let log = self.paths.setup_log(&key);
        let timeout = Duration::from_secs(self.cfg.step_timeout_secs);
        let target = rec.managed_sha.clone().unwrap_or_default();
        match SetupJob::start(key.clone(), rec.generation, &target, &repo.setup, &rec.path, &log, timeout, self.wrap) {
            Ok(job) => {
                info(&format!("{key}: setup iniciado"));
                self.job = Some(job);
            }
            Err(e) => {
                if let Some(r) = self.state.get_mut(&key) {
                    r.phase = Phase::Failed { step: "iniciar setup".into(), reason: format!("{e:#}") };
                }
                if let Err(e) = self.save() {
                    info(&format!("{e:#}"));
                }
            }
        }
    }

    fn finish_job(&mut self, job: &SetupJob, status: JobStatus) {
        let key = job.key.clone();
        let Some(rec) = self.state.get(&key).cloned() else { return };
        if rec.generation != job.generation
            || rec.phase != Phase::Preparing
            || rec.managed_sha.as_deref() != Some(job.target_sha.as_str())
        {
            info(&format!("{key}: resultado de setup obsoleto descartado"));
            return;
        }
        let facts = self
            .cfg
            .repo(&key.repo)
            .and_then(|repo| facts::collect(self.runner, &rec.path, &repo.disposable_ignored).ok());
        let on_ready = self.cfg.notify.on_ready;
        let Some(rec) = self.state.get_mut(&key) else { return };
        match status {
            JobStatus::Succeeded => {
                info(&format!("{key}: pronto"));
                rec.phase = Phase::Ready;
                rec.prepared_sha = rec.managed_sha.clone();
                rec.notify_pending = on_ready;
            }
            JobStatus::Failed { step, reason } => {
                info(&format!("{key}: setup falhou em {step}: {reason}"));
                rec.phase = Phase::Failed { step, reason };
            }
            JobStatus::Running => {}
        }
        if rec.ownership == Ownership::Managed {
            if let Some(Divergence::Diverged(reason)) = facts.as_ref().and_then(|f| divergence(rec, f)) {
                rec.ownership = Ownership::Adopted { reason: format!("depois do setup: {reason}"), at: Utc::now() };
                rec.warning = Some("o worktree não ficou limpo; ele não será removido sozinho".into());
            }
        }
    }

    pub fn deliver_notifications(&mut self) {
        if self.last_notify_try.is_some_and(|t| t.elapsed() < self.notify_backoff) {
            return;
        }
        let pending: Vec<PrKey> = self
            .state
            .prs
            .values()
            .filter(|r| r.notify_pending && r.phase == Phase::Ready)
            .map(|r| r.key.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        self.last_notify_try = Some(Instant::now());
        for key in pending {
            let Some(rec) = self.state.get(&key).cloned() else { continue };
            if let Err(e) = self.herdr.notify(&format!("#{} pronto para review", key.number), &rec.title, &self.cfg.notify.sound) {
                info(&format!("{key}: notificação falhou: {e:#}"));
                continue;
            }
            if let Some(ws) = &rec.workspace_id {
                let _ = self.herdr.report_metadata(ws, &format!("#{} pronto", key.number));
            }
            if let Some(r) = self.state.get_mut(&key) {
                r.notify_pending = false;
            }
            if let Err(e) = self.save() {
                info(&format!("{e:#}"));
            }
        }
    }

    fn process_requests(&mut self) -> bool {
        let entries = match requests::list(&self.paths.requests_dir()) {
            Ok(e) => e,
            Err(e) => {
                info(&format!("falha lendo pedidos: {e:#}"));
                return false;
            }
        };
        let mut force = false;
        for entry in entries {
            let (path, result) = match entry {
                Entry::Invalid(path, err) => {
                    let id = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let result = (!self.state.request_seen(&id)).then(|| RequestResult {
                        id,
                        kind: "inválido".into(),
                        pr: None,
                        ok: false,
                        message: format!("pedido ilegível: {err}"),
                        at: Utc::now(),
                    });
                    (path, result)
                }
                Entry::Valid(path, req) => {
                    if self.state.request_seen(&req.id) {
                        (path, None)
                    } else {
                        let (ok, message) = match self.handle_request(&req) {
                            Ok(m) => (true, m),
                            Err(e) => (false, format!("{e:#}")),
                        };
                        if ok && matches!(req.kind, RequestKind::Sync | RequestKind::Retry) {
                            force = true;
                        }
                        if ok && req.kind == RequestKind::Sync {
                            self.counter_requested = true;
                        }
                        info(&format!("pedido {:?} {:?}: {message}", req.kind, req.pr));
                        let result = RequestResult {
                            id: req.id.clone(),
                            kind: format!("{:?}", req.kind).to_lowercase(),
                            pr: req.pr.clone(),
                            ok,
                            message,
                            at: Utc::now(),
                        };
                        (path, Some(result))
                    }
                }
            };
            if let Some(r) = result {
                self.state.push_request_result(r);
            }
            // o resultado precisa estar em disco antes de apagar o pedido
            if let Err(e) = self.save() {
                info(&format!("{e:#}; pedido mantido para nova tentativa"));
                break;
            }
            let _ = requests::ack(&path);
        }
        force
    }

    fn handle_request(&mut self, req: &Request) -> Result<String> {
        if req.kind == RequestKind::Sync {
            return Ok("sincronizando".into());
        }
        let raw = req.pr.as_deref().unwrap_or("");
        let key = PrKey::parse(raw).with_context(|| format!("PR inválido {raw:?} (use owner/repo#n)"))?;
        if self.state.get(&key).is_none() {
            bail!("{key} não está sendo acompanhado");
        }
        if req.kind == RequestKind::Release {
            if self.job.as_ref().is_some_and(|j| j.key == key) {
                self.job = None;
                self.job_pid = None;
                self.state.setup_process = None;
            }
            let ctx = Ctx { cfg: &self.cfg, runner: self.runner, herdr: self.herdr, now: Utc::now() };
            return executor::release(&ctx, &mut self.state, &key);
        }
        if req.kind == RequestKind::Adopt {
            if self.job.as_ref().is_some_and(|j| j.key == key) {
                self.job = None;
                self.job_pid = None;
                self.state.setup_process = None;
            }
            let rec = self.state.get_mut(&key).expect("checado acima");
            if rec.ownership == Ownership::Managed {
                rec.ownership = Ownership::Adopted { reason: "adotado manualmente".into(), at: Utc::now() };
            }
            if rec.phase == Phase::Preparing {
                // setup cancelado (ou que nunca vai começar): não pode ficar "preparando" para sempre
                rec.phase = Phase::Failed { step: "setup".into(), reason: "cancelado pela adoção".into() };
            }
            return Ok(format!("{key} adotado"));
        }
        let runner = self.runner;
        let repo_path = self.cfg.repo(&key.repo).map(|r| r.path.clone());
        let rec = self.state.get_mut(&key).expect("checado acima");
        match req.kind {
            RequestKind::Retry => {
                if rec.ownership != Ownership::Managed {
                    bail!("{key} está adotado; use release para liberá-lo");
                }
                match rec.phase.clone() {
                    Phase::Failed { .. } => {
                        rec.phase = Phase::Preparing;
                        rec.generation += 1;
                        Ok(format!("{key}: setup será refeito"))
                    }
                    Phase::Blocked { .. } if !rec.path.exists() => {
                        if rec.created_branch {
                            if let Some(path) = repo_path {
                                if let Some(note) = executor::delete_owned_branch(&Git::new(runner), &path, rec)? {
                                    bail!("{note}; resolva a branch antes do retry");
                                }
                            }
                            rec.created_branch = false;
                        }
                        rec.phase = Phase::Creating;
                        rec.managed_sha = None;
                        rec.prepared_sha = None;
                        rec.workspace_id = None;
                        Ok(format!("{key}: será recriado no próximo ciclo"))
                    }
                    _ => bail!("{key} não está em falha nem bloqueado sem worktree"),
                }
            }
            RequestKind::Sync | RequestKind::Release | RequestKind::Adopt => unreachable!("tratados acima"),
        }
    }
}

pub fn run(paths: Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir)?;
    crate::applog::init(paths.log_file());
    let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(paths.lock_file())?;
    lock.try_lock_exclusive().context("outro daemon já está rodando (daemon.lock)")?;
    let cfg = Config::load(&paths.config_file, &paths.home)?;
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let runner = RealRunner;
    let herdr = HerdrCli::new(&runner, cfg.herdr_session.clone());
    // o painel usa a sessão da config (nunca o ambiente) com timeout curto por chamada
    let ui_herdr = HerdrCli::new(&runner, cfg.herdr_session.clone()).with_timeout(crate::ui::UI_CALL_TIMEOUT);
    let mut d = Daemon::new(paths, cfg, &runner, &herdr, mise_wrap)?.with_ui_herdr(&ui_herdr);
    info("daemon iniciado");
    while !stopping() {
        d.tick();
        std::thread::sleep(Duration::from_secs(1));
    }
    d.job = None; // Drop cancela o setup em andamento
    d.state.setup_process = None;
    if let Err(e) = d.save() {
        info(&format!("{e:#}"));
    }
    info("daemon encerrado");
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::page_json;
    use crate::herdr::FakeHerdr;
    use crate::runner::{FakeRunner, Output};
    use crate::testutil::{config, fixture, git};
    use std::os::unix::process::CommandExt;
    use wait_timeout::ChildExt;

    struct Routing {
        gh: FakeRunner,
    }

    impl Runner for Routing {
        fn run(&self, c: &Cmd) -> Result<crate::runner::Output> {
            if c.program == "gh" { self.gh.run(c) } else { RealRunner.run(c) }
        }
    }

    fn sh(step: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), step.into()]
    }

    fn wait_until(mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(10), "condição não aconteceu");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pending(runner: &Routing, sha: &str) {
        runner.gh.on("q=repo:o/r", Output::ok(&page_json("cirdes", &[(7, "feat/x", sha)], None)));
    }

    fn none_pending(runner: &Routing) {
        runner.gh.on("q=repo:o/r", Output::ok(&page_json("cirdes", &[], None)));
    }

    const CLEAN_SETUP: &str = "mkdir -p node_modules && echo ok > node_modules/.setup-done";

    #[test]
    fn full_cycle_create_prepare_notify_remove() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        pending(&runner, &fx.remote_sha("feat/x"));
        let herdr = FakeHerdr::new();
        let mut cfg = config(&fx, CLEAN_SETUP);
        cfg.remove_grace_secs = 0;
        let mut d = Daemon::new(paths.clone(), cfg, &runner, &herdr, sh).unwrap();
        let key = PrKey::new("o/r", 7);

        d.tick();
        assert_eq!(d.state.get(&key).unwrap().phase, Phase::Preparing);
        wait_until(|| {
            d.drive_setup();
            d.state.get(&key).unwrap().phase == Phase::Ready
        });
        assert_eq!(d.state.get(&key).unwrap().ownership, Ownership::Managed);
        assert!(d.state.setup_process.is_none());
        d.deliver_notifications();
        assert!(herdr.notifications.borrow().iter().any(|n| n.contains("#7")));
        assert!(!d.state.get(&key).unwrap().notify_pending);
        let wt = d.state.get(&key).unwrap().path.clone();
        assert!(wt.join("node_modules/.setup-done").exists());
        assert!(State::read(&paths.state_file()).unwrap().get(&key).is_some());

        none_pending(&runner);
        d.poll_cycle();
        assert!(d.state.get(&key).is_none());
        assert!(!wt.exists());
        assert!(git(&fx.clone, &["branch", "--list", "feat/x"]).is_empty());
    }

    #[test]
    fn dirty_setup_adopts_and_is_never_removed() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        pending(&runner, &fx.remote_sha("feat/x"));
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "echo mudou >> README"), &runner, &herdr, sh).unwrap();
        let key = PrKey::new("o/r", 7);
        d.tick();
        wait_until(|| {
            d.drive_setup();
            d.state.get(&key).unwrap().phase == Phase::Ready
        });
        assert!(matches!(&d.state.get(&key).unwrap().ownership, Ownership::Adopted { reason, .. } if reason.contains("README")));
        none_pending(&runner);
        d.poll_cycle();
        let rec = d.state.get(&key).unwrap();
        assert_eq!(rec.remote, crate::state::Remote::NotPending);
        assert!(rec.path.join("README").exists());
    }

    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 7, h, m, 0).unwrap()
    }

    fn rt_after(day_forced_at: DateTime<Utc>, attempt_at: DateTime<Utc>, ok: bool, backoff: Option<DateTime<Utc>>) -> crate::state::ReviewsToday {
        crate::state::ReviewsToday {
            last_attempt: Some(crate::state::ReviewsTodayAttempt { at: attempt_at, ok, error: None, failed_repos: vec![] }),
            backoff_until: backoff,
            last_day_forced: Some(crate::today::local_day(day_forced_at)),
            ..Default::default()
        }
    }

    #[test]
    fn counter_cadence_after_success() {
        let mut rt = crate::state::ReviewsToday::default();
        assert!(counter_due(&rt, at(12, 0), 300, false)); // nunca tentou
        rt = rt_after(at(12, 0), at(12, 0), true, None);
        assert!(!counter_due(&rt, at(12, 4), 300, false));
        assert!(counter_due(&rt, at(12, 5), 300, false));
        assert!(counter_due(&rt, at(12, 1), 300, true)); // sync
    }

    #[test]
    fn counter_failure_retries_in_backoff_not_in_cadence() {
        // cadência de 1 h, falha às 12:00: nova tentativa às 12:05, não às 13:00
        let rt = rt_after(at(12, 0), at(12, 0), false, Some(at(12, 5)));
        assert!(!counter_due(&rt, at(12, 4), 3600, false));
        assert!(counter_due(&rt, at(12, 5), 3600, false));
        assert!(counter_due(&rt, at(12, 1), 3600, true)); // sync ignora o backoff
    }

    #[test]
    fn counter_day_change_forces_one_try_even_in_backoff() {
        // falha 23:59 SP (02:59 UTC do dia 8) com backoff até 03:04 UTC
        let fail = Utc.with_ymd_and_hms(2026, 10, 8, 2, 59, 0).unwrap();
        let backoff = Utc.with_ymd_and_hms(2026, 10, 8, 3, 4, 0).unwrap();
        let mut rt = rt_after(Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap(), fail, false, Some(backoff));
        let after_midnight = Utc.with_ymd_and_hms(2026, 10, 8, 3, 0, 30).unwrap();
        assert!(counter_due(&rt, after_midnight, 300, false));
        // depois da tentativa forçada (registrada em last_day_forced) o backoff volta a valer
        rt.last_day_forced = Some(crate::today::local_day(after_midnight));
        assert!(!counter_due(&rt, after_midnight, 300, false));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_secs(1), 300);
        assert_eq!(backoff_secs(2), 600);
        assert_eq!(backoff_secs(3), 1200);
        assert_eq!(backoff_secs(4), 1800);
        assert_eq!(backoff_secs(9), 1800);
    }

    fn reviewed(nodes: Vec<serde_json::Value>) -> String {
        serde_json::json!({"data": {"repository": {"id": "R1"}, "search": {"issueCount": nodes.len(),
            "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": nodes}}}).to_string()
    }

    fn approved_today(n: u64, when: &str) -> serde_json::Value {
        serde_json::json!({"__typename": "PullRequest", "number": n, "reviews": {"pageInfo": {"hasPreviousPage": false, "startCursor": null},
            "nodes": [{"submittedAt": when, "state": "APPROVED"}]}})
    }

    const VIEWER: &str = r#"{"data":{"viewer":{"login":"cirdes"}}}"#;

    #[test]
    fn counter_success_failure_and_midnight() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();

        // sucesso: 2 PRs hoje (o review de ontem não conta)
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![
            approved_today(1, "2026-10-07T12:00:00Z"), approved_today(2, "2026-10-07T13:00:00Z"), approved_today(3, "2026-10-06T13:00:00Z")])));
        d.collect_reviews_today_at(at(20, 0));
        let s = d.state.reviews_today.last_success.clone().unwrap();
        assert_eq!((s.count, s.viewer.as_str(), s.repos.clone()), (2, "cirdes", vec!["o/r".to_string()]));
        assert_eq!(s.day, chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());

        // falha: mantém o valor, registra erro e backoff
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::fail(1, "HTTP 502"));
        d.collect_reviews_today_at(at(20, 10));
        assert_eq!(d.state.reviews_today.last_success.as_ref().unwrap().count, 2);
        let a = d.state.reviews_today.last_attempt.clone().unwrap();
        assert!(!a.ok && a.failed_repos == vec!["o/r".to_string()]);
        let b = d.state.reviews_today.backoff_until.unwrap();
        assert!(b >= at(20, 15) && b < at(20, 16), "{b}");

        // coleta iniciada 23:59:59 SP: dia gravado é o do as_of
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![approved_today(4, "2026-10-08T02:00:00Z")])));
        let as_of = Utc.with_ymd_and_hms(2026, 10, 8, 2, 59, 59).unwrap();
        d.collect_reviews_today_at(as_of);
        let s = d.state.reviews_today.last_success.clone().unwrap();
        assert_eq!(s.day, chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
        assert_eq!(s.count, 1);
        assert!(d.state.reviews_today.backoff_until.is_none());
    }

    #[test]
    fn counter_ignores_review_after_as_of_at_midnight() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        runner.gh.on("viewer", Output::ok(VIEWER));
        // 4 antes da meia-noite SP; 5 já é do dia 8 (03:00:30Z), depois do as_of
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![
            approved_today(4, "2026-10-08T02:00:00Z"), approved_today(5, "2026-10-08T03:00:30Z")])));
        d.collect_reviews_today_at(Utc.with_ymd_and_hms(2026, 10, 8, 2, 59, 59).unwrap());
        let s = d.state.reviews_today.last_success.clone().unwrap();
        assert_eq!(s.count, 1);
        assert_eq!(s.day, chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
    }

    #[test]
    fn counter_partial_failure_keeps_previous_total_and_names_only_failed_repo() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut cfg = config(&fx, "true");
        let mut second = cfg.repos[0].clone();
        second.name = "o/r2".into();
        cfg.repos.push(second);
        let mut d = Daemon::new(paths, cfg, &runner, &herdr, sh).unwrap();
        let both = vec!["o/r".to_string(), "o/r2".to_string()];

        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("repo:o/r is:pr", Output::ok(&reviewed(vec![approved_today(1, "2026-10-07T12:00:00Z")])));
        runner.gh.on("repo:o/r2 is:pr", Output::ok(&reviewed(vec![approved_today(2, "2026-10-07T12:00:00Z")])));
        d.collect_reviews_today_at(at(15, 0));
        let s = d.state.reviews_today.last_success.clone().unwrap();
        assert_eq!((s.count, s.repos.clone()), (2, both));

        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("repo:o/r is:pr", Output::ok(&reviewed(vec![approved_today(1, "2026-10-07T12:00:00Z")])));
        runner.gh.on("repo:o/r2 is:pr", Output::fail(1, "HTTP 502"));
        d.collect_reviews_today_at(at(15, 10));
        assert_eq!(d.state.reviews_today.last_success.as_ref().unwrap().count, 2);
        let a = d.state.reviews_today.last_attempt.clone().unwrap();
        assert!(!a.ok);
        assert_eq!(a.failed_repos, vec!["o/r2".to_string()]);
    }

    #[test]
    fn counter_slow_collection_schedules_from_the_end() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        // falha que terminou 80 s depois do início: backoff conta do fim
        runner.gh.on("viewer", Output::fail(1, "HTTP 502"));
        d.collect_reviews_today_timed(at(12, 0), Duration::from_secs(80));
        let rt = &d.state.reviews_today;
        let b = rt.backoff_until.unwrap();
        let want = at(12, 0) + chrono::Duration::seconds(80 + 300);
        assert!(b >= want && b < want + chrono::Duration::seconds(2), "{b}");
        assert!(!counter_due(rt, at(12, 5), 3600, false));
        assert!(counter_due(rt, at(12, 6) + chrono::Duration::seconds(21), 3600, false));
        // sucesso lento: próxima coleta = fim + cadência
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![])));
        d.collect_reviews_today_timed(at(13, 0), Duration::from_secs(80));
        let rt = &d.state.reviews_today;
        assert!(!counter_due(rt, at(13, 0) + chrono::Duration::seconds(80 + 3599), 3600, false));
        assert!(counter_due(rt, at(13, 0) + chrono::Duration::seconds(80 + 3601), 3600, false));
    }

    #[test]
    fn counter_viewer_change_invalidates_cache_on_failure() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        d.state.reviews_today.last_success = Some(crate::state::ReviewsTodaySuccess {
            day: chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(), count: 5,
            per_repo: Default::default(), as_of: at(12, 0), viewer: "outra".into(), repos: vec!["o/r".into()] });
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::fail(1, "HTTP 502"));
        d.collect_reviews_today_at(at(13, 0));
        assert!(d.state.reviews_today.last_success.is_none());
    }

    #[test]
    fn counter_cancelled_by_sigterm_is_discarded_not_a_failure() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![approved_today(1, "2026-10-07T12:00:00Z")])));
        d.collect_reviews_today_at(at(12, 0));
        let before = d.state.reviews_today.clone();
        assert_eq!(before.last_success.as_ref().unwrap().count, 1);

        // SIGTERM chega no meio da coleta: nada de falha, backoff ou tentativa registrada
        runner.gh.on("viewer", Output::ok(VIEWER));
        d.collect_reviews_today_with(at(12, 10), Duration::ZERO, &|| true);
        assert_eq!(d.state.reviews_today, before, "tentativa cancelada é descartada");
        assert_eq!(d.state.reviews_today.failures, 0);
        assert!(d.state.reviews_today.backoff_until.is_none());
    }

    /// Runner que anota, na primeira chamada ao `gh`, quantos workspaces o painel já criou.
    struct OrderProbe<'h> {
        gh: FakeRunner,
        herdr: &'h FakeHerdr,
        panel_before_first_gh: std::cell::Cell<Option<usize>>,
    }

    impl Runner for OrderProbe<'_> {
        fn run(&self, c: &Cmd) -> Result<crate::runner::Output> {
            if c.program == "gh" {
                if self.panel_before_first_gh.get().is_none() {
                    self.panel_before_first_gh.set(Some(self.herdr.created_cwds.borrow().len()));
                }
                self.gh.run(c)
            } else {
                RealRunner.run(c)
            }
        }
    }

    #[test]
    fn first_tick_ensures_panel_before_polling() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let herdr = FakeHerdr::new();
        let runner = OrderProbe { gh: FakeRunner::new(), herdr: &herdr, panel_before_first_gh: Default::default() };
        runner.gh.on("q=repo:o/r", Output::ok(&page_json("cirdes", &[], None)));
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![])));
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        d.tick();
        assert!(d.panel_ensured);
        assert_eq!(runner.panel_before_first_gh.get(), Some(1), "o painel sobe antes da primeira chamada ao gh");
    }

    #[test]
    fn sync_request_triggers_counter_but_retry_does_not() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        let mut rec = crate::state::PrRecord::fixture("o/r", 99, "feat/x", "s");
        rec.phase = Phase::Failed { step: "setup".into(), reason: "x".into() };
        d.state.insert(rec);
        requests::write(&paths.requests_dir(), RequestKind::Retry, Some("o/r#99".into())).unwrap();
        d.process_requests_for_test();
        assert!(d.state.last_requests.iter().any(|r| r.kind == "retry" && r.ok), "{:?}", d.state.last_requests);
        assert!(!d.counter_requested);
        requests::write(&paths.requests_dir(), RequestKind::Sync, None).unwrap();
        d.process_requests_for_test();
        assert!(d.counter_requested);
    }

    #[test]
    fn several_syncs_before_a_tick_collect_once() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        none_pending(&runner);
        runner.gh.on("viewer", Output::ok(VIEWER));
        runner.gh.on("reviewed-by:cirdes", Output::ok(&reviewed(vec![])));
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        for _ in 0..3 {
            requests::write(&paths.requests_dir(), RequestKind::Sync, None).unwrap();
        }
        d.tick();
        let viewer_calls = runner.gh.lines().iter().filter(|l| l.ends_with("query={ viewer { login } }")).count();
        assert_eq!(viewer_calls, 1, "{:?}", runner.gh.lines());
        assert!(!d.counter_requested);
    }

    #[test]
    fn sync_request_triggers_immediate_poll() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        none_pending(&runner);
        none_pending(&runner);
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        let polls = || runner.gh.lines().iter().filter(|l| l.contains("review-requested")).count();
        d.tick();
        assert_eq!(polls(), 1);
        d.tick(); // dentro do intervalo: não consulta
        assert_eq!(polls(), 1);
        requests::write(&paths.requests_dir(), RequestKind::Sync, None).unwrap();
        d.tick();
        assert_eq!(polls(), 2);
    }

    #[test]
    fn bad_requests_do_not_crash_and_are_recorded() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        none_pending(&runner);
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        std::fs::create_dir_all(paths.requests_dir()).unwrap();
        std::fs::write(paths.requests_dir().join("lixo.json"), "???").unwrap();
        requests::write(&paths.requests_dir(), RequestKind::Release, Some("o/r#99".into())).unwrap();
        d.tick();
        assert!(requests::list(&paths.requests_dir()).unwrap().is_empty());
        assert!(d.state.last_requests.iter().any(|r| r.kind == "inválido" && !r.ok));
        assert!(d.state.last_requests.iter().any(|r| !r.ok && r.message.contains("o/r#99")));
    }

    #[test]
    fn retry_never_touches_adopted() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        none_pending(&runner);
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        let mut rec = crate::state::PrRecord::fixture("o/r", 5, "feat/x", "s");
        rec.ownership = Ownership::Adopted { reason: "x".into(), at: Utc::now() };
        rec.phase = Phase::Blocked { reason: "remoção cancelada".into() };
        d.state.insert(rec);
        requests::write(&paths.requests_dir(), RequestKind::Retry, Some("o/r#5".into())).unwrap();
        d.tick();
        let rec = d.state.get(&PrKey::new("o/r", 5)).unwrap();
        assert!(matches!(rec.ownership, Ownership::Adopted { .. }));
        assert!(d.state.last_requests.iter().any(|r| !r.ok && r.message.contains("release")));
    }

    #[test]
    fn auth_failure_notifies_once_and_keeps_records() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        for _ in 0..2 {
            runner.gh.on("q=repo:o/r", Output::fail(1, "please run:  gh auth login"));
        }
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        d.state.insert(crate::state::PrRecord::fixture("o/r", 1, "feat/x", "s"));
        d.poll_cycle();
        d.poll_cycle();
        assert_eq!(herdr.notifications.borrow().len(), 1);
        assert!(d.state.get(&PrKey::new("o/r", 1)).is_some());
    }

    #[test]
    fn failed_notification_is_retried() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        pending(&runner, &fx.remote_sha("feat/x"));
        let herdr = FakeHerdr::new();
        herdr.fail_notify.set(true);
        let mut d = Daemon::new(paths, config(&fx, CLEAN_SETUP), &runner, &herdr, sh).unwrap();
        d.notify_backoff = Duration::ZERO;
        let key = PrKey::new("o/r", 7);
        d.tick();
        wait_until(|| {
            d.drive_setup();
            d.state.get(&key).unwrap().phase == Phase::Ready
        });
        d.deliver_notifications();
        assert!(d.state.get(&key).unwrap().notify_pending);
        herdr.fail_notify.set(false);
        d.deliver_notifications();
        assert!(!d.state.get(&key).unwrap().notify_pending);
        assert_eq!(herdr.notifications.borrow().len(), 1);
    }

    #[test]
    fn corrupt_state_is_isolated_with_alert() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(paths.state_file(), "{{{").unwrap();
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        assert!(d.state.prs.is_empty());
        assert!(d.state.alerts.iter().any(|a| a.message.contains("corrompido")));
    }

    #[test]
    fn orphan_setup_from_previous_run_is_killed_on_start() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let mut orphan = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let pid = orphan.id();
        let previous = State {
            setup_process: Some(SetupProcess {
                key: PrKey::new("o/r", 7),
                pgid: pid as i32,
                started: process_start(&RealRunner, pid).unwrap(),
            }),
            ..State::default()
        };
        previous.save(&paths.state_file()).unwrap();
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        assert!(orphan.wait_timeout(Duration::from_secs(3)).unwrap().is_some(), "órfão continua vivo");
        assert!(d.state.setup_process.is_none());
        assert!(d.state.alerts.iter().any(|a| a.message.contains("órfão")));
    }

    #[test]
    fn save_failure_stops_cycle_before_any_effect() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        pending(&runner, &fx.remote_sha("feat/x"));
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        d.poll_cycle();
        std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(herdr.open.borrow().is_empty(), "criou worktree sem estado salvo");
        assert!(d.state.prs.is_empty());
    }

    fn alive(pgid: i32) -> bool {
        unsafe { libc::killpg(pgid, 0) == 0 }
    }

    /// Daemon com setup lento rodando; devolve o pgid do job.
    fn running_setup<'a>(fx: &crate::testutil::Fixture, paths: Paths, runner: &'a Routing, herdr: &'a FakeHerdr) -> (Daemon<'a>, i32) {
        pending(runner, &fx.remote_sha("feat/x"));
        let mut cfg = config(fx, "sleep 30");
        cfg.remove_grace_secs = 0;
        let mut d = Daemon::new(paths, cfg, runner, herdr, sh).unwrap();
        d.tick();
        wait_until(|| {
            d.drive_setup();
            d.job_pid.is_some()
        });
        let pgid = d.job_pid.unwrap() as i32;
        assert!(alive(pgid));
        (d, pgid)
    }

    #[test]
    fn remove_cancels_running_setup() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let (mut d, pgid) = running_setup(&fx, paths, &runner, &herdr);
        none_pending(&runner);
        d.poll_cycle();
        assert!(d.job.is_none() && d.state.setup_process.is_none());
        wait_until(|| !alive(pgid));
    }

    #[test]
    fn release_cancels_running_setup() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let (mut d, pgid) = running_setup(&fx, paths.clone(), &runner, &herdr);
        requests::write(&paths.requests_dir(), RequestKind::Release, Some("o/r#7".into())).unwrap();
        d.process_requests();
        assert!(d.job.is_none() && d.state.setup_process.is_none());
        wait_until(|| !alive(pgid));
    }

    #[test]
    fn adopt_cancels_running_setup_and_blocks_restart() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let (mut d, pgid) = running_setup(&fx, paths.clone(), &runner, &herdr);
        requests::write(&paths.requests_dir(), RequestKind::Adopt, Some("o/r#7".into())).unwrap();
        d.process_requests();
        assert!(d.job.is_none());
        wait_until(|| !alive(pgid));
        d.drive_setup();
        assert!(d.job.is_none(), "setup não pode reiniciar em PR adotado");
        assert_eq!(
            d.state.get(&PrKey::new("o/r", 7)).unwrap().phase,
            Phase::Failed { step: "setup".into(), reason: "cancelado pela adoção".into() }
        );
    }

    #[test]
    fn request_result_survives_save_failure() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths.clone(), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        requests::write(&paths.requests_dir(), RequestKind::Release, Some("o/r#99".into())).unwrap();
        std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        d.process_requests();
        assert_eq!(requests::list(&paths.requests_dir()).unwrap().len(), 1);
        d.process_requests();
        assert_eq!(requests::list(&paths.requests_dir()).unwrap().len(), 1);
        std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        d.process_requests();
        assert!(requests::list(&paths.requests_dir()).unwrap().is_empty());
        assert!(State::read(&paths.state_file()).unwrap().last_requests.iter().any(|r| r.message.contains("o/r#99")));
    }

    #[test]
    fn unknown_repo_does_not_starve_setup_queue() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, "sleep 30"), &runner, &herdr, sh).unwrap();
        let mut old = crate::state::PrRecord::fixture("x/fora", 1, "b", "s");
        old.phase = Phase::Preparing;
        old.first_seen_at = Utc::now() - chrono::Duration::hours(1);
        d.state.insert(old);
        let mut new = crate::state::PrRecord::fixture("o/r", 2, "feat/x", "s");
        new.phase = Phase::Preparing;
        new.path = fx.root.join("wt2");
        std::fs::create_dir_all(&new.path).unwrap();
        d.state.insert(new);
        d.drive_setup();
        assert!(d.job.as_ref().is_some_and(|j| j.key == PrKey::new("o/r", 2)));
        assert!(matches!(&d.state.get(&PrKey::new("x/fora", 1)).unwrap().phase, Phase::Failed { reason, .. } if reason.contains("fora da config")));
    }

    #[test]
    fn mismatched_orphan_is_not_killed_and_alerts() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let mut other = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let previous = State {
            setup_process: Some(SetupProcess {
                key: PrKey::new("o/r", 7),
                pgid: other.id() as i32,
                started: "Thu Jan  1 00:00:00 1970".into(),
            }),
            ..State::default()
        };
        previous.save(&paths.state_file()).unwrap();
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        let d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        assert!(other.try_wait().unwrap().is_none(), "processo alheio foi morto");
        assert!(d.state.alerts.iter().any(|a| a.message.contains("não confirmei")));
        let _ = other.kill();
        let _ = other.wait();
    }

    #[test]
    fn panel_is_ensured_once_per_start_with_backoff() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        herdr.fail_herdr.set(true);
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        let panel_alerts = |d: &Daemon| d.state.alerts.iter().filter(|a| a.message.contains("painel")).count();
        d.ensure_panel_now();
        assert!(!d.panel_ensured);
        assert_eq!(panel_alerts(&d), 0, "uma falha isolada não vira alerta");
        d.ensure_panel_now(); // 2ª falha seguida: alerta
        assert_eq!(panel_alerts(&d), 1);
        d.ensure_panel_now(); // 3ª: sem segundo alerta
        assert_eq!(panel_alerts(&d), 1);
        assert_eq!(d.panel_failures, 3);
        herdr.fail_herdr.set(false);
        d.ensure_panel_now();
        assert!(d.panel_ensured);
        assert_eq!(d.panel_failures, 0, "sucesso zera a sequência de falhas");
        let n = herdr.workspaces.borrow().len();
        // fechado pelo usuário depois do sucesso: o daemon não recria
        herdr.workspaces.borrow_mut().clear();
        d.maybe_ensure_panel();
        assert_eq!(herdr.workspaces.borrow().len(), 0, "não recria depois do primeiro sucesso (era {n})");
    }

    #[test]
    fn panel_success_message_records_recovery() {
        assert_eq!(panel_ok_message(0), "painel reviewq garantido");
        assert_eq!(panel_ok_message(2), "painel reviewq garantido (recuperado depois de 2 falhas seguidas)");
    }

    #[test]
    fn maybe_ensure_panel_honors_next_try_and_doubles_backoff() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        let herdr = FakeHerdr::new();
        herdr.fail_list_workspaces.set(true);
        let mut d = Daemon::new(paths, config(&fx, "true"), &runner, &herdr, sh).unwrap();
        assert_eq!(d.panel_backoff, Duration::from_secs(30));
        d.maybe_ensure_panel(); // primeira tentativa: falha e agenda a próxima
        let next = d.panel_next_try.expect("falha agenda nova tentativa");
        assert!(next > Instant::now() + Duration::from_secs(25), "primeira espera é ~30 s");
        assert_eq!(d.panel_backoff, Duration::from_secs(60));

        // antes do prazo: não toca no herdr
        herdr.fail_list_workspaces.set(false);
        d.maybe_ensure_panel();
        assert!(herdr.created_cwds.borrow().is_empty(), "antes de panel_next_try não chama o herdr");
        assert!(!d.panel_ensured);

        // prazo vencido (tempo injetado no campo): tenta e consegue
        d.panel_next_try = Some(Instant::now() - Duration::from_secs(1));
        d.maybe_ensure_panel();
        assert!(d.panel_ensured);
        assert_eq!(herdr.created_cwds.borrow().len(), 1);

        // o backoff dobra até 300 s
        let mut d2 = Daemon::new(Paths::from_home(fx.root.join("home2")), config(&fx, "true"), &runner, &herdr, sh).unwrap();
        herdr.fail_herdr.set(true);
        for _ in 0..6 {
            d2.ensure_panel_now();
        }
        assert_eq!(d2.panel_backoff, Duration::from_secs(300));
    }

    #[test]
    fn approved_pr_keeps_worktree_during_grace() {
        let fx = fixture("feat/x");
        let paths = Paths::from_home(fx.root.join("home"));
        let runner = Routing { gh: FakeRunner::new() };
        pending(&runner, &fx.remote_sha("feat/x"));
        let herdr = FakeHerdr::new();
        let mut d = Daemon::new(paths, config(&fx, CLEAN_SETUP), &runner, &herdr, sh).unwrap();
        let key = PrKey::new("o/r", 7);
        d.tick();
        wait_until(|| {
            d.drive_setup();
            d.state.get(&key).unwrap().phase == Phase::Ready
        });
        none_pending(&runner);
        d.poll_cycle();
        let rec = d.state.get(&key).unwrap();
        assert_eq!(rec.phase, Phase::Ready);
        assert!(rec.not_pending_since.is_some());
        assert!(rec.path.exists());
    }
}
