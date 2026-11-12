//! SHA-256 verification and the download manager (progress, pause/resume,
//! cancel).

use crate::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Download progress snapshot for the progress callback.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

impl DownloadProgress {
    pub fn fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.downloaded_bytes as f64 / self.total_bytes as f64
        }
    }
}
/// Stream a file's SHA-256 hex digest with a bounded 1 MiB buffer,
/// regardless of file size. This is the one hashing implementation
/// for the whole app — the model verifier and the setup wizard both
/// use it. A whole-file read of a multi-GiB GGUF materializes the
/// file in memory, and a failed allocation aborts the process: the
/// wizard's former read-then-hash implementation did exactly that
/// on the 45.9 GiB Coder-Next file (the v0.1.31 "Verify checksum"
/// crash).
pub fn sha256_hex(path: &Path) -> Result<String, ModelError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|e| ModelError::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| ModelError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// SHA-256-verify a downloaded file against the catalog hash.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch (callers decide
/// whether to block "installed" / offer re-download). Malformed expected
/// hashes and I/O failures are errors.
pub async fn verify_sha256(path: &Path, expected_hex: &str) -> Result<bool, ModelError> {
    let expected = expected_hex.trim().to_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ModelError::InvalidChecksum(format!(
            "expected 64 hex chars, got {expected_hex:?}"
        )));
    }
    let owned = path.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || sha256_hex(&owned))
        .await
        .map_err(|e| ModelError::Io(format!("hash task failed: {e}")))??;
    Ok(actual == expected)
}

// ---------------------------------------------------------------------------
// Download manager
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerState {
    Downloading,
    Paused,
    Cancelled,
    Done,
    Failed,
}

struct DownloadShared {
    state: Mutex<WorkerState>,
    notify: Notify,
    downloaded: AtomicU64,
    total: AtomicU64,
    result: Mutex<Option<Result<(), ModelError>>>,
    /// Set by the worker when it has parked in the paused state; lets
    /// `pause()` return only once the transfer is actually stopped.
    paused_ack: AtomicBool,
}

impl DownloadShared {
    fn current_state(&self) -> WorkerState {
        *mutex_lock(&self.state)
    }
}

/// Lock a mutex, recovering from poisoning instead of panicking.
/// Used for the state/result mutexes, which are only held across
/// non-panicking code — a poisoned lock here must not wedge the handle.
/// (The progress-callback mutex intentionally does NOT use this: a
/// panicking callback is skipped, see `report_progress`.)
fn mutex_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

type ProgressCallback = Arc<Mutex<Box<dyn Fn(DownloadProgress) + Send + 'static>>>;

/// Handle to an in-flight download.
///
/// `download()` returns immediately after spawning the background task; use
/// [`DownloadHandle::wait`] to await completion, [`DownloadHandle::progress`]
/// to poll, and pause/resume/cancel to control the transfer.
#[derive(Clone)]
pub struct DownloadHandle {
    spec_id: String,
    shared: Arc<DownloadShared>,
}

impl DownloadHandle {
    /// Stable id of the model being downloaded.
    pub fn id(&self) -> &str {
        &self.spec_id
    }

    /// Pause the download (keeps the partial file for resume). Synchronous:
    /// returns only once the worker has parked, so no further bytes land
    /// after this returns. No-op once the download has reached a terminal
    /// state.
    pub async fn pause(&self) -> Result<(), ModelError> {
        let needs_ack = {
            let mut state = mutex_lock(&self.shared.state);
            if *state == WorkerState::Downloading {
                *state = WorkerState::Paused;
                self.shared.paused_ack.store(false, Ordering::SeqCst);
                self.shared.notify.notify_waiters();
                true
            } else {
                false
            }
        };
        if needs_ack {
            // Wait for the worker to acknowledge the parked state (or for a
            // concurrent terminal transition / overtake by resume(), so we
            // can't hang if the worker finished or resumed concurrently).
            loop {
                if self.shared.paused_ack.load(Ordering::SeqCst) {
                    break;
                }
                match self.shared.current_state() {
                    WorkerState::Done
                    | WorkerState::Failed
                    | WorkerState::Cancelled
                    | WorkerState::Downloading => break,
                    _ => {}
                }
                self.shared.notify.notified().await;
            }
        }
        Ok(())
    }

    /// Resume a paused download via HTTP range requests. No-op unless paused.
    pub async fn resume(&self) -> Result<(), ModelError> {
        let mut state = mutex_lock(&self.shared.state);
        if *state == WorkerState::Paused {
            *state = WorkerState::Downloading;
            self.shared.notify.notify_waiters();
        }
        Ok(())
    }

    /// Cancel and delete the partial file. No-op once done/failed.
    pub async fn cancel(&self) -> Result<(), ModelError> {
        let mut state = mutex_lock(&self.shared.state);
        match *state {
            WorkerState::Downloading | WorkerState::Paused => {
                *state = WorkerState::Cancelled;
                self.shared.notify.notify_waiters();
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the download has reached a terminal state (done, failed,
    /// or cancelled). Paused is NOT terminal: the transfer can resume.
    /// Callers use this to reap finished handles from their registries.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.shared.current_state(),
            WorkerState::Done | WorkerState::Failed | WorkerState::Cancelled
        )
    }

    /// Whether the download is currently paused (parked, partial file
    /// kept for resume).
    pub fn is_paused(&self) -> bool {
        self.shared.current_state() == WorkerState::Paused
    }

    /// Latest progress snapshot (lock-free read).
    pub fn progress(&self) -> DownloadProgress {
        DownloadProgress {
            downloaded_bytes: self.shared.downloaded.load(Ordering::SeqCst),
            total_bytes: self.shared.total.load(Ordering::SeqCst),
        }
    }

    /// Await the download reaching a terminal state.
    pub async fn wait(&self) -> Result<(), ModelError> {
        loop {
            if let Some(result) = mutex_lock(&self.shared.result).clone() {
                return result;
            }
            self.shared.notify.notified().await;
        }
    }
}

/// Start downloading `spec` to `dest`, invoking `on_progress` as chunks land.
///
/// Returns immediately with a [`DownloadHandle`]; the transfer runs on a
/// background task. Bytes land in a sibling partial file (`dest` with
/// `.part` appended) and are moved into place at `dest` only once the
/// transfer completes, so `dest` never holds a partial file. An existing
/// partial file is resumed with `Range` when the server honors it,
/// otherwise the transfer restarts from zero. Fails fast on an
/// unparseable URL or an unwritable destination.
pub async fn download<F>(
    spec: &ModelSpec,
    dest: &Path,
    on_progress: F,
) -> Result<DownloadHandle, ModelError>
where
    F: Fn(DownloadProgress) + Send + 'static,
{
    let _: reqwest::Url = spec
        .url
        .parse()
        .map_err(|e| ModelError::Network(format!("invalid url {:?}: {e}", spec.url)))?;

    // Fail fast on an unwritable destination: create the parent dir here,
    // before spawning the worker, rather than surfacing it via `wait()`.
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ModelError::Io(e.to_string()))?;
        }
    }

    let shared = Arc::new(DownloadShared {
        state: Mutex::new(WorkerState::Downloading),
        notify: Notify::new(),
        downloaded: AtomicU64::new(0),
        total: AtomicU64::new(0),
        result: Mutex::new(None),
        paused_ack: AtomicBool::new(false),
    });
    let callback: ProgressCallback = Arc::new(Mutex::new(Box::new(on_progress)));

    let worker_spec = spec.clone();
    let worker_dest = dest.to_path_buf();
    let worker_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        download_worker(worker_spec, worker_dest, worker_shared, callback).await;
    });

    Ok(DownloadHandle {
        spec_id: spec.id.clone(),
        shared,
    })
}

fn report_progress(
    shared: &DownloadShared,
    callback: &ProgressCallback,
    downloaded: u64,
    total: u64,
) {
    shared.downloaded.store(downloaded, Ordering::SeqCst);
    shared.total.store(total, Ordering::SeqCst);
    let progress = DownloadProgress {
        downloaded_bytes: downloaded,
        total_bytes: total,
    };
    // A callback that panicked previously poisons its mutex; skip rather
    // than re-invoking it (which would panic the worker task and wedge
    // `wait()` forever). State/result mutexes use `mutex_lock` recovery
    // instead, since they are only held across non-panicking code.
    if let Ok(cb) = callback.lock() {
        cb(progress);
    }
}

async fn wait_while_paused(shared: &DownloadShared) {
    let mut first = true;
    while shared.current_state() == WorkerState::Paused {
        if first {
            // Acknowledge the parked state so `pause()` can return.
            shared.paused_ack.store(true, Ordering::SeqCst);
            shared.notify.notify_waiters();
            first = false;
        }
        shared.notify.notified().await;
    }
}

async fn download_worker(
    spec: ModelSpec,
    dest: PathBuf,
    shared: Arc<DownloadShared>,
    callback: ProgressCallback,
) {
    let outcome = download_inner(&spec, &dest, &shared, &callback).await;
    let terminal = match &outcome {
        Ok(()) => WorkerState::Done,
        Err(ModelError::Cancelled(_)) => WorkerState::Cancelled,
        Err(_) => WorkerState::Failed,
    };
    *mutex_lock(&shared.state) = terminal;
    *mutex_lock(&shared.result) = Some(outcome);
    shared.notify.notify_waiters();
}

/// The partial-file path for a download destination: `dest` with `.part`
/// appended to the file name. Transfers write here and are renamed into
/// place at `dest` only on success.
pub fn partial_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_owned();
    os.push(".part");
    PathBuf::from(os)
}

/// Parse `Content-Range: bytes START-END/TOTAL` into `(start, total)`.
fn parse_content_range(header: &str) -> Option<(u64, u64)> {
    let rest = header.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

/// Stream a response body into the partial file (appending when
/// `append`, truncating otherwise), starting the byte counter at
/// `downloaded`. Pause parks the transfer; cancel deletes the partial
/// file. On a complete body the counter and the on-disk length must
/// agree, then the partial file is moved into place at `dest`.
#[allow(clippy::too_many_arguments)]
async fn transfer_body(
    response: &mut reqwest::Response,
    part: &Path,
    dest: &Path,
    append: bool,
    mut downloaded: u64,
    total: u64,
    spec: &ModelSpec,
    shared: &DownloadShared,
    callback: &ProgressCallback,
) -> Result<(), ModelError> {
    use tokio::io::AsyncWriteExt;

    let mut file = if append {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(part)
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?
    } else {
        tokio::fs::File::create(part)
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?
    };
    report_progress(shared, callback, downloaded, total);

    loop {
        match shared.current_state() {
            WorkerState::Cancelled => {
                drop(file);
                let _ = tokio::fs::remove_file(part).await;
                return Err(ModelError::Cancelled(spec.id.clone()));
            }
            WorkerState::Paused => {
                wait_while_paused(shared).await;
                continue;
            }
            _ => {}
        }

        tokio::select! {
            chunk_result = response.chunk() => {
                match chunk_result {
                    Ok(Some(chunk)) => {
                        file.write_all(&chunk)
                            .await
                            .map_err(|e| ModelError::Io(e.to_string()))?;
                        downloaded += chunk.len() as u64;
                        report_progress(shared, callback, downloaded, total);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(ModelError::Network(e.to_string())),
                }
            }
            _ = shared.notify.notified() => {
                // Control state changed (pause/cancel); re-check above.
                continue;
            }
        }
    }

    if total > 0 && downloaded < total {
        return Err(ModelError::Network(format!(
            "short read for {}: {downloaded}/{total} bytes",
            spec.id
        )));
    }
    // The byte counter and the file must agree before we report
    // success: never mark a download done with a short file.
    // `sync_all` first: the durability check below must observe
    // every byte `write_all` reported, even on filesystems where
    // close() completion and stat visibility can otherwise race
    // under heavy parallel load.
    file.sync_all()
        .await
        .map_err(|e| ModelError::Io(e.to_string()))?;
    drop(file);
    let file_len = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if file_len != downloaded {
        return Err(ModelError::Network(format!(
            "file size mismatch for {}: counted {downloaded} bytes but file is {file_len} bytes",
            spec.id
        )));
    }
    // Transfer complete: move the partial file into place. A previous
    // file at `dest` (e.g. an older download being replaced) is removed
    // first — rename does not overwrite on Windows.
    if tokio::fs::metadata(dest).await.is_ok() {
        tokio::fs::remove_file(dest)
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?;
    }
    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| ModelError::Io(e.to_string()))?;
    Ok(())
}

async fn download_inner(
    spec: &ModelSpec,
    dest: &Path,
    shared: &DownloadShared,
    callback: &ProgressCallback,
) -> Result<(), ModelError> {
    // Parent dir is created fail-fast in `download()`; the worker assumes it.
    let part = partial_path(dest);
    let client = reqwest::Client::new();
    let mut offset: u64 = match tokio::fs::metadata(&part).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };

    // Bounded attempts: a ranged try, plus fresh restarts after a
    // mismatched 206 range or a stale-partial 416.
    for _ in 0..3 {
        match shared.current_state() {
            WorkerState::Cancelled => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(ModelError::Cancelled(spec.id.clone()));
            }
            WorkerState::Paused => {
                wait_while_paused(shared).await;
                if shared.current_state() == WorkerState::Cancelled {
                    let _ = tokio::fs::remove_file(&part).await;
                    return Err(ModelError::Cancelled(spec.id.clone()));
                }
            }
            _ => {}
        }

        let ranged = offset > 0;
        let mut request = client.get(&spec.url);
        if ranged {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;
        let status = response.status();

        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let (start, total) = response
                .headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    ModelError::Network("206 response without a parseable Content-Range".into())
                })?;
            if start != offset {
                // The server answered a different range than requested:
                // appending would corrupt the file. Discard the response
                // and the partial, and restart with a plain GET.
                drop(response);
                let _ = tokio::fs::remove_file(&part).await;
                offset = 0;
                continue;
            }
            return transfer_body(
                &mut response,
                &part,
                dest,
                true,
                offset,
                total,
                spec,
                shared,
                callback,
            )
            .await;
        } else if status == reqwest::StatusCode::OK {
            // A 200 to a ranged request means the server ignored the
            // Range header: this body is the whole file from byte zero,
            // so the partial is truncated and rewritten from scratch.
            let total = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(spec.size_bytes);
            return transfer_body(
                &mut response,
                &part,
                dest,
                false,
                0,
                total,
                spec,
                shared,
                callback,
            )
            .await;
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The partial is at or past the end of the resource. It is
            // the complete file only when its size matches the total
            // the server reports in `Content-Range: bytes */TOTAL`.
            let total = response
                .headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range_total);
            if offset > 0 && total == Some(offset) {
                report_progress(shared, callback, offset, offset);
                if tokio::fs::metadata(dest).await.is_ok() {
                    tokio::fs::remove_file(dest)
                        .await
                        .map_err(|e| ModelError::Io(e.to_string()))?;
                }
                tokio::fs::rename(&part, dest)
                    .await
                    .map_err(|e| ModelError::Io(e.to_string()))?;
                return Ok(());
            }
            // Stale or oversized partial: delete it and retry fresh.
            let _ = tokio::fs::remove_file(&part).await;
            offset = 0;
            continue;
        } else {
            return Err(ModelError::Network(format!(
                "download of {} failed: HTTP {status}",
                spec.id
            )));
        }
    }

    Err(ModelError::Network(format!(
        "download of {} exhausted retries",
        spec.id
    )))
}

/// Parse the total length out of `Content-Range: bytes 123-456/789`.
fn parse_content_range_total(header: &str) -> Option<u64> {
    header.split('/').nth(1)?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;

    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // -- hermetic HTTP server -----------------------------------------------

    fn test_payload() -> &'static [u8] {
        static PAYLOAD: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        PAYLOAD.get_or_init(|| (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect())
    }

    struct TestServer {
        addr: std::net::SocketAddr,
        served_range: Arc<AtomicBool>,
        served_bad_range: Arc<AtomicBool>,
    }

    async fn start_server() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served_range = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&served_range);
        let served_bad_range = Arc::new(AtomicBool::new(false));
        let bad_flag = Arc::clone(&served_bad_range);
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let flag = Arc::clone(&flag);
                let bad_flag = Arc::clone(&bad_flag);
                tokio::spawn(async move { handle_test_conn(socket, flag, bad_flag).await });
            }
        });
        TestServer {
            addr,
            served_range,
            served_bad_range,
        }
    }

    /// Minimal HTTP/1.1 server. `/file` honors `Range` (206); `/no-range`
    /// ignores it (200, no Accept-Ranges); `/bad-range` answers a Range
    /// request with a 206 whose Content-Range lies about the start (always
    /// claims 0). Body is drip-fed in 64 KiB chunks so pause/cancel can
    /// interleave deterministically.
    async fn handle_test_conn(
        mut socket: TcpStream,
        served_range: Arc<AtomicBool>,
        served_bad_range: Arc<AtomicBool>,
    ) {
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => {
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if req.len() > 65536 {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
        let req_str = String::from_utf8_lossy(&req);
        let mut lines = req_str.lines();
        let request_line = lines.next().unwrap_or("");
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let mut range_start: Option<u64> = None;
        for line in lines {
            if line.is_empty() {
                break;
            }
            // Header names are case-insensitive (hyper normalizes to
            // lowercase on the wire), so match against the lowercased line.
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("range:") {
                let value = value.trim();
                if let Some(rest) = value.strip_prefix("bytes=") {
                    if let Some((start, _)) = rest.split_once('-') {
                        range_start = start.parse().ok();
                    }
                }
            }
        }

        let payload = test_payload();
        let payload_len = payload.len() as u64;
        let supports_range = path == "/file" || path == "/bad-range";

        if supports_range {
            if let Some(start) = range_start {
                if start >= payload_len {
                    let headers = format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\n\
                         Content-Range: bytes */{payload_len}\r\n\
                         Connection: close\r\nContent-Length: 0\r\n\r\n"
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    return;
                }
                if path == "/bad-range" {
                    // Lie about the range: claim the body starts at 0
                    // whatever was requested, and send the whole payload.
                    served_bad_range.store(true, Ordering::SeqCst);
                    let headers = format!(
                        "HTTP/1.1 206 Partial Content\r\n\
                         Content-Length: {payload_len}\r\n\
                         Accept-Ranges: bytes\r\n\
                         Content-Range: bytes 0-{}/{payload_len}\r\n\
                         Connection: close\r\n\r\n",
                        payload_len - 1
                    );
                    if socket.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    drip_body(&mut socket, payload, 0).await;
                    return;
                }
                served_range.store(true, Ordering::SeqCst);
                let body_len = payload_len - start;
                let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Length: {body_len}\r\n\
                     Accept-Ranges: bytes\r\n\
                     Content-Range: bytes {start}-{}/{payload_len}\r\n\
                     Connection: close\r\n\r\n",
                    payload_len - 1
                );
                if socket.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                drip_body(&mut socket, payload, start as usize).await;
                return;
            }
        }

        // Full 200 response (range ignored on /no-range).
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: {payload_len}\r\n\
             Connection: close\r\n"
        );
        let headers = if supports_range {
            format!("{headers}Accept-Ranges: bytes\r\n\r\n")
        } else {
            format!("{headers}\r\n")
        };
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        drip_body(&mut socket, payload, 0).await;
    }

    async fn drip_body(socket: &mut TcpStream, payload: &[u8], from: usize) {
        let mut off = from;
        while off < payload.len() {
            let end = (off + 65536).min(payload.len());
            if socket.write_all(&payload[off..end]).await.is_err() {
                return;
            }
            off = end;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "manager-models-test-{}-{}-{name}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn download_spec(server: &TestServer, path: &str) -> ModelSpec {
        ModelSpec {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: test_payload().len() as u64,
            sha256: PLACEHOLDER_SHA256.to_string(),
            url: format!("http://{}{path}", server.addr),
            params_b: 1.0,
            ctx_max: 0,
            enabled_by_default: true,
            expert_bytes: None,
            moe_layers: None,
            kv_bytes_per_token: None,
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            mtp: false,
        }
    }

    async fn wait_for_partial(handle: &DownloadHandle) {
        for _ in 0..200 {
            let p = handle.progress();
            if p.downloaded_bytes > 0 && p.total_bytes > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("download never made progress");
    }

    // -- download tests ------------------------------------------------------

    #[tokio::test]
    async fn download_completes_and_reports_progress() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-complete");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let fractions = Arc::new(Mutex::new(Vec::new()));
        let fractions2 = Arc::clone(&fractions);
        let handle = download(&spec, &dest, move |p| {
            fractions2.lock().unwrap().push(p.fraction());
        })
        .await
        .unwrap();
        handle.wait().await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(
            !partial_path(&dest).exists(),
            "partial file must be moved into place on success"
        );
        let fractions = fractions.lock().unwrap();
        assert!(!fractions.is_empty());
        assert!((fractions.last().unwrap() - 1.0).abs() < 1e-9);
        assert!(fractions.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(handle.progress().total_bytes, test_payload().len() as u64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_pause_resume() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-pause");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        wait_for_partial(&handle).await;

        handle.pause().await.unwrap();
        let snap = handle.progress().downloaded_bytes;
        assert!(snap > 0);
        // While paused the bytes live only in the partial file; the
        // destination must not exist yet.
        assert!(handle.is_paused());
        assert!(partial_path(&dest).exists());
        assert!(!dest.exists(), "dest must not hold a partial download");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            handle.progress().downloaded_bytes,
            snap,
            "no bytes may land while paused"
        );

        handle.resume().await.unwrap();
        handle.wait().await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_cancel_deletes_partial() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-cancel");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        wait_for_partial(&handle).await;

        handle.cancel().await.unwrap();
        let result = handle.wait().await;
        assert!(
            matches!(result, Err(ModelError::Cancelled(_))),
            "expected Cancelled, got {result:?}"
        );
        assert!(!dest.exists(), "dest must never hold a partial file");
        assert!(
            !partial_path(&dest).exists(),
            "partial file must be removed on cancel"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_cancel_while_paused_deletes_partial() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-cancel-paused");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        wait_for_partial(&handle).await;

        handle.pause().await.unwrap();
        assert!(partial_path(&dest).exists());
        handle.cancel().await.unwrap();
        let result = handle.wait().await;
        assert!(
            matches!(result, Err(ModelError::Cancelled(_))),
            "expected Cancelled, got {result:?}"
        );
        assert!(!dest.exists(), "dest must never hold a partial file");
        assert!(
            !partial_path(&dest).exists(),
            "partial file must be removed when a paused download is cancelled"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_resumes_partial_file_with_range() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-resume");
        let dest = dir.join("model.gguf");
        // Pre-write the first MiB of the partial file, as if from an
        // earlier interrupted download.
        std::fs::write(partial_path(&dest), &test_payload()[..1024 * 1024]).unwrap();
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert!(
            server.served_range.load(Ordering::SeqCst),
            "server should have seen a Range request"
        );
        assert!(!partial_path(&dest).exists());
        let got = std::fs::read(&dest).unwrap();
        let want = test_payload();
        if got != want {
            let div = got
                .iter()
                .zip(want.iter())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| got.len().min(want.len()));
            panic!(
                "downloaded file differs: got {} bytes, want {} bytes, first divergence at byte {}",
                got.len(),
                want.len(),
                div
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_finalizes_complete_partial_on_416() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-complete-part");
        let dest = dir.join("model.gguf");
        // The partial already holds the whole file (e.g. a previous
        // transfer finished writing but the app died before the rename).
        std::fs::write(partial_path(&dest), test_payload()).unwrap();
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_restarts_when_server_ignores_range() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-norange");
        let dest = dir.join("model.gguf");
        std::fs::write(partial_path(&dest), &test_payload()[..1024]).unwrap();
        let spec = download_spec(&server, "/no-range");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert!(
            !server.served_range.load(Ordering::SeqCst),
            "server must not see a Range request on /no-range"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_restarts_on_mismatched_206_range() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-badrange");
        let dest = dir.join("model.gguf");
        // Partial from an earlier interrupted download: the client asks
        // for `Range: bytes=1048576-`, the server answers 206 claiming
        // the range starts at 0. Appending that body would corrupt the
        // file; the client must discard the partial and restart with a
        // plain GET.
        std::fs::write(partial_path(&dest), &test_payload()[..1024 * 1024]).unwrap();
        let spec = download_spec(&server, "/bad-range");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert!(
            server.served_bad_range.load(Ordering::SeqCst),
            "server should have served the mismatched 206"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_discards_stale_partial_on_416_total_mismatch() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-stale416");
        let dest = dir.join("model.gguf");
        // Stale partial larger than the resource itself (e.g. the model
        // file changed upstream). The Range request starts past the end;
        // the server's 416 reports the real total, which does not match
        // the partial size, so the partial must be deleted — not renamed
        // into place — and the download retried fresh.
        let mut stale = test_payload().to_vec();
        stale.extend_from_slice(&[0xEE; 1024 * 1024]);
        std::fs::write(partial_path(&dest), &stale).unwrap();
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_rejects_bad_url() {
        let dir = test_temp_dir("dl-badurl");
        let mut spec = test_spec("bad", 10);
        spec.url = "://not-a-url".to_string();
        let result = download(&spec, &dir.join("x.gguf"), |_| {}).await;
        assert!(matches!(result, Err(ModelError::Network(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- checksum tests ------------------------------------------------------

    #[tokio::test]
    async fn verify_sha256_pass_fail_and_malformed() {
        let dir = test_temp_dir("sha256");
        let file = dir.join("x.bin");
        std::fs::write(&file, b"hello manager-models").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello manager-models");
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        assert!(verify_sha256(&file, &hex).await.unwrap());
        assert!(verify_sha256(&file, &hex.to_uppercase()).await.unwrap());
        assert!(!verify_sha256(&file, &"0".repeat(64)).await.unwrap());
        assert!(matches!(
            verify_sha256(&file, "zzz").await,
            Err(ModelError::InvalidChecksum(_))
        ));
        assert!(matches!(
            verify_sha256(&dir.join("missing.bin"), &hex).await,
            Err(ModelError::Io(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sha256_hex_matches_incremental_reference_across_chunks() {
        // 3 full 1 MiB buffers + a 17-byte tail, patterned so a
        // chunk-boundary or length bug changes the digest. The
        // reference digest is computed incrementally over odd-sized
        // slices of the same pattern.
        let dir = test_temp_dir("sha256hex");
        let file = dir.join("pattern.bin");
        let total = 3 * (1usize << 20) + 17;
        let pattern: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &pattern).unwrap();
        let mut reference = Sha256::new();
        for chunk in pattern.chunks(65_537) {
            reference.update(chunk);
        }
        let expected: String = reference
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sha256_hex(&file).unwrap(), expected);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The direct regression guard for the v0.1.31 crash class: the
    /// digest of a multi-GiB file must be computable with bounded
    /// memory. Writes ~5 GiB, so it is ignored by default; run it
    /// explicitly in release gates (`cargo test -p manager-models
    /// -- --ignored sha256_hex_streams`).
    #[test]
    #[ignore = "writes ~5 GiB; run explicitly in release gates"]
    fn sha256_hex_streams_multi_gib_file() {
        use std::io::Write;
        let dir = test_temp_dir("sha256big");
        let file = dir.join("big.bin");
        // One 1 MiB pattern block, repeated 5,120 times (~5 GiB).
        let block: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
        let mut reference = Sha256::new();
        {
            let mut w = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
            for _ in 0..5_120 {
                w.write_all(&block).unwrap();
                reference.update(&block);
            }
            w.flush().unwrap();
        }
        let expected: String = reference
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sha256_hex(&file).unwrap(), expected);
        std::fs::remove_dir_all(&dir).ok();
    }
}
