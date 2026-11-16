//! A private browser process and a small, sequential CDP connection.
use crate::{Cancel, Log};
use anyhow::{Result, bail};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::ErrorKind,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

#[derive(Debug)]
pub struct BrowserDisconnected;
impl std::fmt::Display for BrowserDisconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "La conexión con el navegador se cerró")
    }
}
impl std::error::Error for BrowserDisconnected {}

pub struct Cdp {
    socket: WebSocket<TcpStream>,
    cancel: Cancel,
    timeout: Duration,
    next_id: u64,
}
impl Cdp {
    pub fn connect(address: &str, cancel: Cancel, timeout: Duration) -> Result<Self> {
        let url = url::Url::parse(address)?;
        if url.scheme() != "ws" || url.host_str() != Some("127.0.0.1") {
            bail!("El protocolo del navegador debe ser local")
        }
        cancel.check()?;
        let stream = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", url.port().unwrap_or(80)).parse()?,
            Duration::from_secs(2),
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let (socket, _) = tungstenite::client(address, stream)?;
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))?;
        Ok(Self {
            socket,
            cancel,
            timeout,
            next_id: 0,
        })
    }
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.cancel.check()?;
        self.next_id += 1;
        let id = self.next_id;
        self.socket
            .send(Message::Text(
                json!({"id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .map_err(|_| BrowserDisconnected)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            self.cancel.check()?;
            if Instant::now() >= deadline {
                bail!("Tiempo de respuesta del navegador agotado")
            }
            let message = match self.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(_) => return Err(BrowserDisconnected.into()),
            };
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if value.get("error").is_some() {
                bail!("El navegador rechazó la operación CDP")
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
    pub fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let result = self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
        )?;
        if result.get("exceptionDetails").is_some() {
            bail!("Error JavaScript en la página; no se registran sus datos privados")
        }
        Ok(result["result"]["value"].clone())
    }
}

#[derive(Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
}

enum BrowserProfile {
    Temporary(tempfile::TempDir),
    Dedicated { path: PathBuf, _lock: std::fs::File },
}
impl BrowserProfile {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(dir) => dir.path(),
            Self::Dedicated { path, .. } => path,
        }
    }
    fn dedicated(base: &Path, kind: &str) -> Result<Self> {
        let marker = base.join(".rust-downloader-profile");
        if base.exists() && !marker.is_file() && std::fs::read_dir(base)?.next().is_some() {
            bail!("El directorio indicado no es un perfil dedicado del descargador")
        }
        std::fs::create_dir_all(base)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(base.join(".session.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("La sesión dedicada ya está en uso"))?;
        if !marker.exists() {
            std::fs::write(&marker, b"rust-downloaders-browser-v1\n")?;
        }
        let path = base.join(kind);
        std::fs::create_dir_all(&path)?;
        let preferences = path.join("Default/Preferences");
        if !preferences.exists() {
            std::fs::create_dir_all(preferences.parent().unwrap())?;
            std::fs::write(
                preferences,
                serde_json::to_vec(
                    &json!({"signin":{"allowed":false,"allowed_on_next_startup":false},"sync":{"requested":false},"credentials_enable_service":false,"profile":{"password_manager_enabled":false}}),
                )?,
            )?;
        }
        Ok(Self::Dedicated { path, _lock: lock })
    }
}
struct OwnedProcess {
    child: Child,
    profile: BrowserProfile,
    debug_port: Option<u16>,
    browser_endpoint: Option<String>,
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // Also close a browser that took over from an exited launcher. The
        // endpoint is read only from this session's explicitly owned profile.
        if close_private_browser(
            self.profile.path(),
            self.debug_port,
            self.browser_endpoint.as_deref(),
        ) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && self.child.try_wait().is_ok_and(|s| s.is_none()) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Kill only the process tree we launched, never an existing browser.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            if self.child.try_wait().is_ok_and(|status| status.is_none()) {
                let _ = Command::new("taskkill.exe")
                    .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                    .creation_flags(0x08000000)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
pub struct BrowserSession {
    cdp: Cdp,
    process: OwnedProcess,
    cancel: Cancel,
}
impl BrowserSession {
    pub fn launch(headless: bool, cancel: Cancel, log: Log) -> Result<Self> {
        cancel.check()?;
        let executable = browser_path()?;
        let profile = tempfile::Builder::new()
            .prefix("rust-downloaders-browser-")
            .tempdir()?;
        Self::launch_profile(
            executable,
            BrowserProfile::Temporary(profile),
            headless,
            true,
            cancel,
            log,
        )
    }
    pub fn launch_dedicated(
        directory: &Path,
        headless: bool,
        cancel: Cancel,
        log: Log,
    ) -> Result<Self> {
        cancel.check()?;
        let executable = browser_path()?;
        let chrome = executable
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("chrome.exe"));
        let profile =
            BrowserProfile::dedicated(directory, if chrome { "chrome" } else { "edge-guest" })?;
        let session =
            Self::launch_profile(executable, profile, headless, !chrome, cancel, log.clone())?;
        log(if chrome {
            "La sesión conserva cookies y caché en un perfil exclusivo de Chrome."
        } else {
            "Edge se usa como invitado; instala Chrome para conservar la sesión entre intentos."
        }
        .into());
        Ok(session)
    }
    fn launch_profile(
        executable: PathBuf,
        profile: BrowserProfile,
        headless: bool,
        guest: bool,
        cancel: Cancel,
        log: Log,
    ) -> Result<Self> {
        let allocation = TcpListener::bind(("127.0.0.1", 0))?;
        let port = allocation.local_addr()?.port();
        drop(allocation);
        let mut command = if guest {
            browser_command(&executable, profile.path(), headless, port)
        } else {
            browser_command_mode(&executable, profile.path(), headless, false, port)
        };
        let child = command.spawn()?;
        let mut process = OwnedProcess {
            child,
            profile,
            debug_port: Some(port),
            browser_endpoint: None,
        };
        let cdp = connect_browser(&mut process, &cancel)?;
        (log)("Sesión de navegador propia iniciada; no se usa tu perfil personal.".into());
        Ok(Self {
            cdp,
            process,
            cancel,
        })
    }
    pub fn navigate(&mut self, address: &str) -> Result<()> {
        let url = url::Url::parse(address)?;
        if url.scheme() != "https" {
            bail!("La página requiere HTTPS")
        }
        self.cdp.call("Page.enable", json!({}))?;
        let result = self.cdp.call("Page.navigate", json!({"url":address}))?;
        if result.get("errorText").is_some() {
            bail!("No se pudo cargar la página")
        }
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            self.cancel.check()?;
            let value = match self.evaluate("({url:location.href, ready:document.readyState})") {
                Ok(value) => value,
                Err(error) if error.downcast_ref::<BrowserDisconnected>().is_some() => {
                    return Err(error);
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if value["url"]
                .as_str()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|current| {
                    current.path() == url.path() && current.host_str() == url.host_str()
                })
                && matches!(value["ready"].as_str(), Some("interactive" | "complete"))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Tiempo de carga de la página agotado")
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    pub fn evaluate(&mut self, expression: &str) -> Result<Value> {
        self.cdp.evaluate(expression)
    }
    pub fn block_urls(&mut self, patterns: &[&str]) -> Result<()> {
        self.cdp.call("Network.enable", json!({}))?;
        self.cdp
            .call("Network.setBlockedURLs", json!({"urls":patterns}))?;
        Ok(())
    }
    pub fn bypass_document_cache(&mut self) -> Result<()> {
        self.cdp.call("Network.enable", json!({}))?;
        self.cdp
            .call("Network.setCacheDisabled", json!({"cacheDisabled":true}))?;
        self.cdp
            .call("Network.setBypassServiceWorker", json!({"bypass":true}))?;
        Ok(())
    }
    pub fn cookies(&mut self, address: &str) -> Result<Vec<Cookie>> {
        let result = self
            .cdp
            .call("Network.getCookies", json!({"urls":[address]}))?;
        Ok(serde_json::from_value(result["cookies"].clone())?)
    }
    pub fn profile_path(&self) -> &Path {
        self.process.profile.path()
    }
}
fn browser_path() -> Result<PathBuf> {
    let roots: Vec<_> = ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    browser_paths_from(&roots)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Instala Edge o Chrome"))
}
fn browser_paths_from(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for relative in [
        "Google/Chrome/Application/chrome.exe",
        "Microsoft/Edge/Application/msedge.exe",
    ] {
        for root in roots {
            let path = root.join(relative);
            if path.is_file() && !found.contains(&path) {
                found.push(path)
            }
        }
    }
    found
}

fn browser_command(executable: &Path, profile: &Path, headless: bool, port: u16) -> Command {
    browser_command_mode(executable, profile, headless, true, port)
}
fn browser_command_mode(
    executable: &Path,
    profile: &Path,
    headless: bool,
    guest: bool,
    port: u16,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .args([
            "--remote-debugging-address=127.0.0.1",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-sync",
            "--disable-background-networking",
            "--disable-features=msEdgeSidebarV2",
            "about:blank",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if headless {
        command.arg("--headless=new");
    }
    if guest {
        command.arg("--guest");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

fn connect_browser(process: &mut OwnedProcess, cancel: &Cancel) -> Result<Cdp> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + Duration::from_secs(25);
    let cdp = loop {
        cancel.check()?;
        if Instant::now() >= deadline {
            bail!("El navegador no abrió su sesión local")
        }
        if let Some(port) = process.debug_port
            && process.browser_endpoint.is_none()
        {
            process.browser_endpoint =
                private_browser_endpoint(process.profile.path(), port, cancel)?;
        }
        let port = process.debug_port.or_else(|| {
            std::fs::read_to_string(process.profile.path().join("DevToolsActivePort"))
                .ok()?
                .lines()
                .next()?
                .parse()
                .ok()
        });
        if let Some(port) = port
            && (process.debug_port.is_none() || process.browser_endpoint.is_some())
            && let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/json/list"))
                .send()
            && let Ok(pages) = response.json::<Vec<Value>>()
            && let Some(address) = pages
                .iter()
                .find(|v| v["type"] == "page")
                .and_then(|p| p["webSocketDebuggerUrl"].as_str())
        {
            break Cdp::connect(address, cancel.clone(), Duration::from_secs(60))?;
        }
        // A successful browser launcher can exit before its private browser is ready.
        // Its exit alone is not evidence that the isolated CDP endpoint is gone.
        if let Some(status) = process.child.try_wait()?
            && !status.success()
        {
            bail!("El navegador terminó al iniciar: {status}")
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(cdp)
}

fn owns_profile(command: &str, profile: &Path) -> bool {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in command.chars() {
        if character == '"' {
            quoted = !quoted
        } else if character.is_whitespace() && !quoted {
            if !current.is_empty() {
                arguments.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character)
        }
    }
    if quoted {
        return false;
    }
    if !current.is_empty() {
        arguments.push(current)
    }
    let expected = profile.to_string_lossy().replace('/', "\\").to_lowercase();
    arguments
        .iter()
        .filter_map(|argument| argument.strip_prefix("--user-data-dir="))
        .any(|value| value.replace('/', "\\").to_lowercase() == expected)
}
fn private_browser_endpoint(profile: &Path, port: u16, cancel: &Cancel) -> Result<Option<String>> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(500))
        .build()?;
    let Ok(response) = client
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()
    else {
        return Ok(None);
    };
    let Ok(value) = response.json::<Value>() else {
        return Ok(None);
    };
    let Some(address) = value["webSocketDebuggerUrl"].as_str() else {
        return Ok(None);
    };
    let url = url::Url::parse(address)?;
    if url.scheme() != "ws"
        || url.host_str() != Some("127.0.0.1")
        || url.port() != Some(port)
        || !url.username().is_empty()
        || url.password().is_some()
        || !url.path().starts_with("/devtools/browser/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("El navegador devolvio una conexion local no valida")
    }
    let mut browser = Cdp::connect(address, cancel.clone(), Duration::from_secs(2))?;
    let info = browser.call("SystemInfo.getInfo", json!({}))?;
    if !info["commandLine"]
        .as_str()
        .is_some_and(|c| owns_profile(c, profile))
    {
        bail!("El puerto no pertenece al perfil exclusivo del descargador")
    }
    Ok(Some(address.into()))
}
fn close_private_browser(profile: &Path, port: Option<u16>, known_endpoint: Option<&str>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    let endpoint = loop {
        if let Some(endpoint) = known_endpoint {
            break endpoint.to_owned();
        }
        if let Some(port) = port {
            match private_browser_endpoint(profile, port, &Cancel::default()) {
                Ok(Some(endpoint)) => break endpoint,
                Err(_) => return false,
                _ => {}
            }
        } else if let Ok(text) = std::fs::read_to_string(profile.join("DevToolsActivePort")) {
            let mut lines = text.lines();
            if let Some(port) = lines
                .next()
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|p| *p > 0)
                && let Some(path) = lines.next().filter(|p| {
                    p.starts_with("/devtools/browser/") && !p.contains(['?', '#', '\\', ' '])
                })
            {
                break format!("ws://127.0.0.1:{port}{path}");
            }
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let Ok(url) = url::Url::parse(&endpoint) else {
        return false;
    };
    let Some(port) = url.port() else { return false };
    if url.scheme() != "ws" || url.host_str() != Some("127.0.0.1") {
        return false;
    }
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(200)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let Ok((mut socket, _)) = tungstenite::client(endpoint, stream) else {
        return false;
    };
    socket
        .send(Message::Text(
            json!({"id":1,"method":"Browser.close","params":{}})
                .to_string()
                .into(),
        ))
        .is_ok()
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn guest_launch_prevents_sync_and_keeps_an_independent_profile() {
        let profile = tempfile::tempdir().unwrap();
        let command = browser_command(Path::new("chrome.exe"), profile.path(), false, 9227);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--guest".to_owned()));
        assert!(args.contains(&"--disable-sync".to_owned()));
        assert!(args.contains(&format!("--user-data-dir={}", profile.path().display())));
        assert!(!args.iter().any(|a| a.starts_with("--profile-directory=")));
    }

    #[test]
    fn chrome_is_preferred_even_when_edge_is_in_an_earlier_install_root() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let edge = first.join("Microsoft/Edge/Application/msedge.exe");
        let chrome = second.join("Google/Chrome/Application/chrome.exe");
        for path in [&edge, &chrome] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, []).unwrap();
        }
        assert_eq!(browser_paths_from(&[first, second]), vec![chrome, edge]);
    }

    #[test]
    fn debug_endpoint_ownership_requires_the_exact_profile_argument() {
        let path = Path::new(r"C:\temporary\own profile");
        assert!(owns_profile(
            r#"chrome.exe --user-data-dir="C:\temporary\own profile" --disable-sync"#,
            path
        ));
        assert!(owns_profile(
            r#"chrome.exe "--user-data-dir=C:\temporary\own profile" --disable-sync"#,
            path
        ));
        assert!(!owns_profile(
            r"chrome.exe --user-data-dir=C:\temporary\own profile --disable-sync",
            path
        ));
        assert!(!owns_profile(
            r#"chrome.exe --user-data-dir="C:\temporary\own profile-foreign" --disable-sync"#,
            path
        ));
        assert!(!owns_profile(
            r#"chrome.exe --other=" --user-data-dir=C:\temporary\own profile" --disable-sync"#,
            path
        ));
    }

    #[test]
    fn an_exited_launcher_does_not_hide_a_delayed_private_debug_endpoint() {
        let profile = tempfile::tempdir().unwrap();
        let profile_path = profile.path().to_path_buf();
        #[cfg(windows)]
        let mut launcher = Command::new("cmd.exe");
        #[cfg(windows)]
        launcher.args(["/c", "exit", "0"]);
        #[cfg(not(windows))]
        let mut launcher = Command::new("sh");
        #[cfg(not(windows))]
        launcher.args(["-c", "exit 0"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            launcher.creation_flags(0x08000000);
        }
        let child = launcher
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut process = OwnedProcess {
            child,
            profile: BrowserProfile::Temporary(profile),
            debug_port: None,
            browser_endpoint: None,
        };
        process.child.wait().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            std::fs::write(
                profile_path.join("DevToolsActivePort"),
                format!("{port}\n/private-browser"),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut served = 0;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        if served == 0 {
                            let mut request = [0u8; 2048];
                            assert!(socket.read(&mut request).unwrap() > 0);
                            let body=json!([{"type":"page","url":"about:blank","webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/page/private")}]).to_string();
                            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
                            served += 1;
                        } else {
                            tungstenite::accept(socket).unwrap();
                            return;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        let result = connect_browser(&mut process, &Cancel::default());
        task.join().unwrap();
        assert!(
            result.is_ok(),
            "A private endpoint can become ready after its launcher exits"
        );
    }

    #[test]
    fn cancellation_during_launcher_handoff_closes_only_the_owned_browser() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().to_path_buf();
        #[cfg(windows)]
        let mut command = Command::new("cmd.exe");
        #[cfg(windows)]
        command.args(["/c", "exit", "0"]);
        #[cfg(not(windows))]
        let mut command = Command::new("sh");
        #[cfg(not(windows))]
        command.args(["-c", "exit 0"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let child = command.spawn().unwrap();
        let mut process = OwnedProcess {
            child,
            profile: BrowserProfile::Temporary(profile),
            debug_port: None,
            browser_endpoint: None,
        };
        process.child.wait().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let observed = closed.clone();
        let writing_path = path.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            if std::fs::write(
                writing_path.join("DevToolsActivePort"),
                format!("{port}\n/devtools/browser/owned-private"),
            )
            .is_err()
            {
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok((socket, _)) = listener.accept() {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut socket = tungstenite::accept(socket).unwrap();
                    let value: Value =
                        serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                    observed.store(value["method"] == "Browser.close", Ordering::SeqCst);
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let cancel = Cancel::default();
        cancel.cancel();
        assert!(connect_browser(&mut process, &cancel).is_err());
        drop(process);
        server.join().unwrap();
        assert!(
            closed.load(Ordering::SeqCst),
            "cancelled handoffs must close their private endpoint"
        );
        assert!(!path.exists());
    }

    #[test]
    fn dedicated_profiles_reject_foreign_data_and_lock_concurrent_sessions() {
        let root = tempfile::tempdir().unwrap();
        let foreign = root.path().join("personal");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("sentinel"), b"untouched").unwrap();
        assert!(BrowserProfile::dedicated(&foreign, "chrome").is_err());
        assert_eq!(
            std::fs::read(foreign.join("sentinel")).unwrap(),
            b"untouched"
        );
        let own = root.path().join("owned");
        let profile = BrowserProfile::dedicated(&own, "chrome").unwrap();
        assert!(BrowserProfile::dedicated(&own, "chrome").is_err());
        drop(profile);
        assert!(BrowserProfile::dedicated(&own, "chrome").is_ok());
    }

    #[test]
    #[ignore = "checks persistent synthetic cookies in an installed Chrome profile"]
    fn dedicated_chrome_cookie_jar_survives_a_clean_restart() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("cookie-session");
        {
            let mut browser = BrowserSession::launch_dedicated(
                &profile,
                true,
                Cancel::default(),
                std::sync::Arc::new(|_| {}),
            )
            .unwrap();
            assert_eq!(browser.profile_path().file_name().unwrap(), "chrome");
            let expires = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600;
            let result=browser.cdp.call("Network.setCookie",json!({"name":"rust_test_session","value":"synthetic_fixture","url":"https://example.invalid/","secure":true,"expires":expires})).unwrap();
            assert_eq!(result["success"], true);
        }
        let mut browser = BrowserSession::launch_dedicated(
            &profile,
            true,
            Cancel::default(),
            std::sync::Arc::new(|_| {}),
        )
        .unwrap();
        let cookies = browser.cookies("https://example.invalid/").unwrap();
        assert!(cookies.iter().any(
            |cookie| cookie.name == "rust_test_session" && cookie.value == "synthetic_fixture"
        ));
    }
}
