mod artwork;
mod audio;
mod auth;
mod catalog;
pub(crate) mod reporting;
#[cfg(test)]
mod tests;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::domain::Error;
use url::Url;

use crate::infrastructure::cache::covers::CoverCache;

// Only this session is persisted in the desktop keyring. Never store the password.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub server: String,
    pub user_id: String,
    pub username: String,
    token: String,
    device_id: String,
}

pub struct AudioStream {
    pub body: ureq::Body,
    pub offset: u64,
    pub length: Option<u64>,
    pub end: Option<u64>,
}

fn parse_audio_range(value: Option<&str>) -> Option<(u64, u64, u64)> {
    let (unit, value) = value?.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, length) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, length) = (start.parse().ok()?, end.parse().ok()?, length.parse().ok()?);
    (start <= end && end < length).then_some((start, end, length))
}

fn request_error(error: ureq::Error) -> Error {
    // Requests carry secrets in headers or the body, never in these URLs.
    // ureq's transport errors describe status, DNS, TLS, timeouts, and I/O.
    match &error {
        ureq::Error::StatusCode(status) => tracing::error!(status, "Jellyfin request failed"),
        _ => tracing::error!(error = %error, "Jellyfin transport failed"),
    }
    match error {
        ureq::Error::StatusCode(401) => Error::Unauthorized,
        ureq::Error::StatusCode(403) => {
            Error::Message("Jellyfin refused access. Check your account's permissions.")
        }
        ureq::Error::StatusCode(404) => {
            Error::Message("Jellyfin could not find that resource. Check the server URL.")
        }
        ureq::Error::StatusCode(500..=599) => {
            Error::Message("Jellyfin encountered a server error. Try again.")
        }
        // Do not surface raw transport errors, which may include request details.
        _ => Error::Message(
            "Could not complete the request. Check the server URL and connection, then retry.",
        ),
    }
}

fn read_response<T: serde::de::DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
    started: Instant,
    message: &'static str,
) -> Result<T, Error> {
    let status = response.status().as_u16();
    tracing::debug!(
        status,
        elapsed_ms = started.elapsed().as_millis(),
        content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown"),
        "Received Jellyfin response"
    );
    if response.status().is_redirection() {
        tracing::warn!(
            status,
            "Jellyfin redirected the request; use the final server URL"
        );
        return Err(Error::Message(
            "Jellyfin redirected the request. Enter the final server URL, including its base path.",
        ));
    }
    response.body_mut().read_json().map_err(|error| {
        match &error {
            ureq::Error::Json(error) => {
                // Serde's full error text can contain values from the response,
                // including access tokens. Log position and category instead.
                tracing::error!(status, category = ?error.classify(), line = error.line(), column = error.column(),
                    "Could not decode Jellyfin response as the expected JSON structure");
            }
            _ => { request_error(error); return Error::Message(message); }
        }
        Error::Message(message)
    })
}

pub fn server_url(value: &str) -> Result<Url, Error> {
    let mut url = Url::parse(value.trim()).map_err(|_| {
        Error::Message("Enter a full server URL, such as https://music.example.com.")
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Message(
            "Use an HTTP or HTTPS server URL without credentials, a query, or a fragment.",
        ));
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

#[derive(Clone)]
pub struct Client {
    pub session: Session,
    agent: ureq::Agent,
    base: Url,
    cover_cache: Option<Arc<CoverCache>>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        // Require the final server URL so credentials never follow a redirect.
        .max_redirects(0)
        .build()
        .into()
}

fn authorization(device_id: &str, token: Option<&str>) -> Result<ureq::http::HeaderValue, Error> {
    let mut value = format!(
        "MediaBrowser Client=\"Jellyui\", Device=\"Linux\", DeviceId=\"{device_id}\", Version=\"{}\"",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(token) = token {
        // Jellyfin URL-decodes authorization values. Encode before quoting so
        // delimiters and '+' characters survive parsing unchanged.
        let token: String = url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
        value.push_str(&format!(", Token=\"{token}\""));
    }
    let mut value = ureq::http::HeaderValue::try_from(value)
        .map_err(|_| Error::Message("Could not construct Jellyfin authorization header."))?;
    value.set_sensitive(true);
    Ok(value)
}
