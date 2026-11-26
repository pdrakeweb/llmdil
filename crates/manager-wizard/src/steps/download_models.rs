//! Step 4: download models and verify checksums.

use super::*;

// ---------------------------------------------------------------------------
// 4. Download models + checksum verify
// ---------------------------------------------------------------------------

/// Step 4: download curated GGUFs with SHA-256 verification.
/// Audit mode re-verifies installed files without downloading.
///
/// Every model whose checksum matches during a run — downloaded,
/// already present, or audited — gets `verified_at` stamped into the
/// persisted config (see [`DownloadModels::stamp_verified_at`]), so
/// the app's fail-closed backend-start gate accepts wizard-verified
/// models without a second verification in the Models view.
///
/// Download URLs come from [`ModelConfig::source_url`](manager_config::ModelConfig)
/// (the curated catalog, spec §4.6); a model with an empty `source_url` fails
/// closed with a clear message.
pub struct DownloadModels;

#[async_trait]
impl WizardStep for DownloadModels {
    fn id(&self) -> &str {
        STEP_DOWNLOAD_MODELS
    }
    fn name(&self) -> &str {
        "Download models and verify checksums"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_INSTALL_LLAMACPP.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        ctx.config
            .models
            .iter()
            .filter(|m| m.enabled && !m.source_url.is_empty())
            .map(|m| {
                let dest = resolve_dest(&ctx.data_dir, &m.gguf_path);
                PlannedCommand::new(vec![
                    "download".to_string(),
                    m.source_url.clone(),
                    dest.display().to_string(),
                ])
            })
            .collect()
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("model checksum mismatch → re-download?")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        // Verified-at stamps are collected as typed config edits
        // (code-review PS-4) and delivered on every exit — a model
        // verified before a later failure keeps its stamp.
        let mut edits: Vec<ConfigEdit> = Vec::new();
        let mut outcome = match self.run_collecting(ctx, &mut edits).await {
            Ok(outcome) => outcome,
            Err(e) if edits.is_empty() => return Err(e),
            // Same conversion the runner applies to a step error, so
            // the edits gathered before it are not lost.
            Err(e) => StepOutcome::failed(format!("step error: {e}"), vec![format!("{e:?}")]),
        };
        outcome.config_edits = edits;
        Ok(deliver_config_edits(ctx, outcome, false))
    }
}

impl DownloadModels {
    async fn run_collecting(
        &self,
        ctx: &WizardContext,
        edits: &mut Vec<ConfigEdit>,
    ) -> Result<StepOutcome, WizardError> {
        if ctx.config.models.is_empty() {
            return Ok(StepOutcome::done(
                "no models configured; nothing to download",
                vec![],
            ));
        }
        // Disabled models are out of scope for the wizard: their files
        // and configuration stay, but nothing downloads or verifies.
        let targets: Vec<&ModelConfig> = ctx.config.models.iter().filter(|m| m.enabled).collect();
        if targets.is_empty() {
            return Ok(StepOutcome::done(
                "no models enabled; nothing to download (enable models in the Models view)",
                vec![],
            ));
        }
        let mut log = Vec::new();
        let mut verified = 0;
        let mut unverified = 0;
        if ctx.mode == RunMode::Install {
            if let Err(outcome) = self.check_disk_space(ctx, &mut log).await {
                return Ok(outcome);
            }
        }
        for model in &targets {
            // Tier-aware: a model on a cold tier is present, not missing.
            let loc = locate_model(ctx, &model.gguf_path).await;
            let dest = loc.dest().to_path_buf();
            if loc.found.is_none() && loc.cold_unavailable {
                log.push(format!(
                    "{}: not in the hot tier and a cold tier is unavailable; not downloading (reconnect the drive)",
                    model.id
                ));
                unverified += 1;
                continue;
            }
            if model.source_url.is_empty() {
                return Ok(StepOutcome::failed(
                    format!("no source URL configured for model '{}'", model.id),
                    vec!["set models[].source_url in the config (spec §4.6)".to_string()],
                ));
            }
            let url = model.source_url.clone();
            // Only a pinned hash can verify. A model without one (a
            // user-added model with no recorded SHA-256) downloads and
            // runs unverified rather than failing a comparison that can
            // never succeed.
            let pinned = is_pinned_hash(&model.sha256);
            let verify = |actual: &str| pinned && checksums_match(actual, &model.sha256);
            match ctx.mode {
                RunMode::Install => {
                    let mut needs_download = true;
                    if ctx.ops.file_exists(&dest).await {
                        if !pinned {
                            log.push(format!(
                                "{}: already present (no pinned SHA-256; not verified)",
                                model.id
                            ));
                            needs_download = false;
                        } else {
                            match ctx.ops.sha256_file(&dest).await {
                                Ok(h) if verify(&h) => {
                                    log.push(format!("{}: already present and verified", model.id));
                                    needs_download = false;
                                }
                                _ => log.push(format!(
                                    "{}: present but checksum mismatch; re-downloading",
                                    model.id
                                )),
                            }
                        }
                    }
                    if needs_download {
                        push_log(
                            ctx,
                            &mut log,
                            format!("downloading {} → {}", url, dest.display()),
                        );
                        let progress = ProgressEmitter::new(ctx, model.id.clone());
                        if let Err(e) = ctx
                            .ops
                            .download(&url, &dest, &|d, t| progress.progress(d, t))
                            .await
                        {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                        }
                        if pinned {
                            let actual = match ctx.ops.sha256_file(&dest).await {
                                Ok(h) => h,
                                Err(e) => {
                                    return Ok(StepOutcome::failed(
                                        format!("{e}"),
                                        vec![format!("{e:?}")],
                                    ))
                                }
                            };
                            if !verify(&actual) {
                                return Ok(StepOutcome::failed(
                                    format!(
                                        "checksum mismatch for '{}': expected {}, got {}",
                                        model.id, model.sha256, actual
                                    ),
                                    vec!["downloaded file rejected".to_string()],
                                ));
                            }
                            log.push(format!("{}: downloaded and SHA-256 verified", model.id));
                        } else {
                            log.push(format!(
                                "{}: downloaded (no pinned SHA-256; not verified)",
                                model.id
                            ));
                        }
                    }
                    if pinned {
                        Self::stamp_verified_at(edits, &model.id, &model.sha256);
                        verified += 1;
                    } else {
                        unverified += 1;
                    }
                }
                RunMode::Audit => {
                    if !ctx.ops.file_exists(&dest).await {
                        return Ok(StepOutcome::failed(
                            format!("model '{}' missing at {}", model.id, dest.display()),
                            vec![],
                        ));
                    }
                    if !pinned {
                        log.push(format!(
                            "{}: present (no pinned SHA-256; cannot verify)",
                            model.id
                        ));
                        unverified += 1;
                        continue;
                    }
                    // Verified-by-stamp shortcut: `verified_at` is
                    // stamped only after a hash match (a download
                    // verification or an earlier audit) — but the
                    // stamp alone does not say WHAT was verified.
                    // The skip therefore also requires the stamp's
                    // recorded hash (`verified_sha256`) to equal the
                    // CURRENT pin and the file to still have exactly
                    // the catalog size; then the model is not
                    // re-hashed. Without this, the launch-time audit
                    // re-reads every multi-GiB GGUF on every start
                    // for no new information — but with only the
                    // timestamp and the length checked, a
                    // same-length replacement file, or a catalog
                    // re-pin, inherited trust it never earned
                    // (code-review 2.4). Any doubt — no stamp, a
                    // stamp from before hash binding existed, a
                    // stamp against a different pin, no catalog
                    // entry to pin the size, a failed stat, a length
                    // mismatch — falls through to the full hash
                    // below.
                    if model.verified_at.is_some()
                        && model
                            .verified_sha256
                            .as_deref()
                            .is_some_and(|s| checksums_match(s, &model.sha256))
                    {
                        let expected = manager_models::curated_models()
                            .iter()
                            .find(|s| s.id == model.id)
                            .map(|s| s.size_bytes);
                        if let (Some(expected), Some(actual)) =
                            (expected, ctx.ops.file_len(&dest).await)
                        {
                            if actual == expected {
                                log.push(format!(
                                    "{}: verified by an earlier checksum pass against the current pin (stamp + file length match; not re-hashed this run)",
                                    model.id
                                ));
                                verified += 1;
                                continue;
                            }
                        }
                    }
                    match ctx.ops.sha256_file(&dest).await {
                        Ok(h) if verify(&h) => {
                            log.push(format!("{}: checksum verified", model.id));
                            Self::stamp_verified_at(edits, &model.id, &model.sha256);
                            verified += 1;
                        }
                        Ok(h) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "model '{}' checksum mismatch: expected {}, got {}",
                                    model.id, model.sha256, h
                                ),
                                log,
                            ));
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(
                                format!("cannot read '{}': {e}", model.id),
                                vec![format!("{e:?}")],
                            ));
                        }
                    }
                }
            }
        }
        let mut message = format!("{verified}/{} model(s) verified", targets.len());
        if unverified > 0 {
            message.push_str(&format!(
                "; {unverified} present without a pinned SHA-256 (not verified)"
            ));
        }
        Ok(StepOutcome::done(message, log))
    }
}

/// The base for a step's load-modify-save of `<data_dir>/config.json`
/// (see [`persisted_config_base`]).
enum PersistedBase {
    /// The persisted primary file loaded cleanly.
    Loaded(AppConfig),
    /// Fresh install: the data dir exists but neither `config.json`
    /// nor `config.json.bak` does, so the context snapshot (the
    /// caller's current config) is the base.
    FirstRun(AppConfig),
    /// No data dir on disk (unit tests drive steps with a fictional
    /// data dir): there is no persisted config to write.
    NoDataDir,
}

/// Choose the base a step edits before saving `<data_dir>/config.json`.
///
/// Code-review PS-4: the context snapshot may be minutes old, so it
/// is a valid base ONLY when no persisted config exists at all (a
/// fresh install). A file that exists but does not load — a parse
/// error, a validation failure, any I/O error other than "not found",
/// or a primary that only loaded through its `.bak` fallback (a save
/// would rotate the unreadable primary over the good `.bak`) — is an
/// error: the step must fail and save nothing over it.
fn persisted_config_base(ctx: &WizardContext) -> Result<PersistedBase, String> {
    let config_path = ctx.data_dir.join("config.json");
    let mut bak = config_path.as_os_str().to_owned();
    bak.push(".bak");
    let bak = PathBuf::from(bak);
    match manager_config::load_detailed(&config_path) {
        Ok(report) if !report.used_backup => Ok(PersistedBase::Loaded(report.config)),
        Ok(_) => Err(format!(
            "{} could not be loaded (only its .bak copy loads); refusing to save over it",
            config_path.display()
        )),
        Err(manager_config::ConfigError::Io(e))
            if e.kind() == std::io::ErrorKind::NotFound && !bak.exists() =>
        {
            if ctx.data_dir.is_dir() {
                Ok(PersistedBase::FirstRun(ctx.config.clone()))
            } else {
                Ok(PersistedBase::NoDataDir)
            }
        }
        Err(e) => Err(format!(
            "{} could not be loaded ({e}); refusing to save over it",
            config_path.display()
        )),
    }
}

/// Persist a step's [`StepOutcome::config_edits`] according to
/// [`WizardContext::config_edit_mode`].
///
/// - [`ConfigEditMode::ReturnToCaller`]: the outcome is returned
///   unchanged; the caller applies the edits under its config lock.
/// - [`ConfigEditMode::SaveToDisk`]: load `<data_dir>/config.json`
///   (see [`persisted_config_base`]), apply, save. A file that does
///   not load fails the step and saves nothing. For a failed save or
///   no data dir on disk, `required` decides: fail the step (the
///   placement step — its edits are its whole result), or log and
///   leave the edits unpersisted (verified-at stamps are best-effort
///   here, as they always were: the verification itself succeeded).
///
/// A failure here keeps an outcome that already failed on its own
/// message (the persistence error joins its log).
pub(crate) fn deliver_config_edits(
    ctx: &WizardContext,
    mut outcome: StepOutcome,
    required: bool,
) -> StepOutcome {
    if outcome.config_edits.is_empty() || ctx.config_edit_mode == ConfigEditMode::ReturnToCaller {
        return outcome;
    }
    let config_path = ctx.data_dir.join("config.json");
    let mut config = match persisted_config_base(ctx) {
        Ok(PersistedBase::Loaded(cfg)) | Ok(PersistedBase::FirstRun(cfg)) => cfg,
        Ok(PersistedBase::NoDataDir) if required => {
            return fail_outcome(
                outcome,
                format!(
                    "could not save config edits: data directory {} does not exist",
                    ctx.data_dir.display()
                ),
            )
        }
        Ok(PersistedBase::NoDataDir) => return outcome,
        Err(e) => return fail_outcome(outcome, format!("config edits not saved: {e}")),
    };
    let applied = apply_config_edits(&mut config, &outcome.config_edits);
    outcome.log_tail.extend(applied.log);
    if applied.applied == 0 {
        return outcome;
    }
    match manager_config::save(&config, &config_path) {
        Ok(()) => {
            outcome.log_tail.push(format!(
                "saved {} config edit(s) to {}",
                applied.applied,
                config_path.display()
            ));
            outcome
        }
        Err(e) => {
            let message = format!(
                "could not save config edits to {}: {e}",
                config_path.display()
            );
            if required {
                fail_outcome(outcome, message)
            } else {
                outcome.log_tail.push(message);
                outcome
            }
        }
    }
}

/// Mark `outcome` failed with `message`, unless it already failed —
/// then its own message stands and `message` joins its log.
fn fail_outcome(mut outcome: StepOutcome, message: String) -> StepOutcome {
    outcome.log_tail.push(message.clone());
    if outcome.state != crate::StepState::Failed {
        outcome.state = crate::StepState::Failed;
        outcome.message = message;
    }
    outcome
}

impl DownloadModels {
    /// Record a `verified_at` stamp for one model this run just
    /// verified, as a typed config edit.
    ///
    /// Steps receive a snapshot clone of the app config
    /// (`WizardContext::config` is built fresh by the caller and steps
    /// run through `&WizardContext`), so the stamp cannot ride back
    /// through `ctx.config`. It is returned as a
    /// [`ConfigEdit::ModelVerified`] (code-review PS-4) and persisted
    /// by [`deliver_config_edits`]: by the app under its config lock,
    /// or — with no app shell — into `<data_dir>/config.json` (the
    /// path the app loads at startup), never over a file that failed
    /// to load. A model deleted while the step ran is skipped when the
    /// edit is applied.
    fn stamp_verified_at(edits: &mut Vec<ConfigEdit>, model_id: &str, verified_sha256: &str) {
        edits.push(ConfigEdit::ModelVerified {
            model_id: model_id.to_string(),
            verified_at: chrono::Utc::now(),
            // The stamp records WHICH pinned hash the file was
            // verified against; the audit's verified-by-stamp
            // shortcut trusts a stamp only while that still equals
            // the current pin.
            verified_sha256: verified_sha256.to_string(),
        });
    }

    /// Fail when the models still needing download exceed the free space on
    /// the data volume (10% headroom). Sizes come from HEAD Content-Length;
    /// models whose size cannot be determined are logged, not counted. A
    /// failed free-space probe warns instead of blocking.
    async fn check_disk_space(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let mut known: u64 = 0;
        let mut unknown: u32 = 0;
        for model in &ctx.config.models {
            if !model.enabled || model.source_url.is_empty() {
                continue; // disabled models are out of scope; the per-model loop reports a missing URL
            }
            let loc = locate_model(ctx, &model.gguf_path).await;
            if loc.found.is_none() && loc.cold_unavailable {
                continue; // may be on the unreachable cold tier; never re-downloaded
            }
            let dest = loc.dest().to_path_buf();
            let needs_download = if ctx.ops.file_exists(&dest).await {
                // An unpinned model already on disk cannot be verified,
                // so it counts as present — same rule as the run loop.
                is_pinned_hash(&model.sha256)
                    && !matches!(ctx.ops.sha256_file(&dest).await, Ok(h) if checksums_match(&h, &model.sha256))
            } else {
                true
            };
            if !needs_download {
                continue;
            }
            match ctx.ops.http_head_content_length(&model.source_url).await {
                Ok(Some(n)) => {
                    known = known.saturating_add(n);
                    log.push(format!("{}: download size ~{n} bytes", model.id));
                }
                _ => {
                    unknown += 1;
                    log.push(format!(
                        "{}: size unknown, excluded from space check",
                        model.id
                    ));
                }
            }
        }
        if known == 0 {
            log.push(format!(
                "disk-space pre-check skipped: no download sizes known ({unknown} model(s) with unknown size)"
            ));
            return Ok(());
        }
        let free = match ctx.ops.free_space_bytes(&ctx.data_dir).await {
            Ok(f) => f,
            Err(e) => {
                log.push(format!(
                    "free-space probe failed ({e}); skipping space check"
                ));
                return Ok(());
            }
        };
        let needed = known + known / 10; // 10% headroom for temp files
        log.push(format!(
            "space check: need ~{}, {} free on {}",
            manager_present::bytes(needed),
            manager_present::bytes(free),
            ctx.data_dir.display()
        ));
        if free < needed {
            return Err(StepOutcome::failed(
                format!(
                    "insufficient disk space: need ~{} for model downloads, have {} free on {}",
                    manager_present::bytes(needed),
                    manager_present::bytes(free),
                    ctx.data_dir.display()
                ),
                vec![
                    "free up disk space or point data_dir at a larger volume, then re-run"
                        .to_string(),
                ],
            ));
        }
        Ok(())
    }
}
