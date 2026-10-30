use crate::runner::{Cmd, Output, Runner};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct CreateReq {
    pub repo: PathBuf,
    pub branch: String,
    pub base: String,
    pub path: PathBuf,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub workspace_id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceInfo {
    pub id: String,
    pub label: String,
    /// Workspace focado no cliente (campo `focused` de `workspace list`).
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub id: String,
    pub is_reviewq_tui: bool,
}

/// Agente detectado pelo herdr (`herdr agent list`).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub agent: String,
    pub cwd: Option<PathBuf>,
    pub foreground_cwd: Option<PathBuf>,
}

pub const PLUGIN_ID: &str = "cirdes.reviewq";
/// Nome do binário cujo `tui` identifica o pane do painel.
const TUI_BIN: &str = "herdr-reviewq";

/// Como os comandos herdr da UI chegam ao servidor certo.
#[derive(Debug, Clone, PartialEq)]
pub struct UiRouting {
    pub bin: String,
    pub session: Option<String>,
}

impl UiRouting {
    /// Dentro de um pane/action do herdr (`HERDR_SOCKET_PATH` presente) o filho herda o socket,
    /// então não passa `--session`; `HERDR_BIN_PATH` vira o programa.
    pub fn resolve(get_env: impl Fn(&str) -> Option<String>, cfg_session: Option<String>) -> Self {
        let bin = get_env("HERDR_BIN_PATH").filter(|b| !b.is_empty()).unwrap_or_else(|| "herdr".into());
        let session = if get_env("HERDR_SOCKET_PATH").is_some() { None } else { cfg_session };
        Self { bin, session }
    }

    pub fn from_process_env(cfg_session: Option<String>) -> Self {
        Self::resolve(|k| std::env::var(k).ok(), cfg_session)
    }
}

pub trait Herdr {
    fn create_worktree(&self, req: &CreateReq) -> Result<Created>;
    /// Acha o workspace aberto para `path`; abre-o se o worktree existe mas está fechado.
    fn find_workspace(&self, repo: &Path, path: &Path) -> Result<Option<String>>;
    /// Só consulta: devolve o workspace aberto para `path`, sem nunca abrir um fechado.
    fn find_open_workspace(&self, repo: &Path, path: &Path) -> Result<Option<String>>;
    fn remove_worktree(&self, workspace_id: &str) -> Result<()>;
    fn notify(&self, title: &str, body: &str, sound: &str) -> Result<()>;
    fn report_metadata(&self, workspace_id: &str, value: &str) -> Result<()>;
    // Chamadas da UI: todas recebem o prazo total de quem chama. O prazo é checado antes de cada
    // chamada ao herdr, e cada uma usa no máximo `min(timeout do cliente, o que resta)`.
    fn list_workspaces(&self, deadline: Instant) -> Result<Vec<WorkspaceInfo>>;
    /// Cria um workspace sem foco; devolve (workspace id, root pane).
    fn create_workspace(&self, label: &str, cwd: &Path, deadline: Instant) -> Result<(String, Option<String>)>;
    fn focus_workspace(&self, id: &str, deadline: Instant) -> Result<()>;
    /// Panes do workspace com a identificação do TUI. Para de varrer (erro de prazo) quando `deadline` passa.
    fn list_panes(&self, workspace: &str, deadline: Instant) -> Result<Vec<PaneInfo>>;
    /// Abre o TUI do plugin ancorado em `target_pane` (que pertence a `workspace`).
    /// Atenção: o herdr muda o foco para o workspace do pane, mesmo com `--no-focus`.
    fn open_plugin_pane(&self, workspace: &str, target_pane: &str, deadline: Instant) -> Result<()>;
    /// Agentes que o herdr reconhece na sessão (Claude, Codex…), com o diretório de cada um.
    fn list_agents(&self, deadline: Instant) -> Result<Vec<AgentInfo>>;
}

pub struct HerdrCli<'a> {
    runner: &'a dyn Runner,
    session: Option<String>,
    bin: String,
    timeout: Duration,
}

#[derive(Deserialize)]
struct WorkspaceList {
    workspaces: Vec<WorkspaceEntry>,
}

#[derive(Deserialize)]
struct WorkspaceEntry {
    workspace_id: String,
    #[serde(default)]
    label: Option<String>,
    focused: bool,
}

#[derive(Deserialize)]
struct PaneList {
    panes: Vec<PaneEntry>,
}

#[derive(Deserialize)]
struct PaneEntry {
    pane_id: String,
    workspace_id: String,
}

#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Deserialize)]
struct ProcessInfo {
    foreground_processes: Vec<ForegroundProcess>,
}

#[derive(Deserialize)]
struct ForegroundProcess {
    argv: Vec<String>,
}

/// O processo é o próprio binário `herdr-reviewq` (basename de argv[0]) rodando `tui` (argv[1]).
/// Um shell que só menciona o comando (`sh -c "herdr-reviewq tui"`) ou outro programa que recebe
/// esses nomes como argumentos não conta.
fn is_tui_argv(argv: &[String]) -> bool {
    match argv {
        [program, sub, ..] => Path::new(program).file_name().and_then(|n| n.to_str()) == Some(TUI_BIN) && sub == "tui",
        _ => false,
    }
}

fn parse_as<T: serde::de::DeserializeOwned>(v: Value, what: &str) -> Result<T> {
    serde_json::from_value(v).with_context(|| format!("resposta inesperada do herdr em {what}"))
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub fn parse_response(out: &Output) -> Result<Value> {
    let text = if out.stdout.trim().is_empty() { out.stderr.trim() } else { out.stdout.trim() };
    let v: Value = serde_json::from_str(text).with_context(|| format!("resposta inválida do herdr: {text}"))?;
    if let Some(err) = v.get("error") {
        bail!(
            "herdr: {} ({})",
            err.get("message").and_then(Value::as_str).unwrap_or("erro"),
            err.get("code").and_then(Value::as_str).unwrap_or("?")
        );
    }
    v.get("result").cloned().ok_or_else(|| anyhow!("resposta do herdr sem result"))
}

impl<'a> HerdrCli<'a> {
    pub fn new(runner: &'a dyn Runner, session: Option<String>) -> Self {
        Self { runner, session, bin: "herdr".into(), timeout: Duration::from_secs(60) }
    }

    /// Cliente da UI: programa e sessão vêm do `UiRouting`.
    pub fn routed(runner: &'a dyn Runner, routing: &UiRouting) -> Self {
        Self { runner, session: routing.session.clone(), bin: routing.bin.clone(), timeout: Duration::from_secs(60) }
    }

    /// Timeout por chamada (padrão 60 s; comandos longos do núcleo têm o seu próprio).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn cmd(&self) -> Cmd {
        let c = Cmd::new(&self.bin).timeout(self.timeout);
        match &self.session {
            Some(s) => c.arg("--session").arg(s.clone()),
            None => c,
        }
    }

    fn call(&self, cmd: Cmd) -> Result<Value> {
        parse_response(&self.runner.run(&cmd)?)
    }

    /// Entrada de `worktree list` para `path`, se o herdr conhece o worktree.
    fn list_entry(&self, repo: &Path, path: &Path) -> Result<Option<Value>> {
        let res = self.call(self.cmd().args(["worktree", "list", "--cwd"]).arg(repo.to_string_lossy()))?;
        let want = canon(path);
        Ok(res
            .get("worktrees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|w| w.get("path").and_then(Value::as_str).map(|p| canon(Path::new(p)) == want).unwrap_or(false))
            .cloned())
    }

    /// Timeout de uma chamada limitada por `deadline`; erro se o prazo já passou.
    fn until(&self, deadline: Instant) -> Result<Duration> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("prazo esgotado esperando o herdr");
        }
        Ok(left.min(self.timeout))
    }

    fn exec(&self, cmd: Cmd) -> Result<()> {
        let out = self.runner.run(&cmd)?;
        if !out.success() {
            bail!("{} falhou: {} {}", cmd.line(), out.stderr.trim(), out.stdout.trim());
        }
        Ok(())
    }
}

impl Herdr for HerdrCli<'_> {
    fn create_worktree(&self, r: &CreateReq) -> Result<Created> {
        let res = self.call(
            self.cmd()
                .args(["worktree", "create", "--cwd"])
                .arg(r.repo.to_string_lossy())
                .arg("--branch")
                .arg(r.branch.clone())
                .arg("--base")
                .arg(r.base.clone())
                .arg("--path")
                .arg(r.path.to_string_lossy())
                .arg("--label")
                .arg(r.label.clone())
                .args(["--no-focus", "--trust-repository"])
                .timeout(Duration::from_secs(300)),
        )?;
        let ws = res.pointer("/workspace/workspace_id").and_then(Value::as_str).context("worktree_created sem workspace_id")?;
        let path = res.pointer("/worktree/path").and_then(Value::as_str).context("worktree_created sem path")?;
        Ok(Created { workspace_id: ws.into(), path: path.into() })
    }

    fn find_workspace(&self, repo: &Path, path: &Path) -> Result<Option<String>> {
        let Some(entry) = self.list_entry(repo, path)? else { return Ok(None) };
        if let Some(id) = entry.get("open_workspace_id").and_then(Value::as_str) {
            return Ok(Some(id.into()));
        }
        let opened = self.call(
            self.cmd()
                .args(["worktree", "open", "--cwd"])
                .arg(repo.to_string_lossy())
                .arg("--path")
                .arg(path.to_string_lossy())
                .arg("--no-focus"),
        )?;
        Ok(opened.pointer("/workspace/workspace_id").and_then(Value::as_str).map(String::from))
    }

    fn find_open_workspace(&self, repo: &Path, path: &Path) -> Result<Option<String>> {
        Ok(self
            .list_entry(repo, path)?
            .and_then(|e| e.get("open_workspace_id").and_then(Value::as_str).map(String::from)))
    }

    fn remove_worktree(&self, workspace_id: &str) -> Result<()> {
        self.call(self.cmd().args(["worktree", "remove", "--workspace", workspace_id]).timeout(Duration::from_secs(300)))
            .map(|_| ())
    }

    fn notify(&self, title: &str, body: &str, sound: &str) -> Result<()> {
        self.exec(self.cmd().args(["notification", "show", title, "--body", body, "--sound", sound]))
    }

    fn report_metadata(&self, workspace_id: &str, value: &str) -> Result<()> {
        // o spike mostrou que o id do workspace tem de vir antes das opções
        self.exec(
            self.cmd()
                .args(["workspace", "report-metadata", workspace_id, "--source", "reviewq", "--token"])
                .arg(format!("reviewq={value}")),
        )
    }

    fn list_workspaces(&self, deadline: Instant) -> Result<Vec<WorkspaceInfo>> {
        let cmd = self.cmd().args(["workspace", "list"]).timeout(self.until(deadline)?);
        let list: WorkspaceList = parse_as(self.call(cmd)?, "workspace list")?;
        Ok(list
            .workspaces
            .into_iter()
            .map(|w| WorkspaceInfo { id: w.workspace_id, label: w.label.unwrap_or_default(), focused: w.focused })
            .collect())
    }

    fn create_workspace(&self, label: &str, cwd: &Path, deadline: Instant) -> Result<(String, Option<String>)> {
        let res = self.call(
            self.cmd()
                .args(["workspace", "create", "--label", label, "--no-focus", "--cwd"])
                .arg(cwd.to_string_lossy())
                .timeout(self.until(deadline)?),
        )?;
        let ws = res.pointer("/workspace/workspace_id").and_then(Value::as_str).context("workspace create sem workspace_id")?;
        let root = res.pointer("/root_pane/pane_id").and_then(Value::as_str).map(String::from);
        Ok((ws.into(), root))
    }

    fn focus_workspace(&self, id: &str, deadline: Instant) -> Result<()> {
        self.call(self.cmd().args(["workspace", "focus", id]).timeout(self.until(deadline)?)).map(|_| ())
    }

    fn list_panes(&self, workspace: &str, deadline: Instant) -> Result<Vec<PaneInfo>> {
        let list: PaneList = parse_as(
            self.call(self.cmd().args(["pane", "list", "--workspace", workspace]).timeout(self.until(deadline)?))?,
            "pane list",
        )?;
        let mut out = vec![];
        for p in list.panes.into_iter().filter(|p| p.workspace_id == workspace) {
            let cmd = self.cmd().args(["pane", "process-info", "--pane", &p.pane_id]).timeout(self.until(deadline)?);
            let info: ProcessInfoResult = parse_as(self.call(cmd)?, "pane process-info")?;
            let is_tui = info.process_info.foreground_processes.iter().any(|fp| is_tui_argv(&fp.argv));
            out.push(PaneInfo { id: p.pane_id, is_reviewq_tui: is_tui });
        }
        Ok(out)
    }

    fn open_plugin_pane(&self, _workspace: &str, target_pane: &str, deadline: Instant) -> Result<()> {
        // spike: `--target-pane` é obrigatório e não pode vir com `--workspace`
        self.call(
            self.cmd()
                .args([
                    "plugin", "pane", "open", "--plugin", PLUGIN_ID, "--entrypoint", "tui", "--target-pane", target_pane,
                    "--placement", "zoomed",
                ])
                .timeout(self.until(deadline)?),
        )
        .map(|_| ())
    }

    fn list_agents(&self, deadline: Instant) -> Result<Vec<AgentInfo>> {
        let res = self.call(self.cmd().args(["agent", "list"]).timeout(self.until(deadline)?))?;
        Ok(crate::agents::parse_agents(&res))
    }
}

/// Cliente herdr do backend da config: o do terminal, roteado como antes, ou o app desktop.
pub fn client<'a>(
    cfg: &crate::config::Config,
    runner: &'a dyn Runner,
    routing: &UiRouting,
    home: &Path,
    timeout: Duration,
) -> Box<dyn Herdr + 'a> {
    match cfg.backend {
        crate::config::Backend::Herdr => Box::new(HerdrCli::routed(runner, routing).with_timeout(timeout)),
        crate::config::Backend::HerdrApp => Box::new(HerdrApp::new(runner, home.join(".local/bin").join(TUI_BIN)).with_timeout(timeout)),
    }
}

/// Label que marca os terminais de um worktree do reviewq no herdr-core.
const WORKTREE_LABEL: &str = "reviewq.worktree";
/// Quanto esperar a janela do herdr-app adotar um space recém-criado.
const ADOPT_WAIT: Duration = Duration::from_secs(3);

/// Id do space de um worktree: estável para o mesmo caminho, então o daemon e a CLI acham o
/// mesmo space sem guardar nada além do caminho.
pub fn space_id(path: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in canon(path).to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("rq-{h:016x}")
}

fn app_json(out: &Output, what: &str) -> Result<Value> {
    let text = if out.stdout.trim().is_empty() { out.stderr.trim() } else { out.stdout.trim() };
    let v: Value = serde_json::from_str(text).with_context(|| format!("resposta inválida de {what}: {text}"))?;
    if let Some(err) = v.get("error") {
        bail!("{what}: {}", err.as_str().map(String::from).unwrap_or_else(|| err.to_string()));
    }
    if !out.success() {
        bail!("{what} falhou: {text}");
    }
    Ok(v)
}

/// O app desktop do Herdr. Os spaces nascem pelo herdr-core (`hc run` com o label
/// `herdr.space`), então o daemon cria worktrees com a janela fechada; o que só a janela faz
/// (listar, nomear e mostrar spaces) passa pelo `herdr-app do`. Workspace aqui é space, e
/// pane é terminal do herdr-core.
pub struct HerdrApp<'a> {
    runner: &'a dyn Runner,
    tui_bin: PathBuf,
    timeout: Duration,
}

impl<'a> HerdrApp<'a> {
    pub fn new(runner: &'a dyn Runner, tui_bin: PathBuf) -> Self {
        Self { runner, tui_bin, timeout: Duration::from_secs(60) }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn until(&self, deadline: Instant) -> Result<Duration> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("prazo esgotado esperando o herdr-app");
        }
        Ok(left.min(self.timeout))
    }

    fn hc(&self, timeout: Duration) -> Cmd {
        Cmd::new("hc").timeout(timeout)
    }

    fn app(&self, command: &str, args: Value, timeout: Duration) -> Result<Value> {
        let cmd = Cmd::new("herdr-app").args(["do", command]).arg(args.to_string()).timeout(timeout);
        app_json(&self.runner.run(&cmd)?, &format!("herdr-app {command}"))
    }

    /// Terminais vivos do space.
    fn space_terminals(&self, id: &str, timeout: Duration) -> Result<Vec<String>> {
        let cmd = self.hc(timeout).args(["ls", "--json", "--label"]).arg(format!("herdr.space={id}"));
        let ls = app_json(&self.runner.run(&cmd)?, "hc ls")?;
        Ok(ls
            .get("terminals")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|t| t.get("state").and_then(Value::as_str) == Some("live"))
            .filter_map(|t| t.get("address").and_then(Value::as_str).map(String::from))
            .collect())
    }

    fn terminal(&self, address: &str, timeout: Duration) -> Result<Value> {
        app_json(&self.runner.run(&self.hc(timeout).args(["get", address, "--json"]))?, "hc get")
    }

    /// Roda `argv` (ou o shell, se vazio) num terminal novo do space `id`, em `cwd`.
    fn run_in_space(&self, id: &str, cwd: &Path, labels: &[String], argv: &[String], timeout: Duration) -> Result<String> {
        let mut cmd = self
            .hc(timeout)
            .args(["run", "--bg", "--keep", "--no-parent", "--json", "--cwd"])
            .arg(cwd.to_string_lossy())
            .arg("--label")
            .arg(format!("herdr.space={id}"));
        for l in labels {
            cmd = cmd.arg("--label").arg(l.clone());
        }
        if !argv.is_empty() {
            cmd = cmd.arg("--").args(argv.iter().cloned());
        }
        let v = app_json(&self.runner.run(&cmd)?, "hc run")?;
        Ok(v.get("address").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    /// A janela adota o terminal um instante depois do `hc run`; nomear antes disso falha com
    /// "no such space". Com a janela fechada desiste na hora: o space aparece quando ela abrir,
    /// com o nome da pasta.
    fn name_when_adopted(&self, id: &str, label: &str) {
        let until = Instant::now() + ADOPT_WAIT;
        loop {
            match self.list_workspaces(Instant::now() + self.timeout) {
                Ok(list) if list.iter().any(|w| w.id == id) => {
                    let _ = self.app("space.rename", serde_json::json!({ "id": id, "name": label }), self.timeout);
                    return;
                }
                Ok(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(100)),
                _ => return,
            }
        }
    }

    fn open_space(&self, path: &Path, label: &str) -> Result<String> {
        let id = space_id(path);
        let tag = format!("{WORKTREE_LABEL}={}", canon(path).display());
        self.run_in_space(&id, path, &[tag], &[], self.timeout)?;
        self.name_when_adopted(&id, label);
        Ok(id)
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self.runner.run(&Cmd::new("git").cwd(dir.to_path_buf()).args(args.iter().copied()).timeout(Duration::from_secs(300)))?;
        if !out.success() {
            bail!("git {} falhou: {}", args.join(" "), out.stderr.trim());
        }
        Ok(out.stdout)
    }
}

impl Herdr for HerdrApp<'_> {
    fn create_worktree(&self, r: &CreateReq) -> Result<Created> {
        self.git(&r.repo, &["worktree", "add", "-q", "-b", &r.branch, &r.path.to_string_lossy(), &r.base])?;
        let id = self.open_space(&r.path, &r.label)?;
        Ok(Created { workspace_id: id, path: r.path.clone() })
    }

    fn find_workspace(&self, repo: &Path, path: &Path) -> Result<Option<String>> {
        if let Some(id) = self.find_open_workspace(repo, path)? {
            return Ok(Some(id));
        }
        if !path.exists() {
            return Ok(None);
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.open_space(path, &name).map(Some)
    }

    fn find_open_workspace(&self, _repo: &Path, path: &Path) -> Result<Option<String>> {
        let id = space_id(path);
        Ok((!self.space_terminals(&id, self.timeout)?.is_empty()).then_some(id))
    }

    /// O caminho vem do label que o reviewq pôs no terminal ao abrir o space; o repo, do
    /// `--git-common-dir` do próprio worktree. Fecha o space e só então remove, sem `--force`.
    fn remove_worktree(&self, id: &str) -> Result<()> {
        let terminals = self.space_terminals(id, self.timeout)?;
        let path = terminals
            .iter()
            .find_map(|t| self.terminal(t, self.timeout).ok()?.pointer(&format!("/labels/{WORKTREE_LABEL}"))?.as_str().map(PathBuf::from))
            .with_context(|| format!("space {id} sem o label {WORKTREE_LABEL}"))?;
        let common = self.git(&path, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let repo = Path::new(common.trim()).parent().context("--git-common-dir sem pai")?.to_path_buf();
        let _ = self.app("space.close", serde_json::json!({ "id": id, "confirmed": true }), self.timeout);
        for t in self.space_terminals(id, self.timeout)? {
            let out = self.runner.run(&self.hc(self.timeout).args(["kill", &t]))?;
            if !out.success() {
                bail!("hc kill {t} falhou: {}", out.stderr.trim());
            }
        }
        self.git(&repo, &["worktree", "remove", &path.to_string_lossy()]).map(|_| ())
    }

    /// O herdr-app não tem notificação por CLI: vai a do macOS. Título e corpo seguem como
    /// argumentos do script, nunca dentro dele.
    fn notify(&self, title: &str, body: &str, sound: &str) -> Result<()> {
        let script = match sound {
            "none" => "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run",
            _ => "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv) sound name \"Glass\"\nend run",
        };
        let out = self.runner.run(&Cmd::new("osascript").arg("-e").arg(script).arg(title).arg(body).timeout(self.timeout))?;
        if !out.success() {
            bail!("osascript falhou: {}", out.stderr.trim());
        }
        Ok(())
    }

    fn report_metadata(&self, _workspace_id: &str, _value: &str) -> Result<()> {
        Ok(())
    }

    fn list_workspaces(&self, deadline: Instant) -> Result<Vec<WorkspaceInfo>> {
        let out = self.runner.run(&Cmd::new("herdr-app").arg("ls").timeout(self.until(deadline)?))?;
        let ls = app_json(&out, "herdr-app ls")?;
        Ok(ls
            .get("spaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some(WorkspaceInfo {
                    id: s.get("id")?.as_str()?.into(),
                    label: s.get("name").and_then(Value::as_str).unwrap_or_default().into(),
                    focused: s.get("active").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect())
    }

    /// O space do painel já nasce com o TUI como primeiro terminal, para não sobrar um shell
    /// vazio ao lado dele: o `ensure` acha o TUI rodando e não abre outro.
    fn create_workspace(&self, label: &str, cwd: &Path, deadline: Instant) -> Result<(String, Option<String>)> {
        let id = format!("rq-panel-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let argv = [self.tui_bin.to_string_lossy().into_owned(), "tui".into()];
        let address = self.run_in_space(&id, cwd, &[], &argv, self.until(deadline)?)?;
        self.name_when_adopted(&id, label);
        Ok((id, Some(address)))
    }

    fn focus_workspace(&self, id: &str, deadline: Instant) -> Result<()> {
        self.app("space.select", serde_json::json!({ "id": id }), self.until(deadline)?).map(|_| ())
    }

    fn list_panes(&self, workspace: &str, deadline: Instant) -> Result<Vec<PaneInfo>> {
        let mut out = vec![];
        for t in self.space_terminals(workspace, self.until(deadline)?)? {
            let info = self.terminal(&t, self.until(deadline)?)?;
            let argv: Vec<String> = info
                .pointer("/foreground/argv")
                .or_else(|| info.get("argv"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            out.push(PaneInfo { id: t, is_reviewq_tui: is_tui_argv(&argv) });
        }
        Ok(out)
    }

    fn open_plugin_pane(&self, workspace: &str, _target_pane: &str, deadline: Instant) -> Result<()> {
        let argv = [self.tui_bin.to_string_lossy().into_owned(), "tui".into()];
        let home = self.tui_bin.ancestors().nth(3).unwrap_or(Path::new("/")).to_path_buf();
        self.run_in_space(workspace, &home, &[], &argv, self.until(deadline)?).map(|_| ())
    }

    fn list_agents(&self, deadline: Instant) -> Result<Vec<AgentInfo>> {
        let ls = app_json(&self.runner.run(&self.hc(self.until(deadline)?).args(["ls", "--json"]))?, "hc ls")?;
        let mut out = vec![];
        for t in ls.get("terminals").and_then(Value::as_array).into_iter().flatten() {
            if t.get("state").and_then(Value::as_str) != Some("live") {
                continue;
            }
            let agent = match t.get("agent").and_then(Value::as_str) {
                Some("claude-code") => "claude",
                Some(a @ "codex") => a,
                _ => continue,
            };
            let Some(address) = t.get("address").and_then(Value::as_str) else { continue };
            let cwd = t.get("cwd").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute());
            let foreground_cwd = self
                .terminal(address, self.until(deadline)?)
                .ok()
                .and_then(|v| v.pointer("/foreground/cwd").and_then(Value::as_str).map(PathBuf::from))
                .filter(|p| p.is_absolute());
            if cwd.is_some() || foreground_cwd.is_some() {
                out.push(AgentInfo { agent: agent.into(), cwd, foreground_cwd });
            }
        }
        Ok(out)
    }
}

/// Dublê para testes: cria/remove worktrees com git de verdade, sem `--force`.
#[cfg(test)]
pub struct FakeHerdr {
    pub open: std::cell::RefCell<std::collections::BTreeMap<String, (PathBuf, PathBuf)>>,
    pub notifications: std::cell::RefCell<Vec<String>>,
    pub metadata: std::cell::RefCell<Vec<(String, String)>>,
    /// Quando true, `notify` falha (simula herdr fora do ar).
    pub fail_notify: std::cell::Cell<bool>,
    pub workspaces: std::cell::RefCell<Vec<WorkspaceInfo>>,
    pub panes: std::cell::RefCell<std::collections::BTreeMap<String, Vec<PaneInfo>>>,
    /// Workspace focado; `list_workspaces` deriva `focused` daqui.
    pub focused: std::cell::RefCell<Option<String>>,
    /// Ids passados a `focus_workspace`, em ordem.
    pub focus_calls: std::cell::RefCell<Vec<String>>,
    /// `cwd` de cada `create_workspace`.
    pub created_cwds: std::cell::RefCell<Vec<PathBuf>>,
    /// (workspace, target_pane) de cada `open_plugin_pane`.
    pub opened_on: std::cell::RefCell<Vec<(String, String)>>,
    /// Todas as chamadas de workspace/pane falham.
    pub fail_herdr: std::cell::Cell<bool>,
    pub fail_list_workspaces: std::cell::Cell<bool>,
    pub fail_list_panes: std::cell::Cell<bool>,
    pub fail_open_pane: std::cell::Cell<bool>,
    /// `open_plugin_pane` cria um pane cujo TUI não está vivo.
    pub tui_starts_dead: std::cell::Cell<bool>,
    /// Como o herdr real (spike): abrir o pane do plugin rouba o foco. Padrão true.
    pub open_steals_focus: std::cell::Cell<bool>,
    /// Demora de cada `process-info` simulado em `list_panes`.
    pub pane_scan_delay: std::cell::Cell<Duration>,
    /// Resposta de `list_agents`.
    pub agents: std::cell::RefCell<Vec<AgentInfo>>,
    pub fail_agents: std::cell::Cell<bool>,
    next: std::cell::Cell<u32>,
}

#[cfg(test)]
impl Default for FakeHerdr {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl FakeHerdr {
    pub fn new() -> Self {
        Self {
            open: Default::default(),
            notifications: Default::default(),
            metadata: Default::default(),
            fail_notify: std::cell::Cell::new(false),
            workspaces: Default::default(),
            panes: Default::default(),
            focused: Default::default(),
            focus_calls: Default::default(),
            created_cwds: Default::default(),
            opened_on: Default::default(),
            fail_herdr: Default::default(),
            fail_list_workspaces: Default::default(),
            fail_list_panes: Default::default(),
            fail_open_pane: Default::default(),
            tui_starts_dead: Default::default(),
            open_steals_focus: std::cell::Cell::new(true),
            pane_scan_delay: Default::default(),
            agents: Default::default(),
            fail_agents: Default::default(),
            next: std::cell::Cell::new(1),
        }
    }

    /// Como o `HerdrCli`: prazo vencido, nenhuma chamada.
    fn before(deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            bail!("prazo esgotado esperando o herdr");
        }
        Ok(())
    }

    fn check(&self, what: &str, fail: bool) -> Result<()> {
        if self.fail_herdr.get() || fail {
            bail!("herdr {what} falhou (teste)");
        }
        Ok(())
    }

    fn git(dir: &Path, args: &[&str]) -> Result<()> {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output()?;
        if !out.status.success() {
            bail!("git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    }
}

#[cfg(test)]
impl Herdr for FakeHerdr {
    fn create_worktree(&self, r: &CreateReq) -> Result<Created> {
        Self::git(&r.repo, &["worktree", "add", "-q", "-b", &r.branch, &r.path.to_string_lossy(), &r.base])?;
        let id = format!("w{}", self.next.get());
        self.next.set(self.next.get() + 1);
        self.open.borrow_mut().insert(id.clone(), (r.repo.clone(), r.path.clone()));
        Ok(Created { workspace_id: id, path: r.path.clone() })
    }

    fn find_workspace(&self, _repo: &Path, path: &Path) -> Result<Option<String>> {
        let want = canon(path);
        Ok(self.open.borrow().iter().find(|(_, (_, p))| canon(p) == want).map(|(id, _)| id.clone()))
    }

    fn find_open_workspace(&self, repo: &Path, path: &Path) -> Result<Option<String>> {
        self.find_workspace(repo, path)
    }

    fn remove_worktree(&self, id: &str) -> Result<()> {
        let (repo, path) = self.open.borrow().get(id).cloned().context("workspace desconhecido")?;
        Self::git(&repo, &["worktree", "remove", &path.to_string_lossy()])?;
        self.open.borrow_mut().remove(id);
        Ok(())
    }

    fn notify(&self, title: &str, body: &str, _sound: &str) -> Result<()> {
        if self.fail_notify.get() {
            bail!("herdr fora do ar (teste)");
        }
        self.notifications.borrow_mut().push(format!("{title} | {body}"));
        Ok(())
    }

    fn report_metadata(&self, workspace_id: &str, value: &str) -> Result<()> {
        self.metadata.borrow_mut().push((workspace_id.into(), value.into()));
        Ok(())
    }

    fn list_workspaces(&self, deadline: Instant) -> Result<Vec<WorkspaceInfo>> {
        Self::before(deadline)?;
        self.check("workspace list", self.fail_list_workspaces.get())?;
        let focused = self.focused.borrow().clone();
        Ok(self
            .workspaces
            .borrow()
            .iter()
            .map(|w| WorkspaceInfo { focused: focused.as_deref() == Some(w.id.as_str()), ..w.clone() })
            .collect())
    }

    fn create_workspace(&self, label: &str, cwd: &Path, deadline: Instant) -> Result<(String, Option<String>)> {
        Self::before(deadline)?;
        self.check("workspace create", false)?;
        let id = format!("ws{}", self.next.get());
        self.next.set(self.next.get() + 1);
        let root = format!("{id}:p1");
        self.workspaces.borrow_mut().push(WorkspaceInfo { id: id.clone(), label: label.into(), focused: false });
        self.panes.borrow_mut().insert(id.clone(), vec![PaneInfo { id: root.clone(), is_reviewq_tui: false }]);
        self.created_cwds.borrow_mut().push(cwd.to_path_buf());
        Ok((id, Some(root)))
    }

    fn focus_workspace(&self, id: &str, deadline: Instant) -> Result<()> {
        Self::before(deadline)?;
        self.check("workspace focus", false)?;
        self.focus_calls.borrow_mut().push(id.into());
        *self.focused.borrow_mut() = Some(id.into());
        Ok(())
    }

    fn list_panes(&self, workspace: &str, deadline: Instant) -> Result<Vec<PaneInfo>> {
        self.check("pane list", self.fail_list_panes.get())?;
        let panes = self.panes.borrow().get(workspace).cloned().unwrap_or_default();
        // como o HerdrCli: prazo checado antes de cada `process-info`, que demora `pane_scan_delay`
        for _ in &panes {
            Self::before(deadline)?;
            std::thread::sleep(self.pane_scan_delay.get());
        }
        Ok(panes)
    }

    fn open_plugin_pane(&self, workspace: &str, target_pane: &str, deadline: Instant) -> Result<()> {
        Self::before(deadline)?;
        self.check("plugin pane open", self.fail_open_pane.get())?;
        self.opened_on.borrow_mut().push((workspace.into(), target_pane.into()));
        self.panes
            .borrow_mut()
            .entry(workspace.into())
            .or_default()
            .push(PaneInfo { id: format!("{workspace}:tui"), is_reviewq_tui: !self.tui_starts_dead.get() });
        if self.open_steals_focus.get() {
            *self.focused.borrow_mut() = Some(workspace.into());
        }
        Ok(())
    }

    fn list_agents(&self, deadline: Instant) -> Result<Vec<AgentInfo>> {
        Self::before(deadline)?;
        self.check("agent list", self.fail_agents.get())?;
        Ok(self.agents.borrow().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{FakeRunner, Output};

    fn far() -> std::time::Instant {
        std::time::Instant::now() + Duration::from_secs(60)
    }

    const CREATED: &str = r#"{"id":"cli:worktree:create","result":{"type":"worktree_created",
        "workspace":{"workspace_id":"w7"},"tab":{},"root_pane":{},
        "worktree":{"path":"/w/o/r/pr-7","is_bare":false,"is_detached":false,"is_prunable":false,"is_linked_worktree":true,"label":"x"}}}"#;

    #[test]
    fn create_parses_ids_and_passes_label_as_single_arg() {
        let fake = FakeRunner::new();
        fake.on("worktree create", Output::ok(CREATED));
        let h = HerdrCli::new(&fake, None);
        let label = "#7 Ajusta \"webhook\" de fornecedores — ação";
        let created = h
            .create_worktree(&CreateReq {
                repo: "/r".into(), branch: "feat/x".into(), base: "origin/feat/x".into(),
                path: "/w/o/r/pr-7".into(), label: label.into(),
            })
            .unwrap();
        assert_eq!(created.workspace_id, "w7");
        let call = fake.calls.borrow()[0].clone();
        let i = call.args.iter().position(|a| a == "--label").unwrap();
        assert_eq!(call.args[i + 1], label);
        assert!(call.args.contains(&"--no-focus".to_string()));
        assert!(!call.args.iter().any(|a| a == "--force"));
    }

    #[test]
    fn session_flag_comes_before_subcommand() {
        let fake = FakeRunner::new();
        fake.on("worktree list", Output::ok(r#"{"result":{"type":"worktree_list","source":{},"worktrees":[]}}"#));
        HerdrCli::new(&fake, Some("main".into())).find_workspace(Path::new("/r"), Path::new("/w/x")).unwrap();
        assert!(fake.lines()[0].starts_with("herdr --session main worktree list"));
    }

    #[test]
    fn error_response_becomes_err_even_on_stderr() {
        let out = Output { status: Some(1), stderr: r#"{"error":{"code":"not_git_worktree","message":"sem repo"}}"#.into(), ..Default::default() };
        let err = parse_response(&out).unwrap_err().to_string();
        assert!(err.contains("sem repo") && err.contains("not_git_worktree"));
    }

    #[test]
    fn find_workspace_open_closed_and_absent() {
        let fake = FakeRunner::new();
        fake.on("worktree list", Output::ok(r#"{"result":{"worktrees":[{"path":"/w/a","open_workspace_id":"w1"},{"path":"/w/b","open_workspace_id":null}]}}"#));
        fake.on("worktree list", Output::ok(r#"{"result":{"worktrees":[{"path":"/w/a","open_workspace_id":"w1"},{"path":"/w/b","open_workspace_id":null}]}}"#));
        fake.on("worktree open", Output::ok(r#"{"result":{"type":"worktree_opened","workspace":{"workspace_id":"w2"}}}"#));
        fake.on("worktree list", Output::ok(r#"{"result":{"worktrees":[]}}"#));
        let h = HerdrCli::new(&fake, None);
        assert_eq!(h.find_workspace(Path::new("/r"), Path::new("/w/a")).unwrap().as_deref(), Some("w1"));
        assert_eq!(h.find_workspace(Path::new("/r"), Path::new("/w/b")).unwrap().as_deref(), Some("w2"));
        assert_eq!(h.find_workspace(Path::new("/r"), Path::new("/w/c")).unwrap(), None);
    }

    #[test]
    fn find_open_workspace_never_opens_a_closed_one() {
        let fake = FakeRunner::new();
        let list = r#"{"result":{"worktrees":[{"path":"/w/a","open_workspace_id":"w1"},{"path":"/w/b","open_workspace_id":null}]}}"#;
        for _ in 0..3 {
            fake.on("worktree list", Output::ok(list));
        }
        let h = HerdrCli::new(&fake, None);
        assert_eq!(h.find_open_workspace(Path::new("/r"), Path::new("/w/a")).unwrap().as_deref(), Some("w1"));
        assert_eq!(h.find_open_workspace(Path::new("/r"), Path::new("/w/b")).unwrap(), None);
        assert_eq!(h.find_open_workspace(Path::new("/r"), Path::new("/w/c")).unwrap(), None);
        assert!(fake.lines().iter().all(|l| !l.contains("worktree open")), "{:?}", fake.lines());
    }

    #[test]
    fn notify_checks_exit_code_only() {
        let fake = FakeRunner::new();
        fake.on("notification show", Output::ok(""));
        fake.on("notification show", Output::fail(1, "sem servidor"));
        let h = HerdrCli::new(&fake, None);
        h.notify("t", "b", "request").unwrap();
        assert!(h.notify("t", "b", "request").is_err());
        assert!(fake.lines()[0].contains("--sound request"));
    }

    fn proc_info(argvs: &[&[&str]]) -> String {
        let procs: Vec<Value> = argvs
            .iter()
            .enumerate()
            .map(|(i, a)| serde_json::json!({"pid": i + 10, "name": a[0], "argv": a, "argv0": a[0], "cmdline": a.join(" "), "cwd": "/"}))
            .collect();
        serde_json::json!({"result":{"type":"pane_process_info","process_info":{"pane_id":"x","shell_pid":1,"foreground_process_group_id":1,"foreground_processes":procs}}}).to_string()
    }

    #[test]
    fn workspaces_and_plugin_pane_commands() {
        let fake = FakeRunner::new();
        fake.on("workspace list", Output::ok(r#"{"result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1","label":"~","focused":true,"number":1},{"workspace_id":"w9","label":"reviewq","focused":false,"number":2}]}}"#));
        fake.on("workspace create", Output::ok(r#"{"result":{"workspace":{"workspace_id":"w10"},"tab":{},"root_pane":{"pane_id":"w10:p1"}}}"#));
        fake.on("plugin pane open", Output::ok(r#"{"result":{"plugin_pane":{"pane":{"pane_id":"w10:p2"}}}}"#));
        fake.on("workspace focus", Output::ok(r#"{"result":{}}"#));
        let h = HerdrCli::new(&fake, None);
        let ws = h.list_workspaces(far()).unwrap();
        assert_eq!(ws[0], WorkspaceInfo { id: "w1".into(), label: "~".into(), focused: true });
        assert_eq!(ws[1], WorkspaceInfo { id: "w9".into(), label: "reviewq".into(), focused: false });
        assert_eq!(h.create_workspace("reviewq", Path::new("/Users/c"), far()).unwrap(), ("w10".into(), Some("w10:p1".into())));
        h.open_plugin_pane("w10", "w10:p1", far()).unwrap();
        h.focus_workspace("w10", far()).unwrap();
        let lines = fake.lines();
        assert!(lines[1].contains("--label reviewq") && lines[1].contains("--no-focus") && lines[1].contains("--cwd /Users/c"), "{}", lines[1]);
        let open = fake.calls.borrow()[2].clone();
        let pair = |flag: &str| open.args.iter().position(|a| a == flag).map(|i| open.args[i + 1].clone());
        assert_eq!(pair("--plugin").as_deref(), Some(PLUGIN_ID));
        assert_eq!(pair("--entrypoint").as_deref(), Some("tui"));
        assert_eq!(pair("--target-pane").as_deref(), Some("w10:p1"));
        assert_eq!(pair("--placement").as_deref(), Some("zoomed"));
        assert!(!open.args.iter().any(|a| a == "--workspace"), "spike: --workspace com --target-pane é erro");
        assert_eq!(lines[3], "herdr workspace focus w10");
    }

    #[test]
    fn list_workspaces_rejects_incomplete_entries() {
        for bad in [
            r#"{"result":{"workspaces":[{"label":"x","focused":false}]}}"#,
            r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"x"}]}}"#,
            r#"{"result":{"workspaces":[{"workspace_id":7,"label":"x","focused":false}]}}"#,
            r#"{"result":{}}"#,
        ] {
            let fake = FakeRunner::new();
            fake.on("workspace list", Output::ok(bad));
            assert!(HerdrCli::new(&fake, None).list_workspaces(far()).is_err(), "{bad}");
        }
    }

    #[test]
    fn list_panes_identifies_tui_by_live_process_argv() {
        let fake = FakeRunner::new();
        fake.on("pane list", Output::ok(r#"{"result":{"type":"pane_list","panes":[
            {"pane_id":"w5:p1","workspace_id":"w5","focused":false},
            {"pane_id":"w5:p2","workspace_id":"w5","label":"reviewq","focused":true},
            {"pane_id":"w5:p3","workspace_id":"w5","label":"reviewq","focused":false},
            {"pane_id":"w5:p4","workspace_id":"w5","label":"reviewq","focused":false}]}}"#));
        // shell comum
        fake.on("process-info --pane w5:p1", Output::ok(&proc_info(&[&["-zsh"]])));
        // sh -c ainda vivo + o binário de verdade como filho
        fake.on(
            "process-info --pane w5:p2",
            Output::ok(&proc_info(&[&["sh", "-c", "exec herdr-reviewq tui"], &["/Users/c/.cargo/bin/herdr-reviewq", "tui"]])),
        );
        // shell que só menciona o comando (o TUI não está rodando)
        fake.on("process-info --pane w5:p3", Output::ok(&proc_info(&[&["bash", "-c", "herdr-reviewq tui"], &["vim", "herdr-reviewq", "tui"]])));
        // pane do plugin sem processo vivo
        fake.on("process-info --pane w5:p4", Output::ok(&proc_info(&[])));
        let panes = HerdrCli::new(&fake, None).list_panes("w5", far()).unwrap();
        let got: Vec<(&str, bool)> = panes.iter().map(|p| (p.id.as_str(), p.is_reviewq_tui)).collect();
        assert_eq!(got, vec![("w5:p1", false), ("w5:p2", true), ("w5:p3", false), ("w5:p4", false)]);
        assert!(fake.lines()[0].contains("pane list --workspace w5"), "{}", fake.lines()[0]);
    }

    #[test]
    fn list_panes_rejects_incomplete_json() {
        let fake = FakeRunner::new();
        fake.on("pane list", Output::ok(r#"{"result":{"panes":[{"workspace_id":"w5"}]}}"#));
        assert!(HerdrCli::new(&fake, None).list_panes("w5", far()).is_err());

        let fake = FakeRunner::new();
        fake.on("pane list", Output::ok(r#"{"result":{"panes":[{"pane_id":"w5:p1","workspace_id":"w5"}]}}"#));
        fake.on("process-info", Output::ok(r#"{"result":{"process_info":{"pane_id":"w5:p1"}}}"#));
        assert!(HerdrCli::new(&fake, None).list_panes("w5", far()).is_err());

        let fake = FakeRunner::new();
        fake.on("pane list", Output::ok(r#"{"result":{"panes":[{"pane_id":"w5:p1","workspace_id":"w5"}]}}"#));
        fake.on("process-info", Output::ok(r#"{"result":{"process_info":{"foreground_processes":[{"pid":1,"name":"x"}]}}}"#));
        assert!(HerdrCli::new(&fake, None).list_panes("w5", far()).is_err());

        let fake = FakeRunner::new();
        fake.on("workspace create", Output::ok(r#"{"result":{"workspace":{},"root_pane":{"pane_id":"w1:p1"}}}"#));
        assert!(HerdrCli::new(&fake, None).create_workspace("reviewq", Path::new("/h"), far()).is_err());
    }

    /// Responde `pane list` com 5 panes e demora `delay` em cada `process-info`.
    struct SlowPanes {
        delay: Duration,
        calls: std::cell::RefCell<Vec<Cmd>>,
    }

    impl Runner for SlowPanes {
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.calls.borrow_mut().push(cmd.clone());
            let line = cmd.line();
            if line.contains("pane list") {
                let panes: Vec<Value> = (1..=5).map(|i| serde_json::json!({"pane_id": format!("w5:p{i}"), "workspace_id": "w5"})).collect();
                return Ok(Output::ok(&serde_json::json!({"result":{"panes":panes}}).to_string()));
            }
            std::thread::sleep(self.delay);
            Ok(Output::ok(&proc_info(&[&["-zsh"]])))
        }
    }

    #[test]
    fn list_panes_stops_scanning_at_the_deadline() {
        let slow = SlowPanes { delay: Duration::from_millis(60), calls: Default::default() };
        let h = HerdrCli::new(&slow, None).with_timeout(Duration::from_secs(5));
        let start = std::time::Instant::now();
        let err = h.list_panes("w5", start + Duration::from_millis(100)).unwrap_err().to_string();
        assert!(err.contains("prazo"), "{err}");
        let infos: Vec<Cmd> = slow.calls.borrow().iter().filter(|c| c.line().contains("process-info")).cloned().collect();
        assert!(infos.len() <= 2, "parou entre panes, não varreu os 5: {}", infos.len());
        assert!(start.elapsed() < Duration::from_millis(250), "{:?}", start.elapsed());
        // cada chamada usa no máximo o tempo que resta
        assert!(infos.iter().all(|c| c.timeout <= Duration::from_millis(100)), "{:?}", infos.iter().map(|c| c.timeout).collect::<Vec<_>>());

        let slow = SlowPanes { delay: Duration::ZERO, calls: Default::default() };
        let h = HerdrCli::new(&slow, None);
        let err = h.list_panes("w5", std::time::Instant::now()).unwrap_err().to_string();
        assert!(err.contains("prazo"), "{err}");
        assert!(slow.calls.borrow().is_empty(), "prazo vencido: nenhuma chamada");
    }

    #[test]
    fn ui_routing_resolves_bin_and_session_from_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        let both = UiRouting::resolve(env(&[("HERDR_BIN_PATH", "/opt/herdr/bin/herdr"), ("HERDR_SOCKET_PATH", "/tmp/s.sock")]), Some("main".into()));
        assert_eq!(both, UiRouting { bin: "/opt/herdr/bin/herdr".into(), session: None });
        let none = UiRouting::resolve(env(&[]), Some("main".into()));
        assert_eq!(none, UiRouting { bin: "herdr".into(), session: Some("main".into()) });
        let empty_bin = UiRouting::resolve(env(&[("HERDR_BIN_PATH", "")]), None);
        assert_eq!(empty_bin.bin, "herdr");
    }

    #[test]
    fn routed_cli_uses_bin_session_and_timeout() {
        let fake = FakeRunner::new();
        fake.on("worktree list", Output::ok(r#"{"result":{"worktrees":[]}}"#));
        fake.on("worktree list", Output::ok(r#"{"result":{"worktrees":[]}}"#));
        let r = UiRouting { bin: "/opt/herdr/bin/herdr".into(), session: None };
        HerdrCli::routed(&fake, &r).with_timeout(Duration::from_secs(5)).find_open_workspace(Path::new("/r"), Path::new("/w")).unwrap();
        let r = UiRouting { bin: "herdr".into(), session: Some("main".into()) };
        HerdrCli::routed(&fake, &r).find_open_workspace(Path::new("/r"), Path::new("/w")).unwrap();
        let calls = fake.calls.borrow();
        assert!(calls[0].line().starts_with("/opt/herdr/bin/herdr worktree list"), "{}", calls[0].line());
        assert_eq!(calls[0].timeout, Duration::from_secs(5));
        assert!(calls[1].line().starts_with("herdr --session main worktree list"));
        assert_eq!(calls[1].timeout, Duration::from_secs(60), "timeout padrão do núcleo continua 60 s");
    }

    #[test]
    fn fake_herdr_creates_and_removes_real_worktree() {
        let fx = crate::testutil::fixture("feat/x");
        crate::git::Git::new(&crate::runner::RealRunner).fetch_branch(&fx.clone, "feat/x").unwrap();
        let h = FakeHerdr::new();
        let path = fx.root.join("wt/pr-1");
        let c = h
            .create_worktree(&CreateReq { repo: fx.clone.clone(), branch: "feat/x".into(), base: "origin/feat/x".into(), path: path.clone(), label: "x".into() })
            .unwrap();
        assert!(path.join("feature.txt").exists());
        assert_eq!(h.find_workspace(&fx.clone, &path).unwrap(), Some(c.workspace_id.clone()));
        h.remove_worktree(&c.workspace_id).unwrap();
        assert!(!path.exists());
    }

    fn app(fake: &FakeRunner) -> HerdrApp<'_> {
        HerdrApp::new(fake, "/Users/c/.local/bin/herdr-reviewq".into())
    }

    fn spaces(ids: &[(&str, &str, bool)]) -> String {
        serde_json::json!({ "spaces": ids.iter().map(|(id, name, active)| serde_json::json!({ "id": id, "name": name, "active": active })).collect::<Vec<_>>() }).to_string()
    }

    #[test]
    fn client_follows_the_configured_backend() {
        let fake = FakeRunner::new();
        fake.on("herdr-app ls", Output::ok(&spaces(&[("sp1", "linkana", true)])));
        let cfg = crate::config::Config::parse(
            "backend = \"herdr-app\"\nworktrees_dir = \"/w\"\n[[repos]]\nname = \"o/r\"\npath = \"/r\"\nsetup = []\n",
            Path::new("/h"),
        )
        .unwrap();
        let routing = UiRouting { bin: "herdr".into(), session: None };
        let ws = client(&cfg, &fake, &routing, Path::new("/h"), Duration::from_secs(5)).list_workspaces(far()).unwrap();
        assert_eq!(ws, vec![WorkspaceInfo { id: "sp1".into(), label: "linkana".into(), focused: true }]);
    }

    #[test]
    fn app_creates_worktree_without_force_and_names_the_space_once_adopted() {
        let fake = FakeRunner::new();
        let id = space_id(Path::new("/w/o/r/pr-7"));
        fake.on("git worktree add", Output::ok(""));
        fake.on("hc run", Output::ok(r#"{"address":"t9"}"#));
        fake.on("herdr-app ls", Output::ok(&spaces(&[("sp1", "linkana", true)])));
        fake.on("herdr-app ls", Output::ok(&spaces(&[("sp1", "linkana", true), (&id, "pr-7", false)])));
        fake.on("space.rename", Output::ok(r#"{"ok":true}"#));
        let created = app(&fake)
            .create_worktree(&CreateReq {
                repo: "/r".into(), branch: "feat/x".into(), base: "origin/feat/x".into(), path: "/w/o/r/pr-7".into(), label: "#7 título".into(),
            })
            .unwrap();
        assert_eq!(created.workspace_id, id);
        let calls = fake.calls.borrow();
        assert_eq!(calls[0].args, ["worktree", "add", "-q", "-b", "feat/x", "/w/o/r/pr-7", "origin/feat/x"]);
        assert_eq!(calls[0].cwd.as_deref(), Some(Path::new("/r")));
        assert!(!calls.iter().any(|c| c.args.iter().any(|a| a == "--force")));
        assert!(calls[1].args.contains(&format!("herdr.space={id}")) && calls[1].args.contains(&"reviewq.worktree=/w/o/r/pr-7".to_string()));
        let rename: Value = serde_json::from_str(calls[4].args.last().unwrap()).unwrap();
        assert_eq!((rename["id"].as_str(), rename["name"].as_str()), (Some(id.as_str()), Some("#7 título")));
    }

    #[test]
    fn app_creates_spaces_with_the_window_closed() {
        let fake = FakeRunner::new();
        fake.on("git worktree add", Output::ok(""));
        fake.on("hc run", Output::ok(r#"{"address":"t9"}"#));
        fake.on("herdr-app ls", Output::fail(1, r#"{"error":"Herdr is not running"}"#));
        let req = CreateReq { repo: "/r".into(), branch: "b".into(), base: "origin/b".into(), path: "/w/pr-1".into(), label: "#1".into() };
        assert!(app(&fake).create_worktree(&req).is_ok());
        assert!(fake.lines().iter().all(|l| !l.contains("space.rename")));
    }

    #[test]
    fn app_find_open_workspace_never_opens_and_find_workspace_opens_only_existing_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr-1");
        std::fs::create_dir_all(&path).unwrap();
        let fake = FakeRunner::new();
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t3","state":"finished"}]}"#));
        fake.on("hc ls", Output::ok(r#"{"terminals":[]}"#));
        fake.on("hc run", Output::ok(r#"{"address":"t4"}"#));
        fake.on("herdr-app ls", Output::fail(1, "{}"));
        fake.on("hc ls", Output::ok(r#"{"terminals":[]}"#));
        let h = app(&fake);
        assert_eq!(h.find_open_workspace(Path::new("/r"), &path).unwrap(), None);
        assert_eq!(h.find_workspace(Path::new("/r"), &path).unwrap(), Some(space_id(&path)));
        assert_eq!(h.find_workspace(Path::new("/r"), &dir.path().join("sumiu")).unwrap(), None);
        assert_eq!(fake.lines().iter().filter(|l| l.starts_with("hc run")).count(), 1);
    }

    #[test]
    fn app_remove_finds_the_worktree_by_label_closes_the_space_and_removes_without_force() {
        let fake = FakeRunner::new();
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t3","state":"live"},{"address":"t5","state":"live"}]}"#));
        fake.on("hc get t3", Output::ok(r#"{"address":"t3","labels":{"herdr.space":"rq-1","reviewq.worktree":"/w/o/r/pr-7"}}"#));
        fake.on("git rev-parse", Output::ok("/r/.git\n"));
        fake.on("space.close", Output::fail(1, r#"{"error":"Herdr is not running"}"#));
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t3","state":"live"},{"address":"t5","state":"live"}]}"#));
        fake.on("hc kill t3", Output::ok(""));
        fake.on("hc kill t5", Output::ok(""));
        fake.on("git worktree remove", Output::ok(""));
        app(&fake).remove_worktree("rq-1").unwrap();
        let calls = fake.calls.borrow();
        let last = calls.last().unwrap();
        assert_eq!((last.args.clone(), last.cwd.clone()), (vec!["worktree".to_string(), "remove".into(), "/w/o/r/pr-7".into()], Some("/r".into())));
        assert!(!calls.iter().any(|c| c.args.iter().any(|a| a == "--force")));
    }

    #[test]
    fn app_remove_refuses_a_space_it_did_not_open() {
        let fake = FakeRunner::new();
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t3","state":"live"}]}"#));
        fake.on("hc get t3", Output::ok(r#"{"address":"t3","labels":{"herdr.space":"sp1"}}"#));
        assert!(app(&fake).remove_worktree("sp1").unwrap_err().to_string().contains("reviewq.worktree"));
        assert!(fake.lines().iter().all(|l| !l.contains("kill") && !l.contains("worktree remove")));
    }

    #[test]
    fn app_notifies_through_macos_with_text_as_arguments() {
        let fake = FakeRunner::new();
        fake.on("osascript", Output::ok(""));
        fake.on("osascript", Output::ok(""));
        let h = app(&fake);
        h.notify("#7 pronto", "título \"com\" aspas", "request").unwrap();
        h.notify("reviewq", "x", "none").unwrap();
        let calls = fake.calls.borrow();
        assert_eq!(&calls[0].args[2..], ["#7 pronto", "título \"com\" aspas"]);
        assert!(!calls[0].args[1].contains("título"), "o texto nunca entra no script");
        assert!(calls[0].args[1].contains("sound name") && !calls[1].args[1].contains("sound name"));
    }

    #[test]
    fn app_ensure_opens_the_panel_space_with_the_tui_and_finds_it_running() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeRunner::new();
        fake.on("herdr-app ls", Output::ok(&spaces(&[("sp1", "linkana", true)])));
        fake.on("hc run", Output::ok(r#"{"address":"t8"}"#));
        fake.on("herdr-app ls", Output::fail(1, "{}"));
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t8","state":"live"}]}"#));
        fake.on("hc get t8", Output::ok(r#"{"address":"t8","foreground":{"argv":["/Users/c/.local/bin/herdr-reviewq","tui"]}}"#));
        let (ws, outcome) = crate::ui::ensure(&app(&fake), &dir.path().join("ui.lock"), dir.path(), far(), Duration::from_secs(1)).unwrap();
        assert!(ws.starts_with("rq-panel-"));
        assert_eq!(outcome, crate::ui::UiOutcome::AlreadyOpen);
        let run = fake.calls.borrow()[1].clone();
        assert!(run.args.ends_with(&["--".to_string(), "/Users/c/.local/bin/herdr-reviewq".into(), "tui".into()]), "{:?}", run.args);
        assert!(fake.lines().iter().filter(|l| l.starts_with("hc run")).count() == 1, "sem shell vazio ao lado do painel");
    }

    #[test]
    fn app_ensure_reuses_the_panel_space_and_reopens_a_closed_tui() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeRunner::new();
        fake.on("herdr-app ls", Output::ok(&spaces(&[("sp1", "linkana", true), ("rq-panel-1", "reviewq", false)])));
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t2","state":"live"}]}"#));
        fake.on("hc get t2", Output::ok(r#"{"address":"t2","foreground":{"argv":["-zsh"]}}"#));
        fake.on("hc run", Output::ok(r#"{"address":"t9"}"#));
        fake.on("herdr-app ls", Output::ok(&spaces(&[("sp1", "linkana", true), ("rq-panel-1", "reviewq", false)])));
        fake.on("hc ls", Output::ok(r#"{"terminals":[{"address":"t2","state":"live"},{"address":"t9","state":"live"}]}"#));
        fake.on("hc get t2", Output::ok(r#"{"address":"t2","foreground":{"argv":["-zsh"]}}"#));
        fake.on("hc get t9", Output::ok(r#"{"address":"t9","argv":["/Users/c/.local/bin/herdr-reviewq","tui"]}"#));
        let (ws, outcome) = crate::ui::ensure(&app(&fake), &dir.path().join("ui.lock"), dir.path(), far(), Duration::from_secs(1)).unwrap();
        assert_eq!((ws.as_str(), outcome), ("rq-panel-1", crate::ui::UiOutcome::Opened));
        assert!(fake.lines().iter().any(|l| l.starts_with("hc run") && l.contains("herdr.space=rq-panel-1")));
    }

    #[test]
    fn app_lists_known_agents_with_their_foreground_folder() {
        let fake = FakeRunner::new();
        fake.on("hc ls", Output::ok(r#"{"terminals":[
            {"address":"t2","agent":"claude-code","cwd":"/w/pr-1","state":"live"},
            {"address":"t3","agent":"zsh","cwd":"/w/pr-2","state":"live"},
            {"address":"t4","agent":"codex","cwd":"/w/pr-3","state":"finished"}]}"#));
        fake.on("hc get t2", Output::ok(r#"{"address":"t2","foreground":{"cwd":"/w/pr-1/app"}}"#));
        assert_eq!(
            app(&fake).list_agents(far()).unwrap(),
            vec![AgentInfo { agent: "claude".into(), cwd: Some("/w/pr-1".into()), foreground_cwd: Some("/w/pr-1/app".into()) }]
        );
    }
}
