//! TOML config: which programs may run, with which fixed arguments.
//!
//! Nothing outside this file can be executed. The agent only picks a tool
//! (one configured command) and fills its declared `{placeholders}`, each
//! checked against an `enum`, a `pattern` or an integer range.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde::Deserialize;

use crate::notify::{NotifyKind, NotifyTarget};
use crate::runner::{ArgSpec, ArgType, CommandDef, Program};
use crate::state::StateDef;

/// Environment variables passed through from the server's own environment
/// when `inherit_env` is not set. Everything else is cleared.
const DEFAULT_INHERIT_ENV: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR"];

/// Suffixes reserved for the built-in tools of a CLI with health / login.
pub const RESERVED_COMMANDS: &[&str] = &["status", "login", "login_complete"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

fn invalid(msg: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(msg.into())
}

// ── Raw TOML shape ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    cli: BTreeMap<String, RawCli>,
    #[serde(default)]
    notify: Vec<RawNotify>,
    #[serde(default)]
    events: Option<RawEvents>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    name: Option<String>,
    instructions: Option<String>,
    login_wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCli {
    bin: String,
    description: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    inherit_env: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_max_output")]
    max_output_bytes: usize,
    health: Option<RawHealth>,
    login: Option<RawLogin>,
    #[serde(default)]
    commands: BTreeMap<String, RawCommand>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHealth {
    run: Vec<String>,
    #[serde(default)]
    ok_if: Vec<String>,
    #[serde(default)]
    needs_login_if: Vec<String>,
    expiry: Option<Vec<String>>,
    #[serde(default = "default_every")]
    every_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogin {
    run: Vec<String>,
    #[serde(default = "default_url_prefixes")]
    url_prefixes: Vec<String>,
    #[serde(default = "default_url_wait")]
    url_wait_secs: u64,
    #[serde(default = "default_login_timeout")]
    timeout_secs: u64,
    #[serde(default)]
    callback_ports: Vec<u16>,
    #[serde(default)]
    auto: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    description: String,
    run: Vec<String>,
    timeout_secs: Option<u64>,
    needs_login: Option<bool>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    args: BTreeMap<String, RawArg>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArg {
    description: Option<String>,
    #[serde(rename = "type", default)]
    kind: RawArgType,
    #[serde(rename = "enum")]
    one_of: Option<Vec<String>>,
    pattern: Option<String>,
    min: Option<i64>,
    max: Option<i64>,
    default: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawArgType {
    #[default]
    String,
    Integer,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotify {
    kind: NotifyKind,
    url: Option<String>,
    /// Read the URL from this environment variable (keeps tokens out of the file).
    url_env: Option<String>,
    #[serde(default = "default_notify_on")]
    on: Vec<String>,
    /// Only these CLIs. Empty = all.
    #[serde(default)]
    cli: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvents {
    #[serde(default = "yes")]
    enabled: bool,
    store: Option<PathBuf>,
    default_ttl_secs: Option<u64>,
    max_ttl_secs: Option<u64>,
    #[serde(default)]
    allow_insecure_callbacks: bool,
    #[serde(default)]
    include_login_url: bool,
}

fn default_timeout() -> u64 {
    30
}
fn default_max_output() -> usize {
    64 * 1024
}
fn default_every() -> u64 {
    60
}
fn default_url_prefixes() -> Vec<String> {
    vec!["https://".into()]
}
fn default_url_wait() -> u64 {
    30
}
fn default_login_timeout() -> u64 {
    600
}
fn default_notify_on() -> Vec<String> {
    vec!["needs_login".into(), "down".into(), "ready".into()]
}
fn yes() -> bool {
    true
}

// ── Validated config ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
    pub server_name: String,
    pub instructions: Option<String>,
    pub login_wait_secs: u64,
    pub clis: Vec<CliDef>,
    pub notify: Vec<NotifyTarget>,
    pub events: Option<crate::events::EventsConfig>,
}

#[derive(Debug, Clone)]
pub struct CliDef {
    pub name: String,
    pub description: Option<String>,
    pub program: Arc<Program>,
    pub state: Option<StateDef>,
    pub auto_login: bool,
    pub commands: Vec<CommandDef>,
}

impl CliDef {
    pub fn has_login(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.login.is_some())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, &|k| std::env::var(k).ok(), &resolve_bin)
    }

    /// Parse and validate. `env` and `resolve` are injected for tests.
    pub fn parse(
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
        resolve: &dyn Fn(&str) -> Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text)?;
        if raw.cli.is_empty() {
            return Err(invalid("no [cli.<name>] sections"));
        }

        let mut clis = Vec::new();
        let mut tool_names = HashSet::new();
        for (name, cli) in raw.cli {
            let def = build_cli(&name, cli, env, resolve)?;
            for tool in tool_names_of(&def) {
                if !tool_names.insert(tool.clone()) {
                    return Err(invalid(format!("duplicate tool name '{tool}'")));
                }
            }
            clis.push(def);
        }

        let known: Vec<&str> = clis.iter().map(|c| c.name.as_str()).collect();
        let mut notify = Vec::new();
        for (i, n) in raw.notify.into_iter().enumerate() {
            let url = match (n.url, n.url_env) {
                (Some(url), None) => url,
                (None, Some(var)) => {
                    env(&var).ok_or_else(|| invalid(format!("[[notify]] #{}: ${var} is not set", i + 1)))?
                }
                _ => {
                    return Err(invalid(format!(
                        "[[notify]] #{}: set exactly one of url / url_env",
                        i + 1
                    )))
                }
            };
            for state in &n.on {
                if !matches!(
                    state.as_str(),
                    "needs_login" | "down" | "ready" | "login_started" | "login_finished"
                ) {
                    return Err(invalid(format!("[[notify]] #{}: unknown state '{state}'", i + 1)));
                }
            }
            for c in &n.cli {
                if !known.contains(&c.as_str()) {
                    return Err(invalid(format!("[[notify]] #{}: unknown cli '{c}'", i + 1)));
                }
            }
            notify.push(NotifyTarget {
                kind: n.kind,
                url,
                on: n.on,
                clis: n.cli,
            });
        }

        let events = match raw.events {
            Some(e) if e.enabled => {
                let defaults = crate::events::EventsConfig::default();
                Some(crate::events::EventsConfig {
                    store_path: e.store.or_else(|| data_dir().map(|d| d.join("events.json"))),
                    default_ttl_secs: e.default_ttl_secs.unwrap_or(defaults.default_ttl_secs),
                    max_ttl_secs: e.max_ttl_secs.unwrap_or(defaults.max_ttl_secs),
                    allow_insecure_callbacks: e.allow_insecure_callbacks,
                    include_login_url: e.include_login_url,
                })
            }
            _ => None,
        };

        Ok(Config {
            server_name: raw.server.name.unwrap_or_else(|| "mcp-cli".into()),
            instructions: raw.server.instructions,
            login_wait_secs: raw.server.login_wait_secs.unwrap_or(20),
            clis,
            notify,
            events,
        })
    }

    #[cfg(test)]
    pub fn cli(&self, name: &str) -> Option<&CliDef> {
        self.clis.iter().find(|c| c.name == name)
    }
}

/// Every tool name a CLI exposes.
pub fn tool_names_of(cli: &CliDef) -> Vec<String> {
    let mut names: Vec<String> = cli.commands.iter().map(|c| c.tool.clone()).collect();
    if let Some(state) = &cli.state {
        names.push(format!("{}_status", cli.name));
        if let Some(login) = &state.login {
            names.push(format!("{}_login", cli.name));
            if !login.callback_ports.is_empty() {
                names.push(format!("{}_login_complete", cli.name));
            }
        }
    }
    names
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn build_cli(
    name: &str,
    raw: RawCli,
    env: &dyn Fn(&str) -> Option<String>,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<CliDef, ConfigError> {
    let at = |msg: String| invalid(format!("[cli.{name}] {msg}"));
    if !valid_name(name) {
        return Err(at(
            "name must be lowercase letters, digits and '_' (max 40)".into()
        ));
    }
    let bin = resolve(&raw.bin).ok_or_else(|| at(format!("bin '{}' not found", raw.bin)))?;

    let mut program_env: BTreeMap<String, String> = BTreeMap::new();
    let inherit: Vec<String> = raw
        .inherit_env
        .unwrap_or_else(|| DEFAULT_INHERIT_ENV.iter().map(|s| s.to_string()).collect());
    for key in inherit {
        if let Some(value) = env(&key) {
            program_env.insert(key, value);
        }
    }
    program_env.extend(raw.env);

    let program = Arc::new(Program {
        cli: name.to_string(),
        bin,
        env: program_env,
        cwd: raw.cwd,
        timeout_secs: raw.timeout_secs.max(1),
        max_output_bytes: raw.max_output_bytes.max(1024),
    });

    let (state, auto_login) = match (raw.health, raw.login) {
        (None, Some(_)) => {
            return Err(at(
                "[login] needs a [health] check to know when login finished".into()
            ))
        }
        (None, None) => (None, false),
        (Some(h), login) => {
            if h.run.is_empty() {
                return Err(at("health.run is empty".into()));
            }
            let auto = login.as_ref().is_some_and(|l| l.auto);
            let login = match login {
                Some(l) if l.run.is_empty() => return Err(at("login.run is empty".into())),
                Some(l) if l.url_prefixes.is_empty() => return Err(at("login.url_prefixes is empty".into())),
                Some(l) => Some(crate::state::LoginDef {
                    run: l.run,
                    url_prefixes: l.url_prefixes,
                    url_wait_secs: l.url_wait_secs.max(1),
                    timeout_secs: l.timeout_secs.max(10),
                    callback_ports: l.callback_ports,
                }),
                None => None,
            };
            let state = StateDef {
                name: name.to_string(),
                program: Arc::clone(&program),
                health: h.run,
                ok_if: h.ok_if,
                needs_login_if: h.needs_login_if,
                expiry: h.expiry,
                every_secs: h.every_secs.max(5),
                login,
            };
            (Some(state), auto)
        }
    };

    let mut commands = Vec::new();
    for (cmd_name, cmd) in raw.commands {
        if !valid_name(&cmd_name) {
            return Err(at(format!(
                "command '{cmd_name}': name must be lowercase letters, digits and '_'"
            )));
        }
        if RESERVED_COMMANDS.contains(&cmd_name.as_str()) && state.is_some() {
            return Err(at(format!(
                "command '{cmd_name}' clashes with the built-in {name}_{cmd_name} tool"
            )));
        }
        commands.push(build_command(name, &cmd_name, cmd, state.is_some(), &program)?);
    }
    if commands.is_empty() && state.is_none() {
        return Err(at("defines no commands and no health check".into()));
    }

    Ok(CliDef {
        name: name.to_string(),
        description: raw.description,
        program,
        state,
        auto_login,
        commands,
    })
}

fn build_command(
    cli: &str,
    name: &str,
    raw: RawCommand,
    has_health: bool,
    program: &Arc<Program>,
) -> Result<CommandDef, ConfigError> {
    let at = |msg: String| invalid(format!("[cli.{cli}.commands.{name}] {msg}"));
    if raw.run.is_empty() {
        return Err(at("run is empty".into()));
    }
    if raw.needs_login == Some(true) && !has_health {
        return Err(at("needs_login = true but the cli has no [health] check".into()));
    }

    let used = placeholders(&raw.run).map_err(at)?;
    for p in &used {
        if !raw.args.contains_key(p) {
            return Err(at(format!(
                "placeholder '{{{p}}}' has no [args.{p}] (write {{{{ and }}}} for literal braces)"
            )));
        }
    }

    let mut args = Vec::new();
    for (arg_name, a) in raw.args {
        if !used.contains(&arg_name) {
            return Err(at(format!("arg '{arg_name}' is not used in run")));
        }
        if !valid_name(&arg_name) {
            return Err(at(format!(
                "arg '{arg_name}': name must be lowercase letters, digits and '_'"
            )));
        }
        let kind = match a.kind {
            RawArgType::Integer => {
                if a.one_of.is_some() || a.pattern.is_some() {
                    return Err(at(format!(
                        "arg '{arg_name}': integer args use min/max, not enum/pattern"
                    )));
                }
                ArgType::Integer {
                    min: a.min,
                    max: a.max,
                }
            }
            RawArgType::String => {
                if a.min.is_some() || a.max.is_some() {
                    return Err(at(format!(
                        "arg '{arg_name}': min/max only apply to type = \"integer\""
                    )));
                }
                match (a.one_of, a.pattern) {
                    (Some(values), None) if !values.is_empty() => ArgType::Enum(values),
                    (None, Some(p)) => {
                        let re = Regex::new(&format!("^(?:{p})$"))
                            .map_err(|e| at(format!("arg '{arg_name}': bad pattern: {e}")))?;
                        ArgType::Pattern(p, re)
                    }
                    _ => {
                        return Err(at(format!(
                        "arg '{arg_name}': string args need exactly one of enum = [...] or pattern = \"...\""
                    )))
                    }
                }
            }
        };
        let default = match a.default {
            None => None,
            Some(toml::Value::String(s)) => Some(s),
            Some(toml::Value::Integer(i)) => Some(i.to_string()),
            Some(_) => {
                return Err(at(format!(
                    "arg '{arg_name}': default must be a string or integer"
                )))
            }
        };
        let spec = ArgSpec {
            name: arg_name.clone(),
            description: a.description,
            kind,
            default,
        };
        if let Some(d) = &spec.default {
            spec.check(d)
                .map_err(|e| at(format!("arg '{arg_name}': default {e}")))?;
        }
        args.push(spec);
    }

    Ok(CommandDef {
        tool: format!("{cli}_{name}"),
        description: raw.description,
        run: raw.run,
        args,
        timeout_secs: raw.timeout_secs.unwrap_or(program.timeout_secs).max(1),
        needs_login: raw.needs_login.unwrap_or(has_health),
        read_only: raw.read_only,
    })
}

/// Names of `{placeholders}` used in `run`. `{{` / `}}` are literal braces.
pub fn placeholders(run: &[String]) -> Result<HashSet<String>, String> {
    let mut out = HashSet::new();
    for item in run {
        for part in crate::runner::split_template(item)? {
            if let crate::runner::Part::Arg(name) = part {
                out.insert(name);
            }
        }
    }
    Ok(out)
}

/// Resolve `bin` to an absolute path once, at startup.
pub fn resolve_bin(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let path = PathBuf::from(bin);
        return is_executable(&path).then(|| std::fs::canonicalize(&path).unwrap_or(path));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Default config file: `$MCP_CLI_CONFIG`, else `~/.config/mcp-cli/config.toml`.
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MCP_CLI_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("mcp-cli").join("config.toml")
}

/// Where event subscriptions are stored: `~/.local/share/mcp-cli`.
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("mcp-cli"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(
            text,
            &|k| match k {
                "PATH" => Some("/usr/bin".into()),
                "SLACK_URL" => Some("https://hooks.slack.example/x".into()),
                _ => None,
            },
            &|bin| Some(PathBuf::from(format!("/usr/bin/{bin}"))),
        )
    }

    const NETBIRD: &str = r#"
        [cli.netbird]
        bin = "netbird"
        env = { NB_LOG_LEVEL = "warn" }

        [cli.netbird.health]
        run = ["status"]
        ok_if = ["Management: Connected"]
        needs_login_if = ["NeedsLogin"]

        [cli.netbird.login]
        run = ["up", "--no-browser"]
        callback_ports = [53000]

        [cli.netbird.commands.peers]
        description = "List peers"
        run = ["status", "--detail"]
        read_only = true

        [cli.netbird.commands.route]
        description = "Select a route"
        run = ["routes", "select", "{route}"]
        args.route = { enum = ["office", "prod"] }

        [[notify]]
        kind = "slack"
        url_env = "SLACK_URL"
        on = ["needs_login"]
    "#;

    #[test]
    fn parses_full_example() {
        let config = parse(NETBIRD).unwrap();
        let cli = config.cli("netbird").unwrap();
        assert_eq!(cli.program.bin, PathBuf::from("/usr/bin/netbird"));
        assert_eq!(cli.program.env.get("PATH").unwrap(), "/usr/bin");
        assert_eq!(cli.program.env.get("NB_LOG_LEVEL").unwrap(), "warn");
        assert!(!cli.program.env.contains_key("HOME"));
        let mut tools = tool_names_of(cli);
        tools.sort();
        assert_eq!(
            tools,
            [
                "netbird_login",
                "netbird_login_complete",
                "netbird_peers",
                "netbird_route",
                "netbird_status"
            ]
        );
        assert!(cli.commands.iter().all(|c| c.needs_login));
        assert_eq!(config.notify[0].url, "https://hooks.slack.example/x");
    }

    #[test]
    fn notify_accepts_login_events_and_rejects_unknown_names() {
        let with_on = |on: &str| NETBIRD.replace(r#"on = ["needs_login"]"#, on);
        let config = parse(&with_on(r#"on = ["login_started", "login_finished"]"#)).unwrap();
        assert_eq!(config.notify[0].on, ["login_started", "login_finished"]);
        assert!(parse(&with_on(r#"on = ["login_done"]"#)).is_err());
    }

    #[test]
    fn rejects_unsafe_or_inconsistent_configs() {
        let cases = [
            // free-form string argument
            r#"[cli.a]
               bin = "x"
               [cli.a.commands.c]
               description = "d"
               run = ["{v}"]
               args.v = {}"#,
            // placeholder without declaration
            r#"[cli.a]
               bin = "x"
               [cli.a.commands.c]
               description = "d"
               run = ["{v}"]"#,
            // declared arg not used
            r#"[cli.a]
               bin = "x"
               [cli.a.commands.c]
               description = "d"
               run = ["ls"]
               args.v = { enum = ["x"] }"#,
            // login without health
            r#"[cli.a]
               bin = "x"
               [cli.a.login]
               run = ["login"]"#,
            // reserved name
            r#"[cli.a]
               bin = "x"
               [cli.a.health]
               run = ["ok"]
               [cli.a.commands.status]
               description = "d"
               run = ["s"]"#,
            // unknown key (typo)
            r#"[cli.a]
               bin = "x"
               comands = {}"#,
            // default outside enum
            r#"[cli.a]
               bin = "x"
               [cli.a.commands.c]
               description = "d"
               run = ["{v}"]
               args.v = { enum = ["x"], default = "y" }"#,
        ];
        for case in cases {
            assert!(parse(case).is_err(), "should reject:\n{case}");
        }
    }

    #[test]
    fn missing_bin_is_an_error() {
        let err = Config::parse(
            "[cli.a]\nbin = \"nope\"\n[cli.a.commands.c]\ndescription = \"d\"\nrun = [\"x\"]",
            &|_| None,
            &|_| None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
