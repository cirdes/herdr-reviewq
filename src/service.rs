use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const LABEL: &str = "dev.cirdes.herdr-reviewq";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn plist(home: &Path, exe: &Path, state_dir: &Path) -> String {
    let h = esc(&home.display().to_string());
    let path_env = format!(
        "{h}/.local/share/mise/shims:{h}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>{h}</string>
    <key>PATH</key><string>{path_env}</string>
    <key>LANG</key><string>en_US.UTF-8</string>
    <key>LC_ALL</key><string>en_US.UTF-8</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>ExitTimeOut</key><integer>120</integer>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{out}</string>
</dict>
</plist>
"#,
        exe = esc(&exe.display().to_string()),
        out = esc(&state_dir.join("daemon.stderr.log").display().to_string()),
    )
}

/// Copia o binário para o caminho estável trocando o arquivo de uma vez (o daemon
/// em execução continua com o inode antigo até o restart).
pub fn install_binary(current: &Path, target: &Path) -> Result<()> {
    if let (Ok(a), Ok(b)) = (current.canonicalize(), target.canonicalize()) {
        if a == b {
            return Ok(());
        }
    }
    let dir = target.parent().context("destino sem diretório")?;
    std::fs::create_dir_all(dir)?;
    let tmp = target.with_extension("new");
    std::fs::copy(current, &tmp).with_context(|| format!("falha ao copiar {}", current.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn plist_path(paths: &Paths) -> PathBuf {
    paths.home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

/// Chama `loaded` a cada `step` até devolver false ou estourar `timeout`. Devolve se descarregou.
pub fn wait_unloaded(mut loaded: impl FnMut() -> bool, timeout: Duration, step: Duration) -> bool {
    let start = Instant::now();
    loop {
        if !loaded() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(step);
    }
}

/// Tenta `f`; se falhar, espera `delay` e tenta mais uma vez.
pub fn retry_once<T>(mut f: impl FnMut() -> Result<T>, delay: Duration) -> Result<T> {
    match f() {
        Ok(v) => Ok(v),
        Err(_) => {
            std::thread::sleep(delay);
            f()
        }
    }
}

fn service_target() -> String {
    format!("{}/{LABEL}", domain())
}

fn is_loaded() -> bool {
    Command::new("launchctl")
        .args(["print", &service_target()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn install(paths: &Paths) -> Result<()> {
    if !paths.config_file.exists() {
        bail!("crie {} antes (veja config.example.toml)", paths.config_file.display());
    }
    let exe = paths.home.join(".local/bin/herdr-reviewq");
    install_binary(&std::env::current_exe()?, &exe)?;
    std::fs::create_dir_all(&paths.state_dir)?;
    let target = plist_path(paths);
    std::fs::create_dir_all(target.parent().expect("tem pai"))?;
    std::fs::write(&target, plist(&paths.home, &exe, &paths.state_dir))?;
    let _ = Command::new("launchctl").args(["bootout", &service_target()]).status();
    // o bootout é assíncrono: bootstrap antes de descarregar falha com "Input/output error"
    if !wait_unloaded(is_loaded, Duration::from_secs(10), Duration::from_millis(200)) {
        println!("nota: o serviço anterior ainda aparece carregado depois de 10s; tentando mesmo assim");
    }
    retry_once(
        || {
            let st = Command::new("launchctl").args(["bootstrap", &domain()]).arg(&target).status()?;
            if !st.success() {
                bail!("launchctl bootstrap falhou");
            }
            Ok(())
        },
        Duration::from_secs(1),
    )?;
    println!("serviço {LABEL} instalado e iniciado com {}", exe.display());
    Ok(())
}

pub fn uninstall(paths: &Paths) -> Result<()> {
    let bootout = Command::new("launchctl").args(["bootout", &service_target()]).output();
    let target = plist_path(paths);
    if target.exists() {
        std::fs::remove_file(&target)?;
        println!("serviço {LABEL} removido");
    } else {
        println!("{} não existe; nada a remover", target.display());
    }
    match bootout {
        Ok(o) if o.status.success() => {}
        Ok(o) => println!("nota: launchctl bootout falhou (talvez o serviço não estivesse carregado): {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => println!("nota: não consegui rodar launchctl bootout: {e}"),
    }
    Ok(())
}

pub fn restart() -> Result<()> {
    let st = Command::new("launchctl").args(["kickstart", "-k", &format!("{}/{LABEL}", domain())]).status()?;
    if !st.success() {
        bail!("launchctl kickstart falhou");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plugin_manifest_matches_code() {
        let m: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
        assert_eq!(m["id"].as_str(), Some(crate::herdr::PLUGIN_ID));
        assert_eq!(m["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(m["panes"][0]["id"].as_str(), Some("tui"));
        let ids: Vec<&str> = m["actions"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["open", "sync-now", "first-ready"]);
        let wrapper = include_str!("../bin/herdr-reviewq");
        assert!(wrapper.contains("exec \"$HOME/.local/bin/herdr-reviewq\" \"$@\""));
    }

    #[test]
    fn plist_is_valid_and_has_stable_paths() {
        let xml = plist(Path::new("/Users/c"), Path::new("/Users/c/.local/bin/herdr-reviewq"), Path::new("/Users/c/.local/state/herdr-reviewq"));
        assert!(xml.contains("<string>/Users/c/.local/bin/herdr-reviewq</string><string>daemon</string>"));
        assert!(xml.contains("/Users/c/.local/share/mise/shims:"));
        assert!(xml.contains("<key>KeepAlive</key><true/>"));
        assert!(xml.contains("<key>ExitTimeOut</key><integer>120</integer>"));
        assert!(xml.contains("<key>LANG</key><string>en_US.UTF-8</string>"));
        assert!(xml.contains("<key>LC_ALL</key><string>en_US.UTF-8</string>"));
        assert!(!xml.contains('~'));
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("t.plist");
        std::fs::write(&f, &xml).unwrap();
        let lint = std::process::Command::new("plutil").arg("-lint").arg(&f).output().unwrap();
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
    }

    #[test]
    fn install_binary_copies_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("novo");
        let target = dir.path().join("bin/herdr-reviewq");
        std::fs::write(&src, "v2").unwrap();
        install_binary(&src, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2");
        std::fs::write(&src, "v3").unwrap();
        install_binary(&src, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v3");
        install_binary(&target, &target).unwrap(); // mesmo arquivo: não faz nada
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o111, 0o111);
    }

    #[test]
    fn wait_unloaded_polls_until_gone_or_timeout() {
        let mut calls = 0;
        assert!(wait_unloaded(|| { calls += 1; calls < 3 }, Duration::from_secs(5), Duration::from_millis(1)));
        assert_eq!(calls, 3);
        let start = Instant::now();
        assert!(!wait_unloaded(|| true, Duration::from_millis(50), Duration::from_millis(10)));
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn retry_once_tries_exactly_twice() {
        let mut calls = 0;
        let r: Result<u32> = retry_once(|| { calls += 1; if calls == 1 { bail!("primeira") } else { Ok(7) } }, Duration::ZERO);
        assert_eq!(r.unwrap(), 7);
        let mut calls = 0;
        let r: Result<()> = retry_once(|| { calls += 1; bail!("sempre") }, Duration::ZERO);
        assert!(r.is_err());
        assert_eq!(calls, 2);
    }

    #[test]
    fn xml_escape() {
        assert_eq!(esc("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}
