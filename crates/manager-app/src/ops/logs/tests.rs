use super::*;
use crate::test_support::*;

/// T-05: the diagnostics bundle carries the JSON Lines app log
/// (rotated `.prev` first, then the live file) as `logs/app.log`,
/// and omits the scope when the app has logged nothing.
#[test]
fn diagnostics_bundle_includes_app_log() {
    let dir = unique_test_dir("llm-mgr-diag-applog");
    assert!(app_log_scope(&dir).is_none());
    let path = logging::app_log_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(prev_log_path(&path), "{\"n\":1}\n").unwrap();
    std::fs::write(&path, "{\"n\":2}\n{\"n\":3}\n").unwrap();
    let scope = app_log_scope(&dir).expect("app scope");
    assert_eq!(scope.0, "app");
    let bundle = manager_config::export_diagnostics(
        &AppConfig::default_config(),
        &[scope],
        "{}",
        &[],
        &dir.join("diagnostics"),
    )
    .unwrap();
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&bundle).unwrap()).unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut archive.by_name("logs/app.log").unwrap(), &mut text)
        .unwrap();
    assert_eq!(text, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn optimize_log_roundtrip_with_previous_file() {
    // Tracker #52: the optimize scope's append/reader pair —
    // lines land in logs/optimize.log under source "optimize",
    // and a rotated optimize.log.prev reads back archived
    // with has_previous set (the Logs-view scope contract).
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-optimize-log-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(
        dir.join("logs").join("optimize.log.prev"),
        "[2026-01-01T00:00:00Z] earlier run line\n",
    )
    .unwrap();
    append_optimize_log(
        &dir,
        &["[2026-01-01T00:01:00Z] consultation start".to_string()],
    );
    append_optimize_log(&dir, &[]); // empty appends are no-ops
    let (records, has_previous) = optimize_log_records(&dir);
    assert!(has_previous);
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.source == "optimize"));
    let archived: Vec<&LogRecord> = records.iter().filter(|r| r.archived).collect();
    assert_eq!(archived.len(), 1);
    assert!(archived[0].line.contains("earlier run line"));
    let current: Vec<&LogRecord> = records.iter().filter(|r| !r.archived).collect();
    assert_eq!(current.len(), 1);
    assert!(current[0].line.contains("consultation start"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_scope_records_order_markers_by_their_own_timestamps() {
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-file-scope-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    // Raw LiteLLM output: no per-line stamps (file mtime = now).
    std::fs::write(
        logs.join("gateway.stdout.log"),
        "INFO: boot\nINFO: serving\n",
    )
    .unwrap();
    std::fs::write(logs.join("gateway.stderr.log.prev"), "old crash trace\n").unwrap();
    let lifecycle_log = lifecycle::log_path(&dir, lifecycle::GATEWAY);
    for (secs, event) in [(0, "restart requested"), (5, "stopped"), (9, "started")] {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-12T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(secs);
        manager_supervisor::lifecycle::append_marker(
            &lifecycle_log,
            &manager_supervisor::lifecycle::marker_line(at, "gateway", event, None, None),
        );
    }
    let files = file_log_scopes(&dir, &[])
        .into_iter()
        .find(|(id, _)| id == "gateway")
        .unwrap()
        .1;
    let (mut records, previous) = file_scope_records("gateway", &files, true);
    assert!(previous, "the stderr .prev run is reported");
    assert!(records.iter().all(|r| r.source == "gateway"));
    assert_eq!(records.iter().filter(|r| r.archived).count(), 1);
    records.sort_by_key(|r| std::cmp::Reverse(r.ts));
    let markers: Vec<&str> = records
        .iter()
        .filter(|r| r.line.contains("[lifecycle]"))
        .map(|r| r.line.rsplit(": ").next().unwrap())
        .collect();
    // Newest first by the markers' own stamps, not the file mtime.
    assert_eq!(markers, ["started", "stopped", "restart requested"]);
    let started = records
        .iter()
        .find(|r| r.line.ends_with("started"))
        .unwrap();
    assert_eq!(started.ts.to_rfc3339(), "2026-11-12T10:00:09+00:00");
    // Unstamped lines keep their file order (newest-first emission).
    let raw: Vec<&str> = records
        .iter()
        .filter(|r| r.line.starts_with("INFO"))
        .map(|r| r.line.as_str())
        .collect();
    assert_eq!(raw, ["INFO: serving", "INFO: boot"]);
    // Presence without reading.
    assert_eq!(file_scope_presence(&files), (true, true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// F-prev: lifecycle lines above the newest start are the previous
/// run's, like its rotated stderr.
#[test]
fn lifecycle_lines_before_the_newest_start_are_archived() {
    let dir = unique_test_dir("llm-mgr-lifecycle-archived");
    let key = lifecycle::backend_key("planner");
    for (event, detail) in [
        ("start requested", None),
        ("started", Some("pid 1")),
        ("crashed", Some("exit code: 1")),
        ("restart requested", None),
        ("start failed", Some("port 8081 is already in use")),
    ] {
        lifecycle::mark(&dir, &key, event, detail, Some(lifecycle::Origin::Ui));
    }
    let files = file_log_scopes(&dir, &["planner".to_string()])
        .into_iter()
        .find(|(id, _)| id == "planner")
        .unwrap()
        .1;
    let (records, _) = file_scope_records("planner", &files, true);
    let archived: Vec<bool> = records.iter().map(|r| r.archived).collect();
    assert_eq!(archived, [false, false, true, true, true], "{records:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn get_logs_serves_file_backed_lifecycle_scopes() {
    let mut cfg = test_cfg();
    cfg.backends.truncate(1);
    let backend = cfg.backends[0].id.clone();
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    lifecycle::mark(&dir, lifecycle::POSTGRES, "ready", Some("pid 7"), None);
    lifecycle::mark(
        &dir,
        &lifecycle::backend_key(&backend),
        "restart requested",
        None,
        Some(lifecycle::Origin::Activation),
    );
    std::fs::write(dir.join("logs").join("service-manage.log"), "registered\n").unwrap();
    let page = |v: &Value| -> Vec<String> {
        v["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["line"].as_str().unwrap().to_string())
            .collect()
    };
    let pg = op_get_logs(
        &state,
        &sink,
        "postgres".to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let lines = page(&pg);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].ends_with("postgres: ready: pid 7"));
    let ids: Vec<&str> = pg["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"postgres"));
    assert!(ids.contains(&"services"));
    assert!(ids.contains(&backend.as_str()), "{ids:?}");
    assert!(!ids.contains(&"gateway"), "empty scopes are not listed");
    let be = op_get_logs(
        &state,
        &sink,
        backend.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let lines = page(&be);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("restart requested — by activation"));
    assert_eq!(be["lines"][0]["source"], json!(backend));
    let all = op_get_logs(
        &state,
        &sink,
        "all".to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(page(&all).len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// B3: a file line carries its own time (LiteLLM's time of day,
/// PostgreSQL's date and time); an unstamped line takes the stamped
/// line before it, never the read time, and file order holds.
#[test]
fn file_lines_carry_their_own_timestamps() {
    let dir = unique_test_dir("llm-mgr-line-ts");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gateway.stderr.log");
    let local = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    std::fs::write(
        &path,
        "boot\n23:59:58 - LiteLLM:ERROR: a.py:1 - late\nTraceback\n\
         \u{1b}[92m00:00:02 - LiteLLM Proxy:WARNING: b.py:2 - early\u{1b}[0m\n\
         2026-11-25 12:55:29.833 EST [8712] LOG:  local\n\
         2026-11-25 23:00:00.000 UTC [8712] LOG:  utc\n",
    )
    .unwrap();
    let files = [(path, LogStream::Stderr)];
    let (records, _) = timed_file_records("gateway", &files, false);
    let by = |needle: &str| {
        records
            .iter()
            .find(|r| r.record.line.contains(needle))
            .unwrap()
    };
    assert_eq!(by("local").record.ts, local("2026-11-25 12:55:29.833"));
    assert_eq!(
        by("utc").record.ts.to_rfc3339(),
        "2026-11-25T23:00:00+00:00"
    );
    // Time-only stamps are dated backwards from the next known instant,
    // across midnight.
    assert_eq!(by("early").record.ts, local("2026-11-25 00:00:02.0"));
    assert_eq!(by("late").record.ts, local("2026-11-24 23:59:58.0"));
    assert!(!by("late").estimated);
    let trace = by("Traceback");
    assert_eq!(trace.record.ts, by("late").record.ts);
    assert!(trace.estimated && by("boot").estimated);
    // From/To: an unstamped line stays while it can lie in the window.
    let ms = |s: &str| Some(local(s).timestamp_millis());
    assert!(trace.within(ms("2026-11-25 00:00:00.0"), None));
    assert!(!trace.within(ms("2026-11-25 00:00:03.0"), None));
    assert!(!trace.within(None, ms("2026-11-24 23:00:00.0")));
    let mut sorted: Vec<&TimedRecord> = records.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.record.ts));
    let order: Vec<&str> = sorted.iter().map(|r| r.record.line.as_str()).collect();
    let file_order: Vec<&str> = records.iter().map(|r| r.record.line.as_str()).collect();
    assert_eq!(order, file_order);
    let _ = std::fs::remove_dir_all(&dir);
}
