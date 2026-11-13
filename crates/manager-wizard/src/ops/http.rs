//! Shared HTTP GET/POST implementations behind the `SystemOps` HTTP methods.

use super::*;

pub(crate) async fn http_get_impl(
    url: &str,
    headers: &[(String, String)],
) -> Result<HttpResponse, WizardError> {
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(HTTP_GET_TIMEOUT)
        .build()
        .map_err(|e| WizardError::HttpError {
            url: url.to_string(),
            reason: format!("client build failed: {e}"),
        })?;
    let mut req = client.get(url);
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    Ok(HttpResponse {
        status,
        body: body.to_vec(),
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

/// Shared POST implementation behind the trait's plain and
/// header-carrying variants.
pub(crate) async fn http_post_impl(
    url: &str,
    body: &str,
    headers: &[(String, String)],
) -> Result<HttpResponse, WizardError> {
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(HTTP_POST_TIMEOUT)
        .build()
        .map_err(|e| WizardError::HttpError {
            url: url.to_string(),
            reason: format!("client build failed: {e}"),
        })?;
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| WizardError::HttpError {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    Ok(HttpResponse {
        status,
        body: bytes.to_vec(),
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}
