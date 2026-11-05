//! Восстановление на сервер по SSH: сервер ставит на цель бинарник HopeBackup, передаёт задание
//! (с ключом доступа к площадке) через stdin и читает ход выполнения. Если у цели нет доступа к
//! площадке, данные идут через сервер управления по обратному туннелю (`ssh -R`).

use anyhow::{anyhow, bail, Context, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::state::Target;
use super::Server;
use crate::plugins::LogFn;
use crate::proto::{ExecEvent, JobSpec};

#[allow(unused_imports)]
use std::os::unix::process::CommandExt;

const REMOTE_BIN: &str = "/usr/local/bin/hopebackup";

pub fn base_cmd(t: &Target) -> Result<tokio::process::Command> {
    let dir = crate::agent::data_dir().join("ssh");
    std::fs::create_dir_all(&dir)?;
    let kf = dir.join(format!("target-{}.key", t.id));
    let mut k = t.private_key.replace("\r\n", "\n");
    if !k.ends_with('\n') { k.push('\n'); }
    crate::util::write_secret(&kf, k.as_bytes())?;
    let mut c = tokio::process::Command::new("ssh");
    c.args(["-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=15", "-o", "IdentitiesOnly=yes"]);
    c.arg("-o").arg(format!("UserKnownHostsFile={}", dir.join("known_hosts").display()));
    if t.private_key.trim().is_empty() && !t.password.is_empty() {
        // Вход по паролю: пароль отдаёт сама программа как SSH_ASKPASS (без терминала, без sshpass);
        // сам пароль — в файле 0600, в окружение попадает только путь.
        let pf = dir.join(format!("target-{}.pw", t.id));
        crate::util::write_secret(&pf, t.password.as_bytes())?;
        c.args(["-o", "BatchMode=no", "-o", "PreferredAuthentications=keyboard-interactive,password", "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1"]);
        c.env("SSH_ASKPASS", std::env::current_exe()?).env("SSH_ASKPASS_REQUIRE", "force").env("DISPLAY", ":0").env("HOPEBACKUP_ASKPASS", &pf);
        // Без управляющего терминала старые OpenSSH тоже берут пароль у SSH_ASKPASS.
        unsafe { c.pre_exec(|| { libc::setsid(); Ok(()) }); }
    } else {
        c.args(["-o", "BatchMode=yes"]);
        c.arg("-i").arg(&kf);
    }
    c.arg("-p").arg(t.port.to_string());
    Ok(c)
}

/// Адрес цели `user@host` (с проверкой: ни пользователь, ни адрес не должны выглядеть как опция ssh).
pub fn dest(t: &Target) -> Result<String> {
    crate::util::valid_ssh_user(&t.user)?;
    crate::util::valid_host(&t.host)?;
    Ok(format!("{}@{}", t.user, t.host))
}

/// Как получать root на цели: префикс команды и то, что первым идёт в stdin (пароль для `sudo -S`).
pub struct Sudo { pub pre: &'static str, pub prefix: Vec<u8> }

/// Определить способ повышения прав: root, sudo без пароля (в т. ч. NOPASSWD) или sudo с паролем.
/// Пароль в stdin пишем, только когда sudo его действительно спросит, — иначе он попал бы в данные команды.
pub async fn sudo_mode(t: &Target) -> Result<Sudo> {
    if t.user == "root" { return Ok(Sudo { pre: "", prefix: vec![] }); }
    let mut c = base_cmd(t)?;
    c.arg("--").arg(dest(t)?).arg("sudo -n true 2>&1; echo \"rc=$?\"");
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let o = tokio::time::timeout(std::time::Duration::from_secs(60), c.output()).await.map_err(|_| anyhow!("ssh {}: нет ответа 60 с", t.host))??;
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    if !o.status.success() && !out.contains("rc=") {
        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
        bail!("ssh {}@{}: {}", t.user, t.host, if e.contains("Permission denied") { format!("вход не выполнен — проверьте ключ или пароль ({e})") } else { e });
    }
    if out.contains("rc=0") { return Ok(Sudo { pre: "sudo -n ", prefix: vec![] }); }
    if !t.password.is_empty() { return Ok(Sudo { pre: "sudo -k -S -p '' ", prefix: format!("{}\n", t.password).into_bytes() }); }
    if out.contains("requiretty") || out.contains("a tty") { bail!("на цели sudo требует терминал (Defaults requiretty) — разрешите sudo без терминала для {} или входите под root", t.user); }
    bail!("нужны права root: sudo без пароля или пароль пользователя {}", t.user)
}

/// Проверка цели: вход по ключу или паролю, права root (или sudo), ОС.
pub async fn test(t: &Target) -> Result<String> {
    let su = sudo_mode(t).await?;
    test_with(t, &su).await
}

async fn test_with(t: &Target, su: &Sudo) -> Result<String> {
    let mut c = base_cmd(t)?;
    c.arg("--").arg(dest(t)?).arg(format!("{}sh -c 'id -u; uname -srm; cat /etc/os-release 2>/dev/null | grep ^PRETTY_NAME'", su.pre));
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().context("ssh")?;
    if let Some(mut si) = child.stdin.take() { si.write_all(&su.prefix).await?; }
    let o = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output()).await.map_err(|_| anyhow!("ssh {}: нет ответа 90 с", t.host))??;
    if !o.status.success() {
        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
        bail!("ssh {}@{}: {}", t.user, t.host, if e.contains("Permission denied") { format!("вход не выполнен — проверьте ключ или пароль ({e})") } else if e.contains("password is required") || e.contains("a terminal is required") || e.contains("incorrect password") || e.contains("try again") { format!("нужны права root: sudo без пароля или верный пароль ({e})") } else if e.contains("requiretty") || e.contains("must have a tty") { format!("sudo требует терминал (Defaults requiretty) ({e})") } else { e });
    }
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let mut lines = out.lines();
    if lines.next().map(|s| s.trim()) != Some("0") { bail!("нужны права root (или sudo без пароля)"); }
    Ok(lines.collect::<Vec<_>>().join(" · "))
}

pub async fn run(s: &Server, mut spec: JobSpec, t: &Target, via: &str, log: LogFn) -> Result<serde_json::Value> {
    let dest = dest(t)?;
    log(&format!("цель {dest}: проверка доступа"));
    let su = sudo_mode(t).await?;
    let info = test_with(t, &su).await?;
    log(&format!("цель: {info}"));

    // 1. Бинарник HopeBackup на цель (тот же, что у сервера).
    let exe = std::env::current_exe()?;
    log(&format!("установка {REMOTE_BIN} на цель"));
    let mut c = base_cmd(t)?;
    c.arg("--").arg(&dest).arg(format!("{}sh -c 'cat > {REMOTE_BIN}.new && chmod 755 {REMOTE_BIN}.new && mv -f {REMOTE_BIN}.new {REMOTE_BIN}'", su.pre));
    c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = c.spawn().context("ssh")?;
    let mut stdin = child.stdin.take().unwrap();
    let data = tokio::fs::read(&exe).await?;
    stdin.write_all(&su.prefix).await?;
    stdin.write_all(&data).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() { bail!("не удалось установить бинарник: {}", String::from_utf8_lossy(&out.stderr).trim()); }

    // 2. Путь данных: напрямую с площадки или через сервер (обратный туннель).
    let mut c = base_cmd(t)?;
    c.args(["-o", "ExitOnForwardFailure=yes"]);
    if via == "server" {
        let site = spec.site.as_mut().ok_or_else(|| anyhow!("нет площадки"))?;
        let remote = s.tunnels.connect(site, &*log).await?;
        let u = remote.url.trim_end_matches('/');
        let (scheme, rest) = u.split_once("://").ok_or_else(|| anyhow!("адрес площадки"))?;
        let (host, port) = rest.rsplit_once(':').ok_or_else(|| anyhow!("в адресе площадки нет порта"))?;
        let rport: u16 = 20000 + (rand::random::<u16>() % 30000);
        c.arg("-R").arg(format!("127.0.0.1:{rport}:{host}:{port}"));
        site.remote.url = format!("{scheme}://127.0.0.1:{rport}");
        site.connectors.clear();
        log(&format!("данные пойдут через сервер управления (обратный туннель, порт {rport} на цели)"));
    } else {
        log("данные пойдут с площадки напрямую на цель");
    }

    // 3. Выполнение: задание — через stdin (секреты не попадают в командную строку и журналы).
    c.arg("--").arg(&dest).arg(format!("{}env HOPEBACKUP_DATA=/var/lib/hopebackup {REMOTE_BIN} restore-exec", su.pre));
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().context("ssh")?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&su.prefix).await?;
    stdin.write_all(serde_json::to_string(&spec)?.as_bytes()).await?;
    drop(stdin);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let l2 = log.clone();
    let errs = tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        let mut tail = Vec::new();
        while let Ok(Some(l)) = r.next_line().await {
            if l.trim().is_empty() || l.contains("Warning: Permanently added") { continue; }
            l2(&format!("цель: {l}"));
            tail.push(l);
        }
        tail
    });
    let mut r = BufReader::new(stdout).lines();
    let mut result = None;
    let mut error = None;
    while let Some(l) = r.next_line().await? {
        match serde_json::from_str::<ExecEvent>(&l) {
            Ok(ExecEvent::Log(m)) => log(&m),
            Ok(ExecEvent::Result(v)) => result = Some(v),
            Ok(ExecEvent::Error(e)) => error = Some(e),
            Err(_) => log(&l),
        }
    }
    let st = child.wait().await?;
    let tail = errs.await.unwrap_or_default();
    if let Some(e) = error { bail!(e); }
    if !st.success() { bail!("выполнение на цели завершилось с кодом {}: {}", st.code().unwrap_or(-1), tail.join(" | ")); }
    result.ok_or_else(|| anyhow!("цель не вернула результат"))
}
