//! Gateway launch plumbing shared with the app: the service launch spec and the probe host.

use super::*;

/// Transform a fully built gateway spawn command into the service
/// launch spec for its definition. The argv and non-secret env carry
/// over verbatim; the secret-bearing values become credential
/// references the service host resolves at start, so a secret value
/// is never written into the definition file: the master key and the
/// OpenRouter key by their Credential Manager targets, `DATABASE_URL`
/// as the composable Postgres form (password credential + port), and
/// every armed gateway MCP server's credential under each of its
/// env-key names by that server's `cred_ref` target (code-review
/// B-7). The MCP mapping is safe for arbitrary env names because
/// the spawn env only carries an MCP env key when the arming pass
/// resolved that server's credential into it, and the service host
/// resolves any `CredentialRef` entry generically by target
/// (`manager_services::resolve_env` is not name-locked).
pub(crate) fn gateway_launch_spec(
    cmd: &PlannedCommand,
    config: &AppConfig,
) -> manager_services::LaunchSpec {
    // Env-key name -> Credential Manager target, for every
    // gateway-exposed MCP server that names a credential. The first
    // server wins on a shared env-key name (the spawn env itself
    // holds only one value per name, from the same iteration).
    let mut mcp_cred_refs: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for server in &config.mcp.servers {
        if server.exposure != manager_config::McpExposure::Gateway {
            continue;
        }
        let Some(target) = server.cred_ref.as_deref() else {
            continue;
        };
        for key in &server.env_keys {
            mcp_cred_refs.entry(key.as_str()).or_insert(target);
        }
    }
    let mut env: Vec<manager_services::EnvEntry> = cmd
        .env
        .iter()
        .map(|(name, value)| match name.as_str() {
            "LITELLM_MASTER_KEY" => manager_services::EnvEntry::credential_ref(
                name,
                crate::LITELLM_MASTER_KEY_CRED_TARGET,
            ),
            "DATABASE_URL" => manager_services::EnvEntry {
                name: name.clone(),
                value: manager_services::EnvValue::DatabaseUrl {
                    password_credential: crate::POSTGRES_PASSWORD_CRED_TARGET.to_string(),
                    port: config.postgres.port,
                },
            },
            "OPENROUTER_API_KEY" => match &config.gateway.openrouter.cred_ref {
                Some(target) => manager_services::EnvEntry::credential_ref(name, target),
                None => manager_services::EnvEntry::literal(name, value.clone()),
            },
            _ => match mcp_cred_refs.get(name.as_str()) {
                Some(target) => manager_services::EnvEntry::credential_ref(name, target),
                None => manager_services::EnvEntry::literal(name, value.clone()),
            },
        })
        .collect();
    env.sort_by(|a, b| a.name.cmp(&b.name));
    let (program, args) = match cmd.argv.split_first() {
        Some((program, rest)) => (PathBuf::from(program), rest.to_vec()),
        None => (PathBuf::new(), Vec::new()),
    };
    manager_services::LaunchSpec {
        program,
        args,
        env,
        workdir: cmd.cwd.clone(),
    }
}

/// The gateway's service launch spec built from configuration alone,
/// without a wizard context: the Settings register action needs a
/// definition before any start-gateway run exists. This mirrors
/// `StartGateway::spawn_command` + `spawn_command_with_env` with the
/// same secret-as-reference transform. It is a starting point, not a
/// second source of truth: the start-gateway step regenerates the
/// definition from its fully resolved spawn command on every run in
/// service mode.
pub fn gateway_service_launch(config: &AppConfig, data_dir: &Path) -> manager_services::LaunchSpec {
    let venv = data_dir.join("python").join("venv");
    let python = if cfg!(windows) {
        // Same interpreter choice as StartGateway::gateway_python:
        // python.exe, not pythonw — the GUI-subsystem interpreter
        // cannot hand a hidden console to the gateway's console
        // children (notably the Prisma query engine), so it would
        // keep the visible-engine-window defect this spec exists
        // to avoid. See that function's comment.
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    };
    let cfg_path = data_dir.join("gateway").join("config.yaml");
    let args = vec![
        "-c".to_string(),
        StartGateway::GATEWAY_BOOTSTRAP.to_string(),
        "--config".to_string(),
        cfg_path.display().to_string(),
        "--host".to_string(),
        config.gateway.bind_host.clone(),
        "--port".to_string(),
        config.gateway.port.to_string(),
    ];
    let mut env = vec![
        manager_services::EnvEntry::literal("PYTHONIOENCODING", "utf-8".to_string()),
        manager_services::EnvEntry::literal("PYTHONUTF8", "1".to_string()),
        manager_services::EnvEntry::credential_ref(
            "LITELLM_MASTER_KEY",
            crate::LITELLM_MASTER_KEY_CRED_TARGET,
        ),
    ];
    // PostgreSQL installed (binaries plus cluster or marker — the
    // same ground truth `postgres_installed` uses) → the gateway
    // receives a DATABASE_URL composed at service start.
    let postgres_present = postgres_bin_dir(data_dir).join("pg_ctl.exe").exists()
        && (postgres_marker_path(data_dir).exists()
            || postgres_data_dir(data_dir).join("PG_VERSION").exists());
    if postgres_present {
        env.push(manager_services::EnvEntry {
            name: "DATABASE_URL".to_string(),
            value: manager_services::EnvValue::DatabaseUrl {
                password_credential: crate::POSTGRES_PASSWORD_CRED_TARGET.to_string(),
                port: config.postgres.port,
            },
        });
    }
    if config.gateway.openrouter.enabled {
        if let Some(target) = &config.gateway.openrouter.cred_ref {
            env.push(manager_services::EnvEntry::credential_ref(
                "OPENROUTER_API_KEY",
                target,
            ));
        }
    }
    manager_services::LaunchSpec {
        program: python,
        args,
        env,
        workdir: None,
    }
}

/// The address the app itself uses to reach the gateway: loopback
/// whenever the bind covers it (127.0.0.1 and the all-interfaces
/// 0.0.0.0 / :: forms), otherwise the configured bind address
/// itself. Probing through loopback keeps health checks working
/// regardless of LAN state.
pub fn gateway_probe_host(config: &AppConfig) -> String {
    probe_host_for_bind(&config.gateway.bind_host)
}

/// [`gateway_probe_host`] for a bind address alone.
pub fn probe_host_for_bind(bind_host: &str) -> String {
    match bind_host {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        other => other.to_string(),
    }
}
