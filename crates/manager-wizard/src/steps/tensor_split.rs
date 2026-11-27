//! Step 3: configure tensor-split.

use super::*;

// ---------------------------------------------------------------------------
// 3. Configure tensor-split
// ---------------------------------------------------------------------------

/// Compute per-GPU tensor-split fractions (hundredths, summing to exactly
/// 1.0), proportional to VRAM.
pub fn compute_splits(vram_mib: &[u64]) -> Result<Vec<f32>, WizardError> {
    let total: u64 = vram_mib.iter().sum();
    if total == 0 {
        return Err(WizardError::StepFailed(
            STEP_CONFIGURE_TENSOR_SPLIT.to_string(),
            "zero total VRAM across discovered GPUs".to_string(),
        ));
    }
    Ok(hundredths_of(
        &vram_mib.iter().map(|v| *v as f64).collect::<Vec<_>>(),
    ))
}

/// Split weights into hundredths fractions summing to exactly 1.0
/// (integer arithmetic; the rounding residue goes to the largest
/// share). Zero or negative total weight yields all-zero fractions.
fn hundredths_of(weights: &[f64]) -> Vec<f32> {
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return weights.iter().map(|_| 0.0).collect();
    }
    // Integer hundredths: exact sum, rounding residue goes to the largest share.
    let mut hundredths: Vec<i64> = weights
        .iter()
        .map(|v| ((*v / total) * 100.0).round() as i64)
        .collect();
    let residue = 100 - hundredths.iter().sum::<i64>();
    if let Some(idx) = hundredths
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.cmp(b.1))
        .map(|(i, _)| i)
    {
        hundredths[idx] += residue;
    }
    // Defensive: adversarial rounding on very large GPU counts can push a
    // share negative (each value rounds up by < 0.5, so the residue can be
    // as low as -N/2). Clamp negatives to zero and move the deficit onto
    // the positive shares, largest first; the sum stays exactly 100 and no
    // new negatives are created, so this terminates in one pass.
    let deficit: i64 = hundredths.iter().filter(|&&h| h < 0).map(|h| -h).sum();
    if deficit > 0 {
        for h in hundredths.iter_mut() {
            if *h < 0 {
                *h = 0;
            }
        }
        let mut order: Vec<usize> = (0..hundredths.len()).collect();
        order.sort_by(|&a, &b| hundredths[b].cmp(&hundredths[a]));
        let mut remaining = deficit;
        for &i in &order {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(hundredths[i]);
            hundredths[i] -= take;
            remaining -= take;
        }
        debug_assert_eq!(
            remaining, 0,
            "positive shares always cover the deficit (they sum to 100 + deficit)"
        );
    }
    hundredths.iter().map(|h| *h as f32 / 100.0).collect()
}

/// One backend's input to [`assign_splits`].
pub struct SplitInput {
    /// Model size in bytes (the on-disk file when downloaded, else
    /// the params_b estimate). Zero when unknown — the entry then
    /// receives a VRAM-proportional split.
    pub model_bytes: u64,
    /// The backend's configured tensor split, when one is set.
    pub existing: Option<Vec<f32>>,
    /// True when the caller wants this entry (re)assigned if it has
    /// no valid split. False entries only account for their space.
    pub assign: bool,
}

/// Assign tensor splits across the discovered GPUs.
///
/// The defect this fixes (found on Pete's Precision, 2026-10-15):
/// backends shipped with no split at all, so llama.cpp placed every
/// model on the main GPU and overflowed to system RAM while the
/// second GPU held nothing but CUDA contexts.
///
/// Rules:
/// - An entry with a valid existing split (one fraction per GPU) is
///   kept as-is and its shares are deducted from the free pool. The
///   returned mode for a kept entry is informational only — callers
///   must not overwrite a kept entry's configured `split_mode`.
/// - An accounting-only entry (`assign: false`) with no valid split
///   deducts its whole model from GPU 0 — that is where llama.cpp
///   actually puts an unset backend today.
/// - Assignable entries are placed largest first: a model that fits
///   one GPU's remaining placeable space (90% of VRAM; the rest is
///   headroom for the KV cache, compute buffers, and the CUDA
///   context) goes entirely on the GPU with the most room, as a
///   one-hot split; a larger model is spread across GPUs in
///   proportion to their remaining room.
/// - A single-GPU machine gets no assignments: a split there is a
///   no-op and leaving it unset keeps the configuration honest.
pub fn assign_splits(
    inputs: &[SplitInput],
    gpu_vram_mib: &[u64],
) -> Vec<Option<(Vec<f32>, SplitMode)>> {
    let n = gpu_vram_mib.len();
    let mut out: Vec<Option<(Vec<f32>, SplitMode)>> = vec![None; inputs.len()];
    if n == 0 {
        return out;
    }
    let mut remaining: Vec<f64> = gpu_vram_mib
        .iter()
        .map(|v| *v as f64 * 1_048_576.0 * 0.90)
        .collect();
    // Pass 1: account for entries that will not be (re)assigned.
    for (i, input) in inputs.iter().enumerate() {
        match &input.existing {
            Some(split) if split.len() == n => {
                out[i] = Some((split.clone(), SplitMode::Layer));
                for (g, share) in split.iter().enumerate() {
                    remaining[g] =
                        (remaining[g] - *share as f64 * input.model_bytes as f64).max(0.0);
                }
            }
            _ if !input.assign => {
                remaining[0] = (remaining[0] - input.model_bytes as f64).max(0.0);
            }
            _ => {}
        }
    }
    if n == 1 {
        return out;
    }
    // Pass 2: assign the rest, largest first (stable for ties).
    let mut order: Vec<usize> = (0..inputs.len())
        .filter(|&i| out[i].is_none() && inputs[i].assign)
        .collect();
    order.sort_by(|&a, &b| inputs[b].model_bytes.cmp(&inputs[a].model_bytes));
    for i in order {
        let bytes = inputs[i].model_bytes as f64;
        // GPU with the most room; lowest index wins ties (GPU 0 is
        // the main GPU — prefer filling it first, deterministically).
        let mut g = 0usize;
        for cand in 1..n {
            if remaining[cand] > remaining[g] {
                g = cand;
            }
        }
        let room = remaining[g];
        if bytes > 0.0 && bytes <= room {
            let mut split = vec![0.0f32; n];
            split[g] = 1.0;
            remaining[g] -= bytes;
            out[i] = Some((split, SplitMode::Layer));
            continue;
        }
        // Does not fit on one GPU: spread over the room that remains,
        // falling back to raw capacity proportions when the pool is
        // exhausted (llama.cpp will spill the excess to system RAM,
        // as it does today — but distributed, not all on GPU 0).
        let weights: Vec<f64> = if remaining.iter().sum::<f64>() > 0.0 {
            remaining.clone()
        } else {
            gpu_vram_mib.iter().map(|v| *v as f64).collect()
        };
        let split = hundredths_of(&weights);
        for (g, share) in split.iter().enumerate() {
            remaining[g] = (remaining[g] - *share as f64 * bytes).max(0.0);
        }
        out[i] = Some((split, SplitMode::Layer));
    }
    out
}

/// Estimate a model's on-disk size from its parameter count, using
/// the catalog's derivation (~4.92 bits per weight for Q4_K_M plus
/// ~5% overhead) — the same estimate `placement_propose` uses in the
/// Tauri layer. Used only when the model file is not downloaded yet
/// (the tensor-split step runs before the download step).
pub fn estimate_model_bytes(params_b: f64) -> u64 {
    if params_b > 0.0 {
        (params_b * 1e9 * 4.92 / 8.0 * 1.05) as u64
    } else {
        0
    }
}

/// Step 3: compute per-backend tensor splits from discovered VRAM, write the
/// flag sets, validate sums to 1.0. Audit mode re-validates.
pub struct ConfigureTensorSplit;

fn fmt_split(split: &[f32]) -> String {
    split
        .iter()
        .map(|v| format!("{v:.2}"))
        .collect::<Vec<_>>()
        .join("/")
}

impl ConfigureTensorSplit {
    /// Size of the model a backend serves: the configured model whose
    /// gguf_path the backend's model_file resolves to. The on-disk
    /// length when the file is downloaded, else the params_b
    /// estimate (this step runs before the download step, so the
    /// estimate is the common case on a first run). Zero when the
    /// backend's model cannot be resolved — the assignment then
    /// falls back to a VRAM-proportional split.
    fn model_for<'a>(
        &self,
        ctx: &'a WizardContext,
        backend: &BackendConfig,
    ) -> Option<&'a manager_config::ModelConfig> {
        let target = resolve_dest(&ctx.data_dir, &backend.model_file);
        ctx.config
            .models
            .iter()
            .find(|m| resolve_dest(&ctx.data_dir, &m.gguf_path) == target)
    }

    pub(crate) async fn model_bytes(&self, ctx: &WizardContext, backend: &BackendConfig) -> u64 {
        match self.model_for(ctx, backend) {
            Some(m) => {
                let path = locate_model(ctx, &m.gguf_path).await.dest().to_path_buf();
                match ctx.ops.file_len(&path).await {
                    Some(len) => len,
                    None => estimate_model_bytes(m.params_b as f64),
                }
            }
            None => 0,
        }
    }

    /// The share of a backend's model that actually occupies GPU
    /// memory under expert offload, from the curated catalog's
    /// pinned expert total (the step runs before downloads, so the
    /// GGUF itself is usually unavailable; an unpinned split falls
    /// back to the full size). Non-offload backends return `total`
    /// unchanged.
    fn gpu_resident_bytes(&self, ctx: &WizardContext, backend: &BackendConfig, total: u64) -> u64 {
        if manager_models::expert_offload(backend.raw_flags.as_deref())
            != manager_models::ExpertOffload::AllExperts
        {
            return total;
        }
        let Some(model) = self.model_for(ctx, backend) else {
            return total;
        };
        let pin = manager_models::curated_models()
            .into_iter()
            .find(|s| s.id == model.id)
            .and_then(|s| s.expert_bytes);
        match pin {
            Some(experts) => total.saturating_sub(experts),
            None => total,
        }
    }

    async fn run_install(
        &self,
        ctx: &WizardContext,
        vrams: &[u64],
    ) -> Result<StepOutcome, WizardError> {
        let mut log: Vec<String> = Vec::new();
        let enabled: Vec<&BackendConfig> =
            ctx.config.backends.iter().filter(|b| b.enabled).collect();
        if enabled.is_empty() {
            return Ok(StepOutcome::done(
                "no enabled backends; nothing to place".to_string(),
                log,
            ));
        }
        if vrams.len() == 1 {
            return Ok(StepOutcome::done(
                "single GPU discovered; no tensor split needed — llama.cpp uses the only GPU"
                    .to_string(),
                log,
            ));
        }
        let mut inputs: Vec<SplitInput> = Vec::new();
        for b in &enabled {
            let total = self.model_bytes(ctx, b).await;
            // A backend served by expert offload is not split: its
            // entry only accounts for the GPU-resident share of its
            // weights on GPU 0, where llama.cpp places an unsplit
            // backend.
            let offload = manager_models::expert_offload(b.raw_flags.as_deref())
                != manager_models::ExpertOffload::None;
            inputs.push(SplitInput {
                model_bytes: if offload {
                    self.gpu_resident_bytes(ctx, b, total)
                } else {
                    total
                },
                existing: if b.flags.tensor_split.is_empty() {
                    None
                } else {
                    Some(b.flags.tensor_split.clone())
                },
                assign: !offload,
            });
        }
        let assigned = assign_splits(&inputs, vrams);
        // Steps receive a snapshot clone, so placement is returned as
        // typed config edits (code-review PS-4) and persisted by
        // `deliver_config_edits`: by the app under its config lock,
        // or by this step itself when no app shell owns the config.
        let mut edits: Vec<ConfigEdit> = Vec::new();
        let mut changed = 0usize;
        for (b, result) in enabled.iter().zip(assigned.iter()) {
            let Some((split, mode)) = result else {
                continue;
            };
            if b.flags.tensor_split == *split {
                log.push(format!(
                    "{}: kept existing split [{}]",
                    b.id,
                    fmt_split(split)
                ));
                continue;
            }
            edits.push(ConfigEdit::BackendPlacement {
                backend_id: b.id.clone(),
                tensor_split: split.clone(),
                split_mode: *mode,
            });
            changed += 1;
            log.push(format!(
                "{}: split [{}] assigned (model ~{} GB)",
                b.id,
                fmt_split(split),
                manager_present::gb(
                    inputs[enabled.iter().position(|x| x.id == b.id).unwrap_or(0)].model_bytes
                        as f64
                        / 1_073_741_824.0
                )
            ));
        }
        if changed > 0 {
            // Pre-check against the snapshot; whoever persists the
            // edits validates the real target again on save.
            let mut check = ctx.config.clone();
            apply_config_edits(&mut check, &edits);
            if let Err(e) = manager_config::validate(&check) {
                return Ok(StepOutcome::failed(
                    format!("computed splits failed validation: {e}"),
                    log,
                ));
            }
        }
        let mut outcome = StepOutcome::done(
            format!(
                "tensor splits set for {} enabled backend(s) across {} GPUs ({changed} updated)",
                enabled.len(),
                vrams.len()
            ),
            log,
        );
        outcome.config_edits = edits;
        Ok(deliver_config_edits(ctx, outcome, true))
    }

    fn run_audit(&self, ctx: &WizardContext, vrams: &[u64]) -> StepOutcome {
        let gpu_count = vrams.len();
        let mut log: Vec<String> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let enabled: Vec<&BackendConfig> =
            ctx.config.backends.iter().filter(|b| b.enabled).collect();
        for b in &enabled {
            if gpu_count == 1 {
                log.push(format!("{}: single GPU, no split required", b.id));
                continue;
            }
            if b.flags.tensor_split.is_empty() {
                // An empty split is a valid placement when the
                // backend needs no split: expert offload serves it
                // unsplit by design, and a model whose GPU-resident
                // weights fit GPU 0 is placed whole by llama.cpp.
                if manager_models::expert_offload(b.raw_flags.as_deref())
                    != manager_models::ExpertOffload::None
                {
                    log.push(format!(
                        "{}: no split — served by expert offload, experts in system RAM",
                        b.id
                    ));
                    continue;
                }
                let total = match self.model_for(ctx, b) {
                    Some(m) => manager_models::curated_models()
                        .into_iter()
                        .find(|s| s.id == m.id)
                        .map(|s| s.size_bytes)
                        .unwrap_or_else(|| estimate_model_bytes(m.params_b as f64)),
                    None => 0,
                };
                let resident = self.gpu_resident_bytes(ctx, b, total);
                // An unsplit backend launches on the DEFAULT
                // device, GPU 0 — not whichever GPU is largest.
                // Crediting the largest GPU's capacity cleared
                // models that then failed to start on a smaller
                // GPU 0 (code-review 2.14).
                let gpu0 = vrams.first().copied().unwrap_or(0) * manager_models::MIB;
                if resident > 0 && resident <= gpu0 {
                    log.push(format!(
                        "{}: no split — GPU-resident weights fit GPU 0",
                        b.id
                    ));
                    continue;
                }
                problems.push(format!("{}: no tensor split configured", b.id));
            } else if b.flags.tensor_split.len() != gpu_count {
                problems.push(format!(
                    "{}: split covers {} GPUs, {} discovered",
                    b.id,
                    b.flags.tensor_split.len(),
                    gpu_count
                ));
            } else {
                let sum: f32 = b.flags.tensor_split.iter().sum();
                if (sum - 1.0).abs() > 1e-3 {
                    problems.push(format!("{}: splits sum to {sum:.4}, not 1.0", b.id));
                } else {
                    log.push(format!(
                        "{}: split [{}] valid",
                        b.id,
                        fmt_split(&b.flags.tensor_split)
                    ));
                }
            }
        }
        if problems.is_empty() {
            StepOutcome::done(
                format!(
                    "tensor splits valid for {} enabled backend(s)",
                    enabled.len()
                ),
                log,
            )
        } else {
            for p in &problems {
                log.push(p.clone());
            }
            StepOutcome::failed(
                format!("tensor-split problems: {}", problems.join("; ")),
                log,
            )
        }
    }
}

#[async_trait]
impl WizardStep for ConfigureTensorSplit {
    fn id(&self) -> &str {
        STEP_CONFIGURE_TENSOR_SPLIT
    }
    fn name(&self) -> &str {
        "Configure tensor-split"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![
            STEP_DETECT_HARDWARE.to_string(),
            STEP_INSTALL_LLAMACPP.to_string(),
        ]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Pure compute + config write; no external commands.
        vec![]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run configure-tensor-split to recompute splits from current VRAM")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let gpus = match ctx.ops.gpu_inventory().await {
            Ok(g) => g,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("GPU probe failed: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        };
        if gpus.is_empty() {
            return Ok(StepOutcome::failed(
                "no GPUs discovered; cannot compute tensor splits".to_string(),
                vec![],
            ));
        }
        let vrams: Vec<u64> = gpus.iter().map(|g| g.total_vram_mib).collect();
        match ctx.mode {
            RunMode::Install => self.run_install(ctx, &vrams).await,
            RunMode::Audit => Ok(self.run_audit(ctx, &vrams)),
        }
    }
}
