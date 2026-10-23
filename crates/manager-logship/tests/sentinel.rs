//! The redaction proof test (design §5.3) — a RELEASE GATE for any
//! build that touches the shipper or the redactor.
//!
//! Seeds a synthetic log stream containing every §5.1 secret class
//! in realistic shapes, using known sentinel values, runs the FULL
//! shipper pipeline (tail → redact → batch → gzip → POST) against a
//! local capture endpoint, and asserts:
//!  - none of the sentinels appears in any captured byte (headers
//!    or body), and
//!  - the lines themselves WERE shipped (redaction must not become
//!    silent dropping).
//!
//! The ingest token is the transport credential itself: it is
//! expected in the `Authorization` header and is asserted absent
//! from every captured BODY. The sentinel set lives here, in the
//! test, never in config.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manager_logship::{ShipperConfig, ShipLevel};

// Literal-layer sentinels (handed to the shipper as the deployment's
// known Credential Manager values).
const MASTER: &str = "SENTINELMASTERKEY7f3a9c";
const PGPASS: &str = "SENTINELPGPASS5e6f7a";
const MCPENV: &str = "SENTINELMCPENV8a1b2c";
const TOKEN: &str = "SENTINELINGESTTOKEN3d4e5f";
// Pattern-layer sentinels (NOT in the literal list — only the §5.2
// pattern classes can catch these).
const BEARER: &str = "SENTINELBEARER6f7a8b";
const APIKEY: &str = "SENTINELAPIKEY0c1d2e";
const JWT: &str =
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6InNlbnRpbmVsIn0.SENTINELJWTSIG9a8b7c6d";
const URLTOKEN: &str = "SENTINELURLTOKEN4a5b6c";
const DSNPASS: &str = "SENTINELDSNPASS7d8e9f";
const SKKEY: &str = "sk-SENTINELCLIENTKEY9b2d4e";

fn seed_lines() -> Vec<String> {
    vec![
        format!("gateway starting; master key {MASTER} loaded from credential store"),
        // Literal layer only: no key=value shape for the pattern pass.
        format!("note: the postgres password is {PGPASS} per the vault"),
        format!(r#"mcp server def {{"name":"brave","env":{{"BRAVE_API_KEY":"{MCPENV}"}}}}"#),
        format!("upstream request Authorization: Bearer {BEARER}"),
        format!(r#"config fragment {{"api_key": "{APIKEY}", "model": "planner"}}"#),
        format!("session jwt {JWT} accepted"),
        format!("GET https://api.example.test/v1/models?token={URLTOKEN}&limit=5"),
        format!("connecting postgres://llm:{DSNPASS}@127.0.0.1:54329/litellm"),
        format!("client presented key {SKKEY} on /v1/chat/completions"),
        "=== step start-gateway (install): started ===".to_string(),
    ]
}

struct Capture {
    requests: Mutex<Vec<Vec<u8>>>,
}

impl Capture {
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn all_bytes(&self) -> Vec<u8> {
        self.requests.lock().unwrap().concat()
    }
    /// Decoded records from every captured request body.
    fn records(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for req in self.requests.lock().unwrap().iter() {
            let Some(pos) = find_subslice(req, b"\r\n\r\n") else {
                continue;
            };
            let body = &req[pos + 4..];
            let mut dec = flate2::read::GzDecoder::new(body);
            let mut text = String::new();
            if dec.read_to_string(&mut text).is_ok() {
                if let Ok(serde_json::Value::Array(items)) =
                    serde_json::from_str::<serde_json::Value>(&text)
                {
                    out.extend(items);
                }
            }
        }
        out
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Minimal HTTP capture server: one thread per connection, reads
/// content-length framed requests until the peer goes quiet.
fn serve(listener: TcpListener, capture: Arc<Capture>) {
    for stream in listener.incoming() {
        let Ok(mut sock) = stream else { break };
        let capture = capture.clone();
        std::thread::spawn(move || {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(800)));
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                // Try to complete a request from what we have.
                if let Some(total) = complete_request_len(&buf) {
                    let req = buf.drain(..total).collect::<Vec<u8>>();
                    capture.requests.lock().unwrap().push(req);
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
                    );
                    continue;
                }
                match sock.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        });
    }
}

fn complete_request_len(buf: &[u8]) -> Option<usize> {
    let head_end = find_subslice(buf, b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())?;
    if buf.len() >= head_end + len {
        Some(head_end + len)
    } else {
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sentinel_redaction_end_to_end() {
    // --- Capture endpoint -----------------------------------------
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let capture = Arc::new(Capture {
        requests: Mutex::new(Vec::new()),
    });
    {
        let capture = capture.clone();
        std::thread::spawn(move || serve(listener, capture));
    }

    // --- Synthetic log files --------------------------------------
    let dir = std::env::temp_dir().join(format!("logship-sentinel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let lines = seed_lines();
    std::fs::write(logs.join("wizard.log"), lines.join("\n") + "\n").unwrap();
    std::fs::write(
        logs.join("optimize.log"),
        "consultation complete: plan with 3 item(s)\n",
    )
    .unwrap();
    let expected_records = lines.len() + 1;

    // --- Full shipper pipeline ------------------------------------
    let handle = manager_logship::spawn(ShipperConfig {
        data_dir: dir.clone(),
        endpoint: format!("http://127.0.0.1:{port}"),
        token: TOKEN.to_string(),
        machine: "sentinel-test".to_string(),
        app_version: "0.0.0-test".to_string(),
        level_floor: ShipLevel::Debug,
        tail_wizard: true,
        tail_optimize: true,
        literal_secrets: vec![
            MASTER.to_string(),
            PGPASS.to_string(),
            MCPENV.to_string(),
            TOKEN.to_string(),
        ],
        spool_cap_bytes: manager_logship::DEFAULT_SPOOL_CAP_BYTES,
    });

    // Wait for the batch to land (batch window is 2 s).
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if capture.records().len() >= expected_records {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shipper did not deliver {} records in time (got {}, {} request(s))",
            expected_records,
            capture.records().len(),
            capture.count()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- Assertions ------------------------------------------------
    let records = capture.records();
    assert_eq!(
        records.len(),
        expected_records,
        "every seeded line must be shipped (redaction is not dropping)"
    );
    let shipped_lines: Vec<String> = records
        .iter()
        .map(|r| r["line"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        shipped_lines.iter().any(|l| l.contains("start-gateway")),
        "the plain line must survive verbatim: {shipped_lines:?}"
    );
    assert!(
        shipped_lines.iter().filter(|l| l.contains("«redacted»")).count() >= 8,
        "redaction marker must appear across the secret-bearing lines: {shipped_lines:?}"
    );

    // No sentinel in ANY captured byte. The token is the transport
    // credential: allowed in the Authorization header only.
    let all = String::from_utf8_lossy(&capture.all_bytes()).to_string();
    for sentinel in [MASTER, PGPASS, MCPENV, BEARER, APIKEY, JWT, URLTOKEN, DSNPASS, SKKEY] {
        assert!(
            !all.contains(sentinel),
            "sentinel leaked onto the wire: {sentinel}"
        );
    }
    for req in capture.requests.lock().unwrap().iter() {
        let pos = find_subslice(req, b"\r\n\r\n").unwrap();
        let body = String::from_utf8_lossy(&req[pos + 4..]).to_string();
        assert!(!body.contains(TOKEN), "ingest token leaked into a batch body");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
