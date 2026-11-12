//! Destination endpoint policy (SEC-16, docs/code-review-v0.1.60.md).
//!
//! Batches carry the ingest token in a header and redacted log lines
//! in the body. Over plain `http://` both cross the network in the
//! clear, so plain HTTP is accepted only for a loopback destination
//! (the local receiver, `http://127.0.0.1:8473`); any other host must
//! be `https://`. The same loopback test decides whether the audit
//! source may ship at all: audit records carry usernames and source
//! addresses and stay on the machine unless the destination is local.

use reqwest::Url;

/// What an accepted endpoint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointInfo {
    /// The destination host is this machine (`127.0.0.0/8`, `::1`,
    /// `localhost` or a `.localhost` name).
    pub loopback: bool,
}

/// Validate a destination base URL. `Err` carries the operator-facing
/// reason the shipper will not use it.
pub fn check_endpoint(endpoint: &str) -> Result<EndpointInfo, String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err("no endpoint is configured".to_string());
    }
    let url = Url::parse(endpoint).map_err(|e| format!("the endpoint is not a URL ({e})"))?;
    // `host_str` is the parsed, normalised host: an address literal
    // (IPv6 in brackets) or a lower-cased name — never userinfo.
    let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
        return Err("the endpoint has no host".to_string());
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => {
            let name = bare.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    };
    match url.scheme() {
        "https" => Ok(EndpointInfo { loopback }),
        "http" if loopback => Ok(EndpointInfo { loopback }),
        "http" => Err(
            "plain http:// is allowed only for a loopback endpoint (127.0.0.1, localhost, [::1]); \
             use https:// for any other host"
                .to_string(),
        ),
        other => Err(format!(
            "the endpoint scheme must be https (or http on loopback), not {other}"
        )),
    }
}

/// Whether `endpoint` is an accepted loopback destination.
pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    check_endpoint(endpoint)
        .map(|e| e.loopback)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_http_is_accepted() {
        for ok in [
            "http://127.0.0.1:8473",
            "http://127.0.0.1:8473/",
            "http://127.8.9.1",
            "http://localhost:8473",
            "http://LOCALHOST:8473",
            "http://receiver.localhost:8473",
            "http://[::1]:8473",
            " http://127.0.0.1:8473 ",
        ] {
            assert_eq!(
                check_endpoint(ok),
                Ok(EndpointInfo { loopback: true }),
                "{ok}"
            );
            assert!(is_loopback_endpoint(ok), "{ok}");
        }
    }

    #[test]
    fn https_is_accepted_anywhere() {
        assert_eq!(
            check_endpoint("https://logs.example.test"),
            Ok(EndpointInfo { loopback: false })
        );
        assert_eq!(
            check_endpoint("https://127.0.0.1:8473"),
            Ok(EndpointInfo { loopback: true })
        );
        assert!(!is_loopback_endpoint("https://logs.example.test"));
    }

    #[test]
    fn non_loopback_http_is_refused() {
        for bad in [
            "http://logs.example.test",
            "http://192.168.1.20:8473",
            "http://100.64.0.7:8473",
            "http://0.0.0.0:8473",
            // Look-alikes that are not loopback.
            "http://127.0.0.1.example.test",
            "http://localhost.example.test",
            "http://notlocalhost:8473",
            "http://user@logs.example.test",
        ] {
            let err = check_endpoint(bad).expect_err(bad);
            assert!(err.contains("loopback"), "{bad}: {err}");
            assert!(!is_loopback_endpoint(bad), "{bad}");
        }
    }

    #[test]
    fn malformed_endpoints_are_refused() {
        assert!(check_endpoint("").is_err());
        assert!(check_endpoint("   ").is_err());
        assert!(check_endpoint("127.0.0.1:8473").is_err());
        assert!(check_endpoint("ftp://127.0.0.1").is_err());
        assert!(check_endpoint("file:///c:/logs").is_err());
    }
}
