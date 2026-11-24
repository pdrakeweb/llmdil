use super::*;

#[test]
fn parse_gpu_rows_handles_realistic_output() {
    let text = "0, NVIDIA RTX A4000, GPU-aaaa, 16384 MiB, 8.6\n\
                    1, Tesla P100-PCIE-16GB, GPU-bbbb, 16384 MiB, 6.0\n";
    let gpus = RealSystemOps::parse_gpu_rows(text, "581.57");
    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0].name, "NVIDIA RTX A4000");
    assert_eq!(gpus[0].compute_capability, (8, 6));
    assert!(!gpus[0].is_pascal_or_older());
    assert_eq!(gpus[1].compute_capability, (6, 0));
    assert!(gpus[1].is_pascal_or_older());
    assert_eq!(gpus[1].total_vram_mib, 16384);
    assert_eq!(gpus[1].driver_version, "581.57");
}

#[test]
fn parse_driver_version_from_header() {
    let header = "| NVIDIA-SMI 581.57                 Driver Version: 581.57         CUDA Version: 12.9     |";
    assert_eq!(RealSystemOps::parse_driver_version(header), "581.57");
    assert_eq!(RealSystemOps::parse_driver_version("nope"), "0.0");
}

#[tokio::test]
async fn run_command_empty_argv_errors() {
    let ops = RealSystemOps::default();
    let err = ops
        .run_command(&PlannedCommand::new(vec![]))
        .await
        .unwrap_err();
    assert!(matches!(err, WizardError::CommandFailed { .. }));
}

/// A stalled child must not hold the caller past the budget:
/// `run_command_with_timeout` returns a timeout error promptly
/// (and, via kill-on-drop in the real run_command, the child is
/// killed rather than left running). Unix-only: `sleep` is the
/// portable stalled child there; the Windows CI path exercises
/// the same code through the default 600 s budget constant.
#[cfg(unix)]
#[tokio::test]
async fn run_command_with_timeout_bounds_stalled_child() {
    let ops = RealSystemOps::default();
    let cmd = PlannedCommand::new(vec!["sleep".to_string(), "30".to_string()]);
    let started = std::time::Instant::now();
    let err = ops
        .run_command_with_timeout(&cmd, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(err, WizardError::CommandFailed { .. }));
    assert!(format!("{err}").contains("timed out"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "returned after {:?}",
        started.elapsed()
    );
}

/// PL-M3 (docs/code-review-v0.1.60.md): a command whose
/// background descendant inherits its stdout (the `pg_ctl
/// --no-wait start` shape) returns when the command itself exits,
/// with the output it wrote, instead of waiting for the pipe to
/// close and timing out. The descendant is left running.
#[cfg(unix)]
#[tokio::test]
async fn run_command_returns_on_child_exit_when_grandchild_holds_pipes() {
    let ops = RealSystemOps::default();
    let cmd = PlannedCommand::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        "sleep 20 & echo started; echo warn >&2; exit 0".to_string(),
    ]);
    let started = std::time::Instant::now();
    let out = ops
        .run_command_with_timeout(&cmd, Duration::from_secs(10))
        .await
        .expect("the command exited 0; it must not be reported as timed out");
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.trim(), "started");
    assert_eq!(out.stderr.trim(), "warn");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "returned after {:?}",
        started.elapsed()
    );
}

/// A timed-out command's whole TREE dies, not just the direct
/// child (code-review 2.2/2.15): the shell records its own pid
/// and its background child's pid, then sleeps; the caller's
/// budget expires, and both pids must be gone afterwards.
/// (The previous coverage asserted only that the call returned.)
#[cfg(unix)]
#[tokio::test]
async fn run_command_timeout_kills_the_process_tree() {
    let ops = RealSystemOps::default();
    let dir = std::env::temp_dir().join(format!("wizard-tree-kill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("pids");
    let script = format!(
        "echo $$ > {}; sleep 60 & echo $! >> {}; wait",
        pid_file.display(),
        pid_file.display()
    );
    let cmd = PlannedCommand::new(vec!["sh".to_string(), "-c".to_string(), script]);
    let err = ops
        .run_command_with_timeout(&cmd, Duration::from_millis(500))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("timed out"), "{err}");
    let text = std::fs::read_to_string(&pid_file).unwrap();
    let pids: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert_eq!(pids.len(), 2, "shell + grandchild pids recorded: {text:?}");
    // The tree kill is asynchronous to the error return; poll
    // briefly for both pids to disappear.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && pids.iter().any(|p| process_alive(*p)) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for pid in &pids {
        assert!(!process_alive(*pid), "pid {pid} survived the tree kill");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cancelled step's extraction stops instead of writing the
/// whole archive in the background (code-review 2.2).
#[tokio::test]
async fn extract_zip_honors_cancellation() {
    use std::io::Write as _;
    let ops = RealSystemOps::default();
    let dir = std::env::temp_dir().join(format!("wizard-extract-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("a.zip");
    {
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for name in ["one.txt", "two.txt", "three.txt"] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(b"payload").unwrap();
        }
        zw.finish().unwrap();
    }
    ops.request_cancellation();
    let dest = dir.join("out");
    let err = ops.extract_zip(&archive, &dest).await.unwrap_err();
    assert!(matches!(err, WizardError::Cancelled), "{err}");
    assert!(
        !dest.join("one.txt").exists(),
        "cancelled extraction wrote files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sha256_roundtrip_on_temp_file() {
    let ops = RealSystemOps::default();
    let path = std::env::temp_dir().join("wizard-sha256-test.bin");
    ops.write_file(&path, b"hello").await.unwrap();
    let hex = ops.sha256_file(&path).await.unwrap();
    assert_eq!(
        hex,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert!(ops.file_exists(&path).await);
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn sha256_streams_multi_chunk_file() {
    // Regression for the v0.1.31 crash: sha256_file must stream,
    // not materialize the file. A patterned file spanning
    // several hash buffers, checked against an independently
    // computed digest (the digest itself is size-independent;
    // the >RAM case is covered by manager-models' ignored
    // multi-GiB test for the shared implementation).
    use sha2::{Digest, Sha256};
    let ops = RealSystemOps::default();
    let path = std::env::temp_dir().join("wizard-sha256-stream-test.bin");
    let total = 3 * (1usize << 20) + 17;
    let pattern: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    ops.write_file(&path, &pattern).await.unwrap();
    let mut reference = Sha256::new();
    reference.update(&pattern);
    let expected = format!("{:x}", reference.finalize());
    assert_eq!(ops.sha256_file(&path).await.unwrap(), expected);
    std::fs::remove_file(&path).ok();
}

#[test]
#[cfg(windows)]
fn windowless_spawn_flags_are_create_no_window_alone() {
    // Regression 2026-11-02: spawn_detached once combined
    // CREATE_NO_WINDOW with DETACHED_PROCESS, which Windows
    // documents as ignoring CREATE_NO_WINDOW — the gateway's
    // Prisma query engine child then opened a visible console
    // window for the gateway's lifetime.
    assert_eq!(windowless_creation_flags(), 0x08000000);
    assert_eq!(windowless_creation_flags() & 0x00000008, 0);
}

/// Regression (S6): `spawn_detached` must deliver `PlannedCommand.env`
/// to the child — the gateway's OpenRouter key travels exclusively
/// that way, and the fake records env verbatim so only a real-spawn
/// test can catch the drop.
#[cfg(unix)]
#[tokio::test]
async fn spawn_detached_delivers_env_to_child() {
    let ops = RealSystemOps::default();
    let out_path =
        std::env::temp_dir().join(format!("wizard-spawn-env-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&out_path);
    let mut cmd = PlannedCommand::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("printf %s \"$WAVE3_TEST_VAR\" > {}", out_path.display()),
    ]);
    cmd.env
        .insert("WAVE3_TEST_VAR".to_string(), "env-delivered".to_string());
    let pid = ops.spawn_detached(&cmd).await.unwrap();
    assert!(pid > 0);
    let mut contents = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&out_path) {
            contents = s;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_file(&out_path);
    assert_eq!(contents, "env-delivered");
}

/// A detached spawn with log files named captures the child's
/// stdout and stderr to them, and a second spawn rotates the
/// first run's files to `.prev`.
#[cfg(unix)]
#[tokio::test]
async fn spawn_detached_captures_output_to_log_files() {
    let ops = RealSystemOps::default();
    let dir = std::env::temp_dir().join(format!("wizard-spawn-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out_log = dir.join("gateway.stdout.log");
    let err_log = dir.join("gateway.stderr.log");
    let spawn_once = |marker: &str| {
        let mut cmd = PlannedCommand::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo out-{marker}; echo err-{marker} >&2"),
        ]);
        cmd.stdout_log = Some(out_log.clone());
        cmd.stderr_log = Some(err_log.clone());
        cmd
    };
    let read_when = async |path: &std::path::Path, needle: &str| {
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(path) {
                if s.contains(needle) {
                    return s;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        String::new()
    };
    ops.spawn_detached(&spawn_once("one")).await.unwrap();
    assert!(read_when(&out_log, "out-one").await.contains("out-one"));
    assert!(read_when(&err_log, "err-one").await.contains("err-one"));
    // Second spawn: run one's logs rotate to .prev.
    ops.spawn_detached(&spawn_once("two")).await.unwrap();
    assert!(read_when(&out_log, "out-two").await.contains("out-two"));
    let prev = prev_log_path(&out_log);
    assert!(
        std::fs::read_to_string(&prev)
            .unwrap_or_default()
            .contains("out-one"),
        "previous run kept in {}",
        prev.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decode_credential_blob_reads_utf16le_ascii() {
    let blob: Vec<u8> = "s3cret"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    assert_eq!(decode_credential_blob(&blob).as_deref(), Some("s3cret"));
}

#[test]
fn decode_credential_blob_reads_even_length_utf8() {
    // An 8-byte ASCII blob: the previous UTF-16LE-first heuristic
    // "decoded" this into garbage codepoints and returned those.
    assert_eq!(
        decode_credential_blob(b"hunter22").as_deref(),
        Some("hunter22")
    );
}

#[test]
fn decode_credential_blob_reads_utf16le_non_ascii() {
    let blob: Vec<u8> = "pässwörd"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    assert_eq!(decode_credential_blob(&blob).as_deref(), Some("pässwörd"));
}

#[test]
fn decode_credential_blob_edge_cases() {
    // Odd length: no UTF-16 reading is attempted.
    assert_eq!(decode_credential_blob(b"abc").as_deref(), Some("abc"));
    assert_eq!(decode_credential_blob(b""), None);
    assert_eq!(decode_credential_blob(&[0, 0]), None);
}

/// `kill_process` terminates a real spawned process (bare-pid
/// SIGTERM on unix; the recorded gateway pid is the server itself).
#[cfg(unix)]
#[tokio::test]
async fn kill_process_terminates_spawned_process() {
    let ops = RealSystemOps::default();
    let cmd = PlannedCommand::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 30".to_string(),
    ]);
    let pid = ops.spawn_detached(&cmd).await.unwrap();
    assert!(pid > 0 && process_alive(pid));
    kill_process(pid).unwrap();
    let mut alive = true;
    for _ in 0..50 {
        if !process_alive(pid) {
            alive = false;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!alive, "spawned process was not terminated");
}

#[test]
fn pick_gateway_pid_prefers_live_pid_file_then_gateway_port_owner() {
    let alive = |p: u32| p == 10 || p == 20 || p == 30;
    let is_gw = |p: u32| match p {
        10 => None,        // cannot tell: still trusted from the pid file
        20 => Some(true),  // a gateway interpreter
        30 => Some(false), // another program
        _ => Some(false),
    };
    // Live recorded pid wins.
    assert_eq!(
        pick_gateway_pid(Some(10), Some(20), alive, is_gw),
        Some((10, FoundBy::PidFile))
    );
    // Dead recorded pid: the gateway port owner.
    assert_eq!(
        pick_gateway_pid(Some(99), Some(20), alive, is_gw),
        Some((20, FoundBy::PortOwner))
    );
    // Recycled recorded pid (another program): port owner.
    assert_eq!(
        pick_gateway_pid(Some(30), Some(20), alive, is_gw),
        Some((20, FoundBy::PortOwner))
    );
    // A port owner is only taken when positively a gateway.
    assert_eq!(pick_gateway_pid(None, Some(10), alive, is_gw), None);
    assert_eq!(pick_gateway_pid(None, Some(30), alive, is_gw), None);
    assert_eq!(pick_gateway_pid(None, None, alive, is_gw), None);
}

#[test]
fn stop_gateway_and_wait_with_nothing_running_reports_not_stopped() {
    let dir = std::env::temp_dir().join(format!("wizard-stop-gw-wait-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = stop_gateway_and_wait(&dir, 0, true).unwrap();
    assert_eq!(
        out,
        GatewayStopOutcome {
            stopped: false,
            pid: None,
            found_by: FoundBy::None,
            graceful: false,
            port_released: true,
        }
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stop_gateway_without_pid_file_is_false() {
    let dir = std::env::temp_dir().join(format!("wizard-stop-gw-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(stop_gateway(&dir), Ok(false));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn stop_gateway_with_stale_pid_cleans_up() {
    let dir = std::env::temp_dir().join(format!("wizard-stop-gw-stale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A pid guaranteed dead: a child that already exited and was
    // reaped. (A merely huge number would be a dangerous stand-in:
    // values above i32::MAX wrap to negative pid_t values, where
    // -1 signals every process the user owns.)
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    std::fs::write(gateway_pid_path(&dir), dead_pid.to_string()).unwrap();
    assert_eq!(stop_gateway(&dir), Ok(false));
    assert!(!gateway_pid_path(&dir).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn out_of_range_pids_are_never_signalled() {
    // 0 = own process group; i32::MAX+1.. wraps to negative pid_t
    // (-1 = every process the user owns). Both must be refused
    // before any syscall.
    assert!(!process_alive(0));
    assert!(!process_alive(u32::MAX));
    assert!(kill_process(0).is_err());
    assert!(kill_process(u32::MAX).is_err());
}

#[cfg(unix)]
#[test]
fn stop_gateway_kills_recorded_process() {
    let dir = std::env::temp_dir().join(format!("wizard-stop-gw-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    std::fs::write(gateway_pid_path(&dir), pid.to_string()).unwrap();
    assert!(process_alive(pid));
    assert_eq!(stop_gateway(&dir), Ok(true));
    assert!(!gateway_pid_path(&dir).exists());
    let mut exited = false;
    for _ in 0..50 {
        if child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(exited, "recorded process was not terminated");
}

/// `cmdkey /list:<target>` echoes the target in its header even when
/// nothing is stored, so a presence check that searched its output for
/// the target answered "present" for every name. Read-only: looks up a
/// random target that cannot exist.
#[tokio::test]
async fn credential_exists_is_false_for_an_absent_target() {
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).unwrap();
    let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let target = format!("llmdil/test/absent/{hex}");
    assert!(!RealSystemOps::default()
        .credential_exists(&target)
        .await
        .unwrap());
}
