# AWS CLI with SSO (IAM Identity Center)

`aws sso login` sessions expire (typically 8-12h). With `mcp-cli` the agent
runs only the AWS commands you list, always with the profile you pinned. When
the session expires it gets the device-code link to show you.

## Setup

1. An SSO profile in `~/.aws/config`:

   ```ini
   [profile prod-readonly]
   sso_session = corp
   sso_account_id = 123456789012
   sso_role_name = ReadOnly
   region = ap-southeast-1

   [sso-session corp]
   sso_start_url = https://corp.awsapps.com/start
   sso_region = ap-southeast-1
   sso_registration_scopes = sso:account:access
   ```

2. Config: start from `examples/aws.toml`.

   ```toml
   [cli.aws]
   bin = "aws"
   env = { AWS_PROFILE = "prod-readonly", AWS_REGION = "ap-southeast-1", AWS_PAGER = "" }
   inherit_env = ["PATH", "HOME"]   # HOME: ~/.aws config and token cache

   [cli.aws.health]
   run = ["sts", "get-caller-identity", "--output", "json"]
   needs_login_if = ["aws sso login", "Error loading SSO Token", "Token has expired", "UnauthorizedSSOToken"]
   every_secs = 300

   [cli.aws.login]
   run = ["sso", "login", "--use-device-code", "--no-browser"]
   ```

3. Check it: `mcp-cli check && mcp-cli status && mcp-cli login aws`.

## Why the profile can't change

- `AWS_PROFILE`, `AWS_REGION` and the rest come from `env`, and the
  environment is otherwise cleared.
- No command contains `--profile` or `{profile}`, so the agent can't add one.
  Arguments only fill declared placeholders, and pattern values may not
  start with `-`.
- To allow several accounts, add one `[cli.<name>]` per profile, e.g.
  `aws_prod` and `aws_staging`. Each gets its own health check and login.

## Writing commands

Use `--query` and `--output json` to keep output small (the default cap is
64 KB):

```toml
[cli.aws.commands.list_instances]
description = "List EC2 instances, optionally filtered by Name tag."
run = [
  "ec2", "describe-instances", "--region", "{region}",
  "--filters", "Name=tag:Name,Values={name}",
  "--query", "Reservations[].Instances[].{{id:InstanceId,state:State.Name}}",
  "--output", "json",
]
read_only = true
args.region = { enum = ["ap-southeast-1", "us-east-1"], default = "ap-southeast-1" }
args.name = { pattern = "[A-Za-z0-9*._-]{1,64}", default = "*" }
```

`{{ }}` are literal braces (JMESPath); `{region}` is an argument.

Only add write commands (`ec2 stop-instances`, ...) with tight `enum` args,
and leave `read_only` off so clients can ask before running them.

## AWS MCP server instead of the CLI

If you use the AWS MCP server (`mcp-proxy-for-aws`) through headless-mcp, you
can still use `mcp-cli` just for the login: configure `[health]` and `[login]`
with no commands. The agent then has `aws_status` and `aws_login` next to the
AWS MCP tools.
