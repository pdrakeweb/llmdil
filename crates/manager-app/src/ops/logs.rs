//! Log views (file-backed scopes, wizard/optimize logs), tasks, and the
//! diagnostics bundle.

use crate::*;

/// Lines read from the tail of each file-backed log file.
pub(crate) const FILE_SCOPE_TAIL_LINES: usize = 2000;

/// Records of a file-backed log scope: each file in `files` (and, with
/// `with_prev`, its rotated `.prev` run, marked archived), stamped as
/// `source`. A line that starts with an `[<rfc3339>]` stamp (lifecycle
/// markers, wizard/optimize lines) carries its own timestamp; any other
/// line (raw LiteLLM / llama-server / PostgreSQL output) is stamped
/// with its file's modification time. Lines are emitted newest-first
/// within each file so same-timestamp groups keep their order under
/// the caller's stable sort. At most the last 2,000 lines of each file
/// are read. Returns the records and whether any `.prev` run exists.
pub(crate) fn file_scope_records(
    source: &str,
    files: &[(PathBuf, LogStream)],
    with_prev: bool,
) -> (Vec<LogRecord>, bool) {
    let mut records = Vec::new();
    let mut has_previous = false;
    for (base, stream) in files {
        let variants: &[bool] = if with_prev { &[false, true] } else { &[false] };
        for &archived in variants {
            let path = if archived {
                prev_log_path(base)
            } else {
                base.clone()
            };
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if archived {
                has_previous = true;
            }
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(chrono::DateTime::<chrono::Utc>::from)
                .unwrap_or_else(|_| chrono::Utc::now());
            let text = String::from_utf8_lossy(&bytes);
            // A lifecycle log is not rotated per run: everything above
            // the newest start belongs to previous runs.
            let lifecycle_log = base.to_string_lossy().ends_with(".lifecycle.log");
            let mut archived = archived;
            for line in text.lines().rev().take(FILE_SCOPE_TAIL_LINES) {
                records.push(LogRecord {
                    ts: manager_supervisor::lifecycle::parse_line_ts(line).unwrap_or(mtime),
                    stream: *stream,
                    line: line.to_string(),
                    source: source.to_string(),
                    archived,
                });
                archived |= lifecycle_log && begins_run(line);
            }
        }
    }
    (records, has_previous)
}

/// Whether a lifecycle marker is the first line of a service's run.
fn begins_run(line: &str) -> bool {
    use manager_supervisor::lifecycle::{is_marker_line, MARKER_TAG};
    is_marker_line(line)
        && line
            .split_once(MARKER_TAG)
            .and_then(|(_, rest)| rest.split_once(": "))
            .is_some_and(|(_, event)| {
                [
                    "start requested",
                    "restart requested",
                    "adopted",
                    "started: outside the app",
                ]
                .iter()
                .any(|e| event.starts_with(e))
            })
}

/// Whether a file-backed scope has anything to show (a non-empty file
/// or `.prev` run) and whether a `.prev` run exists — metadata only,
/// for listing scopes without reading them.
pub(crate) fn file_scope_presence(files: &[(PathBuf, LogStream)]) -> (bool, bool) {
    let nonempty = |p: &std::path::Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
    let mut any = false;
    let mut prev = false;
    for (base, _) in files {
        let p = prev_log_path(base);
        if nonempty(&p) {
            any = true;
            prev = true;
        }
        if nonempty(base) {
            any = true;
        }
    }
    (any, prev)
}

/// Every file-backed log scope and its files (design H):
/// - `gateway`: LiteLLM stdout/stderr plus `gateway.lifecycle.log`;
/// - `wizard`, `optimize`: the step and pipeline logs;
/// - `postgres`: `postgres.log` plus `postgres.lifecycle.log`;
/// - `log-receiver`: the receiver's stdout/stderr/lifecycle logs and
///   its `deploy.log`;
/// - `services`: `service-manage.log` (the elevated service helper);
/// - `<id>` for each backend id: `backend-<id>.lifecycle.log` and, in
///   service mode, the shim's `backend-<id>.stdout/stderr.log` — merged
///   with the supervisor ring of the same id by `get_logs`.
pub(crate) fn file_log_scopes(
    data_dir: &std::path::Path,
    backend_ids: &[String],
) -> Vec<(String, Vec<(PathBuf, LogStream)>)> {
    let logs = data_dir.join("logs");
    let receiver = manager_services::log_receiver_definition(
        data_dir,
        std::path::Path::new("llmdil-log-receiver.exe"),
    );
    let receiver_dir = manager_services::log_receiver_dir(data_dir);
    let mut receiver_files: Vec<(PathBuf, LogStream)> = Vec::new();
    if let Some(p) = receiver.stdout_log.clone() {
        receiver_files.push((p, LogStream::Stdout));
    }
    if let Some(p) = receiver.stderr_log.clone() {
        receiver_files.push((p, LogStream::Stderr));
    }
    receiver_files.push((
        lifecycle::log_path(data_dir, lifecycle::LOG_RECEIVER),
        LogStream::Stdout,
    ));
    receiver_files.push((receiver_dir.join("deploy.log"), LogStream::Stdout));
    let mut scopes = vec![
        (
            "gateway".to_string(),
            vec![
                (logs.join("gateway.stdout.log"), LogStream::Stdout),
                (logs.join("gateway.stderr.log"), LogStream::Stderr),
                (
                    lifecycle::log_path(data_dir, lifecycle::GATEWAY),
                    LogStream::Stdout,
                ),
            ],
        ),
        (
            "wizard".to_string(),
            vec![(logs.join("wizard.log"), LogStream::Stdout)],
        ),
        (
            "optimize".to_string(),
            vec![(logs.join("optimize.log"), LogStream::Stdout)],
        ),
        (
            "postgres".to_string(),
            vec![
                (
                    manager_wizard::steps::postgres_log_path(data_dir),
                    LogStream::Stdout,
                ),
                (
                    lifecycle::log_path(data_dir, lifecycle::POSTGRES),
                    LogStream::Stdout,
                ),
            ],
        ),
        ("log-receiver".to_string(), receiver_files),
        (
            "services".to_string(),
            vec![(logs.join("service-manage.log"), LogStream::Stdout)],
        ),
    ];
    for id in backend_ids {
        scopes.push((
            id.clone(),
            vec![
                (
                    lifecycle::log_path(data_dir, &lifecycle::backend_key(id)),
                    LogStream::Stdout,
                ),
                (
                    logs.join(format!("backend-{id}.stdout.log")),
                    LogStream::Stdout,
                ),
                (
                    logs.join(format!("backend-{id}.stderr.log")),
                    LogStream::Stderr,
                ),
            ],
        ));
    }
    scopes
}

/// The scope ids [`file_log_scopes`] serves from files alone (no
/// supervisor ring): `get_logs` skips the supervisor lock for these.
pub(crate) const FILE_ONLY_SCOPES: [&str; 6] = [
    "gateway",
    "wizard",
    "optimize",
    "postgres",
    "log-receiver",
    "services",
];

/// The records of one named file-only scope (tests).
#[cfg(test)]
pub(crate) fn named_file_scope_records(
    data_dir: &std::path::Path,
    scope: &str,
) -> (Vec<LogRecord>, bool) {
    file_log_scopes(data_dir, &[])
        .into_iter()
        .find(|(id, _)| id == scope)
        .map(|(id, files)| file_scope_records(&id, &files, true))
        .unwrap_or_default()
}

/// Wizard step output, persisted to `<data_dir>/logs/wizard.log` and
/// served by `get_logs` as the `wizard` scope. Step outcomes otherwise
/// live only in the in-memory wizard report: a step that hangs or an
/// app restart loses them entirely, leaving nothing to inspect (the
/// install-postgres hang, 2026-10-15). Appends are best-effort —
/// logging must never fail the step it describes. Rotation mirrors
/// the gateway logs: past ~1 MiB the file becomes `wizard.prev.log`.
pub(crate) fn append_wizard_log(data_dir: &std::path::Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let logs = data_dir.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return;
    }
    let path = logs.join("wizard.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1_048_576 {
            let _ = std::fs::rename(&path, prev_log_path(&path));
        }
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// One step's block in the wizard log: an outcome header plus the
/// step's own log lines, each stamped so the file stays readable on
/// its own (the reader stamps records with the file mtime, like the
/// gateway files).
pub(crate) fn wizard_log_block(
    label: &str,
    step_id: &str,
    state: &str,
    message: &str,
    duration_ms: u64,
    log_tail: &[String],
) -> Vec<String> {
    let ts = chrono::Utc::now().to_rfc3339();
    let mut out = vec![format!(
        "[{ts}] === step {step_id} ({label}): {state} — {message} ({duration_ms} ms) ==="
    )];
    for line in log_tail {
        out.push(format!("[{ts}] {line}"));
    }
    out
}

/// Optimize pipeline output, persisted to
/// `<data_dir>/logs/optimize.log` and served by `get_logs` as the
/// `optimize` scope (tracker #52). The Optimize pipeline's
/// decisions otherwise live only in the stored plan and the
/// transient consultation response: when a plan comes back wrong
/// (tracker #51's dead-end items) there was nothing to inspect.
/// The optimize modules write very verbose, timestamped records
/// of every consultation, parse, validation verdict, apply
/// outcome, and experiment event through this one append.
/// Appends are best-effort — logging must never fail the
/// operation it describes. Rotation mirrors the wizard log: past
/// ~1 MiB the file rotates to its `.prev` sibling
/// (`optimize.log.prev`, via `prev_log_path`). Secret-free by
/// construction: the only inputs are the operator's directive,
/// the allowlist-built OptimizeContext document (sentinel-tested
/// in manager-optimize), the consulted model's response to that
/// document, and verdicts computed from them — no call site reads
/// credential stores, environment values, or config fields
/// outside the context document.
pub(crate) fn append_optimize_log(data_dir: &std::path::Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let logs = data_dir.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return;
    }
    let path = logs.join("optimize.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1_048_576 {
            let _ = std::fs::rename(&path, prev_log_path(&path));
        }
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// The file-backed optimize scope (see [`file_log_scopes`]).
#[cfg(test)]
pub(crate) fn optimize_log_records(data_dir: &std::path::Path) -> (Vec<LogRecord>, bool) {
    named_file_scope_records(data_dir, "optimize")
}

/// Query captured logs. `scope` is a process id — live, stopped, or an
/// id that also has an archived previous-run ring; querying the id
/// returns current and archived lines merged — `"gateway"`, the
/// file-backed gateway log (also merged into `"all"`) — or `"all"`,
/// which merges every source's records into one stream. `level` is an
/// optional error/warn/info/debug heuristic filter; `search` is an
/// optional case-insensitive substring over line text; `query` is an
/// optional case-insensitive substring over line text and source.
/// `since_ms`/`until_ms` optionally bound record timestamps (epoch
/// milliseconds, both inclusive). All filters apply before counting
/// and paging, so the reported total is the filtered total.
///
/// Ordering is by timestamp: newest-first (`order` absent or
/// `"desc"`, the default in every scope, archives included) or the
/// exact mirror, oldest-first (`order = "asc"`). `offset` skips that
/// many filtered, ordered records before the page.
///
/// Returns `{lines, total_matching, total, truncated, scopes}`:
/// `lines` is the page (`limit` records from `offset`, default limit
/// 200), each record stamped with its `source` process id and an
/// `archived` flag for previous-run records; `total_matching` — and
/// its alias `total` — counts every record that matched before
/// paging, and `truncated` says whether paging cut records on either
/// side of the page. `scopes` lists every retained scope as
/// `{id, previous}`, where `previous` means the id also has an
/// archived previous-run ring (incl. previous run).
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_get_logs(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    scope: String,
    level: Option<String>,
    search: Option<String>,
    query: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    order: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
) -> ApiResult {
    let limit = limit.unwrap_or(200) as usize;
    let backend_ids: Vec<String> = state
        .config
        .read()
        .await
        .backends
        .iter()
        .map(|b| b.id.clone())
        .collect();
    // Unfiltered fetch: filtering, merging with the file-backed
    // records, ordering, and pagination happen here so every source
    // is treated identically. The supervisor lock is held only to
    // copy the ring records and the scope list (never across a file
    // read), and a file-only scope never waits for it (BE-11): it
    // lists the supervisor scopes only when the lock is free.
    let file_only = FILE_ONLY_SCOPES.contains(&scope.as_str());
    let (mut records, sup_scopes): (Vec<LogRecord>, Vec<manager_supervisor::LogScope>) =
        if file_only {
            match state.supervisor().await.try_lock() {
                Ok(sup) => (Vec::new(), sup.log_scopes()),
                Err(_) => (Vec::new(), Vec::new()),
            }
        } else {
            let sup = state.supervisor().await.lock().await;
            (
                sup.logs_query(&scope, None, None, usize::MAX).records,
                sup.log_scopes(),
            )
        };
    let mut scopes: Vec<Value> = sup_scopes
        .into_iter()
        .map(|s| json!({ "id": s.id, "previous": s.previous }))
        .collect();
    for (id, files) in file_log_scopes(&state.data_dir, &backend_ids) {
        let wanted = scope == "all" || scope == id;
        let (present, previous) = if wanted {
            let (file_records, previous) = file_scope_records(&id, &files, true);
            let present = !file_records.is_empty();
            records.extend(file_records);
            (present, previous)
        } else {
            file_scope_presence(&files)
        };
        if !present {
            continue;
        }
        // A backend id may already be listed from its supervisor ring.
        match scopes.iter_mut().find(|s| s["id"] == json!(id)) {
            Some(existing) => {
                if previous {
                    existing["previous"] = json!(true);
                }
            }
            None => scopes.push(json!({ "id": id, "previous": previous })),
        }
    }
    if let Some(filter) = level.as_deref().and_then(LogLevelFilter::parse) {
        records.retain(|r| filter.matches(&r.line));
    }
    if let Some(search) = &search {
        let needle = search.to_lowercase();
        records.retain(|r| r.line.to_lowercase().contains(&needle));
    }
    if let Some(query) = &query {
        let needle = query.to_lowercase();
        records.retain(|r| {
            r.line.to_lowercase().contains(&needle) || r.source.to_lowercase().contains(&needle)
        });
    }
    // Time window, both bounds inclusive. Every record carries a
    // capture timestamp (file-backed gateway/wizard records are
    // stamped with their file's mtime), so the comparison is direct.
    if since_ms.is_some() || until_ms.is_some() {
        records.retain(|r| {
            let ms = r.ts.timestamp_millis();
            since_ms.is_none_or(|s| ms >= s) && until_ms.is_none_or(|u| ms <= u)
        });
    }
    // Newest first; the sort is stable, so records sharing a timestamp
    // keep their source order (gateway files emit newest-first).
    // "asc" is the exact mirror — the sorted vec reversed — so
    // same-timestamp groups (file-backed lines share one mtime) keep
    // their source order in the oldest-first view too, instead of
    // flipping within the group, and page boundaries mirror between
    // the two orders.
    records.sort_by_key(|r| std::cmp::Reverse(r.ts));
    // `order` is strict: an unrecognized value is a caller bug and
    // is refused, not silently treated as the default (code-review
    // 4.17).
    match order.as_deref() {
        None | Some("desc") => {}
        Some("asc") => records.reverse(),
        Some(other) => {
            return Err(ApiError::InvalidInput(format!(
                "unknown log order {other:?}; expected \"asc\" or \"desc\""
            )))
        }
    }
    let total_matching = records.len();
    let offset = offset.unwrap_or(0) as usize;
    let page: Vec<LogRecord> = records.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "lines": serde_json::to_value(&page)?,
        "total_matching": total_matching,
        "total": total_matching,
        // Paging cut records when any matched record sits outside
        // the page — before it (a nonzero offset into a non-empty
        // result) or after it (code-review 4.9: the offset side was
        // previously not reported).
        "truncated": (offset > 0 && total_matching > 0) || total_matching > offset + page.len(),
        "scopes": scopes,
    }))
}

/// Abort a tracked task: cancels its work cooperatively (routing to the
/// operation's own cancel op where one exists) and waits a bounded
/// grace period for it to stop. See [`tasks`].
pub async fn op_task_abort(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    task_id: String,
) -> ApiResult {
    tasks::abort(state, sink, task_id).await
}

/// All tracked tasks.
pub async fn op_tasks_list(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let tasks = state.tasks.records.lock().await;
    let list: Vec<&TaskRecord> = tasks.values().collect();
    Ok(serde_json::to_value(list)?)
}

/// The app's own JSON Lines log (`logs/app.log` + its `.prev`, see
/// [`logging`]) as the bundle's "app" scope: every tracing event the
/// process logged, task-correlated, oldest first and verbatim so the
/// bundle's `logs/app.log` stays machine-readable. `None` when the
/// app has not logged anything yet.
pub(crate) fn app_log_scope(data_dir: &std::path::Path) -> Option<(String, Vec<String>)> {
    let lines = logging::app_log_lines(data_dir, logging::DIAGNOSTICS_MAX_LINES);
    (!lines.is_empty()).then(|| ("app".to_string(), lines))
}

/// Export a diagnostics bundle (spec §14 `diagnostics_export_bundle`):
/// zip of config snapshot + per-scope recent logs + telemetry snapshot +
/// versions file. Returns the bundle path.
pub async fn op_diagnostics_export_bundle(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    // The telemetry poller starts lazily on first use; in a session
    // that never opened a telemetry view, this call starts it and an
    // immediate snapshot is still empty. Give the first poll a short
    // window to land so the bundle carries the machine's GPU state
    // instead of an empty array.
    let store = state.telemetry().await;
    let mut samples: Vec<GpuSample> = lock_telemetry(&store).snapshot();
    for _ in 0..10 {
        if !samples.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        samples = lock_telemetry(&store).snapshot();
    }
    let telemetry_json = serde_json::to_string_pretty(&samples)?;
    // Every retained supervisor log scope (log_scopes() also covers
    // scopes whose process was pruned but whose ring, incl. an archived
    // previous run, is retained), copied under one short lock.
    let mut scope_records: Vec<(String, Vec<LogRecord>)> = {
        let sup = state.supervisor().await.lock().await;
        let mut ids: Vec<String> = sup.log_scopes().into_iter().map(|s| s.id).collect();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let records = sup.logs_query(&id, None, None, 5000).records;
                (id, records)
            })
            .collect()
    };
    // The file-backed scopes join the bundle (no lock held): the
    // gateway's LiteLLM output and lifecycle markers (the record of
    // the most recent gateway failures), the wizard log (step
    // outcomes, unfinished steps, busy rejections), the optimize log,
    // PostgreSQL's server log and markers, the log receiver's logs,
    // the service helper's log, and each backend's lifecycle markers
    // and service-mode output (merged into that backend's scope).
    let backend_ids: Vec<String> = cfg.backends.iter().map(|b| b.id.clone()).collect();
    for (id, files) in file_log_scopes(&state.data_dir, &backend_ids) {
        let (records, _) = file_scope_records(&id, &files, true);
        if records.is_empty() {
            continue;
        }
        match scope_records.iter_mut().find(|(s, _)| *s == id) {
            Some((_, existing)) => existing.extend(records),
            None => scope_records.push((id, records)),
        }
    }
    let mut log_scopes: Vec<(String, Vec<String>)> = Vec::new();
    for (scope, mut records) in scope_records {
        // Oldest first; reversing the newest-first sources first keeps
        // same-timestamp groups in file order under the stable sort.
        records.reverse();
        records.sort_by_key(|r| r.ts);
        let lines: Vec<String> = records
            .iter()
            .map(|r| {
                // Archived (previous-run) records keep their marker in
                // the bundle so the distinction survives export.
                let archived = if r.archived { " [archived]" } else { "" };
                format!(
                    "[{}] [{:?}]{} {}",
                    r.ts.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                    r.stream,
                    archived,
                    r.line
                )
            })
            .collect();
        log_scopes.push((scope, lines));
    }
    // The JSON Lines app log (T-05) joins as the "app" scope.
    if let Some(scope) = app_log_scope(&state.data_dir) {
        log_scopes.push(scope);
    }
    let versions = vec![
        ("app".to_string(), env!("CARGO_PKG_VERSION").to_string()),
        (
            "config_schema".to_string(),
            manager_config::CURRENT_VERSION.to_string(),
        ),
    ];
    let dest_dir = state.data_dir.join("diagnostics");
    let path = manager_config::export_diagnostics(
        &cfg,
        &log_scopes,
        &telemetry_json,
        &versions,
        &dest_dir,
    )
    .map_err(ApiError::from)?;
    Ok(json!({ "path": path }))
}

#[cfg(test)]
mod tests;
