# NetBird VPN

NetBird's SSO session expires (24h by default). When it does, `netbird status`
prints `Daemon status: NeedsLogin`, and anything that needs the tunnel stops
working. `mcp-cli` notices this, gives the agent the login link, and lets
gated commands through again once you've signed in.

## Setup

```sh
cp examples/netbird.toml ~/.config/mcp-cli/config.toml
mcp-cli check
mcp-cli status        # netbird  ready | needs_login | down
```

Run `mcp-cli` as the same OS user that normally runs `netbird`. It talks to
the daemon socket (`unix:///var/run/netbird.sock`).

## What the agent sees

| Tool | Does |
|---|---|
| `netbird_status` | `netbird status` now; state + any login waiting |
| `netbird_login` | `netbird up --no-browser`; returns the SSO link |
| `netbird_login_complete` | paste-back for `http://localhost:53000/...` redirects |
| `netbird_peers`, `netbird_networks`, ... | the commands you listed |

Flow when the session has expired:

1. The agent calls `netbird_peers` and gets `LOGIN_REQUIRED`. Newer clients
   instead get a login prompt with the link.
2. The agent calls `netbird_login` and shows you the link.
3. You sign in. `netbird up` exits, the health check turns `ready`, and the
   agent retries.

Or log in yourself from a terminal: `mcp-cli login netbird`.

## Localhost redirect (self-hosted IdP)

Some NetBird setups redirect the browser to `http://localhost:53000/?code=...`
after sign-in.

- **Browser on the same machine as `mcp-cli`:** nothing to do.
- **Browser elsewhere:** the page fails to load. Copy the full address from
  the address bar and give it to the agent; it calls `netbird_login_complete`.
  With `mcp-cli login netbird`, paste it into the terminal instead.

Only `http://localhost|127.0.0.1|[::1]` on a port in `callback_ports` is
accepted, and only while a login is waiting.

## Alerts and auto-login

```toml
[cli.netbird.login]
run = ["up", "--no-browser"]
auto = true                       # start the login as soon as it expires

[[notify]]
kind = "ntfy"
url = "https://ntfy.sh/my-private-topic"
on = ["needs_login", "ready"]
```

With `auto = true` the alert contains the sign-in link (ntfy: tap to open).

## Using it behind headless-mcp

```toml
[backends.netbird]
transport = "stdio"
command = "mcp-cli"
args = ["serve", "-c", "/etc/mcp-cli/netbird.toml"]
namespace = "netbird"
connection_mode = "eager"
```
