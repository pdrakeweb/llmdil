//! Building a backend's launch spec: GPU visibility, raw device
//! selection, and the supervisor spec set.

use crate::*;

/// GPU visibility for one backend launch.
///
/// llama.cpp's CUDA backend creates a context on **every visible GPU**
/// at startup (~256 MiB each on Windows), so a backend placed on one
/// GPU permanently wastes that much VRAM on every other GPU — VRAM
/// the placed models cannot use (Pete's machines: GPU memory is the
/// scarce resource, system RAM is not). Launches are therefore
/// restricted with `CUDA_VISIBLE_DEVICES` to exactly the placement's
/// GPUs, expressed as UUIDs (stable across index shifts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GpuVisibility {
    /// Raw flags pin GPU layers to 0: the backend loads nothing on
    /// any GPU. `CUDA_VISIBLE_DEVICES` is set empty, so no context
    /// is created anywhere.
    CpuOnly,
    /// Restrict to these GPU indices (the placement's GPUs, in order).
    Gpus(Vec<u32>),
    /// Placement cannot be determined from the config (a configured
    /// split with no positive entry — possible only in a hand-edited
    /// config, validation requires sum 1.0). The launch is left
    /// exactly as it was: no env restriction, no flag remapping.
    Undetermined,
}

/// The device set a backend's raw flags select, when they carry
/// device selection at all (`--main-gpu`, `--device`/`-dev`,
/// `--tensor-split`/`-ts`, in either spelling). `Unparseable`
/// covers a selection the parser cannot fully resolve (a device
/// entry that is not `CUDA<n>`, a non-numeric main GPU, a split
/// entry that is not a number): the selection exists but its set
/// cannot be determined.
pub(crate) enum RawDeviceSelection {
    None,
    Set(std::collections::BTreeSet<u32>),
    Unparseable,
}

pub(crate) fn raw_device_selection(raw: Option<&str>) -> RawDeviceSelection {
    let Some(raw) = raw else {
        return RawDeviceSelection::None;
    };
    let owned = manager_models::split_raw_flags(raw);
    let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut set = std::collections::BTreeSet::new();
    let mut any = false;
    let mut i = 0;
    while i < tokens.len() {
        let (flag, inline) = match tokens[i].split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (tokens[i], None),
        };
        let value = inline.or_else(|| tokens.get(i + 1).copied());
        match flag {
            "--main-gpu" | "-mg" => {
                any = true;
                match value.and_then(|v| v.parse::<u32>().ok()) {
                    Some(n) => {
                        set.insert(n);
                    }
                    None => return RawDeviceSelection::Unparseable,
                }
            }
            "--device" | "-dev" => {
                any = true;
                let Some(v) = value else {
                    return RawDeviceSelection::Unparseable;
                };
                for entry in v.split(',') {
                    match entry
                        .trim()
                        .strip_prefix("CUDA")
                        .and_then(|n| n.parse::<u32>().ok())
                    {
                        Some(n) => {
                            set.insert(n);
                        }
                        None => return RawDeviceSelection::Unparseable,
                    }
                }
            }
            "--tensor-split" | "-ts" => {
                any = true;
                let Some(v) = value else {
                    return RawDeviceSelection::Unparseable;
                };
                for (idx, entry) in v.split(',').enumerate() {
                    match entry.trim().parse::<f32>() {
                        Ok(f) if f > 0.0 => {
                            set.insert(idx as u32);
                        }
                        Ok(_) => {}
                        Err(_) => return RawDeviceSelection::Unparseable,
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if any {
        RawDeviceSelection::Set(set)
    } else {
        RawDeviceSelection::None
    }
}

/// The backend's GPU placement, from its configuration alone: the
/// configured tensor split's positive entries (placement_apply is
/// what writes splits, so the split IS the placement record); an
/// unsplit backend loads on the main GPU (index 0), llama.cpp's
/// default; raw flags pinning GPU layers to 0 mean no GPU placement.
pub(crate) fn backend_gpu_visibility(b: &manager_config::BackendConfig) -> GpuVisibility {
    if raw_flags_pin_cpu_only(b.raw_flags.as_deref()) {
        return GpuVisibility::CpuOnly;
    }
    if b.flags.tensor_split.is_empty() {
        // The GPU 0 default is an assumption about an unsplit
        // launch — void when the operator's raw flags carry their
        // own device selection. Remapping such a selection into
        // the assumed {0} silently moved the backend to GPU 0
        // (the raw --main-gpu/--device was dropped and visibility
        // restricted to GPU 0's UUID), so a selection that is not
        // exactly {0} makes the placement indeterminable and the
        // launch goes out unrestricted, raw flags verbatim
        // (code-review 4.4). A structured split, when present,
        // remains the source of truth and raw selection yields to
        // it, as before.
        match raw_device_selection(b.raw_flags.as_deref()) {
            RawDeviceSelection::None => {}
            RawDeviceSelection::Set(set) if set.len() == 1 && set.contains(&0) => {}
            _ => return GpuVisibility::Undetermined,
        }
        return GpuVisibility::Gpus(vec![0]);
    }
    let placed: Vec<u32> = b
        .flags
        .tensor_split
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0.0)
        .map(|(i, _)| i as u32)
        .collect();
    if placed.is_empty() {
        GpuVisibility::Undetermined
    } else {
        GpuVisibility::Gpus(placed)
    }
}

/// True when raw flags set GPU layers to 0 (`-ngl 0`,
/// `--gpu-layers 0`, `--n-gpu-layers=0`, ...): the backend is
/// CPU-only. Expert offload (`-ot exps=CPU`) is NOT this — the
/// non-expert weights still load on the GPU.
///
/// llama.cpp applies the LAST occurrence of a repeated flag, so
/// the last `-ngl`-family value decides: `-ngl 99 -ngl 0` is
/// CPU-only and `-ngl 0 -ngl 99` is not. The first-match reading
/// this replaces classified the second form CPU-only and launched
/// it with an empty CUDA_VISIBLE_DEVICES (code-review 4.7).
pub(crate) fn raw_flags_pin_cpu_only(raw: Option<&str>) -> bool {
    let Some(raw) = raw else { return false };
    let owned = manager_models::split_raw_flags(raw);
    let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut last: Option<&str> = None;
    let mut i = 0;
    while i < tokens.len() {
        let (flag, inline) = match tokens[i].split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (tokens[i], None),
        };
        if matches!(flag, "-ngl" | "--gpu-layers" | "--n-gpu-layers") {
            if let Some(value) = inline.or_else(|| tokens.get(i + 1).copied()) {
                last = Some(value);
            }
        }
        i += 1;
    }
    last == Some("0")
}

/// Position of global GPU index `v` within the visible list, if the
/// GPU is placed there.
pub(crate) fn visible_position(v: &str, placed: &[u32]) -> Option<usize> {
    let idx: u32 = v.parse().ok()?;
    placed.iter().position(|p| *p == idx)
}

/// Remap a llama.cpp `--device` list (`CUDA0,CUDA2`) onto the visible
/// subset: entries naming placed GPUs become their visible positions;
/// entries naming other GPUs are dropped; entries that do not parse
/// as `CUDA<n>` pass through unchanged. `None` when nothing survives.
pub(crate) fn remap_device_list(v: &str, placed: &[u32]) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in v.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry
            .strip_prefix("CUDA")
            .and_then(|n| n.parse::<u32>().ok())
        {
            Some(num) => {
                if let Some(pos) = placed.iter().position(|p| *p == num) {
                    out.push(format!("CUDA{pos}"));
                }
            }
            None => out.push(entry.to_string()),
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(","))
    }
}

/// Build the supervisor process spec for one configured backend: the
/// wizard-installed llama-server binary, the configured model (relative
/// paths resolve under the data dir), port, and structured flags.
///
/// The launch is GPU-restricted (see [`GpuVisibility`]): when the
/// placement resolves and every placed GPU's UUID is known from
/// `gpus` (the telemetry inventory), `CUDA_VISIBLE_DEVICES` carries
/// exactly the placed GPUs' UUIDs and the device-selection flags are
/// remapped onto the visible subset — the structured `--tensor-split`
/// keeps only the placed GPUs' proportions in placement order, and
/// operator-supplied `--tensor-split` / `--main-gpu` / `--device` in
/// raw flags are remapped or dropped against the same placement
/// (placement is the source of truth; placement_apply writes splits).
/// When a placed GPU's UUID is unknown (GPU absent from the
/// inventory), the launch is left unrestricted rather than guessing.
pub(crate) fn backend_spec(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
) -> ProcessSpec {
    let program = cfg.data_dir.join("llamacpp").join(if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    });
    // Wherever the file is right now — hot first, then a cold tier
    // (docs/tiered-model-storage.md). A start never waits on a copy.
    let model = cfg.resolve_model(&b.model_file).path;
    let mut args: Vec<String> = vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "--port".into(),
        b.port.to_string(),
        "-c".into(),
        b.flags.n_ctx.to_string(),
        "-b".into(),
        b.flags.n_batch.to_string(),
        // --jinja is explicit for every backend (MCP design §7.1):
        // structured tool calls depend on the Jinja chat template,
        // and the pinned llama.cpp build enables it by default —
        // restating it makes the precondition audit-visible and
        // pin-independent instead of relying on an upstream default.
        "--jinja".into(),
    ];

    // The visible GPU indices (empty for CPU-only) when the launch is
    // restricted; None when it is not (undetermined placement, or a
    // placed GPU missing from the inventory).
    let visible: Option<Vec<u32>> = match backend_gpu_visibility(b) {
        GpuVisibility::CpuOnly => Some(Vec::new()),
        GpuVisibility::Gpus(indices)
            if indices
                .iter()
                .all(|i| gpus.iter().any(|g| g.index == *i && !g.uuid.is_empty())) =>
        {
            Some(indices)
        }
        _ => None,
    };
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(placed) = &visible {
        let uuids = placed
            .iter()
            .map(|i| {
                gpus.iter()
                    .find(|g| g.index == *i)
                    .map(|g| g.uuid.clone())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(",");
        env.push(("CUDA_VISIBLE_DEVICES".into(), uuids));
    }

    // Structured tensor split: remapped onto the visible subset when
    // restricted (placed GPUs' proportions only, in placement order);
    // dropped for a CPU-only launch (a split over no visible devices
    // is meaningless); verbatim when unrestricted.
    let split_arg: Option<String> = if b.flags.tensor_split.is_empty() {
        None
    } else {
        match &visible {
            Some(placed) if !placed.is_empty() => Some(
                placed
                    .iter()
                    .map(|i| b.flags.tensor_split[*i as usize].to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Some(_) => None,
            None => Some(
                b.flags
                    .tensor_split
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        }
    };
    if let Some(split) = split_arg {
        args.push("--tensor-split".into());
        args.push(split);
    }
    args.push("--split-mode".into());
    args.push(split_mode_str(b.flags.split_mode).into());
    // llama-server's own defaults (`-ngl auto`, `--fit on`) move layers
    // to the CPU when the model does not fit, at a fraction of the
    // speed. A backend that does not fit must fail to load instead;
    // only the operator's own flags decide otherwise.
    let raw_has = |flags: &[&str]| {
        b.raw_flags.as_deref().is_some_and(|raw| {
            manager_models::split_raw_flags(raw)
                .iter()
                .any(|t| flags.contains(&t.split('=').next().unwrap_or(t)))
        })
    };
    if !raw_has(&["-ngl", "--gpu-layers", "--n-gpu-layers"]) {
        args.extend(["-ngl".into(), "all".into()]);
    }
    if !raw_has(&["-fit", "--fit"]) {
        args.extend(["--fit".into(), "off".into()]);
    }
    // llama-server's default lets any web page in the operator's
    // browser call a backend and read the answer. The gateway and this
    // app are not browsers, so only localhost pages keep access.
    if !raw_has(&["--cors-origins"]) {
        args.extend(["--cors-origins".into(), "localhost".into()]);
    }
    if !raw_has(&["--cors-credentials", "--no-cors-credentials"]) {
        args.push("--no-cors-credentials".into());
    }
    if let Some(raw) = &b.raw_flags {
        // raw_flags are operator-supplied llama-server arguments, with
        // three reservations:
        // - --host is filtered out (both "--host X" and "--host=X").
        //   Backends bind loopback; the LiteLLM gateway is the only
        //   network entry point, and an unauthenticated llama-server
        //   bound to the LAN by a stray flag would bypass it.
        // - --no-jinja is filtered out (both spellings, bare tokens
        //   with no value, plus the "--jinja=false" form). --jinja is
        //   app-owned (see the structured args above): it is the
        //   precondition for structured tool calling (MCP design
        //   §7.1), so an operator flag must not silently disable it.
        // - When the launch is GPU-restricted, device-selection flags
        //   are remapped onto the visible subset (placement is the
        //   source of truth — see the fn docs).
        // Tokenized like a shell would for the operator's quotes —
        // `'…'`/`"…"` group and are removed — but with every
        // backslash literal (regexes in `-ot` need them); there is
        // no shell, so whitespace-splitting alone handed
        // llama-server literal quote characters
        // (see manager_models::split_raw_flags for the full rule).
        let owned = manager_models::split_raw_flags(raw);
        let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
        let mut i = 0;
        while i < tokens.len() {
            let t = tokens[i];
            if t == "--host" {
                i += 2; // skip the flag and its value
                continue;
            }
            if t.starts_with("--host=") {
                i += 1;
                continue;
            }
            if t == "--no-jinja" || t == "--no_jinja" || t == "--jinja=false" {
                i += 1; // bare token, no value to skip
                continue;
            }
            if let Some(placed) = &visible {
                if t == "--tensor-split" {
                    i += 2; // the structured split above is authoritative
                    continue;
                }
                if t.starts_with("--tensor-split=") {
                    i += 1;
                    continue;
                }
                if t == "--main-gpu" {
                    if let Some(pos) = tokens.get(i + 1).and_then(|v| visible_position(v, placed)) {
                        args.push("--main-gpu".into());
                        args.push(pos.to_string());
                    }
                    i += 2;
                    continue;
                }
                if let Some(v) = t.strip_prefix("--main-gpu=") {
                    if let Some(pos) = visible_position(v, placed) {
                        args.push("--main-gpu".into());
                        args.push(pos.to_string());
                    }
                    i += 1;
                    continue;
                }
                if t == "--device" || t == "-dev" {
                    if let Some(mapped) =
                        tokens.get(i + 1).and_then(|v| remap_device_list(v, placed))
                    {
                        args.push(t.to_string());
                        args.push(mapped);
                    }
                    i += 2;
                    continue;
                }
                if let Some((flag, v)) = t
                    .split_once('=')
                    .filter(|(f, _)| *f == "--device" || *f == "-dev")
                {
                    if let Some(mapped) = remap_device_list(v, placed) {
                        args.push(flag.to_string());
                        args.push(mapped);
                    }
                    i += 1;
                    continue;
                }
            }
            args.push(t.to_string());
            i += 1;
        }
    }
    ProcessSpec {
        id: b.id.clone(),
        program,
        args,
        env,
        workdir: None,
        port: Some(b.port),
        // llama-server backends spawn below normal priority: a
        // backend thrashing memory must yield the operator's
        // desktop session (Part B of the start-preflight build).
        low_priority: true,
        // Captured output also persists to logs/backend-<id>.std*.log
        // (the previous run in `.prev`, a 32 MiB segment cap): the
        // cause of an earlier crash must survive the in-memory ring
        // and an app restart (debug console design §1).
        stdout_log: Some(backend_output_log(&cfg.data_dir, &b.id, "stdout")),
        stderr_log: Some(backend_output_log(&cfg.data_dir, &b.id, "stderr")),
    }
}

/// `logs/backend-<id>.<stream>.log` under the data dir: the file both
/// process mode (supervisor capture) and service mode (the shim)
/// write a backend's output to.
pub(crate) fn backend_output_log(
    data_dir: &std::path::Path,
    id: &str,
    stream: &str,
) -> std::path::PathBuf {
    data_dir
        .join("logs")
        .join(format!("backend-{id}.{stream}.log"))
}

/// (Re)register supervisor specs from the current config so start/stop/
/// restart/status always act on what is configured now. Registration only
/// touches the spec table, never live processes: re-registering a running
/// backend replaces the spec its NEXT spawn will use. Specs for ids no
/// longer configured are pruned — except while their process is live
/// (unregister refuses), so a removed-but-running backend stays visible
/// and stoppable until it exits.
pub(crate) async fn ensure_backend_specs(state: &AppState) {
    let cfg = state.config.read().await.clone();
    // GPU inventory (index -> UUID) for launch construction. The
    // config read above has completed and dropped its guard, so
    // touching the telemetry accessor here cannot nest (it reads the
    // config while initializing) — the same rule placement_propose
    // follows. Inventory is populated once at telemetry init.
    let store = state.telemetry().await;
    let gpus = lock_telemetry(&store).inventory();
    let mut sup = state.supervisor().await.lock().await;
    for b in &cfg.backends {
        sup.register_spec(backend_spec(&cfg, b, &gpus));
    }
    let configured: std::collections::HashSet<&str> =
        cfg.backends.iter().map(|b| b.id.as_str()).collect();
    // Record which spec ids belong to backends. The Supervisor is
    // shared with the MCP subsystem, whose specs must never be pruned
    // or surfaced by the backend commands.
    {
        let mut owned = state
            .backend_spec_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for b in &cfg.backends {
            owned.insert(b.id.clone());
        }
        // Prune backend-owned specs whose ids left the config.
        // unregister_spec refuses while a process is live under the id,
        // so a removed-but-running backend keeps its spec (and stays
        // visible + stoppable via list_backends) until it exits.
        let stale: Vec<String> = owned
            .iter()
            .filter(|id| !configured.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale {
            if sup.unregister_spec(&id) {
                owned.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests;
