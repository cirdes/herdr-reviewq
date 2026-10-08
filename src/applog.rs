use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static LOG: OnceLock<PathBuf> = OnceLock::new();
const MAX_BYTES: u64 = 10 * 1024 * 1024;

pub fn init(path: PathBuf) {
    let _ = LOG.set(path);
}

/// Linha do daemon.log, com segredos redigidos.
pub fn format_line(msg: &str) -> String {
    let msg = msg.lines().map(crate::setup::redact).collect::<Vec<_>>().join("\n");
    format!("{} {msg}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))
}

pub fn info(msg: &str) {
    let line = format_line(msg);
    let Some(path) = LOG.get() else {
        eprint!("{line}");
        return;
    };
    if write_line(path, &line).is_err() {
        eprint!("{line}");
    }
}

/// Acrescenta uma linha (com data e segredos redigidos) a um log próprio, como o `tui.log`:
/// cria o diretório, gira acima de 10 MB e grava em modo 600.
pub fn append(path: &Path, msg: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_line(path, &format_line(msg))
}

fn write_line(path: &Path, line: &str) -> std::io::Result<()> {
    if std::fs::metadata(path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_dir_and_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/tui.log");
        append(&path, "primeira").unwrap();
        append(&path, "segunda").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].ends_with(" primeira") && lines[1].ends_with(" segunda"), "{text}");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn log_lines_are_redacted() {
        let line = format_line("git fetch https://u:segredo@github.com/o/r falhou\nGH_TOKEN=ghp_abc");
        assert!(!line.contains("segredo") && !line.contains("ghp_abc"), "{line}");
        assert!(line.contains("https://***@github.com/o/r falhou"), "{line}");
        assert!(line.ends_with("\n***\n"), "{line}");
    }
}
