//! Health state and SSO login for each CLI.
//!
//! A CLI with a `[health]` section is checked every `every_secs`. Its state
//! is `ready`, `needs_login` or `down`. The `[login]` command is expected to
//! print an SSO link (device-code or browser flow) and then block until the
//! user finishes signing in. [`StateManager::start_login`] returns as soon as
//! the link appears; the process keeps running in the background, and when
//! it exits the health check runs again.
//!
//! Every command here comes from the config file: callers only pick a CLI
//! by name.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;

use crate::runner::Program;

/// Max lines of login output kept for the message shown to the user.
const LOGIN_OUTPUT_LINES: usize = 10;
/// Max characters kept per output line.
const MAX_LINE_CHARS: usize = 300;

/// Health check and optional login of one CLI. Commands are argv after
/// the program's binary (no shell).
#[derive(Debug, Clone)]
pub struct StateDef {
    pub name: String,
    pub program: Arc<Program>,
    /// Exit 0 (plus `ok_if`, if set) = ready.
    pub health: Vec<String>,
    /// Output substrings meaning "ready". Empty = exit code 0 is enough.
    pub ok_if: Vec<String>,
    /// Output substrings meaning "login required". Checked first.
    pub needs_login_if: Vec<String>,
    /// Optional command whose first output line is reported as `expires_at`.
    pub expiry: Option<Vec<String>>,
    pub every_secs: u64,
    pub login: Option<LoginDef>,
}

#[derive(Debug, Clone)]
pub struct LoginDef {
    /// Must print the SSO link, then wait for the user.
    pub run: Vec<String>,
    /// Prefixes that mark the login link in the login output.
    pub url_prefixes: Vec<String>,
    /// How long to wait for the login link to appear.
    pub url_wait_secs: u64,
    /// How long the login process may wait for the user before it is killed.
    pub timeout_secs: u64,
    /// Local ports the login command listens on for its browser redirect.
    /// Non-empty enables paste-back ([`StateManager::complete_login`]).
    pub callback_ports: Vec<u16>,
}

impl StateDef {
    /// Whether this output (from any command) says a login is needed.
    pub fn output_needs_login(&self, output: &str) -> bool {
        self.needs_login_if.iter().any(|p| output.contains(p.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CliState {
    /// Not checked yet.
    Unknown,
    Ready,
    NeedsLogin,
    /// Health check failed for another reason.
    Down,
}

impl std::fmt::Display for CliState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CliState::Unknown => "unknown",
            CliState::Ready => "ready",
            CliState::NeedsLogin => "needs_login",
            CliState::Down => "down",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CliStatus {
    pub name: String,
    pub state: CliState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<PendingLogin>,
}

/// A login that is waiting for the user to finish SSO.
#[derive(Debug, Clone, Serialize)]
pub struct PendingLogin {
    pub url: String,
    /// Last lines of the login output (may contain a device code).
    pub message: String,
    pub started_at_unix: u64,
    pub timeout_at_unix: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum LoginOutcome {
    AlreadyReady { status: CliStatus },
    Pending { login: PendingLogin },
}

#[derive(Debug, Clone)]
pub enum StateEvent {
    StateChanged {
        name: String,
        from: CliState,
        to: CliState,
    },
}

#[derive(Debug, Clone)]
pub struct StateError(pub String);

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateError {}

struct Entry {
    def: StateDef,
    status: Mutex<CliStatus>,
    /// Serializes login starts so only one login process runs per CLI.
    login_lock: tokio::sync::Mutex<()>,
}

pub struct StateManager {
    entries: HashMap<String, Arc<Entry>>,
    events: broadcast::Sender<StateEvent>,
}

impl StateManager {
    pub fn new(defs: Vec<StateDef>) -> Arc<Self> {
        let entries = defs
            .into_iter()
            .map(|def| {
                let status = CliStatus {
                    name: def.name.clone(),
                    state: CliState::Unknown,
                    detail: None,
                    expires_at: None,
                    checked_at_unix: None,
                    login: None,
                };
                let entry = Entry {
                    def,
                    status: Mutex::new(status),
                    login_lock: tokio::sync::Mutex::new(()),
                };
                (entry.def.name.clone(), Arc::new(entry))
            })
            .collect();
        let (events, _) = broadcast::channel(16);
        Arc::new(Self { entries, events })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured CLI names (with a health check), sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.keys().cloned().collect();
        names.sort();
        names
    }

    /// Last known status (no command is run).
    pub fn status(&self, name: &str) -> Option<CliStatus> {
        self.entries.get(name).map(|e| e.status.lock().unwrap().clone())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StateEvent> {
        self.events.subscribe()
    }

    /// Run the health (and expiry) commands now and update the status.
    pub async fn refresh(&self, name: &str) -> Result<CliStatus, StateError> {
        let entry = self.entry(name)?;
        let def = &entry.def;
        let timeout = Duration::from_secs(def.program.timeout_secs);

        let (state, detail) = match def.program.run(&def.health, timeout).await {
            Ok(out) => classify(def, out.success, &out.combined()),
            Err(e) => (CliState::Down, Some(e)),
        };

        let expires_at = match &def.expiry {
            Some(cmd) => match def.program.run(cmd, timeout).await {
                Ok(out) if out.success => out
                    .stdout
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(|l| truncate(l, 200)),
                _ => None,
            },
            None => None,
        };

        let (from, status) = {
            let mut status = entry.status.lock().unwrap();
            let from = status.state;
            status.state = state;
            status.detail = detail;
            status.expires_at = expires_at;
            status.checked_at_unix = Some(now_unix());
            (from, status.clone())
        };

        if from != state {
            tracing::info!(cli = %name, %from, to = %state, "state changed");
            let _ = self.events.send(StateEvent::StateChanged {
                name: name.to_string(),
                from,
                to: state,
            });
        }
        Ok(status)
    }

    /// Re-check every CLI on its `every_secs`, starting now.
    pub fn start_watchers(self: &Arc<Self>) {
        for (name, entry) in &self.entries {
            let manager = Arc::clone(self);
            let name = name.clone();
            let period = Duration::from_secs(entry.def.every_secs.max(5));
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(period);
                interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    if let Err(e) = manager.refresh(&name).await {
                        tracing::warn!(cli = %name, %e, "health check failed");
                    }
                }
            });
        }
    }

    /// Start the login command and return its SSO link.
    ///
    /// If a login is already waiting for the user, its link is returned
    /// instead of starting a second process. If the CLI is already
    /// ready, nothing is run.
    pub async fn start_login(self: &Arc<Self>, name: &str) -> Result<LoginOutcome, StateError> {
        let entry = self.entry(name)?;
        let _guard = entry.login_lock.lock().await;

        if let Some(login) = entry.status.lock().unwrap().login.clone() {
            if login.timeout_at_unix > now_unix() {
                return Ok(LoginOutcome::Pending { login });
            }
        }

        let status = self.refresh(name).await?;
        if status.state == CliState::Ready {
            return Ok(LoginOutcome::AlreadyReady { status });
        }

        let def = entry
            .def
            .login
            .as_ref()
            .ok_or_else(|| StateError(format!("'{name}' has no [login] configured")))?;
        let mut child = entry
            .def
            .program
            .command(&def.run)
            .spawn()
            .map_err(|e| StateError(format!("failed to start login for '{name}': {e}")))?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        forward_lines(child.stdout.take(), tx.clone());
        forward_lines(child.stderr.take(), tx);

        let mut tail: Vec<String> = Vec::new();
        let url_wait = tokio::time::sleep(Duration::from_secs(def.url_wait_secs));
        tokio::pin!(url_wait);

        let url = loop {
            tokio::select! {
                line = rx.recv() => match line {
                    Some(line) => {
                        push_tail(&mut tail, &line);
                        if let Some(url) = extract_url(&line, &def.url_prefixes) {
                            break url;
                        }
                    }
                    None => {
                        // Output closed before any link: the command finished.
                        let _ = child.wait().await;
                        let status = self.refresh(name).await?;
                        if status.state == CliState::Ready {
                            return Ok(LoginOutcome::AlreadyReady { status });
                        }
                        return Err(StateError(format!(
                            "login command for '{name}' exited without printing a login link. Output:\n{}",
                            tail.join("\n")
                        )));
                    }
                },
                _ = &mut url_wait => {
                    let _ = child.kill().await;
                    return Err(StateError(format!(
                        "no login link from '{name}' within {}s. Output:\n{}",
                        def.url_wait_secs,
                        tail.join("\n")
                    )));
                }
            }
        };

        // Keep a few more lines that arrive right after the link (device codes
        // are often printed on the next line).
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            push_tail(&mut tail, &line);
        }

        let started = now_unix();
        let login = PendingLogin {
            url,
            message: tail.join("\n"),
            started_at_unix: started,
            timeout_at_unix: started + def.timeout_secs,
        };
        entry.status.lock().unwrap().login = Some(login.clone());
        tracing::info!(cli = %name, url = %login.url, "login link issued; waiting for user");

        let manager = Arc::clone(self);
        let name = name.to_string();
        let login_timeout = Duration::from_secs(def.timeout_secs);
        tokio::spawn(async move {
            let finished = tokio::time::timeout(login_timeout, async {
                while rx.recv().await.is_some() {}
                child.wait().await
            })
            .await;
            match finished {
                Ok(Ok(exit)) => tracing::info!(cli = %name, %exit, "login command finished"),
                Ok(Err(e)) => tracing::warn!(cli = %name, %e, "login command failed"),
                Err(_) => {
                    tracing::warn!(cli = %name, "login timed out; stopping login command");
                    let _ = child.kill().await;
                }
            }
            if let Ok(entry) = manager.entry(&name) {
                entry.status.lock().unwrap().login = None;
            }
            if let Err(e) = manager.refresh(&name).await {
                tracing::warn!(cli = %name, %e, "health check after login failed");
            }
        });

        Ok(LoginOutcome::Pending { login })
    }

    /// Whether this CLI accepts a pasted-back localhost redirect.
    pub fn supports_paste_back(&self, name: &str) -> bool {
        self.entries
            .get(name)
            .is_some_and(|e| e.def.login.as_ref().is_some_and(|l| !l.callback_ports.is_empty()))
    }

    /// Finish a browser-redirect login whose redirect landed on the user's
    /// own machine: forward the pasted `http://localhost:<port>/...` address
    /// to the login command listening on this host.
    ///
    /// Only accepted while a login is pending, and only for loopback hosts
    /// on one of the configured `callback_ports`.
    pub async fn complete_login(&self, name: &str, callback_url: &str) -> Result<String, StateError> {
        let entry = self.entry(name)?;
        let ports = match &entry.def.login {
            Some(l) if !l.callback_ports.is_empty() => l.callback_ports.clone(),
            _ => return Err(StateError(format!("paste-back is not enabled for '{name}'"))),
        };
        if entry.status.lock().unwrap().login.is_none() {
            return Err(StateError(format!(
                "no login is waiting for '{name}'; start one with the login tool first"
            )));
        }
        let target = parse_callback_url(callback_url, &ports)?;
        let status = tokio::time::timeout(Duration::from_secs(10), forward_callback(&target))
            .await
            .map_err(|_| StateError("timed out forwarding the callback".into()))??;
        tracing::info!(cli = %name, port = target.port, %status, "login callback forwarded");
        Ok(status)
    }

    fn entry(&self, name: &str) -> Result<Arc<Entry>, StateError> {
        self.entries
            .get(name)
            .cloned()
            .ok_or_else(|| StateError(format!("unknown CLI '{name}' (or it has no [health] check)")))
    }
}

fn classify(def: &StateDef, success: bool, output: &str) -> (CliState, Option<String>) {
    let last_line = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .map(|l| truncate(l, MAX_LINE_CHARS));

    if let Some(hit) = def.needs_login_if.iter().find(|p| output.contains(p.as_str())) {
        return (CliState::NeedsLogin, Some(format!("output contains '{hit}'")));
    }
    let healthy = success && (def.ok_if.is_empty() || def.ok_if.iter().any(|p| output.contains(p.as_str())));
    if healthy {
        (CliState::Ready, None)
    } else {
        (CliState::Down, last_line)
    }
}

/// First token starting with one of `prefixes`, with trailing punctuation removed.
fn extract_url(line: &str, prefixes: &[String]) -> Option<String> {
    let start = prefixes.iter().filter_map(|p| line.find(p.as_str())).min()?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>')
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')', ']', ';']);
    (!url.is_empty()).then(|| url.to_string())
}

#[derive(Debug, PartialEq)]
struct CallbackTarget {
    ipv6: bool,
    port: u16,
    path_and_query: String,
}

fn parse_callback_url(url: &str, allowed_ports: &[u16]) -> Result<CallbackTarget, StateError> {
    let bad = |why: &str| StateError(format!("invalid callback address: {why}"));
    let url = url.trim();
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad("contains whitespace or control characters"));
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| bad("must start with http://"))?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    if authority.contains('@') {
        return Err(bad("credentials are not allowed"));
    }
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| bad("missing port"))?;
    let ipv6 = match host {
        "localhost" | "127.0.0.1" => false,
        "[::1]" => true,
        _ => return Err(bad("host must be localhost")),
    };
    let port: u16 = port.parse().map_err(|_| bad("bad port"))?;
    if !allowed_ports.contains(&port) {
        return Err(bad(&format!("port {port} is not an allowed callback port")));
    }
    let path_and_query = match path {
        "" => "/".to_string(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_string(),
    };
    Ok(CallbackTarget {
        ipv6,
        port,
        path_and_query,
    })
}

/// Minimal HTTP/1.1 GET to the loopback listener; returns the status line.
async fn forward_callback(target: &CallbackTarget) -> Result<String, StateError> {
    let addr = if target.ipv6 {
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, target.port))
    } else {
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, target.port))
    };
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| StateError(format!("nothing is listening on port {}: {e}", target.port)))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        target.path_and_query, target.port
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| StateError(format!("failed to send callback: {e}")))?;
    let mut buf = vec![0u8; 1024];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| StateError(format!("failed to read callback response: {e}")))?;
    let head = String::from_utf8_lossy(&buf[..n]);
    Ok(head.lines().next().unwrap_or("").trim().to_string())
}

fn forward_lines<R>(reader: Option<R>, tx: mpsc::UnboundedSender<String>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let Some(reader) = reader else { return };
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

fn push_tail(tail: &mut Vec<String>, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    tail.push(truncate(line, MAX_LINE_CHARS));
    if tail.len() > LOGIN_OUTPUT_LINES {
        tail.remove(0);
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Test helper: a CLI whose health and login are `sh -c` scripts.
#[cfg(test)]
pub(crate) fn sh_def(name: &str, health: &str, login: &str) -> StateDef {
    let program = Arc::new(Program {
        cli: name.into(),
        bin: "/bin/sh".into(),
        env: std::collections::BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
        cwd: None,
        timeout_secs: 5,
        max_output_bytes: 64 * 1024,
    });
    StateDef {
        name: name.into(),
        program,
        health: vec!["-c".into(), health.into()],
        ok_if: vec!["Management: Connected".into()],
        needs_login_if: vec!["NeedsLogin".into()],
        expiry: None,
        every_secs: 60,
        login: Some(LoginDef {
            run: vec!["-c".into(), login.into()],
            url_prefixes: vec!["https://".into()],
            url_wait_secs: 5,
            timeout_secs: 5,
            callback_ports: vec![],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(health: &str, login: &str) -> StateDef {
        sh_def("netbird", health, login)
    }

    #[test]
    fn callback_url_must_be_loopback_on_allowed_port() {
        let ports = [53000];
        assert_eq!(
            parse_callback_url("http://localhost:53000/?code=abc&state=xyz", &ports).unwrap(),
            CallbackTarget {
                ipv6: false,
                port: 53000,
                path_and_query: "/?code=abc&state=xyz".into()
            }
        );
        assert_eq!(
            parse_callback_url("http://127.0.0.1:53000?code=1", &ports)
                .unwrap()
                .path_and_query,
            "/?code=1"
        );
        for bad in [
            "https://localhost:53000/",
            "http://evil.example:53000/",
            "http://localhost:22/",
            "http://user@localhost:53000/",
            "http://localhost:53000/a b",
            "http://localhost.evil.example:53000/",
        ] {
            assert!(parse_callback_url(bad, &ports).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn complete_login_forwards_to_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let mut d = def("echo NeedsLogin", "echo 'open https://idp.example/auth'; sleep 3");
        d.login.as_mut().unwrap().callback_ports = vec![port];
        let m = StateManager::new(vec![d]);

        // Rejected while no login is pending.
        let url = format!("http://localhost:{port}/?code=abc");
        assert!(m.complete_login("netbird", &url).await.is_err());

        m.start_login("netbird").await.unwrap();
        let status = m.complete_login("netbird", &url).await.unwrap();
        assert!(status.contains("200"), "{status}");
        assert!(server.await.unwrap().starts_with("GET /?code=abc HTTP/1.1"));
    }

    #[test]
    fn extract_url_strips_trailing_punctuation() {
        let prefixes = vec!["https://".to_string()];
        assert_eq!(
            extract_url(
                "use this URL to log in: https://idp.example/device?code=AB-CD.",
                &prefixes
            ),
            Some("https://idp.example/device?code=AB-CD".into())
        );
        assert_eq!(extract_url("no link here", &prefixes), None);
    }

    #[tokio::test]
    async fn refresh_classifies_states() {
        let m = StateManager::new(vec![def("echo 'Management: Connected'", "true")]);
        assert_eq!(m.refresh("netbird").await.unwrap().state, CliState::Ready);

        let m = StateManager::new(vec![def("echo 'Daemon status: NeedsLogin'; exit 1", "true")]);
        assert_eq!(m.refresh("netbird").await.unwrap().state, CliState::NeedsLogin);

        let m = StateManager::new(vec![def("echo 'Management: Disconnected'", "true")]);
        assert_eq!(m.refresh("netbird").await.unwrap().state, CliState::Down);
    }

    #[tokio::test]
    async fn login_returns_link_then_reconnects_in_background() {
        let dir = std::env::temp_dir().join(format!("mcp-cli-state-{}", now_unix_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let flag = dir.join("logged-in");
        let flag = flag.display();

        let m = StateManager::new(vec![def(
            &format!("test -f {flag} && echo 'Management: Connected' || echo NeedsLogin"),
            &format!("echo 'Please log in: https://idp.example/device'; echo 'Code: ABCD-1234'; sleep 1; touch {flag}"),
        )]);
        let mut events = m.subscribe();

        let LoginOutcome::Pending { login } = m.start_login("netbird").await.unwrap() else {
            panic!("expected pending login");
        };
        assert_eq!(login.url, "https://idp.example/device");
        assert!(login.message.contains("ABCD-1234"));

        // A second call reuses the pending login instead of spawning again.
        let LoginOutcome::Pending { login: again } = m.start_login("netbird").await.unwrap() else {
            panic!("expected pending login");
        };
        assert_eq!(again.url, login.url);

        let event = loop {
            let StateEvent::StateChanged { to, .. } =
                tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if to == CliState::Ready {
                break to;
            }
        };
        assert_eq!(event, CliState::Ready);
        assert!(m.status("netbird").unwrap().login.is_none());
    }

    #[tokio::test]
    async fn login_skipped_when_already_connected() {
        let m = StateManager::new(vec![def("echo 'Management: Connected'", "exit 1")]);
        assert!(matches!(
            m.start_login("netbird").await.unwrap(),
            LoginOutcome::AlreadyReady { .. }
        ));
    }

    #[tokio::test]
    async fn unknown_cli_is_rejected() {
        let m = StateManager::new(vec![def("true", "true")]);
        assert!(m.start_login("other").await.is_err());
    }

    fn now_unix_nanos() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }
}
