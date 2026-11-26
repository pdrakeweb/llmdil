//! Test fixture for the debug console integration tests
//! (`tests/console_run.rs`) — never shipped or run by the app.
//!
//! Behaves like the llama.cpp tool its file name says it is:
//!
//! - `llama-bench*`: prints `-o json` (or markdown) results for each
//!   `-d` depth, and at depth >= 8192 with `-fa 1 -ctk q8_0` prints the
//!   CUDA error block from the planner-moe incident and dies with
//!   `0xc0000409` (Windows) / SIGABRT (elsewhere). `FAKE_SLEEP_MS`
//!   makes it sleep first (cancel tests). It echoes whether
//!   `CUDA_LAUNCH_BLOCKING` reached it.
//! - `llama-server*`: serves `/health` on `--port`, logs to stderr, and
//!   crashes the same way on a `/completion` request with a long
//!   prompt.

use std::io::{Read, Write};

fn arg(args: &[String], names: &[&str]) -> Option<String> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if names.contains(&args[i].as_str()) {
            found = args.get(i + 1).cloned();
            i += 1;
        }
        i += 1;
    }
    found
}

fn crash() -> ! {
    let mut e = std::io::stderr();
    let _ = writeln!(e, "ggml-cuda.cu:109: CUDA error");
    let _ = writeln!(e, "CUDA error: invalid argument");
    let _ = writeln!(
        e,
        "  current device: 0, in function ggml_cuda_kernel_launch at D:\\a\\llama.cpp\\llama.cpp\\ggml\\src\\ggml-cuda\\common.cuh:1716"
    );
    let _ = writeln!(e, "  cudaGetLastError()");
    let _ = e.flush();
    let _ = std::io::stdout().flush();
    #[cfg(windows)]
    std::process::exit(0xC000_0409_u32 as i32);
    #[cfg(not(windows))]
    std::process::abort();
}

fn bench(args: &[String]) {
    if let Some(ms) = std::env::var("FAKE_SLEEP_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        eprintln!("fake llama-bench: sleeping {ms} ms");
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    eprintln!(
        "fake llama-bench: CUDA_LAUNCH_BLOCKING={}",
        std::env::var("CUDA_LAUNCH_BLOCKING").unwrap_or_else(|_| "unset".into())
    );
    let depths: Vec<u64> = arg(args, &["-d"])
        .unwrap_or_else(|| "0".into())
        .split(',')
        .filter_map(|d| d.trim().parse().ok())
        .collect();
    let p: u64 = arg(args, &["-p"])
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let fa = arg(args, &["-fa"]).unwrap_or_else(|| "0".into());
    let ctk = arg(args, &["-ctk"]).unwrap_or_else(|| "f16".into());
    let ctv = arg(args, &["-ctv"]).unwrap_or_else(|| "f16".into());
    let json = arg(args, &["-o"]).as_deref() == Some("json");
    let mut out = std::io::stdout();
    if json {
        let _ = writeln!(out, "[");
    } else {
        let _ = writeln!(
            out,
            "| model | size | params | backend | ngl | fa | test | t/s |"
        );
        let _ = writeln!(out, "| --- | ---: | ---: | --- | --: | -: | ---: | ---: |");
    }
    for (i, d) in depths.iter().enumerate() {
        if *d >= 8192 && fa == "1" && ctk == "q8_0" {
            let _ = out.flush();
            eprintln!("fake llama-bench: prompt processing at depth {d}");
            crash();
        }
        let ts = 1000.0 - (*d as f64) / 20.0;
        if json {
            let _ = writeln!(
                out,
                "  {{\"n_prompt\": {p}, \"n_gen\": 0, \"n_depth\": {d}, \"avg_ts\": {ts}, \"stddev_ts\": 1.5, \"flash_attn\": {}, \"type_k\": \"{ctk}\", \"type_v\": \"{ctv}\"}}{}",
                fa == "1",
                if i + 1 < depths.len() { "," } else { "" }
            );
        } else {
            let test = if *d > 0 {
                format!("pp{p} @ d{d}")
            } else {
                format!("pp{p}")
            };
            let _ = writeln!(
                out,
                "| fake 1B Q4_0 | 1.00 GiB | 1.00 B | CUDA | 99 | {fa} | {test} | {ts:.2} ± 1.50 |"
            );
        }
        let _ = out.flush();
    }
    if json {
        let _ = writeln!(out, "]");
    }
}

fn server(args: &[String]) {
    let port: u16 = arg(args, &["--port"])
        .and_then(|v| v.parse().ok())
        .unwrap_or(18099);
    eprintln!("fake llama-server: args {}", args.join(" "));
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fake llama-server: bind {port} failed: {e}");
            std::process::exit(2);
        }
    };
    eprintln!("fake llama-server: listening on 127.0.0.1:{port}");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        // Read headers, then Content-Length bytes of body.
        let (head_end, len) = loop {
            let n = match s.read(&mut chunk) {
                Ok(0) | Err(_) => break (None, 0),
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (Some(pos + 4), len);
            }
        };
        let Some(head_end) = head_end else { continue };
        while buf.len() < head_end + len {
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let body_len = buf.len().saturating_sub(head_end);
        let reply = if path.starts_with("/completion") {
            eprintln!("fake llama-server: completion request, {body_len} body bytes");
            if body_len > 4000 {
                eprintln!("slot update_slots: id  0 | task 0 | prompt processing, n_tokens = 7680, progress = 0.93");
                crash();
            }
            "{\"content\":\"ok\",\"timings\":{\"prompt_n\":10,\"prompt_ms\":5.0,\"prompt_per_second\":2000.0}}".to_string()
        } else {
            "{\"status\":\"ok\"}".to_string()
        };
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
    }
}

fn main() {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if exe.contains("llama-server") {
        server(&args);
    } else {
        bench(&args);
    }
}
