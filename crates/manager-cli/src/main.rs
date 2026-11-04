//! `llmdil-cli` — CLI for the LLMdil Admin API.
//!
//! Secrets rule (hard): no secret is ever a command-line argument.
//! Tokens arrive via `LLMDIL_API_TOKEN` / `LLMDIL_API_REFRESH_TOKEN`
//! or `--token-stdin`; passwords via `LLMDIL_API_PASSWORD` or
//! `--password-stdin`; enrollment codes via `LLMDIL_ENROLL_CODE` or
//! `--code-stdin`; TOTP codes via `LLMDIL_TOTP_CODE` or
//! `--totp-stdin`. Only one `--*-stdin` flag per invocation: stdin
//! is read once, first line, newline trimmed. Tokens are held in
//! memory only and never written to disk or config files.
//!
//! Connection rule (hard): `--base-url` + `--fingerprint` (or the
//! `LLMDIL_BASE_URL` / `LLMDIL_FINGERPRINT` environment variables)
//! are required for every command, including the public identity
//! read. A fingerprint mismatch aborts the connection; there is no
//! click-through.

mod tui;

use std::io::{IsTerminal, Read, Write};

use clap::{Parser, Subcommand};
use manager_cli::{ApiClient, ApiError, LoginOutcome, Operation};
use serde_json::Value;

const TOKEN_ENV: &str = "LLMDIL_API_TOKEN";
const REFRESH_ENV: &str = "LLMDIL_API_REFRESH_TOKEN";
const PASSWORD_ENV: &str = "LLMDIL_API_PASSWORD";
const ENROLL_ENV: &str = "LLMDIL_ENROLL_CODE";
const TOTP_ENV: &str = "LLMDIL_TOTP_CODE";

#[derive(Parser)]
#[command(
    name = "llmdil-cli",
    about = "CLI/TUI client for the LLMdil Admin API (pinned TLS, token sessions)"
)]
struct Cli {
    /// API base URL, e.g. https://192.168.5.23:47821
    #[arg(long, env = "LLMDIL_BASE_URL")]
    base_url: String,

    /// Certificate SHA-256 fingerprint (64 hex chars) as shown by
    /// the LLMdil UI's "Certificate SHA-256" row
    #[arg(long, env = "LLMDIL_FINGERPRINT")]
    fingerprint: String,

    /// Read the access token from stdin (first line) instead of
    /// LLMDIL_API_TOKEN. Only one --*-stdin flag per invocation.
    #[arg(long)]
    token_stdin: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Public server identity document (no token required)
    Server,
    /// Authenticated status resource
    Status,
    /// List backends
    Backends,
    /// Get one backend
    Backend {
        id: String,
    },
    /// Start a backend (202 operation, polled to completion)
    BackendStart {
        id: String,
        /// Allow a WDDM-paged start when the fit exceeds VRAM
        #[arg(long)]
        allow_wddm_overcommit: bool,
    },
    /// Stop a backend (202 operation, polled to completion)
    BackendStop {
        id: String,
        #[arg(long)]
        graceful: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Restart a backend (202 operation, polled to completion)
    BackendRestart {
        id: String,
        #[arg(long)]
        graceful: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Gateway status
    GatewayStatus,
    /// Start the gateway (202 operation, polled to completion)
    GatewayStart,
    /// Stop the gateway (202 operation, polled to completion)
    GatewayStop {
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Gateway config
    GatewayConfig,
    /// GPU telemetry
    Gpus,
    /// GPU telemetry history for a range (e.g. 1h, 24h)
    TelemetryHistory {
        range: String,
    },
    /// List models
    Models,
    /// Get one model
    Model {
        id: String,
    },
    /// Query logs
    Logs {
        /// Log scope (see logs-scopes)
        #[arg(long)]
        scope: Option<String>,
        /// Level filter
        #[arg(long)]
        level: Option<String>,
        /// Free-text search
        #[arg(long)]
        search: Option<String>,
        /// Max rows
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Log scopes
    LogsScopes,
    /// List operations
    Ops,
    /// Get one operation
    Op {
        id: String,
    },
    /// Poll one operation to a terminal state
    OpPoll {
        id: String,
    },
    /// Abort one operation
    OpAbort {
        id: String,
    },
    /// Activation plan
    Activation,
    /// Apply the activation plan (202 operation, polled)
    ActivationApply {
        /// Apply mode: now (default) or drain
        #[arg(long, default_value = "now")]
        mode: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Activation history
    ActivationHistory,
    /// Authentication commands
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Scripted verification (Stage 2 test client): enroll, then a
    /// full read sweep; exit code reflects success. Non-interactive.
    Verify {
        /// Device name recorded for the enrolled session
        #[arg(long, default_value = "llmdil-cli-verify")]
        device_name: String,
        /// Read the enrollment code from stdin instead of
        /// LLMDIL_ENROLL_CODE
        #[arg(long)]
        code_stdin: bool,
    },
    /// Interactive terminal dashboard
    Tui,
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Redeem a one-time admin-issued enrollment code (prints the
    /// token pair as JSON to stdout for piping; never logs it)
    Enroll {
        /// Device name recorded for the enrolled session
        #[arg(long, default_value = "llmdil-cli")]
        device_name: String,
        /// Read the code from stdin instead of LLMDIL_ENROLL_CODE
        #[arg(long)]
        code_stdin: bool,
    },
    /// Log in with username/password (prints the token pair as
    /// JSON to stdout for piping; TOTP challenge completed inline
    /// when a code source is available)
    Login {
        #[arg(long)]
        username: String,
        /// Device name recorded for the session
        #[arg(long, default_value = "llmdil-cli")]
        device_name: String,
        /// Read the password from stdin instead of LLMDIL_API_PASSWORD
        #[arg(long)]
        password_stdin: bool,
        /// Read the TOTP code from stdin instead of LLMDIL_TOTP_CODE
        #[arg(long)]
        totp_stdin: bool,
    },
    /// Complete a TOTP challenge (prints the token pair as JSON)
    Totp {
        #[arg(long)]
        challenge_id: String,
        /// Read the code from stdin instead of LLMDIL_TOTP_CODE
        #[arg(long)]
        code_stdin: bool,
    },
    /// Current user/role for the token
    Me,
    /// Revoke the token server-side
    Logout,
    /// List own sessions
    Sessions,
    /// Revoke one own session
    SessionRevoke {
        id: String,
    },
    /// Rotate a refresh token (prints the new pair as JSON)
    Refresh {
        /// Read the refresh token from stdin instead of
        /// LLMDIL_API_REFRESH_TOKEN
        #[arg(long)]
        token_stdin: bool,
    },
}

/// Read one secret from stdin: first line, newline trimmed. Errors
/// when stdin is empty.
fn read_stdin_secret(what: &str) -> Result<String, ApiError> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| ApiError::Input(format!("could not read {what} from stdin: {e}")))?;
    let line = buf.lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() {
        return Err(ApiError::Input(format!("empty {what} on stdin")));
    }
    Ok(line)
}

/// Resolve a secret from exactly one source: a `--*-stdin` flag
/// (stdin wins when set) or its environment variable.
fn secret_from(
    stdin_flag: bool,
    env_name: &str,
    what: &str,
) -> Result<String, ApiError> {
    if stdin_flag {
        return read_stdin_secret(what);
    }
    std::env::var(env_name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            ApiError::Input(format!(
                "no {what}: set {env_name} or pass the matching --*-stdin flag"
            ))
        })
}

/// Build an authenticated client: token from --token-stdin or
/// LLMDIL_API_TOKEN, refresh token from LLMDIL_API_REFRESH_TOKEN
/// when present.
fn authed_client(cli: &Cli) -> Result<ApiClient, ApiError> {
    let mut client = ApiClient::new(&cli.base_url, &cli.fingerprint)?;
    let token = secret_from(cli.token_stdin, TOKEN_ENV, "access token")?;
    client.set_access_token(token);
    if let Ok(refresh) = std::env::var(REFRESH_ENV) {
        if !refresh.is_empty() {
            client.set_refresh_token(refresh);
        }
    }
    Ok(client)
}

/// Build an unauthenticated client (public routes only).
fn public_client(cli: &Cli) -> Result<ApiClient, ApiError> {
    ApiClient::new(&cli.base_url, &cli.fingerprint)
}

/// Confirm a destructive action. `--yes` skips the prompt. Without
/// it, a TTY prompt is required; a non-TTY without `--yes` refuses
/// rather than guessing.
fn confirm(action: &str, yes: bool) -> Result<(), ApiError> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(ApiError::Input(format!(
            "{action} needs confirmation: re-run with --yes (stdin is not a TTY)"
        )));
    }
    eprint!("{action}? [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| ApiError::Input(format!("could not read confirmation: {e}")))?;
    if line.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        Err(ApiError::Input(format!("{action} aborted (not confirmed)")))
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

fn print_operation(op: &Operation) {
    println!(
        "{}",
        serde_json::to_string_pretty(op)
            .unwrap_or_else(|_| format!("operation {} {}", op.id, op.status))
    );
}

/// Run a 202 operation command: print the accepted record, poll on
/// the API cadence, print the terminal record. A failed operation
/// exits non-zero.
async fn run_operation(
    client: &mut ApiClient,
    path: &str,
    body: Value,
) -> Result<(), ApiError> {
    let op = client.post_operation(path, body).await?;
    eprintln!("operation {} ({}) running…", op.id, op.kind);
    let final_op = client
        .poll_operation(&op.id, &mut |tick| {
            eprintln!("  {} …", tick.status);
        })
        .await?;
    print_operation(&final_op);
    if final_op.status == "failed" {
        let detail = final_op
            .error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(ApiError::OperationFailed(final_op.id, detail));
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<(), ApiError> {
    match &cli.command {
        Command::Server => {
            let mut client = public_client(&cli)?;
            print_json(&client.server().await?);
        }
        Command::Status => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/status").await?);
        }
        Command::Backends => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/backends").await?);
        }
        Command::Backend { id } => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get(&format!("/api/v1/backends/{id}")).await?);
        }
        Command::BackendStart {
            id,
            allow_wddm_overcommit,
        } => {
            let mut client = authed_client(&cli)?;
            let mut body = serde_json::json!({});
            if *allow_wddm_overcommit {
                body["allow_wddm_overcommit"] = serde_json::json!(true);
            }
            run_operation(
                &mut client,
                &format!("/api/v1/backends/{id}/start"),
                body,
            )
            .await?;
        }
        Command::BackendStop { id, graceful, yes } => {
            confirm(&format!("stop backend {id}"), *yes)?;
            let mut client = authed_client(&cli)?;
            run_operation(
                &mut client,
                &format!("/api/v1/backends/{id}/stop"),
                serde_json::json!({ "graceful": graceful }),
            )
            .await?;
        }
        Command::BackendRestart {
            id,
            graceful,
            yes,
        } => {
            confirm(&format!("restart backend {id}"), *yes)?;
            let mut client = authed_client(&cli)?;
            run_operation(
                &mut client,
                &format!("/api/v1/backends/{id}/restart"),
                serde_json::json!({ "graceful": graceful }),
            )
            .await?;
        }
        Command::GatewayStatus => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/gateway/status").await?);
        }
        Command::GatewayStart => {
            let mut client = authed_client(&cli)?;
            run_operation(
                &mut client,
                "/api/v1/gateway/start",
                serde_json::json!({}),
            )
            .await?;
        }
        Command::GatewayStop { yes } => {
            confirm("stop gateway", *yes)?;
            let mut client = authed_client(&cli)?;
            run_operation(
                &mut client,
                "/api/v1/gateway/stop",
                serde_json::json!({}),
            )
            .await?;
        }
        Command::GatewayConfig => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/gateway/config").await?);
        }
        Command::Gpus => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/telemetry/gpus").await?);
        }
        Command::TelemetryHistory { range } => {
            let mut client = authed_client(&cli)?;
            print_json(
                &client
                    .get(&format!("/api/v1/telemetry/history/{range}"))
                    .await?,
            );
        }
        Command::Models => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/models").await?);
        }
        Command::Model { id } => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get(&format!("/api/v1/models/{id}")).await?);
        }
        Command::Logs {
            scope,
            level,
            search,
            limit,
        } => {
            let mut client = authed_client(&cli)?;
            // Scope is a required server-side parameter; "all"
            // covers every retained scope (see logs-scopes).
            let mut query = vec![format!(
                "scope={}",
                url_encode(scope.as_deref().unwrap_or("all"))
            )];
            if let Some(level) = level {
                query.push(format!("level={}", url_encode(level)));
            }
            if let Some(search) = search {
                query.push(format!("search={}", url_encode(search)));
            }
            if let Some(limit) = limit {
                query.push(format!("limit={limit}"));
            }
            let path = if query.is_empty() {
                "/api/v1/logs".to_string()
            } else {
                format!("/api/v1/logs?{}", query.join("&"))
            };
            print_json(&client.get(&path).await?);
        }
        Command::LogsScopes => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/logs/scopes").await?);
        }
        Command::Ops => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/operations").await?);
        }
        Command::Op { id } => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get(&format!("/api/v1/operations/{id}")).await?);
        }
        Command::OpPoll { id } => {
            let mut client = authed_client(&cli)?;
            let final_op = client
                .poll_operation(id, &mut |tick| {
                    eprintln!("  {} …", tick.status);
                })
                .await?;
            print_operation(&final_op);
        }
        Command::OpAbort { id } => {
            let mut client = authed_client(&cli)?;
            print_json(
                &client
                    .post(&format!("/api/v1/operations/{id}/abort"), serde_json::json!({}))
                    .await?,
            );
        }
        Command::Activation => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/activation-plan").await?);
        }
        Command::ActivationApply { mode, yes } => {
            if mode != "now" && mode != "drain" {
                return Err(ApiError::Input(
                    "activation mode must be \"now\" or \"drain\"".to_string(),
                ));
            }
            confirm("apply activation plan", *yes)?;
            let mut client = authed_client(&cli)?;
            run_operation(
                &mut client,
                "/api/v1/activation-plan/apply",
                serde_json::json!({ "mode": mode }),
            )
            .await?;
        }
        Command::ActivationHistory => {
            let mut client = authed_client(&cli)?;
            print_json(&client.get("/api/v1/activation-plan/history").await?);
        }
        Command::Auth(auth) => run_auth(&cli, auth).await?,
        Command::Verify {
            device_name,
            code_stdin,
        } => run_verify(&cli, device_name, *code_stdin).await?,
        Command::Tui => {
            let client = authed_client(&cli)?;
            tui::run(client).await?;
        }
    }
    Ok(())
}

fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn run_auth(cli: &Cli, auth: &AuthCommand) -> Result<(), ApiError> {
    match auth {
        AuthCommand::Enroll {
            device_name,
            code_stdin,
        } => {
            let code = secret_from(*code_stdin, ENROLL_ENV, "enrollment code")?;
            let mut client = public_client(cli)?;
            let pair = client.enroll(&code, Some(device_name)).await?;
            println!("{}", serde_json::to_string(&pair).unwrap());
            eprintln!("enrolled: pipe the token pair to the next command via stdin or env (never to disk)");
        }
        AuthCommand::Login {
            username,
            device_name,
            password_stdin,
            totp_stdin,
        } => {
            let password = secret_from(*password_stdin, PASSWORD_ENV, "password")?;
            let mut client = public_client(cli)?;
            match client.login(username, &password, Some(device_name)).await? {
                LoginOutcome::Pair(pair) => {
                    println!("{}", serde_json::to_string(&pair).unwrap());
                }
                LoginOutcome::TotpRequired { challenge_id } => {
                    let code = totp_code(*totp_stdin)?;
                    let pair = client.totp(&challenge_id, &code).await?;
                    println!("{}", serde_json::to_string(&pair).unwrap());
                }
            }
        }
        AuthCommand::Totp {
            challenge_id,
            code_stdin,
        } => {
            let code = secret_from(*code_stdin, TOTP_ENV, "TOTP code")?;
            let mut client = public_client(cli)?;
            let pair = client.totp(challenge_id, &code).await?;
            println!("{}", serde_json::to_string(&pair).unwrap());
        }
        AuthCommand::Me => {
            let mut client = authed_client(cli)?;
            print_json(&client.get("/api/v1/auth/me").await?);
        }
        AuthCommand::Logout => {
            let mut client = authed_client(cli)?;
            print_json(
                &client
                    .post("/api/v1/auth/logout", serde_json::json!({}))
                    .await?,
            );
        }
        AuthCommand::Sessions => {
            let mut client = authed_client(cli)?;
            print_json(&client.get("/api/v1/auth/sessions").await?);
        }
        AuthCommand::SessionRevoke { id } => {
            let mut client = authed_client(cli)?;
            let value = client
                .delete(&format!("/api/v1/auth/sessions/{id}"))
                .await?;
            print_json(&value);
        }
        AuthCommand::Refresh { token_stdin } => {
            let refresh = secret_from(*token_stdin, REFRESH_ENV, "refresh token")?;
            let mut client = public_client(cli)?;
            let pair = client.refresh(&refresh).await?;
            println!("{}", serde_json::to_string(&pair).unwrap());
        }
    }
    Ok(())
}

/// TOTP code resolution for the login flow: stdin flag, env var,
/// else an interactive TTY prompt (the code is a rotating 6-digit
/// value, so echo is acceptable; a non-TTY without a source errors
/// with the challenge id so `auth totp` can complete the login).
fn totp_code(totp_stdin: bool) -> Result<String, ApiError> {
    if totp_stdin {
        return read_stdin_secret("TOTP code");
    }
    if let Ok(code) = std::env::var(TOTP_ENV) {
        if !code.is_empty() {
            return Ok(code);
        }
    }
    if std::io::stdin().is_terminal() {
        eprint!("TOTP code: ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| ApiError::Input(format!("could not read TOTP code: {e}")))?;
        let code = line.trim().to_string();
        if code.is_empty() {
            return Err(ApiError::Input("empty TOTP code".to_string()));
        }
        return Ok(code);
    }
    Err(ApiError::Input(
        "TOTP required but no code source: set LLMDIL_TOTP_CODE, pass --totp-stdin, or run `auth totp` with the challenge id".to_string(),
    ))
}

/// Scripted verification (Stage 2 test client): enroll, then a full
/// read sweep. Each step prints PASS/FAIL; the exit code is 0 only
/// when every step passes. No TUI, no interaction.
async fn run_verify(
    cli: &Cli,
    device_name: &str,
    code_stdin: bool,
) -> Result<(), ApiError> {
    let code = secret_from(code_stdin, ENROLL_ENV, "enrollment code")?;
    let mut client = public_client(cli)?;
    let mut failures = 0usize;

    let pair = match client.enroll(&code, Some(device_name)).await {
        Ok(pair) => {
            println!("PASS enroll (201 + token pair)");
            pair
        }
        Err(e) => {
            println!("FAIL enroll: {e}");
            return Err(e);
        }
    };
    client.set_tokens(pair);

    let steps: &[(&str, &str)] = &[
        ("identity", "/api/v1/server"),
        ("status", "/api/v1/status"),
        ("backends", "/api/v1/backends"),
        ("gateway", "/api/v1/gateway/status"),
        ("gpus", "/api/v1/telemetry/gpus"),
        ("models", "/api/v1/models"),
        ("logs", "/api/v1/logs?scope=all&limit=1"),
    ];
    for (name, path) in steps {
        match client.get(path).await {
            Ok(_) => println!("PASS {name} ({path})"),
            Err(e) => {
                println!("FAIL {name} ({path}): {e}");
                failures += 1;
            }
        }
    }
    // The sweep above runs authenticated (the identity document is
    // public but also answers authenticated calls); re-check it
    // explicitly without a token to prove the public route.
    let mut anon = public_client(cli)?;
    match anon.server().await {
        Ok(doc) => {
            let api_version = doc
                .get("api_version")
                .and_then(Value::as_str)
                .unwrap_or("");
            if api_version == "1" {
                println!("PASS server identity (api_version 1)");
            } else {
                println!("FAIL server identity (api_version {api_version:?}, expected \"1\")");
                failures += 1;
            }
        }
        Err(e) => {
            println!("FAIL server identity: {e}");
            failures += 1;
        }
    }

    if failures == 0 {
        println!("VERIFY OK: all steps passed");
        Ok(())
    } else {
        Err(ApiError::Input(format!(
            "VERIFY FAILED: {failures} step(s) failed"
        )))
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Count stdin-secret flags across the whole invocation: only
    // one secret may come from stdin (stdin is read once).
    if stdin_secret_count(&cli) > 1 {
        eprintln!("error: only one --*-stdin secret flag per invocation (stdin is read once)");
        std::process::exit(2);
    }
    match run(cli).await {
        Ok(()) => {}
        Err(ApiError::Api(body)) if body.code == "BUSY" => {
            eprintln!(
                "already running: {} ({})",
                body.message,
                body.details
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default()
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn stdin_secret_count(cli: &Cli) -> usize {
    let mut count = usize::from(cli.token_stdin);
    if let Command::Auth(auth) = &cli.command {
        count += match auth {
            AuthCommand::Enroll { code_stdin, .. } => usize::from(*code_stdin),
            AuthCommand::Login {
                password_stdin,
                totp_stdin,
                ..
            } => usize::from(*password_stdin) + usize::from(*totp_stdin),
            AuthCommand::Totp { code_stdin, .. } => usize::from(*code_stdin),
            AuthCommand::Refresh { token_stdin } => usize::from(*token_stdin),
            _ => 0,
        };
    }
    if let Command::Verify { code_stdin, .. } = &cli.command {
        count += usize::from(*code_stdin);
    }
    count
}
