//! Comando da tecla Emoji: `sh -c <comando>` desacoplado (stdin/stdout/stderr nulos).
//!
//! Sem zumbi e sem wakeup periódico: uma thread curta (pilha de 64 KiB) bloqueia em `wait()` e
//! some quando o filho termina.

use std::io;

/// Pilha da thread que colhe o filho.
const REAPER_STACK: usize = 64 * 1024;

/// Roda `sh -c <command>` desacoplado; o filho é colhido por uma thread curta.
pub fn run(command: &str) -> io::Result<()> {
    spawn(command).map(|_| ())
}

/// Como [`run`], devolvendo o pid (para o teste verificar que não sobra zumbi).
fn spawn(command: &str) -> io::Result<u32> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = child.id();
    let reaper = std::thread::Builder::new().name("emoji-wait".into()).stack_size(REAPER_STACK).spawn(move || {
        if let Err(e) = child.wait() {
            eprintln!("logi-osd: comando emoji: wait falhou: {e}");
        }
    });
    if let Err(e) = reaper {
        // Sem a thread o filho viraria zumbi: avisa (o processo segue; é raro e não fatal).
        eprintln!("logi-osd: comando emoji: thread de espera não criada: {e}");
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ok()
    }

    /// `pid` ainda existe (vivo ou zumbi)? Colhido = some de `/proc`. Também confere
    /// `/proc/self/task/*/children`, quando o kernel o expõe.
    fn is_child(pid: u32) -> bool {
        let in_children = std::fs::read_dir("/proc/self/task").is_ok_and(|tasks| {
            tasks.flatten().any(|t| {
                std::fs::read_to_string(t.path().join("children"))
                    .is_ok_and(|s| s.split_whitespace().any(|p| p == pid.to_string()))
            })
        });
        in_children || std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[test]
    fn emoji_command_runs_and_is_reaped() {
        let dir = std::env::temp_dir().join(format!("logi-osd-emoji-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok");
        let pid = spawn(&format!("touch '{}'", ok.display())).unwrap();
        assert!(wait_until(Duration::from_secs(1), || ok.exists()), "o comando não rodou");
        assert!(wait_until(Duration::from_secs(1), || !is_child(pid)), "filho {pid} não foi colhido (zumbi)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_reports_spawn_success() {
        assert!(run("true").is_ok());
    }
}
