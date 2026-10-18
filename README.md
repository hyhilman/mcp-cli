# mcp-cli

Expose a fixed set of CLI commands as MCP tools. You write a TOML file listing
the programs and the exact commands an agent may run, plus an optional health
check and SSO login. Nothing outside that file can run, and no shell is used.

Built for CLIs behind an SSO session that expires (NetBird VPN, AWS IAM
Identity Center): when the session expires, the agent gets the login link to
show the user, you can get a Slack/ntfy alert, and subscribers get an MCP
event.

## Quick start

```sh
cargo install --path .   # or a binary from GitHub Releases, or ghcr.io/hyhilman/mcp-cli
mkdir -p ~/.config/mcp-cli
cp examples/netbird.toml ~/.config/mcp-cli/config.toml
mcp-cli check            # validate, list tools
mcp-cli status           # run health checks
mcp-cli login netbird    # print the SSO link, wait for sign-in
mcp-cli call netbird_peers
```

As an MCP server (stdio), e.g. in Claude Code:

```sh
claude mcp add netbird -- mcp-cli serve -c ~/.config/mcp-cli/config.toml
```

Or behind [headless-mcp](https://github.com/hyhilman/headless-mcp-server) as a
stdio backend. For HTTP: `mcp-cli serve --http 127.0.0.1:8090` (set
`MCP_CLI_TOKEN` to require `Authorization: Bearer <token>`; required for
non-loopback addresses).

Guides: [NetBird](docs/netbird.md), [AWS CLI with SSO](docs/aws.md).

## Config

```toml
[server]                       # optional
name = "mcp-cli"
instructions = "..."           # appended to the model instructions
login_wait_secs = 20           # how long a retried call waits for a login

[cli.netbird]
bin = "netbird"                # resolved to a full path at startup
description = "NetBird VPN"    # prefixes tool descriptions
env = { NB_LOG_LEVEL = "warn" }
inherit_env = ["PATH", "HOME"] # default: PATH HOME USER LANG LC_ALL TMPDIR; all else cleared
cwd = "/tmp"
timeout_secs = 30
max_output_bytes = 65536

[cli.netbird.health]           # optional: state, alerts, events, login gate
run = ["status"]
ok_if = ["Management: Connected"]     # empty = exit code 0 is enough
needs_login_if = ["NeedsLogin"]       # checked first
expiry = ["..."]                      # optional: first output line = expires_at
every_secs = 60

[cli.netbird.login]            # optional, needs [health]
run = ["up", "--no-browser"]   # must print the link, then wait for the user
url_prefixes = ["https://"]
url_wait_secs = 30
timeout_secs = 600
callback_ports = [53000]       # paste-back for http://localhost redirects
auto = false                   # start login on expiry; link goes in alerts

[cli.netbird.commands.select_network]
description = "Select a NetBird network route."
run = ["networks", "select", "{network}"]
read_only = false              # true adds readOnlyHint
needs_login = true             # default: true when [health] exists
timeout_secs = 30
args.network = { enum = ["office", "staging"], description = "..." }

[[notify]]                     # optional alerts
kind = "slack"                 # slack | ntfy | webhook
url_env = "SLACK_WEBHOOK_URL"  # or url = "..."
on = ["needs_login", "ready"]  # needs_login | down | ready
cli = ["netbird"]              # default: all

[events]                       # optional MCP Events (signed webhooks)
enabled = true
# store = "/path/events.json"  # default: ~/.local/share/mcp-cli/events.json
include_login_url = false
allow_insecure_callbacks = false
```

### Arguments

The agent can only fill `{placeholders}` declared under `args`:

| Kind | Example |
|---|---|
| enum | `args.region = { enum = ["us-east-1", "ap-southeast-1"] }` |
| pattern | `args.name = { pattern = "[a-z0-9-]{1,40}" }` (whole value must match) |
| integer | `args.minutes = { type = "integer", min = 1, max = 120 }` |

- A string arg needs exactly one of `enum` / `pattern`; free text is not allowed.
- `default` makes an arg optional.
- A placeholder fills part of one argv item, never adds items: `"--since={minutes}m"`.
- Pattern values may not start with `-`; control characters are always refused.
- `{{` / `}}` are literal braces.
- Unknown arguments are refused.

## Tools

| Tool | When |
|---|---|
| `<cli>_<command>` | each `[cli.X.commands.Y]` |
| `<cli>_status` | `[health]` set: runs the check, returns state / expiry / pending login |
| `<cli>_login` | `[login]` set: starts the login, returns the link |
| `<cli>_login_complete` | `callback_ports` set: forwards a pasted `http://localhost:<port>/...` |

A command with `needs_login` is blocked while the state is `needs_login` or
`down`. It is also re-checked when a command fails with a `needs_login_if`
text in its output, which catches expiry between health checks. When blocked:

- **Clients with URL elicitation (MCP 2026-07-28):** the call returns
  `input_required` with the login link. The client shows the link to the user.
  The retry waits for the login to finish, then runs the command.
- **Other clients:** `LOGIN_REQUIRED: ... Call <cli>_login ...`

## Protocol

- MCP `2025-06-18` (`initialize`) and `2026-07-28` (`server/discover`,
  per-request `_meta`, `resultType`, header checks).
- Resources `cli://<name>/state`, with live updates via `subscriptions/listen`
  (SSE over HTTP, interleaved on stdio until `notifications/cancelled`).
- Multi round-trip login via URL elicitation. `requestState` is HMAC-signed,
  bound to the CLI and tool, and expires after 15 minutes.
- [MCP Events](https://developers.openai.com/plugins/build/mcp-events):
  `events/list`, `events/subscribe`, `events/unsubscribe`.
  - One event, `cli.state_changed`.
  - Standard Webhooks signatures, callback verification.
  - Callbacks must be HTTPS to public addresses; the address is pinned and
    redirects are not followed.

## Safety model

- Only the binaries and argv in the config run: no shell, no extra flags.
- The environment is cleared except `inherit_env` plus `env`. Use `env` to pin
  profiles and accounts (e.g. `AWS_PROFILE`) so the agent can't switch them.
- Output is capped (`max_output_bytes`), and every command has a timeout.
- Paste-back only accepts `http://localhost|127.0.0.1|[::1]:<callback_port>`,
  and only while a login is waiting.
- Login links are given to the agent to show the user. Alerts carry the link
  only to destinations in the config. Event payloads carry it only with
  `include_login_url = true`.
- Every command run is logged (tool, argv, exit code, duration) to stderr.
  `MCP_CLI_LOG=debug` gives more detail.

## Releases

Bumping `version` in `Cargo.toml` on `main` tags `v<version>`, builds the
binaries once (Linux musl x86_64/arm64, macOS arm64/x86_64), attaches them to a
GitHub release, and pushes a multi-arch image built from the same Linux
binaries to `ghcr.io/hyhilman/mcp-cli`. The image has no CLIs in it; extend it
with the ones your config wraps (see `Dockerfile.release`).
