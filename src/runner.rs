//! Runs configured commands: argv only (no shell), cleared environment,
//! timeout, capped output.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// A configured program: resolved binary plus its fixed environment.
#[derive(Debug, Clone)]
pub struct Program {
    pub cli: String,
    pub bin: PathBuf,
    /// The complete environment of every child (nothing else is inherited).
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
}

impl Program {
    /// A `Command` for `args` with the fixed environment. Killed on drop.
    pub fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// Run to completion within `timeout`.
    pub async fn run(&self, args: &[String], timeout: Duration) -> Result<Output, String> {
        let started = Instant::now();
        let mut child = self
            .command(args)
            .spawn()
            .map_err(|e| format!("failed to run {}: {e}", self.bin.display()))?;
        let max = self.max_output_bytes;
        let stdout = tokio::spawn(read_capped(child.stdout.take(), max));
        let stderr = tokio::spawn(read_capped(child.stderr.take(), max));
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status.map_err(|e| format!("{} failed: {e}", self.bin.display()))?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(format!(
                    "'{} {}' timed out after {}s",
                    self.cli,
                    args.first().map(String::as_str).unwrap_or(""),
                    timeout.as_secs()
                ));
            }
        };
        let (stdout, out_cut) = stdout.await.unwrap_or_default();
        let (stderr, err_cut) = stderr.await.unwrap_or_default();
        Ok(Output {
            exit_code: status.code(),
            success: status.success(),
            stdout,
            stderr,
            truncated: out_cut || err_cut,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Output {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub duration_ms: u64,
}

impl Output {
    /// stdout and stderr together, for pattern matching.
    pub fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// Read up to `max` bytes, then keep draining (so the child never blocks
/// on a full pipe) but discard the rest.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, max: usize) -> (String, bool) {
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = max.saturating_sub(kept.len());
                if n > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

/// One configured command, exposed as the tool `<cli>_<name>`.
#[derive(Debug, Clone)]
pub struct CommandDef {
    pub tool: String,
    pub description: String,
    /// argv after the binary; may contain `{arg}` placeholders.
    pub run: Vec<String>,
    pub args: Vec<ArgSpec>,
    pub timeout_secs: u64,
    /// Blocked (with a login prompt) while the CLI's health is not ready.
    pub needs_login: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct ArgSpec {
    pub name: String,
    pub description: Option<String>,
    pub kind: ArgType,
    pub default: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ArgType {
    Enum(Vec<String>),
    /// Source pattern and its anchored regex.
    Pattern(String, Regex),
    Integer {
        min: Option<i64>,
        max: Option<i64>,
    },
}

impl ArgSpec {
    /// Validate one value (already converted to its argv text).
    pub fn check(&self, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err("must not be empty".into());
        }
        if value.chars().any(char::is_control) {
            return Err("must not contain control characters".into());
        }
        match &self.kind {
            ArgType::Enum(values) if !values.iter().any(|v| v == value) => {
                Err(format!("must be one of: {}", values.join(", ")))
            }
            ArgType::Pattern(src, re) if !re.is_match(value) => Err(format!("must match {src}")),
            ArgType::Integer { min, max } => {
                let n: i64 = value.parse().map_err(|_| "must be an integer".to_string())?;
                if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                    return Err(format!(
                        "must be between {} and {}",
                        min.map_or("-inf".into(), |m| m.to_string()),
                        max.map_or("inf".into(), |m| m.to_string())
                    ));
                }
                Ok(())
            }
            // Enum and pattern values come from the config author; anything
            // else that looks like an option is refused.
            ArgType::Pattern(..) if value.starts_with('-') => Err("must not start with '-'".into()),
            _ => Ok(()),
        }
    }

    pub fn schema(&self) -> Value {
        let mut s = match &self.kind {
            ArgType::Enum(values) => serde_json::json!({ "type": "string", "enum": values }),
            ArgType::Pattern(src, _) => {
                serde_json::json!({ "type": "string", "pattern": format!("^(?:{src})$") })
            }
            ArgType::Integer { min, max } => {
                let mut s = serde_json::json!({ "type": "integer" });
                if let Some(m) = min {
                    s["minimum"] = (*m).into();
                }
                if let Some(m) = max {
                    s["maximum"] = (*m).into();
                }
                s
            }
        };
        if let Some(d) = &self.description {
            s["description"] = d.clone().into();
        }
        if let Some(d) = &self.default {
            s["default"] = match self.kind {
                ArgType::Integer { .. } => d.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
                _ => d.clone().into(),
            };
        }
        s
    }
}

impl CommandDef {
    pub fn input_schema(&self) -> Value {
        let properties: serde_json::Map<String, Value> =
            self.args.iter().map(|a| (a.name.clone(), a.schema())).collect();
        let required: Vec<&str> = self
            .args
            .iter()
            .filter(|a| a.default.is_none())
            .map(|a| a.name.as_str())
            .collect();
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }

    /// Build the argv from caller arguments. Unknown arguments are refused.
    pub fn argv(&self, arguments: Option<&Value>) -> Result<Vec<String>, String> {
        let empty = serde_json::Map::new();
        let given = match arguments {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(obj)) => obj,
            Some(_) => return Err("arguments must be an object".into()),
        };
        let known: HashSet<&str> = self.args.iter().map(|a| a.name.as_str()).collect();
        if let Some(extra) = given.keys().find(|k| !known.contains(k.as_str())) {
            return Err(format!("unexpected argument '{extra}'"));
        }

        let mut values: BTreeMap<&str, String> = BTreeMap::new();
        for spec in &self.args {
            let value = match given.get(&spec.name) {
                None | Some(Value::Null) => spec
                    .default
                    .clone()
                    .ok_or_else(|| format!("missing required argument '{}'", spec.name))?,
                // Integers may also arrive as strings ("5"); `check` parses them.
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) if matches!(spec.kind, ArgType::Integer { .. }) => n.to_string(),
                Some(_) => return Err(format!("argument '{}' has the wrong type", spec.name)),
            };
            spec.check(&value)
                .map_err(|e| format!("argument '{}' {e}", spec.name))?;
            values.insert(spec.name.as_str(), value);
        }

        self.run
            .iter()
            .map(|item| {
                let mut out = String::new();
                for part in split_template(item)? {
                    match part {
                        Part::Text(t) => out.push_str(&t),
                        Part::Arg(name) => out.push_str(
                            values
                                .get(name.as_str())
                                .ok_or_else(|| format!("unknown placeholder '{name}'"))?,
                        ),
                    }
                }
                Ok(out)
            })
            .collect()
    }
}

#[derive(Debug, PartialEq)]
pub enum Part {
    Text(String),
    Arg(String),
}

/// Split one argv template item into literal text and `{name}` parts.
/// `{{` and `}}` are literal braces.
pub fn split_template(item: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = item.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => return Err(format!("unclosed '{{' in '{item}'")),
                    }
                }
                if name.is_empty() {
                    return Err(format!("empty placeholder in '{item}'"));
                }
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                parts.push(Part::Arg(name));
            }
            '}' => return Err(format!("stray '}}' in '{item}' (use '}}}}')")),
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> CommandDef {
        CommandDef {
            tool: "aws_list".into(),
            description: "d".into(),
            run: vec![
                "ec2".into(),
                "--region".into(),
                "{region}".into(),
                "--max-items={limit}".into(),
                "--filters".into(),
                "Name=tag:Name,Values={name}".into(),
                "{{literal}}".into(),
            ],
            args: vec![
                ArgSpec {
                    name: "region".into(),
                    description: None,
                    kind: ArgType::Enum(vec!["ap-southeast-1".into(), "us-east-1".into()]),
                    default: Some("ap-southeast-1".into()),
                },
                ArgSpec {
                    name: "limit".into(),
                    description: None,
                    kind: ArgType::Integer {
                        min: Some(1),
                        max: Some(100),
                    },
                    default: Some("10".into()),
                },
                ArgSpec {
                    name: "name".into(),
                    description: None,
                    kind: ArgType::Pattern("[a-z0-9-]+".into(), Regex::new("^(?:[a-z0-9-]+)$").unwrap()),
                    default: None,
                },
            ],
            timeout_secs: 5,
            needs_login: false,
            read_only: true,
        }
    }

    #[test]
    fn fills_placeholders_with_checked_values() {
        let argv = cmd()
            .argv(Some(&serde_json::json!({ "name": "web-1", "limit": 5 })))
            .unwrap();
        assert_eq!(
            argv,
            [
                "ec2",
                "--region",
                "ap-southeast-1",
                "--max-items=5",
                "--filters",
                "Name=tag:Name,Values=web-1",
                "{literal}"
            ]
        );
    }

    #[test]
    fn refuses_bad_arguments() {
        let c = cmd();
        for bad in [
            serde_json::json!({}),                                     // missing name
            serde_json::json!({ "name": "web 1" }),                    // pattern
            serde_json::json!({ "name": "--profile" }),                // pattern
            serde_json::json!({ "name": "a", "region": "eu-west-1" }), // enum
            serde_json::json!({ "name": "a", "limit": 1000 }),         // range
            serde_json::json!({ "name": "a", "limit": "five" }),       // type
            serde_json::json!({ "name": 5 }),                          // type
            serde_json::json!({ "name": "a", "profile": "prod" }),     // unknown
            serde_json::json!({ "name": "a\nb" }),                     // control
        ] {
            assert!(c.argv(Some(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn dash_values_refused_for_patterns() {
        let spec = ArgSpec {
            name: "v".into(),
            description: None,
            kind: ArgType::Pattern(".+".into(), Regex::new("^(?:.+)$").unwrap()),
            default: None,
        };
        assert!(spec.check("--evil").is_err());
        assert!(spec.check("fine").is_ok());
    }

    #[test]
    fn schema_lists_required_and_defaults() {
        let s = cmd().input_schema();
        assert_eq!(s["required"], serde_json::json!(["name"]));
        assert_eq!(s["properties"]["limit"]["default"], 10);
        assert_eq!(s["additionalProperties"], false);
    }

    #[test]
    fn template_errors() {
        assert!(split_template("{open").is_err());
        assert!(split_template("{}").is_err());
        assert!(split_template("a}b").is_err());
    }

    #[tokio::test]
    async fn runs_with_cleared_env_and_caps_output() {
        let program = Program {
            cli: "sh".into(),
            bin: "/bin/sh".into(),
            env: BTreeMap::from([("ONLY".into(), "me".into())]),
            cwd: None,
            timeout_secs: 5,
            max_output_bytes: 1024,
        };
        let out = program
            .run(
                &["-c".into(), "env; head -c 5000 /dev/zero | tr '\\0' x".into()],
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.stdout.contains("ONLY=me"));
        assert!(!out.stdout.contains("HOME="));
        assert!(out.truncated);
        assert_eq!(out.stdout.len(), 1024);

        let err = program
            .run(&["-c".into(), "sleep 5".into()], Duration::from_millis(200))
            .await;
        assert!(err.unwrap_err().contains("timed out"));
    }
}
