//! Phase 2 sources, end to end: each test runs the FULL shipper
//! pipeline (tap → redact → batch → gzip → POST) against a local
//! capture endpoint and asserts on the records that arrived — the
//! source and stream they carry, that secrets in representative
//! lines never reach the wire, that a disabled source ships
//! nothing, and what the per-source status rows report.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manager_logship::shipper::{SourceState, SourceStatus};
use manager_logship::{ShipLevel, ShipperConfig, ShipperHandle};

const INGEST: &str = "SOURCESINGESTTOKEN1a2b3c";

#[derive(Default)]
struct Capture {
    requests: Mutex<Vec<Vec<u8>>>,
}

impl Capture {
    /// Decoded records from every captured request body.
    fn records(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for req in self.requests.lock().unwrap().iter() {
            let Some(pos) = find(req, b"\r\n\r\n") else {
                continue;
            };
            let mut dec = flate2::read::GzDecoder::new(&req[pos + 4..]);
            let mut text = String::new();
            if dec.read_to_string(&mut text).is_ok() {
                if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&text) {
                    out.extend(items);
                }
            }
        }
        out
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn request_len(buf: &[u8]) -> Option<usize> {
    let head_end = find(buf, b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())?;
    (buf.len() >= head_end + len).then_some(head_end + len)
}

/// Minimal HTTP capture server (content-length framed requests).
fn serve(listener: TcpListener, capture: Arc<Capture>) {
    for stream in listener.incoming() {
        let Ok(mut sock) = stream else { break };
        let capture = capture.clone();
        std::thread::spawn(move || {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(800)));
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                if let Some(total) = request_len(&buf) {
                    let req = buf.drain(..total).collect::<Vec<u8>>();
                    capture.requests.lock().unwrap().push(req);
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
                    );
                    continue;
                }
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
        });
    }
}

struct Rig {
    dir: PathBuf,
    capture: Arc<Capture>,
    endpoint: String,
}

impl Rig {
    fn new(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let capture = Arc::new(Capture::default());
        {
            let capture = capture.clone();
            std::thread::spawn(move || serve(listener, capture));
        }
        let dir = std::env::temp_dir().join(format!("logship-src-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        Self {
            dir,
            capture,
            endpoint,
        }
    }

    fn logs(&self) -> PathBuf {
        self.dir.join("logs")
    }

    /// A config with every source off; tests switch on what they
    /// exercise.
    fn config(&self) -> ShipperConfig {
        ShipperConfig {
            data_dir: self.dir.clone(),
            endpoint: self.endpoint.clone(),
            token: INGEST.to_string(),
            machine: "sources-test".to_string(),
            app_version: "0.0.0-test".to_string(),
            level_floor: ShipLevel::Debug,
            tail_wizard: false,
            tail_optimize: false,
            tail_gateway: false,
            tail_service_manage: false,
            tail_audit: false,
            ..ShipperConfig::default()
        }
    }

    /// Wait until `n` records have been delivered.
    async fn wait_for(&self, n: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let records = self.capture.records();
            if records.len() >= n {
                return records;
            }
            assert!(
                Instant::now() < deadline,
                "shipper delivered {} of {n} record(s) in time: {records:?}",
                records.len()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn finish(self, handle: &ShipperHandle) {
        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_lines(path: &Path, lines: &[&str]) {
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn by_source<'a>(records: &'a [serde_json::Value], source: &str) -> Vec<&'a serde_json::Value> {
    records.iter().filter(|r| r["source"] == source).collect()
}

fn row<'a>(rows: &'a [SourceStatus], source: &str) -> &'a SourceStatus {
    rows.iter()
        .find(|r| r.source == source)
        .unwrap_or_else(|| panic!("no status row for {source}: {rows:?}"))
}

fn assert_absent(records: &[serde_json::Value], secrets: &[&str]) {
    let wire = serde_json::to_string(records).unwrap();
    for secret in secrets {
        assert!(!wire.contains(secret), "secret reached the wire: {secret}");
    }
}

/// S2: both gateway files ship as source `gateway` with their own
/// stream, representative LiteLLM lines are redacted, a source that
/// is toggled off ships nothing even though its file exists, and the
/// status rows say which is which.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_tails_ship_with_streams_and_redaction() {
    const MASTER: &str = "sk-GATEWAYMASTERKEY4d5e6f7a";
    const DBPASS: &str = "GATEWAYDBPASS8b9c0d";
    const BEARER: &str = "GATEWAYBEARER1e2f3a4b";
    let rig = Rig::new("gateway");
    write_lines(
        &rig.logs().join("gateway.stdout.log"),
        &[
            "LiteLLM: Proxy initialized with Config, Set models: planner",
            &format!("LiteLLM Proxy: Master Key set: master_key='{MASTER}'"),
            &format!("DATABASE_URL=postgresql://llmdil:{DBPASS}@127.0.0.1:54329/litellm"),
        ],
    );
    write_lines(
        &rig.logs().join("gateway.stderr.log"),
        &[&format!(
            "INFO: 127.0.0.1:53211 - \"POST /v1/chat/completions HTTP/1.1\" 200 OK Authorization: Bearer {BEARER}"
        )],
    );
    // Present, but its source is off: must not ship.
    write_lines(&rig.logs().join("wizard.log"), &["=== step ignored ==="]);

    let handle = manager_logship::spawn(ShipperConfig {
        tail_gateway: true,
        // The master key is a known Credential Manager value too.
        literal_secrets: vec![MASTER.to_string()],
        ..rig.config()
    });
    let records = rig.wait_for(4).await;
    // Give a wrongly-enabled tail time to show itself.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let records = if rig.capture.records().len() > records.len() {
        rig.capture.records()
    } else {
        records
    };

    assert_eq!(records.len(), 4, "only the gateway lines ship: {records:?}");
    let gateway = by_source(&records, "gateway");
    assert_eq!(gateway.len(), 4);
    assert_eq!(
        gateway.iter().filter(|r| r["stream"] == "stdout").count(),
        3
    );
    assert_eq!(
        gateway.iter().filter(|r| r["stream"] == "stderr").count(),
        1
    );
    assert_absent(&records, &[MASTER, DBPASS, BEARER]);
    let lines: Vec<&str> = records.iter().filter_map(|r| r["line"].as_str()).collect();
    assert!(lines.iter().any(|l| l.contains("Set models: planner")));
    assert!(lines
        .iter()
        .any(|l| l.contains("DATABASE_URL=") && l.contains("127.0.0.1:54329")));

    let rows = handle.status().sources;
    assert_eq!(row(&rows, "gateway").state, SourceState::Active);
    assert_eq!(row(&rows, "gateway").records, 4);
    assert_eq!(row(&rows, "wizard").state, SourceState::Disabled);
    assert_eq!(row(&rows, "wizard").records, 0);
    assert_eq!(row(&rows, "optimize").state, SourceState::Disabled);
    rig.finish(&handle).await;
}

/// An enabled source whose files do not exist reports `file_missing`
/// rather than implying it ships.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_source_without_files_reports_file_missing() {
    let rig = Rig::new("missing");
    let handle = manager_logship::spawn(ShipperConfig {
        tail_gateway: true,
        ..rig.config()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.status().sources.is_empty() {
        assert!(Instant::now() < deadline, "no status rows published");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rows = handle.status().sources;
    assert_eq!(row(&rows, "gateway").state, SourceState::FileMissing);
    rig.finish(&handle).await;
}

/// S6: `service-manage.log` is append-only and never rotated by its
/// writer (the elevated service helper). It ships as source
/// `service-manage`, and an operator truncating it mid-run neither
/// re-ships old lines nor loses the new ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_manage_tail_ships_and_survives_truncation() {
    let rig = Rig::new("svcmanage");
    let path = rig.logs().join("service-manage.log");
    write_lines(
        &path,
        &[
            "[1762900000] register llm-backend-planner: ok",
            "[1762900001] register llm-gateway: failed (access denied)",
        ],
    );
    let handle = manager_logship::spawn(ShipperConfig {
        tail_service_manage: true,
        ..rig.config()
    });
    let records = rig.wait_for(2).await;
    assert_eq!(by_source(&records, "service-manage").len(), 2);
    assert!(records
        .iter()
        .any(|r| r["level"] == "error" && r["line"].as_str().unwrap().contains("llm-gateway")));

    // Truncated in place and written again.
    write_lines(&path, &["[1762900100] unregister llm-gateway: ok"]);
    let records = rig.wait_for(3).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let records = if rig.capture.records().len() > records.len() {
        rig.capture.records()
    } else {
        records
    };
    assert_eq!(records.len(), 3, "no re-ship, no loss: {records:?}");
    assert!(records.iter().any(|r| r["line"]
        .as_str()
        .unwrap()
        .contains("unregister llm-gateway")));
    let rows = handle.status().sources;
    assert_eq!(row(&rows, "service-manage").state, SourceState::Active);
    assert_eq!(row(&rows, "service-manage").records, 3);
    rig.finish(&handle).await;
}

/// S7: audit lines are JSON records. They ship verbatim (still one
/// parseable object per line), their level comes from the recorded
/// outcome rather than from words in the line, and a secret-shaped
/// attacker-chosen actor is redacted like anything else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_tail_ships_json_verbatim_with_outcome_levels() {
    const HOSTILE: &str = "sk-AUDITHOSTILEACTOR5f6a7b8c";
    let entry = |actor: &str, action: &str, result: &str, id: &str| {
        format!(
            r#"{{"ts_ms":1762900000000,"actor":"{actor}","auth_type":"session","source_ip":"127.0.0.1","action":"{action}","target":null,"result":"{result}","request_id":"{id}"}}"#
        )
    };
    let ok = entry("alice", "POST /api/v1/backends/{id}/start", "202", "req-1");
    // "error" appears in the action path: the outcome (200) decides.
    let benign = entry("alice", "GET /api/v1/errors", "200", "req-2");
    let denied = entry("bob", "POST /api/v1/config", "403", "req-3");
    let failed = entry("alice", "POST /api/v1/backends/{id}/start", "500", "req-4");
    let hostile = entry(
        HOSTILE,
        "POST /api/v1/auth/login",
        "invalid_credentials",
        "req-5",
    );

    let rig = Rig::new("audit");
    write_lines(
        &rig.logs().join("admin-api-audit.jsonl"),
        &[&ok, &benign, &denied, &failed, &hostile],
    );
    let handle = manager_logship::spawn(ShipperConfig {
        tail_audit: true,
        ..rig.config()
    });
    let records = rig.wait_for(5).await;
    let audit = by_source(&records, "audit");
    assert_eq!(audit.len(), 5);
    assert_absent(&records, &[HOSTILE]);

    let level_of = |id: &str| -> String {
        let rec = audit
            .iter()
            .find(|r| r["line"].as_str().unwrap().contains(id))
            .unwrap_or_else(|| panic!("no audit record {id}"));
        let doc: serde_json::Value =
            serde_json::from_str(rec["line"].as_str().unwrap()).expect("audit line stays JSON");
        assert_eq!(doc["request_id"], id);
        rec["level"].as_str().unwrap().to_string()
    };
    assert_eq!(level_of("req-1"), "info");
    assert_eq!(level_of("req-2"), "info");
    assert_eq!(level_of("req-3"), "warn");
    assert_eq!(level_of("req-4"), "error");
    assert_eq!(level_of("req-5"), "warn");
    // Untouched records are byte-identical to what the API wrote.
    assert!(audit.iter().any(|r| r["line"] == ok.as_str()));
    rig.finish(&handle).await;
}
