//! Model catalog, download, and model-edit operations.

use crate::*;

/// Configured models, probed against the filesystem: on_disk/size_bytes
/// reflect the completed file at the model's gguf_path right now (the
/// download worker only moves a finished transfer into place, so a
/// partial download never shows as on_disk). `partial_bytes` is the size
/// of the in-progress `.part` sibling when one exists, `downloading`
/// marks a live download handle or a running post-download SHA-256
/// verification, `verifying` marks the verification alone, `paused` a
/// parked download, and `download_error` carries the last terminal
/// failure recorded by the completion watcher. `catalog_size_bytes` is
/// the curated catalog's size estimate for the model, when it has one.
/// `fit_check_available` is always false in v0.1.x: the catalog carries
/// no VRAM-fit model yet.
pub async fn op_list_models(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    ensure_catalog_models(state).await;
    reap_downloads(state).await;
    let cfg = state.config.read().await;
    let downloads = state.downloads.handles.lock().await;
    let outcomes = state.downloads.outcomes.lock().await;
    let verifying = state
        .downloads
        .verifying
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let catalog = manager_models::curated_models();
    let catalog_sizes: HashMap<&str, u64> = catalog
        .iter()
        .map(|s| (s.id.as_str(), s.size_bytes))
        .collect();
    let catalog_ctx: HashMap<&str, u32> =
        catalog.iter().map(|s| (s.id.as_str(), s.ctx_max)).collect();
    let catalog_names: HashMap<&str, &str> = catalog
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();
    let catalog_offload: HashMap<&str, (Option<u32>, Option<u32>)> = catalog
        .iter()
        .map(|s| (s.id.as_str(), (s.moe_layers, s.default_offload_layers)))
        .collect();
    let models: Vec<Value> = cfg
        .models
        .iter()
        .map(|m| {
            // The file wherever it lives (hot or a cold tier).
            let path = cfg.resolve_model(&m.gguf_path).path;
            let meta = std::fs::metadata(&path).ok().filter(|md| md.is_file());
            // The downloaded file's own parsed geometry, for
            // the declaration editor's post-download
            // reconciliation (Part 2, P2-D7): the local parse
            // is ground truth for geometry once the file is on
            // disk. Cached by (path, len, mtime) — repeat list
            // calls do not re-read headers.
            let file_layout = if meta.is_some() {
                gguf_layout_cached(&path)
            } else {
                None
            };
            let catalog_moe_layers = catalog_offload.get(m.id.as_str()).and_then(|p| p.0);
            let moe_reconcile = crate::hf_forms::moe_reconciliation(
                m.moe_layers,
                catalog_moe_layers,
                file_layout.as_ref().and_then(|l| l.moe_block_count),
            );
            let partial_meta = std::fs::metadata(manager_models::partial_path(
                &crate::model_storage::download_destination(&cfg, &m.gguf_path),
            ))
                .ok()
                .filter(|md| md.is_file());
            let is_verifying = verifying.contains(&m.id);
            json!({
                "id": m.id,
                "name": m.display_name.as_deref().map(str::trim).filter(|s| !s.is_empty())
                    .or_else(|| catalog_names.get(m.id.as_str()).copied())
                    .unwrap_or(m.id.as_str()),
                "gguf_path": m.gguf_path,
                // The record's source URL: the Edit form's
                // re-fetch (Part 2, P2-D8) recovers the HF repo
                // and file from it, and offers the action only
                // for HF resolve URLs.
                "source_url": m.source_url,
                // The on-disk file's parsed MoE block count and
                // in-file MTP signal (null/false when the file
                // is absent or unparseable — an unknown, not a
                // zero), and the reconciliation proposal the
                // declaration editor renders when the stored
                // declaration disagrees with the file.
                "file_moe_layers": file_layout.as_ref().and_then(|l| l.moe_block_count),
                "file_nextn": file_layout.as_ref().map(|l| {
                    l.nextn_predict_layers.map(|n| n > 0).unwrap_or(false)
                        || l.has_nextn_tensors
                }),
                "moe_reconcile": moe_reconcile.map(|(declared, file)| json!({
                    "declared": declared,
                    "file": file,
                })),
                "quant": m.quant,
                "params_b": m.params_b,
                "sha256": m.sha256,
                "verified_at": m.verified_at,
                "enabled": m.enabled,
                "fit_check_available": false,
                "on_disk": meta.is_some(),
                "size_bytes": meta.map(|md| md.len()),
                "partial_bytes": partial_meta.map(|md| md.len()),
                // Tier residency, pins and any move in flight
                // (docs/tiered-model-storage.md); null for a path
                // outside the tiered models directory.
                "storage": crate::model_storage::model_row_storage(state, &cfg, m),
                "downloading": downloads.contains_key(&m.id) || is_verifying,
                "verifying": is_verifying,
                "paused": downloads.get(&m.id).map(|h| h.is_paused()).unwrap_or(false),
                "download_error": outcomes.get(&m.id).and_then(|o| o.failed.clone()),
                "catalog_size_bytes": catalog_sizes.get(m.id.as_str()).copied(),
                "ctx_max": catalog_ctx.get(m.id.as_str()).copied(),
                // The operator's MoE declaration on the model
                // record, and the catalog's pins for the same
                // figures: the Models view's offload editor reads
                // the pair (catalog wins where it pins a value).
                "moe_layers": m.moe_layers,
                "default_offload_layers": m.default_offload_layers,
                "catalog_moe_layers": catalog_offload.get(m.id.as_str()).and_then(|p| p.0),
                "catalog_default_offload_layers": catalog_offload.get(m.id.as_str()).and_then(|p| p.1),
                // The operator's MTP declaration on the model
                // record, and the catalog's pin for the same fact
                // (resolved by id or source URL, like the
                // capability itself; null when the catalog knows
                // neither). The Models view's capability editor
                // reads the pair — the pin wins where it exists
                // (see manager_models::effective_mtp).
                "mtp": m.mtp,
                "catalog_mtp": manager_models::catalog_mtp_pin(&m.id, &m.source_url),
            })
        })
        .collect();
    Ok(json!({ "models": models }))
}

/// Build a download spec for a configured model: the curated catalog
/// provides URL/size; the config's sha256 wins when it is a real pinned
/// hash (64 hex chars), otherwise the catalog value stands. A model
/// with no catalog entry falls back to its own configured `source_url`
/// (which must be an https URL), with the placeholder hash unless the
/// config pins a real one — verification then fails closed, exactly as
/// for unpinned catalog models.
pub(crate) fn download_spec_for(
    cfg: &AppConfig,
    id: &str,
) -> Result<manager_models::ModelSpec, ApiError> {
    let m = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    if let Some(mut spec) = manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == id)
    {
        if is_real_pin(&m.sha256) {
            spec.sha256 = m.sha256.clone();
        }
        return Ok(spec);
    }
    if m.source_url.starts_with("https://") {
        return Ok(manager_models::ModelSpec {
            id: m.id.clone(),
            name: m.id.clone(),
            quant: m.quant.clone(),
            size_bytes: 0,
            sha256: if is_real_pin(&m.sha256) {
                m.sha256.clone()
            } else {
                manager_models::PLACEHOLDER_SHA256.to_string()
            },
            url: m.source_url.clone(),
            params_b: m.params_b,
            ctx_max: 0,
            enabled_by_default: m.enabled,
            expert_bytes: None,
            moe_layers: m.moe_layers,
            kv_bytes_per_token: None,
            default_offload_layers: m.default_offload_layers,
            offload_restrictions: Vec::new(),
            mtp: false,
        });
    }
    Err(ApiError::NotFound(format!(
        "no catalog entry for model {id} and no https source_url configured"
    )))
}

/// Start a model download; progress streams via `download-progress`.
/// A completion watcher records the terminal outcome in
/// `download_outcomes` (surfaced by `list_models` as `download_error`)
/// and, when the model has a real pinned SHA-256, verifies the finished
/// file and stamps `verified_at` on success. The model is marked
/// verifying before the watcher spawns and unmarked on every watcher
/// exit path; each attempt carries a token so a superseded watcher
/// cannot overwrite a newer attempt's outcome.
pub async fn op_download_model(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    ensure_catalog_models(state).await;
    let (spec, dest, logical) = {
        let cfg = state.config.read().await;
        let spec = download_spec_for(&cfg, &id)?;
        // Download to the model's configured gguf_path (resolved under
        // the data dir) — the same path list_models probes — so a
        // completed download always reports on_disk, for any config.
        let model = cfg
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
        // Wherever a copy already is, else the configured download
        // tier (docs/tiered-model-storage.md).
        if crate::model_storage::move_pending_for(state, &model.gguf_path) {
            return Err(ApiError::Busy(format!(
                "a storage move is running or queued for model {id}"
            )));
        }
        let dest = crate::model_storage::download_destination(&cfg, &model.gguf_path);
        (spec, dest, model.gguf_path.clone())
    };
    // Hold the downloads lock across the check, the in-use guard, and
    // the insert: two concurrent invokes for the same model must not
    // both spawn workers onto one destination file (Range-append vs
    // truncate corrupts it, and the second insert would orphan the
    // first handle).
    let mut downloads = state.downloads.handles.lock().await;
    downloads.retain(|_, h| !h.is_terminal());
    if downloads.contains_key(&id) {
        return Err(ApiError::Busy(format!(
            "download already in flight for model {id}"
        )));
    }
    // In-use guard: re-downloading a file a running backend has loaded
    // truncates it under llama-server's mmap. Refuse while any running
    // backend's model resolves to this destination.
    {
        let cfg = state.config.read().await;
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        let running: std::collections::HashSet<String> = sup
            .status()
            .into_iter()
            .filter(|st| st.state == manager_supervisor::ProcessState::Running)
            .map(|st| st.id)
            .collect();
        for b in &cfg.backends {
            // Identity by logical path: a running backend serving this
            // model holds one of its tier copies open.
            if running.contains(&b.id)
                && resolve_under_data_dir(&cfg.data_dir, &b.model_file)
                    == resolve_under_data_dir(&cfg.data_dir, &logical)
            {
                return Err(ApiError::InvalidInput(format!(
                    "model {id} is loaded by running backend {}; stop it before re-downloading",
                    b.id
                )));
            }
        }
    }
    let progress_sink = Arc::clone(sink);
    let model_id = id.clone();
    let handle = manager_models::download(&spec, &dest, move |progress| {
        let payload = DownloadProgressPayload {
            model_id: model_id.clone(),
            downloaded_bytes: progress.downloaded_bytes,
            total_bytes: Some(progress.total_bytes),
            fraction: Some(progress.fraction()),
        };
        progress_sink.emit(
            events::DOWNLOAD_PROGRESS,
            serde_json::to_value(&payload).unwrap_or(Value::Null),
        );
    })
    .await
    .map_err(ApiError::from)?;
    let watcher_handle = handle.clone();
    downloads.insert(id.clone(), handle);
    // Release `downloads` before taking any other lock below (finding
    // PS-1, docs/code-review-v0.1.60.md): op_list_models takes
    // config.read() then downloads, so waiting for config.write() while
    // still holding downloads deadlocked the whole app (tokio's RwLock
    // then queues every later reader behind the parked writer). The
    // check-and-insert above is complete; the entry now guards against
    // a second worker for this model.
    drop(downloads);
    // The transfer is a tracked task: task_abort routes to
    // cancel_download (the worker removes the partial file).
    let task_id = state
        .record_task("model_download", json!({ "model_id": id, "dest": dest }))
        .await;
    state.tasks.controls.set_canceller(
        &task_id,
        tasks::TaskCanceller::Download {
            model_id: id.clone(),
        },
    );
    // Starting a new attempt clears the previous terminal outcome and
    // any earlier verification stamp: until this attempt's file passes
    // verification the model counts as unverified (the start
    // preflight's durable gate reads `verified_at`).
    state.downloads.outcomes.lock().await.remove(&id);
    let attempt = state.downloads.begin_attempt(&id).await;
    {
        let mut cfg = state.config.write().await;
        if let Some(m) = cfg.models.iter_mut().find(|m| m.id == id) {
            m.verified_at = None;
            m.verified_sha256 = None;
        }
    }
    // Mark verification-pending BEFORE the watcher spawns; the watcher
    // removes the mark on every exit path. While marked, list_models
    // reports the model as verifying and the start preflight refuses
    // to launch a backend on a file that may be deleted as corrupt.
    state
        .downloads
        .verifying
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone());
    // Completion watcher: `download_model` returns as soon as the
    // transfer is spawned; the watcher observes the terminal state so
    // failures are recorded instead of dropped, and a pinned hash is
    // actually verified against the finished file.
    let watcher_sink = Arc::clone(sink);
    let watcher_state = Arc::clone(state);
    let watcher_id = id.clone();
    let watcher_dest = dest.clone();
    let watcher_task = task_id.clone();
    let watcher = async move {
        let outcome = watcher_handle.wait().await;
        let st = watcher_state;
        // Display name for the completion/failure toasts: the
        // operator-set name, else the curated catalog name, else the
        // model id (same precedence as `list_models`).
        let watcher_name = {
            let cfg = st.config.read().await;
            cfg.models
                .iter()
                .find(|m| m.id == watcher_id)
                .and_then(|m| {
                    m.display_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .or_else(|| {
                    manager_models::curated_models()
                        .iter()
                        .find(|s| s.id == watcher_id)
                        .map(|s| s.name.clone())
                })
                .unwrap_or_else(|| watcher_id.clone())
        };
        // Every path below leaves verification; clear the mark first
        // so no outcome branch can skip it.
        st.downloads
            .verifying
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&watcher_id);
        match outcome {
            Ok(()) => {
                let pinned = {
                    let cfg = st.config.read().await;
                    cfg.models
                        .iter()
                        .find(|m| m.id == watcher_id)
                        .map(|m| m.sha256.clone())
                };
                let mut verified = false;
                let (failed, verification_failure) = match pinned {
                    Some(sha) if is_real_pin(&sha) => {
                        match manager_models::verify_sha256(&watcher_dest, &sha).await {
                            Ok(true) => {
                                verified = true;
                                let mut cfg = st.config.write().await;
                                // PS-8: installed only once saved.
                                if let Err(e) =
                                    mutate_save_install(&mut cfg, &st.config_path, |cfg| {
                                        if let Some(m) =
                                            cfg.models.iter_mut().find(|m| m.id == watcher_id)
                                        {
                                            m.verified_at = Some(chrono::Utc::now());
                                            m.verified_sha256 = Some(sha.clone());
                                        }
                                    })
                                {
                                    tracing::warn!(
                                        "download watcher: could not save the verification of {watcher_id}: {e}"
                                    );
                                }
                                (None, false)
                            }
                            Ok(false) => {
                                // Fail closed: a file that does not match
                                // the pinned hash is not kept as a model.
                                match tokio::fs::remove_file(&watcher_dest).await {
                                    Ok(()) => {
                                        let mut cfg = st.config.write().await;
                                        // PS-8: installed only once saved;
                                        // the recorded verification failure
                                        // gates start either way.
                                        if let Err(e) = mutate_save_install(
                                            &mut cfg,
                                            &st.config_path,
                                            |cfg| {
                                                if let Some(m) = cfg
                                                    .models
                                                    .iter_mut()
                                                    .find(|m| m.id == watcher_id)
                                                {
                                                    m.verified_at = None;
                                                    m.verified_sha256 = None;
                                                }
                                            },
                                        ) {
                                            tracing::warn!(
                                                "download watcher: could not save the cleared verification of {watcher_id}: {e}"
                                            );
                                        }
                                        (
                                            Some(
                                                "SHA-256 mismatch after download — file removed; \
                                                 the download did not match the pinned hash."
                                                    .to_string(),
                                            ),
                                            true,
                                        )
                                    }
                                    Err(e) => (
                                        Some(format!(
                                            "SHA-256 mismatch after download — the file could not be removed: {e}"
                                        )),
                                        true,
                                    ),
                                }
                            }
                            Err(e) => (Some(format!("verification failed: {e}")), true),
                        }
                    }
                    _ => (None, false),
                };
                // Toasts: a finished download is the Operations
                // event; a recorded failure in this arm is always a
                // verification failure (transfer errors land in the
                // Err arm below). The operator-cancelled arm stays
                // silent — the operator already knows.
                let toast = match &failed {
                    None => Some((
                        notify::Category::Operations,
                        "Model download finished",
                        if verified {
                            format!(
                                "{watcher_name} downloaded and verified against its pinned SHA-256."
                            )
                        } else {
                            format!("{watcher_name} downloaded.")
                        },
                    )),
                    Some(msg) => Some((
                        notify::Category::Failures,
                        "Model download failed verification",
                        msg.clone(),
                    )),
                };
                st.finish_task(
                    &watcher_task,
                    if failed.is_some() {
                        TaskStatus::Failed
                    } else {
                        TaskStatus::Done
                    },
                    json!({ "model_id": watcher_id, "error": failed, "verified": verified }),
                )
                .await;
                st.downloads
                    .record_outcome(
                        &watcher_id,
                        DownloadOutcome {
                            attempt,
                            failed,
                            verification_failure,
                        },
                    )
                    .await;
                if let Some((category, title, body)) = toast {
                    notify::notify(watcher_sink.as_ref(), &st, category, title, &body).await;
                }
            }
            Err(manager_models::ModelError::Cancelled(_)) => {
                // Operator-initiated: neither a completion nor a
                // failure — clear the outcome instead of recording one.
                st.downloads.clear_outcome(&watcher_id, attempt).await;
                st.finish_task(
                    &watcher_task,
                    TaskStatus::Aborted,
                    json!({ "model_id": watcher_id, "cancelled": true }),
                )
                .await;
            }
            Err(e) => {
                st.finish_task(
                    &watcher_task,
                    TaskStatus::Failed,
                    json!({ "model_id": watcher_id, "error": format!("download failed: {e}") }),
                )
                .await;
                // Transfer failures are not verification failures and
                // do not gate backend start.
                st.downloads
                    .record_outcome(
                        &watcher_id,
                        DownloadOutcome {
                            attempt,
                            failed: Some(format!("download failed: {e}")),
                            verification_failure: false,
                        },
                    )
                    .await;
                notify::notify(
                    watcher_sink.as_ref(),
                    &st,
                    notify::Category::Failures,
                    "Model download failed",
                    &format!("{watcher_name} — download failed: {e}"),
                )
                .await;
            }
        }
    };
    // The watcher logs inside the download task's operation span.
    tokio::spawn(tracing::Instrument::in_current_span(watcher));
    Ok(json!({ "model_id": id, "dest": dest, "task_id": task_id }))
}

/// Cancel an in-flight download (partial file removed by the worker).
pub async fn op_cancel_download(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    // Reap first: cancelling a download that already finished must not
    // report success — the terminal handle is dropped here and the
    // lookup below honestly reports that nothing is in flight.
    reap_downloads(state).await;
    let handle = state
        .downloads
        .handles
        .lock()
        .await
        .remove(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no download in flight for {id}")))?;
    handle.cancel().await.map_err(ApiError::from)?;
    Ok(json!({ "model_id": id, "cancelled": true }))
}

/// Pause an in-flight download (partial file kept for resume).
pub async fn op_model_pause_download(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let downloads = state.downloads.handles.lock().await;
    let handle = downloads
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no download in flight for {id}")))?;
    handle.pause().await?;
    Ok(json!({ "model_id": id, "paused": true }))
}

/// Resume a paused download.
pub async fn op_model_resume_download(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let downloads = state.downloads.handles.lock().await;
    let handle = downloads
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no download in flight for {id}")))?;
    handle.resume().await.map_err(ApiError::from)?;
    Ok(json!({ "model_id": id, "resumed": true }))
}

pub(crate) fn no_pin_error(id: &str) -> ApiError {
    ApiError::InvalidInput(format!(
        "no SHA-256 hash is pinned for model {id}; pin one in the app configuration to enable verification"
    ))
}

/// SHA-256-verify an installed model file against the hash pinned in
/// the model's config entry. Records the outcome in `verified_at`
/// (stamped on success, cleared on mismatch) and persists the config.
/// The download-outcome gate follows the same result: success REMOVES
/// any recorded verification failure for the model (the start
/// preflight stops refusing), a mismatch records one.
pub async fn op_model_verify_checksum(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let (path, sha) = {
        let cfg = state.config.read().await;
        let m = cfg
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
        if !is_real_pin(&m.sha256) {
            return Err(no_pin_error(&id));
        }
        (cfg.resolve_model(&m.gguf_path).path, m.sha256.clone())
    };
    // Track the manual verify in the same set the download watcher
    // uses: list_models then reports the model as verifying (the
    // hash of a multi-GiB file takes minutes and the card must not
    // look idle or re-offer the button), and the start preflight
    // refuses to launch on a file whose verification is in flight.
    // The mark is removed on every exit path below.
    {
        let mut verifying = state
            .downloads
            .verifying
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !verifying.insert(id.clone()) {
            return Err(ApiError::Busy(format!(
                "verification already in progress for model {id}"
            )));
        }
    }
    let verify_result = manager_models::verify_sha256(&path, &sha).await;
    state
        .downloads
        .verifying
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    let ok = verify_result?;
    let save_err = {
        let mut cfg = state.config.write().await;
        mutate_save_install(&mut cfg, &state.config_path, |cfg| {
            if let Some(m) = cfg.models.iter_mut().find(|m| m.id == id) {
                // The stamp and the hash it verified against move
                // together (code-review 2.4).
                if ok {
                    m.verified_at = Some(chrono::Utc::now());
                    m.verified_sha256 = Some(sha.clone());
                } else {
                    m.verified_at = None;
                    m.verified_sha256 = None;
                }
            }
        })
        .err()
    };
    // A failed save leaves the in-memory stamps untouched (PS-8);
    // a mismatch still records its start gate below, so the
    // backend stays fail-closed even when the save failed.
    if ok {
        if let Some(e) = save_err {
            return Err(e.into());
        }
        // A passing manual verify clears the start gate recorded by an
        // earlier failed verification.
        state.downloads.outcomes.lock().await.remove(&id);
    } else {
        state.downloads.outcomes.lock().await.insert(
            id.clone(),
            DownloadOutcome {
                // Not produced by a download attempt; direct insert
                // (the attempt-token gate applies to watchers only).
                attempt: 0,
                failed: Some(
                    "SHA-256 verification failed: the file on disk does not match the pinned hash"
                        .to_string(),
                ),
                verification_failure: true,
            },
        );
        if let Some(e) = save_err {
            return Err(e.into());
        }
    }
    Ok(json!({ "id": id, "ok": ok }))
}

/// Rename a model's short display name (the operator's own label,
/// stored as `display_name` on the model config). The effective
/// short name — this override, else the catalog name, else the id —
/// is what `list_models` reports and every short-name control
/// shows. The model id itself never changes. Mutation happens on a
/// clone installed in memory only after a successful save, so a
/// failed save leaves memory and disk in agreement.
pub async fn op_model_set_short_name(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    name: String,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let m = new_cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    // An empty name CLEARS the override (the effective name falls
    // back to the catalog name / id): there was previously no way
    // to undo a rename — empty input failed validation like any
    // other bad name (code-review 4.14).
    if name.trim().is_empty() {
        m.display_name = None;
        manager_config::save(&new_cfg, &state.config_path)?;
        *cfg = new_cfg;
        return Ok(json!({ "id": id, "name": Value::Null }));
    }
    let validated = manager_config::validate_display_name(&name).map_err(ApiError::InvalidInput)?;
    m.display_name = Some(validated.clone());
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    Ok(json!({ "id": id, "name": validated }))
}

/// Delete a model file (guarded): refuses while any backend references
/// the model, or while a download is in flight for it. Removes the
/// model from the config on success and persists the removal; any
/// partial download file beside it is deleted too.
pub async fn op_model_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    reap_downloads(state).await;
    if state.downloads.handles.lock().await.contains_key(&id) {
        return Err(ApiError::Models(format!(
            "download in flight for model {id}; cancel it before deleting"
        )));
    }
    let mut cfg = state.config.write().await;
    let pos = cfg
        .models
        .iter()
        .position(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    let model = cfg.models[pos].clone();
    // Delete the file first: on failure the config entry is kept. The
    // file path is the model's gguf_path resolved under the data dir —
    // the same path download_model wrote and list_models probes.
    if crate::model_storage::move_pending_for(state, &model.gguf_path) {
        return Err(ApiError::Busy(format!(
            "a storage move is running or queued for model {id}; cancel it before deleting"
        )));
    }
    // Every tier's copy goes (the cold archive included): the
    // resolved copy through the guarded delete, the rest after it.
    let resolved = cfg.resolve_model(&model.gguf_path);
    let file_path = resolved.path.clone();
    let report = manager_models::delete_model_file(&model, &cfg.backends, &file_path)
        .map_err(ApiError::from)?;
    let mut other_bytes = 0u64;
    if let Some(rel) = manager_config::tier_relative(&model.gguf_path) {
        for t in manager_config::all_tiers(&cfg.model_storage) {
            if let Some(p) = manager_config::tier_file(&cfg.data_dir, &cfg.model_storage, t, &rel) {
                if p != file_path {
                    other_bytes += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                    let _ = std::fs::remove_file(&p);
                }
                let _ = std::fs::remove_file(manager_models::partial_path(&p));
                let _ = std::fs::remove_file(manager_config::tier_partial_path(&p));
            }
        }
    }
    cfg.models.remove(pos);
    // Persist the removal — without the save, the deleted model
    // reappears in the config (and the Models view) on the next launch.
    manager_config::save(&cfg, &state.config_path)?;
    // Drop any partial download left beside the deleted file.
    let _ = std::fs::remove_file(manager_models::partial_path(&file_path));
    Ok(json!({
        "model_id": id,
        "deleted": true,
        "path": report.path,
        "file_existed": report.file_existed,
        "bytes_freed": report.bytes_freed + other_bytes,
    }))
}

/// Enable or disable a model for wizard downloads. Disabling keeps the
/// model's file and configuration; the setup wizard's download step
/// skips disabled models.
pub async fn op_model_set_enabled(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    enabled: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let model = cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    model.enabled = enabled;
    manager_config::save(&cfg, &state.config_path)?;
    drop(cfg);
    Ok(json!({ "model_id": id, "enabled": enabled }))
}

/// Set or clear a model's operator MoE declaration: the MoE layer
/// count and the default offload layers that give a model the
/// curated catalog does not carry expert-offload controls (the
/// creation default and the Edit Flags slider's green limit are
/// resolved from it when the catalog pins neither). Both fields
/// `None` clears the declaration. The pair is validated atomically
/// (see [`manager_config::validate_moe_declaration`]) before
/// anything is written, and the save validates again, so a
/// rejected edit leaves the stored model untouched. Catalog
/// models keep resolving from the catalog — a declaration stored
/// on one is shadowed by the catalog pin, never an override.
pub async fn op_model_set_offload_spec(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
) -> ApiResult {
    manager_config::validate_moe_declaration(moe_layers, default_offload_layers)
        .map_err(ApiError::InvalidInput)?;
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let m = new_cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    m.moe_layers = moe_layers;
    m.default_offload_layers = default_offload_layers;
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    Ok(json!({
        "id": id,
        "moe_layers": moe_layers,
        "default_offload_layers": default_offload_layers,
    }))
}

/// Set or clear a model's operator MTP declaration — the
/// declaration that gives a model the curated catalog does not
/// carry the MTP draft control (backend Add/Edit and Edit Flags
/// resolve the EFFECTIVE capability; see
/// [`manager_models::effective_mtp`]). `None` clears the
/// declaration. No validation beyond existence: a boolean
/// declaration has no invalid state. Catalog models keep
/// resolving from the catalog — a declaration stored on one is
/// shadowed by the catalog pin, never an override. Clearing a
/// declaration does not rewrite flags already on a backend (the
/// offload declaration's precedent): spec tokens written while
/// the model was declared capable stay until the operator edits
/// them; the control itself disappears with the capability.
pub async fn op_model_set_mtp(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    mtp: Option<bool>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let m = new_cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    m.mtp = mtp;
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    Ok(json!({
        "id": id,
        "mtp": mtp,
    }))
}

/// Add a model to the registry: an id, the https URL of its GGUF file,
/// and optionally an explicit file name, quant label, parameter
/// count (see [`model_add_entry`] for how the count is resolved and
/// when the add is refused), a MoE declaration (the MoE layer
/// count plus the default offload layers — the pair that gives a
/// model the curated catalog does not carry expert-offload
/// controls; see [`manager_config::validate_moe_declaration`]),
/// and an MTP capability declaration (see
/// [`manager_models::effective_mtp`]), and optionally a
/// SHA-256 pin (design Part 2, P2-D6 — an HF-picked add passes
/// the picked file's `lfs.sha256` and starts pinned). Without
/// a pin the model starts with no pinned SHA-256 — its
/// downloads run unverified (the Models view shows "hash not
/// pinned") until a hash is recorded in the configuration.
#[allow(clippy::too_many_arguments)]
pub async fn op_model_add(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    source_url: String,
    file_name: Option<String>,
    quant: Option<String>,
    params_b: Option<f32>,
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
    mtp: Option<bool>,
    sha256: Option<String>,
) -> ApiResult {
    // Resolve everything before taking the config lock: a rejected
    // add must leave the in-memory config and the saved file
    // untouched. (The previous code pushed the entry into the live
    // config first, so when the save then failed validation the
    // invalid entry stayed in memory and poisoned every later save
    // in the session.)
    let entry = model_add_entry(
        &id,
        &source_url,
        file_name,
        quant,
        params_b,
        moe_layers,
        default_offload_layers,
        mtp,
        sha256,
    )?;
    let id = entry.id.clone();
    let mut cfg = state.config.write().await;
    if cfg.models.iter().any(|m| m.id == id) {
        return Err(ApiError::InvalidInput(format!(
            "a model with id '{id}' already exists"
        )));
    }
    // Save a clone and swap it in only on success — the pattern the
    // other config-mutating commands use. save() validates before
    // writing, so a failure here also leaves state untouched.
    let mut new_cfg = cfg.clone();
    new_cfg.models.push(entry);
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    drop(cfg);
    Ok(json!({ "model_id": id }))
}

/// Assign a model to a backend with a tensor split (spec §14
/// `model_assign`). Sets the backend's model file, applies the split, and
/// records a restart task for the guided rolling restart.
pub async fn op_model_assign(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    backend_id: String,
    split: Vec<f32>,
) -> ApiResult {
    // Validate the split against the discovered GPU count before
    // touching the config. Done before the config write lock is taken:
    // the telemetry accessor reads the config while initializing, so
    // calling it under the write lock could deadlock. A count of 0
    // means telemetry has not polled yet — the check is skipped rather
    // than rejecting against an unknown count.
    let (gpu_count, gpu_inventory) = {
        let store = state.telemetry().await;
        let guard = lock_telemetry(&store);
        (guard.gpu_count(), guard.inventory())
    };
    if gpu_count > 0 && split.len() != gpu_count {
        // Same wording as manager_models::validate_split.
        return Err(ApiError::InvalidInput(format!(
            "tensor split has {} entries but there are {gpu_count} GPUs",
            split.len()
        )));
    }
    // Mutate a clone and commit it to state.config only after
    // placement, validation, journaling, and the save all succeed
    // (the #32 pattern; code review B-4): the mutations below run
    // ahead of checks that can fail — apply_placement, validate,
    // the save itself — and a failure that left them in the live
    // config would persist on the next unrelated save with no
    // journal entry.
    let (mut guard, live) = config_write_for_journal(state).await;
    let pre_cfg = guard.clone();
    let mut cfg = pre_cfg.clone();
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    let gguf_path = model.gguf_path.clone();
    // The model's default offload flags (catalog pin, or the
    // operator's declared default on the model record), resolved
    // before the mutable backend borrow below.
    let default_flags = model_default_raw_flags_at(&cfg, model);
    let took_default;
    let flags_untouched = {
        let backend = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == backend_id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?;
        let prev_model_file = backend.model_file.clone();
        let prev_raw_flags = backend.raw_flags.clone();
        backend.model_file = gguf_path;
        // Advanced flags mode (tracker #60): the assignment itself
        // proceeds — the model a backend serves is not flags state —
        // but its two flags side effects are skipped for an
        // advanced backend: the catalog default fill below (it
        // would write the operator's raw string) and the placement
        // application (placement_apply is refused outright in the
        // mode). The response reports the skip.
        let untouched = backend.flags_advanced;
        // A backend with no raw flags of its own takes the newly
        // assigned model's default offload (e.g. Nemotron's
        // --n-cpu-moe 16 from the catalog, or a user-added MoE
        // model's declared default) — the same preservation rule
        // as the catalog definition refresh: backends carrying
        // operator flags keep them, and the offload slider
        // adjusts from there.
        if !untouched
            && backend
                .raw_flags
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            if let Some(flags) = &default_flags {
                backend.raw_flags = Some(flags.clone());
            }
        }
        if !untouched {
            manager_models::apply_placement(backend, &split).map_err(ApiError::from)?;
        }
        took_default = backend.raw_flags != prev_raw_flags;
        // A model-file or raw-flags change invalidates the recorded
        // tool-call probe (MCP design §7.2).
        if backend.model_file != prev_model_file || backend.raw_flags != prev_raw_flags {
            backend.tool_call_probe = None;
        }
        untouched
    };
    // A default is never left on a position the placed GPU blocks.
    if took_default {
        if let Some(pos) = cfg.backends.iter().position(|b| b.id == backend_id) {
            let mut b = cfg.backends[pos].clone();
            snap_default_offload(&cfg, &mut b, &gpu_inventory);
            cfg.backends[pos] = b;
        }
    }
    if let Some(model) = cfg.models.iter_mut().find(|m| m.id == id) {
        if !model.assigned_backends.contains(&backend_id) {
            model.assigned_backends.push(backend_id.clone());
        }
    }
    manager_config::validate(&cfg).map_err(ApiError::from)?;
    // The assignment activates twice over: the gateway's routes
    // change with the backend's model (gateway scope, recorded only
    // when the generated YAML actually differs), and the backend
    // loads the new model at its next (re)start (backend scope).
    // Recorded before the save, per the journal ordering rule.
    let mut journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &cfg,
        "model_assign",
        format!("Backend `{backend_id}` now serves model `{id}`"),
        false,
    )
    .await?;
    let backend_record = record_backend_change(
        state,
        &live,
        &pre_cfg,
        &cfg,
        "model_assign",
        format!("Backend `{backend_id}` now serves model `{id}`"),
        &backend_id,
    )
    .await;
    chain_journaled(state, &mut journaled, backend_record)?;
    save_journaled(state, &cfg, &journaled)?;
    // Commit point: every fallible step has succeeded, so the
    // working clone becomes the live config.
    *guard = cfg;
    drop(guard);
    let task_id = state
        .record_task(
            "model_assign",
            json!({ "model_id": id, "backend_id": backend_id, "split": split }),
        )
        .await;
    // The assignment is already applied and persisted above; the task is
    // the UI's rolling-restart handle, recorded complete rather than
    // left Running forever.
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "model_id": id, "backend_id": backend_id }),
        )
        .await;
    Ok(json!({
        "task_id": task_id,
        "persisted": true,
        // True when the target backend is in advanced flags mode
        // (tracker #60): its split and raw flags were left
        // untouched by the assignment.
        "flags_untouched": flags_untouched,
    }))
}

/// Build the registry entry for [`model_add`]: every input rule and
/// the parameter-count resolution, decided before the config is
/// touched, so a rejected add leaves the in-memory config and the
/// file on disk exactly as they were. Pure — unit-tested without
/// Tauri state.
///
/// `params_b` (total parameters, in billions) resolves from the
/// caller's explicit value when positive, else from the curated
/// catalog ([`manager_models::catalog_params_b`]: id match, then
/// exact source-URL match). When no source yields a count the add
/// fails: config validation requires a positive `params_b`, and the
/// value feeds memory-fit estimation that nothing re-derives later,
/// so storing a placeholder would silently corrupt fit decisions.
#[allow(clippy::too_many_arguments)]
pub(crate) fn model_add_entry(
    id: &str,
    source_url: &str,
    file_name: Option<String>,
    quant: Option<String>,
    params_b: Option<f32>,
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
    mtp: Option<bool>,
    sha256: Option<String>,
) -> Result<manager_config::ModelConfig, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::InvalidInput("model id is required".to_string()));
    }
    if !source_url.starts_with("https://") {
        return Err(ApiError::InvalidInput(
            "source URL must be an https:// URL for a GGUF file".to_string(),
        ));
    }
    let file_name = file_name
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| {
            source_url
                .rsplit('/')
                .next()
                .unwrap_or("model.gguf")
                .to_string()
        });
    if !file_name.to_lowercase().ends_with(".gguf")
        || file_name.contains('/')
        || file_name.contains('\\')
    {
        return Err(ApiError::InvalidInput(
            "file name must be a plain .gguf file name".to_string(),
        ));
    }
    manager_config::validate_moe_declaration(moe_layers, default_offload_layers)
        .map_err(ApiError::InvalidInput)?;
    // An optional SHA-256 pin (design Part 2, P2-D6): an
    // HF-picked single-file add passes the sibling's lfs.sha256
    // and the record starts pinned, so its download verifies
    // like a catalog model's. A malformed value is refused —
    // a pin that is not a full hash would silently verify
    // nothing.
    let sha256 = match sha256
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(pin) if is_real_pin(&pin) => pin,
        Some(_) => {
            return Err(ApiError::InvalidInput(
                "sha256 must be a full 64-character hex SHA-256 hash".to_string(),
            ));
        }
        None => String::new(),
    };
    let params_b = params_b
        .filter(|p| *p > 0.0)
        .or_else(|| manager_models::catalog_params_b(&id, source_url))
        .ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "parameter count (params_b) is unknown for model '{id}': no value was supplied and the model is not in the curated catalog — supply the model's total parameter count in billions (e.g. 80 for an 80B model)"
            ))
        })?;
    Ok(manager_config::ModelConfig {
        gguf_path: PathBuf::from("models").join(&file_name),
        id,
        quant: quant
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        params_b,
        sha256,
        verified_at: None,
        verified_sha256: None,
        source_url: source_url.to_string(),
        assigned_backends: Vec::new(),
        enabled: true,
        display_name: None,
        moe_layers,
        default_offload_layers,
        mtp,
    })
}

#[cfg(test)]
mod tests;
