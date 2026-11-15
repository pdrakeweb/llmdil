//! Hash-locked requirements for the gateway venv (finding F1 follow-up,
//! `docs/code-review-v0.1.60.md`).
//!
//! The version pin ([`manager_config::DEFAULT_LITELLM_PIN`]) decides WHICH
//! LiteLLM is installed; the lock decides that the bytes installed are the
//! bytes that were reviewed. [`GATEWAY_REQUIREMENTS_LOCK`] is the
//! `uv pip compile --generate-hashes` resolution of
//! `assets/gateway-requirements.in` (`litellm[proxy]`, `prisma`,
//! `prometheus-client`) for CPython [`crate::GATEWAY_PYTHON_VERSION`] on
//! Windows x86_64, every package `==`-pinned with its `--hash=sha256:` lines.
//!
//! The start-gateway step installs with
//! `uv pip install --require-hashes -r <data_dir>/python/gateway-requirements.lock`
//! when the configured LiteLLM pin equals the lock's litellm version
//! ([`gateway_hash_lock_status`]). Any other pin (an operator "pin & apply"
//! from Component Updates) falls back to the plain pinned install and the
//! step outcome says it was **not hash-verified** ([`gateway_hash_lock_warning`]).
//!
//! # Regenerating
//!
//! After bumping `DEFAULT_LITELLM_PIN` or [`crate::PRISMA_PINNED_VERSION`]:
//!
//! 1. edit `crates/manager-wizard/assets/gateway-requirements.in` to the new pins;
//! 2. run `tools/gateway-lock/Update-GatewayLock.ps1` (wraps
//!    `uv pip compile crates/manager-wizard/assets/gateway-requirements.in
//!    --generate-hashes --python-version 3.12 --python-platform
//!    x86_64-pc-windows-msvc --output-file crates/manager-wizard/assets/gateway-requirements.lock`);
//! 3. `cargo test -p manager-wizard gateway_lock` — the
//!    `checked_in_gateway_lock_matches_pins` test fails while the `.in`
//!    pins, the lock, and the Rust constants disagree.
//!
//! `Update-GatewayLock.ps1 -Check` re-resolves and fails when the
//! checked-in lock is stale. Hashes are never edited by hand.

/// The checked-in hash lock, embedded so the installer never depends on a
/// file that could be swapped next to the binary.
pub const GATEWAY_REQUIREMENTS_LOCK: &str = include_str!("../assets/gateway-requirements.lock");

/// The lock's input (the three top-level requirements). Only the pin test
/// reads it; it is embedded so that test needs no file-system path.
#[cfg(test)]
pub(crate) const GATEWAY_REQUIREMENTS_IN: &str = include_str!("../assets/gateway-requirements.in");

/// File name the lock is materialized under (next to the venv) for
/// `uv pip install -r`.
pub const GATEWAY_LOCK_FILE_NAME: &str = "gateway-requirements.lock";

/// One `name==version --hash=...` entry of a requirements lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedRequirement {
    /// PEP 503-normalized name (lowercase, runs of `-_.` become `-`),
    /// extras stripped.
    pub name: String,
    pub version: String,
    /// `sha256:<hex>` values.
    pub hashes: Vec<String>,
}

/// PEP 503 name normalization.
pub fn normalize_package_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
            continue;
        }
        if sep && !out.is_empty() {
            out.push('-');
        }
        sep = false;
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Strip a requirements-file comment: `#` at the start of a line or
/// after whitespace.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

/// Parse a hash-locked requirements file (the subset `uv pip compile
/// --generate-hashes` writes). Strict: every requirement must be
/// `==`-pinned and carry at least one well-formed `sha256` hash, and any
/// other option line is rejected — a lock this parser cannot vouch for
/// is never handed to the installer. A file with no requirements (the
/// placeholder) parses to an empty list.
pub fn parse_requirements_lock(text: &str) -> Result<Vec<LockedRequirement>, String> {
    // Join `\` continuations into logical lines, dropping comments.
    let mut logical: Vec<String> = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw.trim_end_matches('\r'));
        let trimmed = line.trim_end();
        if let Some(body) = trimmed.strip_suffix('\\') {
            current.push_str(body);
            current.push(' ');
            continue;
        }
        current.push_str(trimmed);
        if !current.trim().is_empty() {
            logical.push(current.trim().to_string());
        }
        current.clear();
    }
    if !current.trim().is_empty() {
        logical.push(current.trim().to_string());
    }

    let mut out: Vec<LockedRequirement> = Vec::new();
    for line in logical {
        if line.starts_with('-') {
            return Err(format!("unsupported option line in the lock: {line}"));
        }
        let (spec_part, opts) = match line.find(" --") {
            Some(i) => (&line[..i], &line[i..]),
            None => (line.as_str(), ""),
        };
        // Environment markers (`; sys_platform == ...`) are not part of the pin.
        let spec = spec_part.split(';').next().unwrap_or("").trim();
        let (name_part, version) = spec
            .split_once("==")
            .ok_or_else(|| format!("requirement is not ==-pinned: {spec}"))?;
        let version = version.trim();
        if version.is_empty()
            || version.contains('*')
            || version.contains(|c: char| c.is_whitespace() || c == ',')
        {
            return Err(format!("requirement is not pinned to one version: {spec}"));
        }
        let name = normalize_package_name(name_part.split('[').next().unwrap_or(""));
        if name.is_empty() {
            return Err(format!("requirement has no name: {spec}"));
        }
        let mut hashes = Vec::new();
        for opt in opts.split_whitespace() {
            let value = opt
                .strip_prefix("--hash=")
                .ok_or_else(|| format!("unsupported option `{opt}` on {name}"))?;
            let hex = value
                .strip_prefix("sha256:")
                .ok_or_else(|| format!("{name}: only sha256 hashes are accepted, got {value}"))?;
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("{name}: malformed sha256 hash {hex}"));
            }
            hashes.push(format!("sha256:{}", hex.to_ascii_lowercase()));
        }
        if hashes.is_empty() {
            return Err(format!("{name}=={version} has no --hash"));
        }
        if out.iter().any(|r| r.name == name) {
            return Err(format!("{name} is listed twice"));
        }
        out.push(LockedRequirement {
            name,
            version: version.to_string(),
            hashes,
        });
    }
    Ok(out)
}

/// Whether `lock_text` can hash-verify an install of LiteLLM
/// `litellm_version`: it parses, pins litellm to exactly that version,
/// pins prisma to [`crate::PRISMA_PINNED_VERSION`], and carries
/// prometheus-client. `Err` says why not, in operator words.
pub fn hash_lock_status_for(lock_text: &str, litellm_version: &str) -> Result<(), String> {
    let reqs = parse_requirements_lock(lock_text)
        .map_err(|e| format!("the checked-in hash lock is unusable ({e})"))?;
    if reqs.is_empty() {
        return Err("no hash lock has been generated yet".to_string());
    }
    let find = |n: &str| reqs.iter().find(|r| r.name == n);
    let litellm = find("litellm").ok_or("the checked-in hash lock has no litellm entry")?;
    if litellm.version != litellm_version.trim() {
        return Err(format!(
            "the checked-in hash lock covers LiteLLM {}, not the pinned {}",
            litellm.version,
            litellm_version.trim()
        ));
    }
    match find("prisma") {
        Some(p) if p.version == crate::PRISMA_PINNED_VERSION => {}
        Some(p) => {
            return Err(format!(
                "the checked-in hash lock pins prisma {}, not the required {}",
                p.version,
                crate::PRISMA_PINNED_VERSION
            ))
        }
        None => return Err("the checked-in hash lock has no prisma entry".to_string()),
    }
    if find("prometheus-client").is_none() {
        return Err("the checked-in hash lock has no prometheus-client entry".to_string());
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only replacement for the embedded lock (see
    /// [`set_test_lock_override`]); per thread, so parallel tests on the
    /// current-thread tokio runtime do not see each other's fixture.
    static LOCK_OVERRIDE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Install a fixture lock for the current test thread (`None` restores
/// the embedded one).
#[cfg(test)]
pub(crate) fn set_test_lock_override(lock: Option<String>) {
    LOCK_OVERRIDE.with(|o| *o.borrow_mut() = lock);
}

/// The lock the installer uses: [`GATEWAY_REQUIREMENTS_LOCK`] (or a
/// test fixture).
pub(crate) fn active_gateway_lock() -> String {
    #[cfg(test)]
    if let Some(lock) = LOCK_OVERRIDE.with(|o| o.borrow().clone()) {
        return lock;
    }
    GATEWAY_REQUIREMENTS_LOCK.to_string()
}

/// [`hash_lock_status_for`] against the lock the installer uses.
pub fn gateway_hash_lock_status(litellm_version: &str) -> Result<(), String> {
    hash_lock_status_for(&active_gateway_lock(), litellm_version)
}

/// The operator-facing warning for an install of LiteLLM
/// `litellm_version` that cannot be hash-verified, or `None` when the
/// lock covers it.
pub fn gateway_hash_lock_warning(litellm_version: &str) -> Option<String> {
    gateway_hash_lock_status(litellm_version)
        .err()
        .map(|reason| unverified_install_warning(litellm_version, &reason))
}

pub(crate) fn unverified_install_warning(litellm_version: &str, reason: &str) -> String {
    format!(
        "LiteLLM {} and its dependencies are installed by version pin only — not hash-verified: {reason}",
        litellm_version.trim()
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A syntactically valid lock pinning `litellm` to `litellm_version`
    /// with obviously fake hashes (test fixture only — never a real lock).
    pub(crate) fn fixture_lock(litellm_version: &str) -> String {
        let h = |c: char| c.to_string().repeat(64);
        format!(
            "# fixture\n\
             litellm[proxy]=={v} \\\n    --hash=sha256:{a}\n    # via -r gateway-requirements.in\n\
             prisma=={p} \\\n    --hash=sha256:{b} \\\n    --hash=sha256:{c}\n\
             prometheus-client==0.21.1 \\\n    --hash=sha256:{d}\n",
            v = litellm_version,
            p = crate::PRISMA_PINNED_VERSION,
            a = h('a'),
            b = h('b'),
            c = h('c'),
            d = h('d'),
        )
    }

    #[test]
    fn parses_uv_compile_output() {
        let reqs = parse_requirements_lock(&fixture_lock("1.105.1")).unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].name, "litellm");
        assert_eq!(reqs[0].version, "1.105.1");
        assert_eq!(reqs[1].hashes.len(), 2);
        assert_eq!(reqs[2].name, "prometheus-client");
    }

    #[test]
    fn parses_crlf_and_markers() {
        let h = "e".repeat(64);
        let text =
            format!("Colorama_X==0.4.6 ; sys_platform == 'win32' \\\r\n    --hash=sha256:{h}\r\n");
        let reqs = parse_requirements_lock(&text).unwrap();
        assert_eq!(reqs[0].name, "colorama-x");
        assert_eq!(reqs[0].version, "0.4.6");
    }

    #[test]
    fn rejects_unhashed_unpinned_and_foreign_options() {
        let h = "a".repeat(64);
        assert!(parse_requirements_lock("litellm==1.0\n").is_err());
        assert!(parse_requirements_lock(&format!("litellm>=1.0 --hash=sha256:{h}\n")).is_err());
        assert!(parse_requirements_lock(&format!("litellm==1.* --hash=sha256:{h}\n")).is_err());
        assert!(parse_requirements_lock("litellm==1.0 --hash=md5:abcd\n").is_err());
        assert!(parse_requirements_lock("litellm==1.0 --hash=sha256:abcd\n").is_err());
        assert!(parse_requirements_lock("--index-url https://example.invalid/simple\n").is_err());
        assert!(parse_requirements_lock(&format!(
            "a==1 --hash=sha256:{h}\nA==2 --hash=sha256:{h}\n"
        ))
        .is_err());
    }

    #[test]
    fn placeholder_parses_empty_and_is_not_usable() {
        let reqs = parse_requirements_lock("# nothing here\n\n").unwrap();
        assert!(reqs.is_empty());
        assert!(hash_lock_status_for("# nothing\n", "1.105.1").is_err());
    }

    #[test]
    fn lock_status_requires_matching_litellm_and_prisma() {
        let lock = fixture_lock("1.105.1");
        assert_eq!(hash_lock_status_for(&lock, "1.105.1"), Ok(()));
        let e = hash_lock_status_for(&lock, "1.106.2").unwrap_err();
        assert!(
            e.contains("covers LiteLLM 1.105.1, not the pinned 1.106.2"),
            "{e}"
        );
        let wrong_prisma = lock.replace(
            &format!("prisma=={}", crate::PRISMA_PINNED_VERSION),
            "prisma==0.15.0",
        );
        assert!(hash_lock_status_for(&wrong_prisma, "1.105.1").is_err());
    }

    #[test]
    fn override_switches_the_active_lock() {
        set_test_lock_override(Some(fixture_lock("9.9.9")));
        assert!(gateway_hash_lock_warning("9.9.9").is_none());
        let w = gateway_hash_lock_warning("1.0.0").unwrap();
        assert!(w.contains("not hash-verified"), "{w}");
        set_test_lock_override(None);
    }

    fn input_pin(name: &str) -> Option<String> {
        GATEWAY_REQUIREMENTS_IN
            .lines()
            .map(|l| strip_comment(l).trim())
            .filter(|l| !l.is_empty())
            .find_map(|l| {
                let (n, v) = l.split_once("==")?;
                (normalize_package_name(n.split('[').next()?) == name).then(|| v.trim().to_string())
            })
    }

    /// CI guard: the lock inputs, the lock, and the Rust pin constants
    /// agree. Bumping `DEFAULT_LITELLM_PIN` / `PRISMA_PINNED_VERSION`
    /// without updating `gateway-requirements.in` and regenerating the
    /// lock (tools/gateway-lock/Update-GatewayLock.ps1) fails here.
    #[test]
    fn checked_in_gateway_lock_matches_pins() {
        assert_eq!(
            input_pin("litellm").as_deref(),
            Some(manager_config::DEFAULT_LITELLM_PIN),
            "gateway-requirements.in litellm pin != DEFAULT_LITELLM_PIN"
        );
        assert_eq!(
            input_pin("prisma").as_deref(),
            Some(crate::PRISMA_PINNED_VERSION),
            "gateway-requirements.in prisma pin != PRISMA_PINNED_VERSION"
        );
        let reqs = parse_requirements_lock(GATEWAY_REQUIREMENTS_LOCK)
            .expect("the checked-in gateway lock must parse");
        if reqs.is_empty() {
            // Not generated yet: the placeholder must say so, and the
            // installer then reports every install as not hash-verified.
            assert!(
                GATEWAY_REQUIREMENTS_LOCK.contains("PLACEHOLDER -- NOT YET GENERATED"),
                "an empty gateway lock must be the marked placeholder"
            );
            return;
        }
        assert_eq!(
            hash_lock_status_for(
                GATEWAY_REQUIREMENTS_LOCK,
                manager_config::DEFAULT_LITELLM_PIN
            ),
            Ok(()),
            "regenerate crates/manager-wizard/assets/gateway-requirements.lock"
        );
    }
}
