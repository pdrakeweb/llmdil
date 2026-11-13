//! The device-pairing payload and its QR code (PLAN.md §3.8,
//! android-app-plan.md "Pairing"): the desktop Remote Access card
//! shows a QR encoding `{ base_url, cert_sha256, enrollment_code }`;
//! the phone scans it, pins the fingerprint, and exchanges the
//! one-time code for its per-device session.
//!
//! The QR carries exactly the values the card already shows as
//! copyable text — nothing else — and is rendered here, server-side,
//! as an SVG the webview inlines (the CSP admits no `data:` images).

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};
use serde::Serialize;

/// The QR content. Field names are the wire contract the Android
/// client parses; `enrollment_code` is omitted from the code-less
/// (fingerprint-only) payload.
#[derive(Debug, Serialize)]
pub struct PairingPayload<'a> {
    pub base_url: &'a str,
    pub cert_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrollment_code: Option<&'a str>,
}

impl PairingPayload<'_> {
    /// The compact JSON text the QR encodes.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("string fields always serialize")
    }
}

/// Render `text` as a standalone `<svg>` element (no XML prolog, so
/// it can be inlined into HTML): black modules on white with the
/// standard quiet zone, at least 220 px square, error correction
/// level M (survives a smudged or slightly glared screen).
pub fn qr_svg(text: &str) -> Result<String, String> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)
        .map_err(|e| format!("pairing QR could not be encoded: {e}"))?;
    let rendered = code
        .render::<svg::Color<'_>>()
        .min_dimensions(220, 220)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .quiet_zone(true)
        .build();
    // Drop the XML declaration: inside HTML it is a bogus comment at
    // best. What remains is the renderer's own markup — no payload
    // text is ever interpolated into it.
    let start = rendered
        .find("<svg")
        .ok_or_else(|| "pairing QR renderer produced no <svg> element".to_string())?;
    Ok(rendered[start..].to_string())
}

/// The pairing QR for a payload, or `None` when there is no
/// certificate fingerprint yet (a QR without the pin is useless to
/// the client, which must pin at pairing time).
pub fn pairing_qr_svg(
    base_url: &str,
    cert_sha256: Option<&str>,
    enrollment_code: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(cert_sha256) = cert_sha256 else {
        return Ok(None);
    };
    let payload = PairingPayload {
        base_url,
        cert_sha256,
        enrollment_code,
    };
    qr_svg(&payload.to_json()).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89";

    #[test]
    fn payload_is_the_documented_three_fields() {
        let json = PairingPayload {
            base_url: "https://192.168.1.20:47821",
            cert_sha256: FP,
            enrollment_code: Some("ABCD-EFGH"),
        }
        .to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 3, "{json}");
        assert_eq!(v["base_url"], "https://192.168.1.20:47821");
        assert_eq!(v["cert_sha256"], FP);
        assert_eq!(v["enrollment_code"], "ABCD-EFGH");

        let no_code = PairingPayload {
            base_url: "https://192.168.1.20:47821",
            cert_sha256: FP,
            enrollment_code: None,
        }
        .to_json();
        assert!(!no_code.contains("enrollment_code"), "{no_code}");
    }

    #[test]
    fn qr_is_inlineable_svg_without_payload_text() {
        let svg = pairing_qr_svg("https://192.168.1.20:47821", Some(FP), Some("ABCD-EFGH"))
            .unwrap()
            .expect("a fingerprint yields a QR");
        assert!(svg.starts_with("<svg"), "{}", &svg[..40.min(svg.len())]);
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(!svg.contains("<?xml"));
        // Only modules are drawn: the code and URL never appear as
        // markup text, and there is nothing scriptable.
        assert!(!svg.contains("ABCD-EFGH"));
        assert!(!svg.contains("192.168.1.20"));
        assert!(!svg.to_lowercase().contains("<script"));
        assert!(!svg.to_lowercase().contains(" on"));
    }

    #[test]
    fn no_fingerprint_no_qr() {
        assert_eq!(
            pairing_qr_svg("https://127.0.0.1:47821", None, Some("X")).unwrap(),
            None
        );
    }

    #[test]
    fn qr_is_deterministic_and_payload_sensitive() {
        let a = qr_svg("payload-one").unwrap();
        assert_eq!(a, qr_svg("payload-one").unwrap());
        assert_ne!(a, qr_svg("payload-two").unwrap());
    }
}
