use super::*;
use crate::test_support::*;

/// A one-endpoint fake WinMLServer: answers every request with an
/// OpenAI model list serving `qwen3-8b`. Requests after the first
/// carry an adapter header, so a test can tell a re-probe apart
/// from the registration probe.
fn fake_winml_server() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().take(16).enumerate() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => head.extend_from_slice(&buf[..k]),
                }
            }
            let body = r#"{"object":"list","data":[{"id":"qwen3-8b"}]}"#;
            let adapter = if n == 0 {
                String::new()
            } else {
                "x-winml-adapter: Fake Adapter\r\n".to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{adapter}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    port
}

/// Tech spec §6.5 / P7: "Re-check capabilities" (winml_probe from
/// the WinML card) re-probes through the registry when a model is
/// registered, refreshing the registration — here the adapter hint
/// the registration probe did not get.
#[tokio::test]
async fn winml_probe_rechecks_the_active_registration() {
    let port = fake_winml_server();
    let mut cfg = AppConfig::default_config();
    cfg.winml.enabled = true;
    cfg.winml.port = port;
    cfg.winml.model_id = Some("qwen3-8b".to_string());
    let (state, _dir) = advanced_test_state(cfg);
    let sink = test_sink();

    let reg = op_winml_register(&state, &sink, "qwen3-8b".to_string())
        .await
        .unwrap();
    assert!(reg["responding_adapter"].is_null(), "{reg}");

    let report = op_winml_probe(&state, &sink).await.unwrap();
    assert_eq!(report["models"], json!(["qwen3-8b"]));
    assert_eq!(report["adapter_hint"], "Fake Adapter");
    let status = op_winml_status(&state, &sink).await.unwrap();
    assert_eq!(
        status["registered"]["responding_adapter"], "Fake Adapter",
        "the re-check refreshed the registration: {status}"
    );

    // Unregistered: a plain probe of the configured port.
    op_winml_unregister(&state, &sink).await.unwrap();
    let report = op_winml_probe(&state, &sink).await.unwrap();
    assert_eq!(report["base_url"], manager_winml::base_url(port).as_str());
}
