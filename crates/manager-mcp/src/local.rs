//! Local-process MCP servers derived from the persisted registry
//! (`AppConfig.mcp`, review v0.1.60 PS-5).
//!
//! Mapping (the council decision recorded in the review): a registry
//! entry whose exposure is `Local`, whose transport is `stdio` and
//! whose `command` is non-empty becomes a spawnable local process.
//! Every other `Local` combination is an explicit [`LocalSpecError`]:
//! an http transport, an empty command, a command outside the stdio
//! allowlist, a banned arg, a command path that is relative or
//! outside the known install directories, an invalid env key name,
//! or a needed credential that is missing from Credential Manager.
//! Gateway and Disabled entries are never app-spawned.
//!
//! Spawn rules (every caller that spawns an MCP server from config,
//! including any future Admin API endpoint that writes MCP server
//! config, must go through [`prepare_spawn`]):
//! - The program is launched directly (the supervisor's
//!   `tokio::process::Command`, i.e. `std::process::Command`), never
//!   through a shell such as `cmd /c` built by this code.
//! - A bare command name is resolved here against `PATH` (absolute
//!   entries only; on Windows only the `PATHEXT` extensions are tried,
//!   so `npx` resolves to `npx.cmd`, never to the extension-less
//!   POSIX shim next to it). A command given as a path must be
//!   absolute, free of `..`, and inside a known install directory.
//! - The stdio allowlist and the banned args are re-checked here, at
//!   spawn time, not only at save.
//! - Credential values are read only at spawn time, go only into the
//!   child's environment, and never appear in an error or a log line
//!   (errors name Credential Manager *targets* and env *keys* only).

use crate::{accepted_cred_targets, process_id, server_spec_from_config, ServerSpec};
use manager_config::{McpExposure, McpServerConfig, McpTransport};
use manager_supervisor::ProcessSpec;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Why a registry entry cannot run as an app-supervised local
/// process. Display strings name ids, commands, env keys and
/// credential targets only — never a credential value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocalSpecError {
    #[error("mcp server {id:?} is not Local exposure; only Local servers are app-supervised")]
    NotLocal { id: String },
    #[error(
        "mcp server {id:?} uses the http transport, which has no local process to run; \
         use a stdio command or gateway exposure"
    )]
    HttpTransport { id: String },
    #[error("mcp server {id:?} has no command; a Local stdio server needs one")]
    EmptyCommand { id: String },
    #[error("mcp server {id:?}: command basename {basename:?} is not in the stdio allowlist {allowlist:?}")]
    CommandNotAllowed {
        id: String,
        basename: String,
        allowlist: &'static [&'static str],
    },
    #[error("mcp server {id:?}: arg {banned:?} is banned")]
    BannedArg { id: String, banned: &'static str },
    #[error(
        "mcp server {id:?}: command {command:?} is a relative path; use a bare command \
         name (resolved from PATH) or an absolute path inside a known install directory"
    )]
    RelativeCommandPath { id: String, command: String },
    #[error(
        "mcp server {id:?}: command path {command:?} is not inside a known install \
         directory"
    )]
    CommandOutsideInstallDirs { id: String, command: String },
    #[error("mcp server {id:?}: command {command:?} was not found")]
    CommandNotFound { id: String, command: String },
    #[error("mcp server {id:?}: env key {key:?} is not a valid environment variable name")]
    InvalidEnvKey { id: String, key: String },
    #[error("mcp server {id:?} needs a credential for {keys:?} but names no cred_ref")]
    NoCredentialTarget { id: String, keys: Vec<String> },
    #[error(
        "mcp server {id:?}: credential missing from Credential Manager (looked for \
         {targets:?}); store it first"
    )]
    CredentialMissing { id: String, targets: Vec<String> },
}

/// What [`resolve_command`] needs from the environment, injectable
/// for tests. [`CommandContext::from_env`] builds the real one.
#[derive(Debug, Clone, Default)]
pub struct CommandContext {
    /// The `PATH` value searched for a bare command name.
    pub path: Option<OsString>,
    /// The `PATHEXT` value (Windows); `None` uses the Windows default.
    pub pathext: Option<OsString>,
    /// Directories an absolute command path must be inside.
    pub install_dirs: Vec<PathBuf>,
    /// Windows resolution rules (PATHEXT extensions only, case-
    /// insensitive install-dir comparison).
    pub windows: bool,
}

/// The Windows default `PATHEXT`, used when the variable is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

impl CommandContext {
    /// The real context: this process's `PATH`/`PATHEXT`, and the
    /// known install directories — the per-machine program
    /// directories, the per-user ones Node.js / Python / uv / npm
    /// install into, plus `extra_install_dirs` (the app passes its
    /// data directory).
    pub fn from_env(extra_install_dirs: &[PathBuf]) -> Self {
        let mut install_dirs: Vec<PathBuf> = Vec::new();
        let env_dir = |name: &str| std::env::var_os(name).map(PathBuf::from);
        if cfg!(windows) {
            for name in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
                install_dirs.extend(env_dir(name));
            }
            if let Some(local) = env_dir("LOCALAPPDATA") {
                install_dirs.push(local.join("Programs"));
            }
            if let Some(roaming) = env_dir("APPDATA") {
                install_dirs.push(roaming.join("npm"));
            }
            if let Some(home) = env_dir("USERPROFILE") {
                install_dirs.push(home.join(".local").join("bin"));
            }
        } else {
            for dir in ["/usr/bin", "/usr/local/bin", "/opt"] {
                install_dirs.push(PathBuf::from(dir));
            }
            if let Some(home) = env_dir("HOME") {
                install_dirs.push(home.join(".local").join("bin"));
            }
        }
        install_dirs.extend(extra_install_dirs.iter().cloned());
        install_dirs.retain(|d| d.is_absolute());
        Self {
            path: std::env::var_os("PATH"),
            pathext: std::env::var_os("PATHEXT"),
            install_dirs,
            windows: cfg!(windows),
        }
    }
}

/// True when `command` names a path rather than a bare command.
fn is_path_like(command: &str) -> bool {
    command.contains(['/', '\\']) || Path::new(command).is_absolute()
}

/// A valid environment variable name for a child process: non-empty,
/// no `=`, no NUL, no whitespace.
fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key
            .chars()
            .any(|c| c == '=' || c == '\0' || c.is_whitespace())
}

/// The static checks: the registry entry is a Local stdio server with
/// a non-empty, allowlisted command, no banned args, no relative
/// command path, and valid env key names. Returns its runtime view
/// (command trimmed). Nothing touches the filesystem or the
/// credential store here, so this is the save-time gate too.
pub fn local_launch(cfg: &McpServerConfig) -> Result<ServerSpec, LocalSpecError> {
    let id = cfg.id.clone();
    if cfg.exposure != McpExposure::Local {
        return Err(LocalSpecError::NotLocal { id });
    }
    if cfg.transport != McpTransport::Stdio {
        return Err(LocalSpecError::HttpTransport { id });
    }
    let command = cfg
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| LocalSpecError::EmptyCommand { id: id.clone() })?
        .to_string();
    if !manager_config::mcp_stdio_command_allowed(&command) {
        return Err(LocalSpecError::CommandNotAllowed {
            id,
            basename: manager_config::mcp_command_basename(&command),
            allowlist: manager_config::MCP_STDIO_COMMAND_ALLOWLIST,
        });
    }
    if let Some(banned) = cfg
        .args
        .iter()
        .find_map(|a| manager_config::mcp_banned_arg(a))
    {
        return Err(LocalSpecError::BannedArg { id, banned });
    }
    if is_path_like(&command) && !Path::new(&command).is_absolute() {
        return Err(LocalSpecError::RelativeCommandPath { id, command });
    }
    if let Some(key) = cfg.env_keys.iter().find(|k| !valid_env_key(k)) {
        return Err(LocalSpecError::InvalidEnvKey {
            id,
            key: key.clone(),
        });
    }
    let mut spec = server_spec_from_config(cfg);
    spec.command = Some(command);
    Ok(spec)
}

/// The Credential Manager targets a Local server's credential may be
/// read from, in preference order (see [`accepted_cred_targets`]).
/// Empty when the server needs no credential (no `env_keys`); an
/// error when it needs one but names no `cred_ref`.
pub fn required_credential_targets(spec: &ServerSpec) -> Result<Vec<String>, LocalSpecError> {
    if spec.env_keys.is_empty() {
        return Ok(Vec::new());
    }
    if spec
        .cred_ref
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(LocalSpecError::NoCredentialTarget {
            id: spec.id.clone(),
            keys: spec.env_keys.clone(),
        });
    }
    Ok(accepted_cred_targets(spec))
}

/// Presence-only credential check (no value is read): Ok when the
/// server needs no credential or one of its accepted targets exists.
pub fn check_credential_presence<F>(spec: &ServerSpec, mut exists: F) -> Result<(), LocalSpecError>
where
    F: FnMut(&str) -> bool,
{
    let targets = required_credential_targets(spec)?;
    if targets.is_empty() || targets.iter().any(|t| exists(t)) {
        return Ok(());
    }
    Err(LocalSpecError::CredentialMissing {
        id: spec.id.clone(),
        targets,
    })
}

/// Lowercased components when comparing case-insensitively.
fn comparable_components(p: &Path, fold_case: bool) -> Vec<String> {
    p.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().into_owned();
            if fold_case {
                s.to_lowercase()
            } else {
                s
            }
        })
        .collect()
}

/// True when absolute `path` (no `..` / `.` components) lies strictly
/// inside `dir`.
fn path_within(path: &Path, dir: &Path, fold_case: bool) -> bool {
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let p = comparable_components(path, fold_case);
    let d = comparable_components(dir, fold_case);
    !d.is_empty() && p.len() > d.len() && p[..d.len()] == d[..]
}

/// Resolve the program for a [`local_launch`]-validated server to an
/// absolute path (see the module doc for the rules). The allowlist is
/// re-checked against the RESOLVED file name too, so a `PATHEXT`
/// entry such as `.JS` can never turn `npx` into `npx.js`.
pub fn resolve_command(spec: &ServerSpec, ctx: &CommandContext) -> Result<PathBuf, LocalSpecError> {
    let id = spec.id.clone();
    let command = spec
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| LocalSpecError::EmptyCommand { id: id.clone() })?;
    let not_found = || LocalSpecError::CommandNotFound {
        id: id.clone(),
        command: command.to_string(),
    };
    let resolved = if is_path_like(command) {
        let path = Path::new(command);
        if !path.is_absolute() {
            return Err(LocalSpecError::RelativeCommandPath {
                id,
                command: command.to_string(),
            });
        }
        if !ctx
            .install_dirs
            .iter()
            .any(|d| path_within(path, d, ctx.windows))
        {
            return Err(LocalSpecError::CommandOutsideInstallDirs {
                id,
                command: command.to_string(),
            });
        }
        if !path.is_file() {
            return Err(not_found());
        }
        path.to_path_buf()
    } else {
        let candidates: Vec<String> = if ctx.windows {
            let pathext = ctx
                .pathext
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| DEFAULT_PATHEXT.to_string());
            let exts: Vec<String> = pathext
                .split(';')
                .map(str::trim)
                .filter(|e| e.starts_with('.') && e.len() > 1)
                .map(str::to_lowercase)
                .collect();
            let lower = command.to_lowercase();
            if exts.iter().any(|e| lower.ends_with(e.as_str())) {
                vec![command.to_string()]
            } else {
                exts.iter().map(|e| format!("{command}{e}")).collect()
            }
        } else {
            vec![command.to_string()]
        };
        let path_var = ctx.path.clone().unwrap_or_default();
        std::env::split_paths(&path_var)
            // Relative PATH entries (".", "") would resolve against
            // the app's working directory: never searched.
            .filter(|dir| dir.is_absolute())
            .flat_map(|dir| {
                candidates
                    .iter()
                    .map(move |c| dir.join(c))
                    .collect::<Vec<_>>()
            })
            .find(|p| p.is_file())
            .ok_or_else(not_found)?
    };
    let file_name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !manager_config::mcp_stdio_command_allowed(&file_name) {
        return Err(LocalSpecError::CommandNotAllowed {
            id,
            basename: manager_config::mcp_command_basename(&file_name),
            allowlist: manager_config::MCP_STDIO_COMMAND_ALLOWLIST,
        });
    }
    Ok(resolved)
}

/// The supervisor spec for a Local server: [`crate::process_spec_for`]'s
/// shape with the resolved `program` and the spawn-time `env`.
pub fn local_process_spec(
    spec: &ServerSpec,
    program: PathBuf,
    env: Vec<(String, String)>,
) -> ProcessSpec {
    ProcessSpec {
        id: process_id(&spec.id),
        program,
        args: spec.args.clone(),
        env,
        workdir: None,
        port: None,
        low_priority: false,
        stdout_log: None,
        stderr_log: None,
    }
}

/// Everything needed to spawn a registry entry locally, in order:
/// the static checks ([`local_launch`]), command resolution
/// ([`resolve_command`]), then the credential — `read` is asked for
/// each accepted target in preference order and the first non-empty
/// value is injected under every `env_keys` name (the gateway's
/// convention, MCP design §4.2). The returned spec carries that value
/// in `env`: hand it to the supervisor and drop it; never log it.
pub fn prepare_spawn<F>(
    cfg: &McpServerConfig,
    ctx: &CommandContext,
    mut read: F,
) -> Result<ProcessSpec, LocalSpecError>
where
    F: FnMut(&str) -> Option<String>,
{
    let spec = local_launch(cfg)?;
    let program = resolve_command(&spec, ctx)?;
    let targets = required_credential_targets(&spec)?;
    let env = if targets.is_empty() {
        Vec::new()
    } else {
        let value = targets
            .iter()
            .find_map(|t| read(t).filter(|v| !v.is_empty()))
            .ok_or_else(|| LocalSpecError::CredentialMissing {
                id: spec.id.clone(),
                targets: targets.clone(),
            })?;
        spec.env_keys
            .iter()
            .map(|k| (k.clone(), value.clone()))
            .collect()
    };
    Ok(local_process_spec(&spec, program, env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{default_servers, process_spec_for, McpMode};

    fn seeded(id: &str) -> McpServerConfig {
        manager_config::seeded_mcp_servers()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap()
    }

    fn local(mut cfg: McpServerConfig) -> McpServerConfig {
        cfg.exposure = McpExposure::Local;
        cfg
    }

    /// A scratch directory holding empty files with the given names.
    fn scratch(tag: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "manager-mcp-local-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        dir
    }

    fn ctx_with_path(dir: &Path, windows: bool) -> CommandContext {
        CommandContext {
            path: Some(std::env::join_paths([dir]).unwrap()),
            pathext: None,
            install_dirs: Vec::new(),
            windows,
        }
    }

    /// Golden regression (PS-5): the specs derived from the seeded
    /// registry for the two built-in servers are the built-in specs
    /// of `default_servers()` — same id, name, transport, mode,
    /// command, args, package, and the same supervisor launch shape.
    /// The only intended divergences are the §10.1 unifications (the
    /// credential target namespace, with the legacy target still
    /// accepted, and the canonical Drive env key); before PS-5 no env
    /// was injected at all, so neither changes an existing launch.
    #[test]
    fn derived_builtin_specs_match_the_builtin_specs() {
        for builtin in default_servers() {
            let derived = local_launch(&seeded(&builtin.id)).expect("seeded builtin is runnable");
            assert_eq!(derived.id, builtin.id);
            assert_eq!(derived.name, builtin.name);
            assert_eq!(derived.transport, builtin.transport);
            assert_eq!(derived.mode, builtin.mode);
            assert_eq!(derived.mode, McpMode::Local);
            assert_eq!(derived.command, builtin.command);
            assert_eq!(derived.args, builtin.args);
            assert_eq!(derived.package, builtin.package);
            assert_eq!(derived.url, builtin.url);
            let a = process_spec_for(&derived).unwrap();
            let b = process_spec_for(&builtin).unwrap();
            assert_eq!(
                (
                    a.id,
                    a.program,
                    a.args,
                    a.env,
                    a.workdir,
                    a.port,
                    a.low_priority
                ),
                (
                    b.id,
                    b.program,
                    b.args,
                    b.env,
                    b.workdir,
                    b.port,
                    b.low_priority
                )
            );
            // The legacy credential target stays accepted.
            let legacy = builtin.cred_ref.clone().unwrap();
            assert!(accepted_cred_targets(&derived).contains(&legacy));
            // local_process_spec with the unresolved program and no
            // env is exactly the built-in launch shape.
            let unresolved = local_process_spec(
                &derived,
                PathBuf::from(derived.command.clone().unwrap()),
                Vec::new(),
            );
            let b = process_spec_for(&builtin).unwrap();
            assert_eq!(unresolved.argv(), b.argv());
            assert_eq!(unresolved.id, b.id);
        }
    }

    #[test]
    fn curated_server_becomes_spawnable_when_local() {
        let spec = local_launch(&local(seeded("brave_search"))).unwrap();
        assert_eq!(spec.mode, McpMode::Local);
        assert_eq!(spec.command.as_deref(), Some("npx"));
        assert!(
            local_launch(&seeded("brave_search")).is_err(),
            "Gateway is not spawnable"
        );
    }

    #[test]
    fn every_other_local_combination_is_an_explicit_error() {
        let base = local(seeded("brave_search"));

        let mut http = base.clone();
        http.transport = McpTransport::Http;
        http.command = None;
        http.url = Some("https://mcp.example.com".to_string());
        assert!(matches!(
            local_launch(&http),
            Err(LocalSpecError::HttpTransport { .. })
        ));

        for empty in [None, Some(String::new()), Some("   ".to_string())] {
            let mut c = base.clone();
            c.command = empty;
            assert!(matches!(
                local_launch(&c),
                Err(LocalSpecError::EmptyCommand { .. })
            ));
        }

        let mut not_allowed = base.clone();
        not_allowed.command = Some("powershell".to_string());
        assert!(matches!(
            local_launch(&not_allowed),
            Err(LocalSpecError::CommandNotAllowed { .. })
        ));

        let mut banned = base.clone();
        banned.args.push("--allow-private-ips=1".to_string());
        assert!(matches!(
            local_launch(&banned),
            Err(LocalSpecError::BannedArg { .. })
        ));

        let mut relative = base.clone();
        relative.command = Some("tools/npx".to_string());
        assert!(matches!(
            local_launch(&relative),
            Err(LocalSpecError::RelativeCommandPath { .. })
        ));
        let mut dotted = base.clone();
        dotted.command = Some("..\\npx.cmd".to_string());
        assert!(matches!(
            local_launch(&dotted),
            Err(LocalSpecError::RelativeCommandPath { .. })
        ));

        let mut bad_key = base.clone();
        bad_key.env_keys = vec!["A=B".to_string()];
        assert!(matches!(
            local_launch(&bad_key),
            Err(LocalSpecError::InvalidEnvKey { .. })
        ));

        let mut no_ref = base.clone();
        no_ref.cred_ref = None;
        let spec = local_launch(&no_ref).unwrap();
        assert!(matches!(
            required_credential_targets(&spec),
            Err(LocalSpecError::NoCredentialTarget { .. })
        ));

        let spec = local_launch(&base).unwrap();
        assert!(matches!(
            check_credential_presence(&spec, |_| false),
            Err(LocalSpecError::CredentialMissing { .. })
        ));
        assert!(check_credential_presence(&spec, |_| true).is_ok());
        // No env keys: no credential needed at all.
        let fetch = local_launch(&local(seeded("web_fetch"))).unwrap();
        assert!(check_credential_presence(&fetch, |_| false).is_ok());

        let mut disabled = base.clone();
        disabled.exposure = McpExposure::Disabled;
        assert!(matches!(
            local_launch(&disabled),
            Err(LocalSpecError::NotLocal { .. })
        ));
    }

    #[test]
    fn windows_resolution_uses_pathext_never_the_bare_shim() {
        // nodejs ships both `npx` (a POSIX shell shim) and `npx.cmd`.
        let dir = scratch("pathext", &["npx", "npx.cmd"]);
        let spec = local_launch(&local(seeded("brave_search"))).unwrap();
        let got = resolve_command(&spec, &ctx_with_path(&dir, true)).unwrap();
        assert_eq!(got, dir.join("npx.cmd"));
        // An explicit allowed extension resolves as named.
        let mut named = spec.clone();
        named.command = Some("npx.cmd".to_string());
        assert_eq!(
            resolve_command(&named, &ctx_with_path(&dir, true)).unwrap(),
            dir.join("npx.cmd")
        );
        // A PATHEXT entry that is not an allowlisted form is refused.
        let js_dir = scratch("pathext-js", &["npx.js"]);
        let mut ctx = ctx_with_path(&js_dir, true);
        ctx.pathext = Some(OsString::from(".JS"));
        assert!(matches!(
            resolve_command(&spec, &ctx),
            Err(LocalSpecError::CommandNotAllowed { .. })
        ));
        // Not on PATH at all.
        let empty = scratch("pathext-empty", &[]);
        assert!(matches!(
            resolve_command(&spec, &ctx_with_path(&empty, true)),
            Err(LocalSpecError::CommandNotFound { .. })
        ));
        for d in [dir, js_dir, empty] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn non_windows_resolution_uses_the_bare_name() {
        let dir = scratch("bare", &["uvx"]);
        let spec = local_launch(&local(seeded("web_fetch"))).unwrap();
        assert_eq!(
            resolve_command(&spec, &ctx_with_path(&dir, false)).unwrap(),
            dir.join("uvx")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn relative_path_entries_are_never_searched() {
        let spec = local_launch(&local(seeded("web_fetch"))).unwrap();
        let ctx = CommandContext {
            path: Some(OsString::from(".")),
            pathext: None,
            install_dirs: Vec::new(),
            windows: cfg!(windows),
        };
        assert!(matches!(
            resolve_command(&spec, &ctx),
            Err(LocalSpecError::CommandNotFound { .. })
        ));
    }

    #[test]
    fn absolute_command_must_be_inside_an_install_dir() {
        let install = scratch("install", &["node.exe", "node"]);
        let elsewhere = scratch("elsewhere", &["node.exe", "node"]);
        let file = if cfg!(windows) { "node.exe" } else { "node" };
        let mut cfg = local(seeded("web_fetch"));
        let ctx = CommandContext {
            path: None,
            pathext: None,
            install_dirs: vec![install.clone()],
            windows: cfg!(windows),
        };

        cfg.command = Some(install.join(file).to_string_lossy().into_owned());
        let spec = local_launch(&cfg).unwrap();
        assert_eq!(resolve_command(&spec, &ctx).unwrap(), install.join(file));

        cfg.command = Some(elsewhere.join(file).to_string_lossy().into_owned());
        let spec = local_launch(&cfg).unwrap();
        assert!(matches!(
            resolve_command(&spec, &ctx),
            Err(LocalSpecError::CommandOutsideInstallDirs { .. })
        ));

        // `..` cannot climb out of an install dir.
        let climbing = install
            .join("..")
            .join(elsewhere.file_name().unwrap())
            .join(file);
        cfg.command = Some(climbing.to_string_lossy().into_owned());
        let spec = local_launch(&cfg).unwrap();
        assert!(matches!(
            resolve_command(&spec, &ctx),
            Err(LocalSpecError::CommandOutsideInstallDirs { .. })
        ));
        for d in [install, elsewhere] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn prepare_spawn_injects_the_credential_only_into_env() {
        let dir = scratch("prepare", &["npx", "npx.cmd"]);
        let ctx = ctx_with_path(&dir, cfg!(windows));
        let cfg = local(seeded("brave_search"));
        let secret = "s3cr3t-value-never-logged";
        let mut asked: Vec<String> = Vec::new();
        let spec = prepare_spawn(&cfg, &ctx, |t| {
            asked.push(t.to_string());
            Some(secret.to_string())
        })
        .unwrap();
        assert_eq!(
            asked,
            vec![manager_config::BRAVE_SEARCH_MCP_CRED_REF.to_string()]
        );
        assert_eq!(spec.id, "mcp-brave_search");
        assert!(spec.program.is_absolute());
        assert_eq!(spec.args, cfg.args);
        assert_eq!(
            spec.env,
            vec![("BRAVE_API_KEY".to_string(), secret.to_string())]
        );
        assert!(!spec.argv().iter().any(|a| a.contains(secret)));

        // Missing credential: explicit error naming the target only.
        let err = prepare_spawn(&cfg, &ctx, |_| None).unwrap_err();
        assert!(matches!(err, LocalSpecError::CredentialMissing { .. }));
        // An empty stored value counts as missing.
        let err = prepare_spawn(&cfg, &ctx, |_| Some(String::new())).unwrap_err();
        assert!(matches!(err, LocalSpecError::CredentialMissing { .. }));

        // Legacy servers fall back to the legacy target, in order.
        let gh = seeded("github");
        let mut asked: Vec<String> = Vec::new();
        let spec = prepare_spawn(&gh, &ctx, |t| {
            asked.push(t.to_string());
            (t == manager_config::LEGACY_GITHUB_MCP_CRED_REF).then(|| secret.to_string())
        })
        .unwrap();
        assert_eq!(
            asked,
            vec![
                manager_config::GITHUB_MCP_CRED_REF.to_string(),
                manager_config::LEGACY_GITHUB_MCP_CRED_REF.to_string()
            ]
        );
        assert_eq!(spec.env[0].0, "GITHUB_PERSONAL_ACCESS_TOKEN");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn errors_never_carry_a_credential_value() {
        let secret = "s3cr3t-value-never-logged";
        let dir = scratch("errors", &[]);
        let ctx = ctx_with_path(&dir, cfg!(windows));
        // Command not found happens before any credential read.
        let mut read_called = false;
        let err = prepare_spawn(&local(seeded("brave_search")), &ctx, |_| {
            read_called = true;
            Some(secret.to_string())
        })
        .unwrap_err();
        assert!(
            !read_called,
            "no credential is read for a server that cannot spawn"
        );
        assert!(!err.to_string().contains(secret));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spawn_time_recheck_catches_a_config_that_skipped_validation() {
        // A config edited on disk (never through validate) still
        // cannot launch a non-allowlisted command or a banned arg.
        let dir = scratch("recheck", &["cmd.exe", "cmd"]);
        let ctx = ctx_with_path(&dir, cfg!(windows));
        let mut cfg = local(seeded("web_fetch"));
        cfg.command = Some("cmd".to_string());
        assert!(matches!(
            prepare_spawn(&cfg, &ctx, |_| None),
            Err(LocalSpecError::CommandNotAllowed { .. })
        ));
        let mut cfg = local(seeded("web_fetch"));
        cfg.args.push("--ignore-robots-txt".to_string());
        assert!(matches!(
            prepare_spawn(&cfg, &ctx, |_| None),
            Err(LocalSpecError::BannedArg { .. })
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
