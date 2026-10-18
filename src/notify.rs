//! `[[notify]]` alerts for CLI state changes (generic webhook, Slack, ntfy).
//!
//! Alerts go only to destinations in the config file, so this is the safe
//! place to send a login link: with `auto = true` in a `[login]`, the server
//! starts the login as soon as it notices the expiry and includes the link.

use std::sync::Arc;
use std::time::Duration;

use crate::state::{CliState, LoginOutcome, StateEvent, StateManager};
use serde::Deserialize;

const ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyKind {
    Webhook,
    Slack,
    Ntfy,
}

#[derive(Debug, Clone)]
pub struct NotifyTarget {
    pub kind: NotifyKind,
    pub url: String,
    /// States that trigger this alert (`needs_login`, `down`, `ready`).
    pub on: Vec<String>,
    /// Only these CLIs. Empty = all.
    pub clis: Vec<String>,
}

/// One alert, rendered per destination.
#[derive(Debug, Clone)]
pub struct Alert {
    pub cli: String,
    pub from: CliState,
    pub to: CliState,
    pub login_url: Option<String>,
    pub login_message: Option<String>,
}

impl Alert {
    pub fn text(&self) -> String {
        let mut text = match self.to {
            CliState::NeedsLogin => format!("'{}' needs login.", self.cli),
            CliState::Down => format!("'{}' is down.", self.cli),
            CliState::Ready => format!("'{}' is ready again.", self.cli),
            CliState::Unknown => format!("'{}' state is unknown.", self.cli),
        };
        match (&self.login_url, self.to) {
            (Some(url), _) => {
                text.push_str(&format!("\nSign in: {url}"));
                if let Some(msg) = &self.login_message {
                    text.push_str(&format!("\n\n{msg}"));
                }
            }
            (None, CliState::NeedsLogin) => text.push_str(&format!(
                "\nRun 'mcp-cli login {0}' or ask an agent to call {0}_login.",
                self.cli
            )),
            _ => {}
        }
        text
    }
}

/// Watch state events: auto-start logins where configured, and send alerts.
pub fn spawn_notifier(states: Arc<StateManager>, targets: Vec<NotifyTarget>, auto_login: Vec<String>) {
    if states.is_empty() || (targets.is_empty() && auto_login.is_empty()) {
        return;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client");
    let mut events = states.subscribe();
    tokio::spawn(async move {
        loop {
            let StateEvent::StateChanged { name, from, to } = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            // Startup check of a healthy CLI is not news.
            if from == CliState::Unknown && to == CliState::Ready {
                continue;
            }
            let mut alert = Alert {
                cli: name.clone(),
                from,
                to,
                login_url: None,
                login_message: None,
            };
            if to == CliState::NeedsLogin && auto_login.contains(&name) {
                match states.start_login(&name).await {
                    Ok(LoginOutcome::Pending { login }) => {
                        alert.login_url = Some(login.url);
                        alert.login_message = Some(login.message);
                    }
                    Ok(LoginOutcome::AlreadyReady { .. }) => continue,
                    Err(e) => tracing::warn!(cli = %name, %e, "auto login failed"),
                }
            }
            let wanted = |t: &&NotifyTarget| {
                t.on.iter().any(|s| s == &to.to_string()) && (t.clis.is_empty() || t.clis.contains(&name))
            };
            for target in targets.iter().filter(wanted) {
                send(&client, target, &alert).await;
            }
        }
    });
}

async fn send(client: &reqwest::Client, target: &NotifyTarget, alert: &Alert) {
    for attempt in 1..=ATTEMPTS {
        match build(client, target, alert).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(cli = %alert.cli, kind = ?target.kind, "alert sent");
                return;
            }
            Ok(resp) => tracing::warn!(cli = %alert.cli, status = %resp.status(), attempt, "alert rejected"),
            Err(e) => tracing::warn!(cli = %alert.cli, error = %e.without_url(), attempt, "alert failed"),
        }
        tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
    }
}

fn build(client: &reqwest::Client, target: &NotifyTarget, alert: &Alert) -> reqwest::RequestBuilder {
    let text = alert.text();
    match target.kind {
        NotifyKind::Webhook => client.post(&target.url).json(&serde_json::json!({
            "cli": alert.cli,
            "from": alert.from,
            "to": alert.to,
            "login_url": alert.login_url,
            "message": text,
        })),
        NotifyKind::Slack => client
            .post(&target.url)
            .json(&serde_json::json!({ "text": text })),
        NotifyKind::Ntfy => {
            let mut req = client
                .post(&target.url)
                .header("Title", format!("mcp-cli: {} {}", alert.cli, alert.to))
                .header(
                    "Tags",
                    if alert.to == CliState::Ready {
                        "white_check_mark"
                    } else {
                        "warning"
                    },
                )
                .body(text);
            if let Some(url) = &alert.login_url {
                req = req.header("Click", url);
            }
            req
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn alert(url: Option<&str>) -> Alert {
        Alert {
            cli: "netbird".into(),
            from: CliState::Ready,
            to: CliState::NeedsLogin,
            login_url: url.map(String::from),
            login_message: url.map(|u| format!("use this URL to log in: {u}\nCode: ABCD")),
        }
    }

    #[test]
    fn text_includes_link_or_instructions() {
        assert!(alert(None).text().contains("mcp-cli login netbird"));
        let with_link = alert(Some("https://idp.example/device")).text();
        assert!(with_link.contains("Sign in: https://idp.example/device"));
        assert!(with_link.contains("Code: ABCD"));
    }

    #[tokio::test]
    async fn webhook_posts_json() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let target = NotifyTarget {
            kind: NotifyKind::Webhook,
            url: format!("http://{addr}/hook"),
            on: vec!["needs_login".into()],
            clis: vec![],
        };
        let client = reqwest::Client::new();
        send(&client, &target, &alert(Some("https://idp.example/device"))).await;
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /hook"));
        assert!(request.contains("\"to\":\"needs_login\""));
        assert!(request.contains("\"login_url\":\"https://idp.example/device\""));
    }
}
