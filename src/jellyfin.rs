use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::album_search::{AlbumSearch, search_words};
use crate::cover_cache::{CoverCache, MAX_COVER_BYTES};
use crate::playback_reporting::Report;

pub const PAGE_SIZE: usize = 100;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum AlbumSort {
    #[default]
    Title,
    Artist,
    Year,
    Added,
}

impl AlbumSort {
    fn sort_by(self) -> &'static str {
        match self {
            Self::Title => "SortName",
            Self::Artist => "AlbumArtist,SortName",
            Self::Year => "ProductionYear,SortName",
            Self::Added => "DateCreated,SortName",
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct AlbumQuery {
    pub search: String,
    pub sort: AlbumSort,
    pub descending: bool,
}

// Only this session is persisted in the desktop keyring. Never store the password.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub server: String,
    pub user_id: String,
    pub username: String,
    token: String,
    device_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Artist {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    pub id: String,
    pub name: String,
    pub sort_name: Option<String>,
    pub date_created: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub artists: Option<Vec<String>>,
    pub artist_items: Option<Vec<Artist>>,
    pub album_artists: Option<Vec<Artist>>,
    pub production_year: Option<u32>,
    pub premiere_date: Option<String>,
    pub child_count: Option<usize>,
    pub index_number: Option<u32>,
    pub parent_index_number: Option<u32>,
    pub run_time_ticks: Option<u64>,
}

impl Item {
    pub fn artist(&self) -> String {
        self.album_artist
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.artists
                    .as_ref()
                    .filter(|a| !a.is_empty())
                    .map(|a| a.join(", "))
            })
            .unwrap_or_else(|| "Unknown artist".into())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Page {
    pub items: Vec<Item>,
    pub total_record_count: usize,
}

pub struct AudioStream {
    pub body: ureq::Body,
    pub offset: u64,
    pub length: Option<u64>,
    pub end: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum Error {
    Unauthorized,
    Message(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unauthorized => "Jellyfin rejected the login. Please sign in again.",
            Self::Message(message) => message,
        })
    }
}

impl std::error::Error for Error {}

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

impl Client {
    pub fn report_playback(&self, report: &Report) -> Result<(), Error> {
        let url = self.base.join(report.kind.path()).unwrap();
        let response = self
            .agent
            .post(url.as_str())
            .config()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            )
            .send_json(report)
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(Error::Message(
                "Jellyfin redirected the playback report. Use the final server URL.",
            ));
        }
        tracing::debug!(kind = ?report.kind, status = response.status().as_u16(), "Playback reported to Jellyfin");
        Ok(())
    }

    #[tracing::instrument(level = "debug", name = "jellyfin.cover", skip_all)]
    pub fn cover(&self, item_id: &str) -> Result<Option<Vec<u8>>, Error> {
        let cache_key = CoverCache::key(self.base.as_str(), &self.session.user_id, item_id);
        if let Some(cache) = &self.cover_cache {
            match cache.get(&cache_key) {
                Ok(Some(bytes)) => {
                    tracing::debug!("Cover disk cache hit");
                    return Ok(Some(bytes));
                }
                Ok(None) => tracing::debug!("Cover disk cache miss"),
                Err(error) => {
                    tracing::warn!(%error, "Could not read cover disk cache; fetching artwork")
                }
            }
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("validated HTTP URL")
            .pop_if_empty()
            .extend(["Items", item_id, "Images", "Primary"]);
        url.query_pairs_mut().extend_pairs([
            ("maxWidth", "640"),
            ("maxHeight", "640"),
            ("format", "Png"),
        ]);
        let mut response = match self
            .agent
            .get(url.as_str())
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            )
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => return Err(request_error(error)),
        };
        if response.status().is_redirection() {
            return Err(Error::Message(
                "Jellyfin redirected the album artwork request.",
            ));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_COVER_BYTES)
            .read_to_vec()
            .map_err(request_error)?;
        if let Some(cache) = &self.cover_cache
            && let Err(error) = cache.put(&cache_key, &bytes)
        {
            tracing::warn!(%error, "Could not persist album artwork in disk cache");
        }
        Ok(Some(bytes))
    }

    /// Stream the original file. A per-read timeout detects a stalled connection
    /// without imposing a total deadline on a long track or a paused player.
    #[tracing::instrument(name = "jellyfin.audio", skip_all)]
    pub fn audio(&self, item_id: &str) -> Result<ureq::Body, Error> {
        self.audio_request(item_id, None).map(|stream| stream.body)
    }

    #[tracing::instrument(name = "jellyfin.audio_range", skip(self, item_id))]
    pub fn audio_range(
        &self,
        item_id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<AudioStream, Error> {
        self.audio_request(item_id, Some((start, end)))
    }

    fn audio_request(
        &self,
        item_id: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<AudioStream, Error> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("validated HTTP URL")
            .pop_if_empty()
            .extend(["Audio", item_id, "stream"]);
        url.query_pairs_mut().append_pair("static", "true");
        tracing::debug!("Requesting original audio file");
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(10)))
            .build()
            .into();
        let mut request = agent
            .get(url.as_str())
            .header("Accept-Encoding", "identity")
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            );
        if let Some((start, end)) = range {
            request = request.header(
                "Range",
                format!(
                    "bytes={start}-{}",
                    end.map(|end| end.to_string()).unwrap_or_default()
                ),
            );
        }
        let mut response = request.call().map_err(request_error)?;
        if response.status().is_redirection() {
            return Err(Error::Message(
                "Jellyfin redirected the audio request. Use the final server URL.",
            ));
        }
        tracing::debug!(
            status = response.status().as_u16(),
            "Original audio stream opened"
        );
        if response
            .headers()
            .get("Content-Encoding")
            .is_some_and(|value| value != "identity")
        {
            return Err(Error::Message(
                "Jellyfin compressed the audio stream; the original file is required for playback.",
            ));
        }
        let (offset, length, end) = match (response.status().as_u16(), range) {
            (200, _) => {
                let length = response.body_mut().content_length();
                (0, length, length)
            }
            (206, Some((start, requested_end))) => {
                let value = response
                    .headers()
                    .get("Content-Range")
                    .and_then(|value| value.to_str().ok());
                let (offset, last, length) = parse_audio_range(value)
                    .ok_or(Error::Message("Jellyfin returned an invalid audio range."))?;
                if offset != start
                    || requested_end.is_some_and(|end| last > end)
                    || (requested_end.is_none() && last != length - 1)
                    || response
                        .body_mut()
                        .content_length()
                        .is_some_and(|size| size != last - offset + 1)
                {
                    return Err(Error::Message(
                        "Jellyfin returned an unexpected audio range.",
                    ));
                }
                (offset, Some(length), Some(last + 1))
            }
            _ => {
                return Err(Error::Message(
                    "Jellyfin returned an unexpected partial audio response.",
                ));
            }
        };
        Ok(AudioStream {
            body: response.into_body(),
            offset,
            length,
            end,
        })
    }

    pub fn restore(session: Session) -> Result<Self, Error> {
        let base = server_url(&session.server)?;
        Ok(Self {
            session,
            agent: agent(),
            base,
            cover_cache: CoverCache::system().map(Arc::new),
        })
    }

    #[tracing::instrument(name = "jellyfin.sign_in", skip_all)]
    pub fn sign_in(server: &str, username: &str, password: &str) -> Result<Self, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Authentication {
            access_token: String,
            user: User,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct User {
            id: String,
            name: String,
        }

        let base = server_url(server)?;
        tracing::info!(server = %base, "Authenticating with Jellyfin");
        let agent = agent();
        let device_id = uuid::Uuid::new_v4().to_string();
        let started = Instant::now();
        let response = agent
            .post(base.join("Users/AuthenticateByName").unwrap().as_str())
            .header("Authorization", authorization(&device_id, None)?)
            .send_json(serde_json::json!({ "Username": username, "Pw": password }))
            .map_err(request_error)?;
        let auth: Authentication = read_response(
            response,
            started,
            "Jellyfin returned an invalid sign-in response.",
        )?;
        if auth.access_token.is_empty() || auth.user.id.is_empty() {
            tracing::error!("Jellyfin sign-in response is missing an access token or user ID");
            return Err(Error::Message(
                "Jellyfin returned an incomplete sign-in response.",
            ));
        }
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            "Jellyfin authentication succeeded"
        );
        Ok(Self {
            session: Session {
                server: base.to_string(),
                user_id: auth.user.id,
                username: auth.user.name,
                token: auth.access_token,
                device_id,
            },
            agent,
            base,
            cover_cache: CoverCache::system().map(Arc::new),
        })
    }

    fn items_url(
        &self,
        kind: &str,
        start: usize,
        parent: Option<&str>,
        sort_by: &str,
        sort_order: &str,
    ) -> Url {
        let mut url = self.base.join("Items").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("UserId", self.session.user_id.as_str()),
            ("IncludeItemTypes", kind),
            ("Recursive", "true"),
            ("SortBy", sort_by),
            ("SortOrder", sort_order),
            ("Fields", "ItemCounts,SortName,DateCreated"),
            ("EnableTotalRecordCount", "true"),
            ("StartIndex", &start.to_string()),
            ("Limit", &PAGE_SIZE.to_string()),
        ]);
        if let Some(parent) = parent {
            url.query_pairs_mut().append_pair("ParentId", parent);
        }
        url
    }

    fn page(&self, url: Url) -> Result<Page, Error> {
        self.library_response(url)
    }

    fn library_response<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T, Error> {
        tracing::debug!(server = %self.base, path = url.path(), "Requesting Jellyfin library page");
        let started = Instant::now();
        let response = self
            .agent
            .get(url.as_str())
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            )
            .call()
            .map_err(request_error)?;
        read_response(
            response,
            started,
            "Jellyfin returned an invalid library response.",
        )
    }

    #[tracing::instrument(level = "debug", name = "jellyfin.artist_by_name", skip_all)]
    pub fn artist_by_name(&self, name: &str) -> Result<Artist, Error> {
        let mut url = self.base.join("Artists/").unwrap();
        url.path_segments_mut().unwrap().pop_if_empty().push(name);
        url.query_pairs_mut()
            .append_pair("UserId", &self.session.user_id);
        let artist: Artist = self.library_response(url)?;
        if artist.id.as_deref().is_none_or(str::is_empty) {
            return Err(Error::Message("Jellyfin returned an artist without an ID."));
        }
        Ok(artist)
    }

    #[tracing::instrument(level = "debug", name = "jellyfin.albums", skip_all, fields(start))]
    pub fn albums(&self, start: usize, query: &AlbumQuery) -> Result<Page, Error> {
        self.album_page(start, query, None)
    }

    #[tracing::instrument(
        level = "debug",
        name = "jellyfin.artist_albums",
        skip_all,
        fields(start)
    )]
    pub fn artist_albums(
        &self,
        start: usize,
        artist: &str,
        query: &AlbumQuery,
    ) -> Result<Page, Error> {
        self.album_page(start, query, Some(artist))
    }

    fn album_page(
        &self,
        start: usize,
        query: &AlbumQuery,
        artist: Option<&str>,
    ) -> Result<Page, Error> {
        let mut url = self.items_url(
            "MusicAlbum",
            start,
            None,
            query.sort.sort_by(),
            if query.descending {
                "Descending"
            } else {
                "Ascending"
            },
        );
        if !query.search.is_empty() {
            url.query_pairs_mut()
                .append_pair("SearchTerm", &query.search);
        }
        if let Some(artist) = artist {
            url.query_pairs_mut().append_pair("ArtistIds", artist);
        }
        self.page(url)
    }

    #[tracing::instrument(level = "debug", name = "jellyfin.artists", skip_all, fields(start))]
    pub fn artists(&self, start: usize, search: &str, descending: bool) -> Result<Page, Error> {
        let mut url = self.base.join("Artists").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("UserId", self.session.user_id.as_str()),
            ("IncludeItemTypes", "Audio,MusicAlbum"),
            ("SortBy", "SortName"),
            (
                "SortOrder",
                if descending {
                    "Descending"
                } else {
                    "Ascending"
                },
            ),
            ("Fields", "ItemCounts,SortName"),
            ("EnableTotalRecordCount", "true"),
            ("StartIndex", &start.to_string()),
            ("Limit", &PAGE_SIZE.to_string()),
        ]);
        if !search.is_empty() {
            url.query_pairs_mut().append_pair("SearchTerm", search);
        }
        self.page(url)
    }

    /// Search titles and credited artists separately, then establish one global
    /// order before paging. ArtistIds matches both artist and album-artist credits.
    pub fn search_albums(
        &self,
        query: &AlbumQuery,
        cancelled: &AtomicBool,
    ) -> Result<AlbumSearch, Error> {
        // Any matching album must contain every word, so one word suffices to
        // retrieve candidates. Prefer a longer word to avoid broad short searches.
        let words = search_words(&query.search);
        let Some(term) = words.iter().max_by_key(|word| word.chars().count()) else {
            return Ok(AlbumSearch { ids: Vec::new() });
        };
        let titles = self.collect_pages(cancelled, |start| {
            let mut url = self.items_url("MusicAlbum", start, None, "SortName", "Ascending");
            url.query_pairs_mut().append_pair("SearchTerm", term);
            url
        })?;
        let mut artists = Vec::new();
        let mut seen = HashSet::new();
        for route in ["Artists", "Artists/AlbumArtists"] {
            let matches = self.collect_pages(cancelled, |start| {
                let mut url = self.base.join(route).unwrap();
                url.query_pairs_mut().extend_pairs([
                    ("UserId", self.session.user_id.as_str()),
                    ("SearchTerm", *term),
                    ("IncludeItemTypes", "Audio,MusicAlbum"),
                    ("SortBy", "SortName"),
                    ("SortOrder", "Ascending"),
                    ("EnableTotalRecordCount", "true"),
                    ("StartIndex", &start.to_string()),
                    ("Limit", &PAGE_SIZE.to_string()),
                ]);
                url
            })?;
            artists.extend(
                matches
                    .into_iter()
                    .filter(|artist| seen.insert(artist.id.clone())),
            );
        }
        let mut credited = Vec::new();
        // Bound request URL size even when a short search matches many artists.
        for artists in artists.chunks(50) {
            let ids = artists
                .iter()
                .map(|artist| artist.id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            credited.extend(self.collect_pages(cancelled, |start| {
                let mut url = self.items_url("MusicAlbum", start, None, "SortName", "Ascending");
                url.query_pairs_mut().append_pair("ArtistIds", &ids);
                url
            })?);
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(Error::Message("Album search cancelled."));
        }
        Ok(AlbumSearch::new(titles, credited, query))
    }

    fn collect_pages(
        &self,
        cancelled: &AtomicBool,
        url: impl Fn(usize) -> Url,
    ) -> Result<Vec<Item>, Error> {
        let mut items = Vec::new();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(Error::Message("Album search cancelled."));
            }
            let page = self.page(url(items.len()))?;
            let expected = page
                .total_record_count
                .saturating_sub(items.len())
                .min(PAGE_SIZE);
            if page.items.len() != expected {
                return Err(Error::Message(
                    "Jellyfin returned incomplete search results. Retry the search.",
                ));
            }
            let done = items.len() + page.items.len() >= page.total_record_count;
            items.extend(page.items);
            if done {
                return Ok(items);
            }
        }
    }

    pub fn search_page(&self, start: usize, search: &AlbumSearch) -> Result<Page, Error> {
        let ids = search
            .ids
            .get(start..search.ids.len().min(start.saturating_add(PAGE_SIZE)))
            .unwrap_or_default();
        if ids.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                total_record_count: search.ids.len(),
            });
        }
        let mut url = self.items_url("MusicAlbum", 0, None, "SortName", "Ascending");
        url.query_pairs_mut().append_pair("Ids", &ids.join(","));
        let page = self.page(url)?;
        let mut items: HashMap<_, _> = page
            .items
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect();
        let items = ids
            .iter()
            .map(|id| {
                items.remove(id).ok_or(Error::Message(
                    "Album search results changed. Retry the search.",
                ))
            })
            .collect::<Result<_, _>>()?;
        Ok(Page {
            items,
            total_record_count: search.ids.len(),
        })
    }

    #[tracing::instrument(level = "debug", name = "jellyfin.tracks", skip_all)]
    pub fn tracks(&self, album_id: &str) -> Result<Vec<Item>, Error> {
        let mut items = Vec::new();
        loop {
            let page = self.page(self.items_url(
                "Audio",
                items.len(),
                Some(album_id),
                "ParentIndexNumber,IndexNumber,SortName",
                "Ascending",
            ))?;
            let done =
                page.items.is_empty() || items.len() + page.items.len() >= page.total_record_count;
            items.extend(page.items);
            if done {
                break;
            }
        }
        items.sort_by_key(|item| {
            (
                item.parent_index_number.unwrap_or(1),
                item.index_number.unwrap_or(u32::MAX),
            )
        });
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };

    struct Request {
        line: String,
        headers: String,
        body: String,
    }

    impl Request {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim())
        }
    }

    fn response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn server(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/jellyfin/", listener.local_addr().unwrap());
        let task = thread::spawn(move || {
            responses
                .into_iter()
                .map(|response| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(Instant::now() < deadline, "expected another request");
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(e) => panic!("{e}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let mut headers = String::new();
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header == "\r\n" {
                            break;
                        }
                        if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                        headers.push_str(&header);
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    stream.write_all(response.as_bytes()).unwrap();
                    Request {
                        line,
                        headers,
                        body: String::from_utf8(body).unwrap(),
                    }
                })
                .collect()
        });
        (url, task)
    }

    fn client(server: String) -> Client {
        let mut client = Client::restore(Session {
            server,
            user_id: "user-id".into(),
            username: "Listener".into(),
            token: "test-token".into(),
            device_id: "test-device".into(),
        })
        .unwrap();
        client.cover_cache = None;
        client
    }

    fn capture_logs(operation: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct LogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = LogWriter::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, operation);
        let bytes = logs.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn logs_authentication_and_http_failures_without_credentials() {
        let (url, requests) = server(vec![
            response(
                200,
                r#"{"AccessToken":"private-access-token","User":{"Id":"user-id","Name":"Listener"}}"#,
            ),
            response(401, "private-response-body"),
        ]);
        let logs = capture_logs(|| {
            let client = Client::sign_in(&url, "private-username", "private-password").unwrap();
            assert!(matches!(
                client.albums(0, &AlbumQuery::default()),
                Err(Error::Unauthorized)
            ));
        });
        requests.join().unwrap();
        assert!(logs.contains("Jellyfin authentication succeeded"));
        assert!(logs.contains("status=401"));
        assert!(logs.contains("Jellyfin request failed"));
        for secret in [
            "private-username",
            "private-password",
            "private-access-token",
            "private-response-body",
        ] {
            assert!(!logs.contains(secret), "secret appeared in logs: {secret}");
        }
    }

    #[test]
    fn malformed_authentication_logs_json_position_without_response_values() {
        let (url, requests) = server(vec![response(
            200,
            r#"{"AccessToken":"private-access-token","User":"private-response-value"}"#,
        )]);
        let logs = capture_logs(|| {
            assert!(Client::sign_in(&url, "Listener", "private-password").is_err());
        });
        requests.join().unwrap();
        assert!(logs.contains("expected JSON structure"));
        assert!(logs.contains("category=Data"));
        assert!(logs.contains("line=1"));
        for secret in [
            "private-access-token",
            "private-response-value",
            "private-password",
        ] {
            assert!(!logs.contains(secret), "secret appeared in logs: {secret}");
        }
    }

    #[test]
    fn normalizes_base_paths_and_rejects_invalid_server_urls() {
        assert_eq!(
            server_url(" https://music.example.com/jellyfin ")
                .unwrap()
                .as_str(),
            "https://music.example.com/jellyfin/"
        );
        for invalid in [
            "music.example.com",
            "ftp://music.example.com",
            "https://user:secret@example.com",
            "https://example.com/?token=secret",
            "https://example.com/#fragment",
        ] {
            assert!(server_url(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn album_filters_and_sorting_are_sent_on_every_page() {
        let (url, requests) = server(vec![
            response(200, r#"{"Items":[],"TotalRecordCount":250}"#),
            response(200, r#"{"Items":[],"TotalRecordCount":250}"#),
        ]);
        let client = client(url);
        let query = AlbumQuery {
            search: "love & blue".into(),
            sort: AlbumSort::Artist,
            descending: true,
        };
        client.albums(0, &query).unwrap();
        client.albums(100, &query).unwrap();
        for (index, request) in requests.join().unwrap().iter().enumerate() {
            let url = Url::parse(&format!(
                "http://localhost{}",
                request.line.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
            assert_eq!(params["SearchTerm"], "love & blue");
            assert!(!params.contains_key("Years"));
            assert_eq!(params["SortBy"], "AlbumArtist,SortName");
            assert_eq!(params["SortOrder"], "Descending");
            assert_eq!(params["StartIndex"], (index * 100).to_string());
            assert_eq!(params["Limit"], "100");
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
        }
    }

    fn library_page(items: Vec<serde_json::Value>, total: usize) -> String {
        response(
            200,
            &serde_json::json!({"Items": items, "TotalRecordCount": total}).to_string(),
        )
    }

    fn params(request: &Request) -> HashMap<String, String> {
        Url::parse(&format!(
            "http://localhost{}",
            request.line.split_whitespace().nth(1).unwrap()
        ))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
    }

    #[test]
    fn artists_are_authenticated_paged_and_filtered_on_the_server() {
        let artist =
            serde_json::json!({"Id":"performer", "Name":"Guest & Friends", "AlbumCount":2});
        let (url, requests) = server(vec![
            library_page(vec![artist.clone()], 201),
            library_page(vec![artist], 201),
        ]);
        let client = client(url);
        for start in [0, 100] {
            let page = client.artists(start, "Guest & Friends", true).unwrap();
            assert_eq!(page.total_record_count, 201);
            assert_eq!(page.items[0].id, "performer");
        }
        for (index, request) in requests.join().unwrap().iter().enumerate() {
            assert!(request.line.starts_with("GET /jellyfin/Artists?"));
            let params = params(request);
            assert_eq!(params["UserId"], "user-id");
            assert_eq!(params["IncludeItemTypes"], "Audio,MusicAlbum");
            assert_eq!(params["SearchTerm"], "Guest & Friends");
            assert_eq!(params["SortBy"], "SortName");
            assert_eq!(params["SortOrder"], "Descending");
            assert_eq!(params["StartIndex"], (index * PAGE_SIZE).to_string());
            assert_eq!(params["Limit"], PAGE_SIZE.to_string());
            assert_eq!(params["EnableTotalRecordCount"], "true");
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
        }
    }

    #[test]
    fn artist_albums_keep_the_artist_scope_on_every_page_without_affecting_global_albums() {
        let (url, requests) = server(vec![
            library_page(vec![], 250),
            library_page(vec![], 250),
            library_page(vec![], 0),
        ]);
        let client = client(url);
        let query = AlbumQuery {
            search: "Blue & Gold".into(),
            sort: AlbumSort::Year,
            descending: true,
        };
        for start in [0, 100] {
            assert_eq!(
                client
                    .artist_albums(start, "featured-artist", &query)
                    .unwrap()
                    .total_record_count,
                250
            );
        }
        client.albums(0, &AlbumQuery::default()).unwrap();
        let requests = requests.join().unwrap();
        for (index, request) in requests[..2].iter().enumerate() {
            assert!(request.line.starts_with("GET /jellyfin/Items?"));
            let params = params(request);
            assert_eq!(params["ArtistIds"], "featured-artist");
            assert_eq!(params["IncludeItemTypes"], "MusicAlbum");
            assert_eq!(params["UserId"], "user-id");
            assert_eq!(params["Recursive"], "true");
            assert_eq!(params["SearchTerm"], "Blue & Gold");
            assert_eq!(params["SortBy"], "ProductionYear,SortName");
            assert_eq!(params["SortOrder"], "Descending");
            assert_eq!(params["StartIndex"], (index * PAGE_SIZE).to_string());
            assert_eq!(params["Limit"], PAGE_SIZE.to_string());
            assert!(!params.contains_key("ParentId"));
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
        }
        let params = params(&requests[2]);
        assert!(!params.contains_key("ArtistIds"));
        assert!(!params.contains_key("SearchTerm"));
        assert_eq!(params["SortBy"], "SortName");
        assert_eq!(params["SortOrder"], "Ascending");
    }

    #[test]
    fn artist_browsing_distinguishes_rejected_tokens_from_retryable_errors() {
        let (url, requests) = server(vec![response(401, "{}"), response(503, "{}")]);
        let client = client(url);
        assert!(matches!(
            client.artists(0, "", false),
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            client.artist_albums(0, "artist", &AlbumQuery::default()),
            Err(Error::Message(_))
        ));
        let requests = requests.join().unwrap();
        assert!(!params(&requests[0]).contains_key("SearchTerm"));
    }

    #[test]
    fn artist_name_lookup_encodes_the_name_and_retains_the_canonical_id() {
        let (url, requests) = server(vec![response(
            200,
            r#"{"Id":"artist-id", "Name":"AC/DC & Friends?#"}"#,
        )]);
        let artist = client(url).artist_by_name("AC/DC & Friends?#").unwrap();
        assert_eq!(artist.id.as_deref(), Some("artist-id"));
        let requests = requests.join().unwrap();
        let request = &requests[0];
        let url = Url::parse(&format!(
            "http://localhost{}",
            request.line.split_whitespace().nth(1).unwrap()
        ))
        .unwrap();
        assert_eq!(url.path_segments().unwrap().count(), 3);
        assert!(url.path().starts_with("/jellyfin/Artists/AC%2FDC"));
        assert!(url.path().contains("%3F%23"));
        assert_eq!(
            params(request),
            HashMap::from([("UserId".into(), "user-id".into())])
        );
        assert!(
            request
                .header("authorization")
                .unwrap()
                .contains("Token=\"test-token\"")
        );
    }

    #[test]
    fn artist_name_lookup_rejects_invalid_metadata_and_expired_logins() {
        let (url, requests) = server(vec![
            response(200, r#"{"Name":"Unknown"}"#),
            response(401, "{}"),
        ]);
        let client = client(url);
        assert!(matches!(
            client.artist_by_name("Unknown"),
            Err(Error::Message(_))
        ));
        assert!(matches!(
            client.artist_by_name("Unknown"),
            Err(Error::Unauthorized)
        ));
        requests.join().unwrap();
    }

    #[test]
    fn album_search_combines_title_performer_and_album_artist_matches() {
        let title = serde_json::json!({"Id":"title","Name":"Blue skies","AlbumArtist":"Other"});
        let both = serde_json::json!({"Id":"both","Name":"Blue","AlbumArtist":"Blue artist"});
        let guest = serde_json::json!({"Id":"a","Name":"A catalog","AlbumArtist":"Various Artists","Artists":["Blue Guest"]});
        let album_artist =
            serde_json::json!({"Id":"z","Name":"Z catalog","AlbumArtist":"Blue artist"});
        let (url, requests) = server(vec![
            library_page(vec![both.clone(), title.clone()], 2),
            library_page(
                vec![serde_json::json!({"Id":"guest","Name":"Blue Guest"})],
                1,
            ),
            library_page(
                vec![
                    serde_json::json!({"Id":"guest","Name":"Blue Guest"}),
                    serde_json::json!({"Id":"album-artist","Name":"Blue artist"}),
                ],
                2,
            ),
            library_page(vec![guest.clone(), both.clone(), album_artist.clone()], 3),
            library_page(vec![guest, album_artist, both, title], 4),
        ]);
        let client = client(url);
        let query = AlbumQuery {
            search: "blue".into(),
            descending: true,
            ..Default::default()
        };
        let search = client
            .search_albums(&query, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(search.ids, ["title", "both", "z", "a"]);
        let page = client.search_page(0, &search).unwrap();
        assert_eq!(page.total_record_count, 4);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            search.ids
        );
        assert_eq!(page.items[3].artists.as_deref().unwrap(), ["Blue Guest"]);
        let requests = requests.join().unwrap();
        for request in &requests {
            assert_eq!(params(request)["UserId"], "user-id");
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
        }
        assert_eq!(params(&requests[0])["SearchTerm"], "blue");
        assert!(requests[1].line.starts_with("GET /jellyfin/Artists?"));
        assert!(
            requests[2]
                .line
                .starts_with("GET /jellyfin/Artists/AlbumArtists?")
        );
        for request in &requests[1..3] {
            assert_eq!(params(request)["SearchTerm"], "blue");
        }
        let credited = params(&requests[3]);
        assert_eq!(credited["ArtistIds"], "guest,album-artist");
        assert!(!credited.contains_key("SearchTerm"));
        assert!(!credited.contains_key("AlbumArtistIds"));
        assert_eq!(params(&requests[4])["Ids"], "title,both,z,a");
        assert!(!params(&requests[4]).contains_key("SearchTerm"));
    }

    #[test]
    fn combined_queries_retrieve_candidates_then_require_all_words_across_fields() {
        let correct =
            serde_json::json!({"Id":"correct", "Name":"Abbey Road", "AlbumArtist":"The Beatles"});
        let (url, requests) = server(vec![
            library_page(
                vec![
                    serde_json::json!({"Id":"title-only", "Name":"Beatles tribute", "AlbumArtist":"Other"}),
                ],
                1,
            ),
            library_page(
                vec![serde_json::json!({"Id":"beatles", "Name":"The Beatles"})],
                1,
            ),
            library_page(
                vec![serde_json::json!({"Id":"beatles", "Name":"The Beatles"})],
                1,
            ),
            library_page(
                vec![
                    correct.clone(),
                    serde_json::json!({"Id":"artist-only", "Name":"Let It Be", "AlbumArtist":"The Beatles"}),
                ],
                2,
            ),
            library_page(vec![correct], 1),
        ]);
        let client = client(url);
        let query = AlbumQuery {
            search: "abbey road beatles".into(),
            ..Default::default()
        };
        let search = client
            .search_albums(&query, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(search.ids, ["correct"]);
        let page = client.search_page(0, &search).unwrap();
        assert_eq!(page.total_record_count, 1);
        assert_eq!(page.items[0].name, "Abbey Road");
        let requests = requests.join().unwrap();
        for request in &requests[..3] {
            assert_eq!(params(request)["SearchTerm"], "beatles");
        }
        let punctuation = AlbumQuery {
            search: "--- / !!!".into(),
            ..Default::default()
        };
        assert!(
            client
                .search_albums(&punctuation, &AtomicBool::new(false))
                .unwrap()
                .ids
                .is_empty()
        );
    }

    #[test]
    fn search_pagination_keeps_global_order_and_total_when_loading_later_results() {
        let albums = |range: std::ops::Range<usize>| {
            range
                .map(
                    |n| serde_json::json!({"Id":format!("{n:03}"), "Name":format!("Match {n:03}")}),
                )
                .collect()
        };
        let mut later: Vec<_> = albums(100..105);
        later.reverse();
        let (url, requests) = server(vec![
            library_page(albums(0..100), 105),
            library_page(albums(100..105), 105),
            library_page(vec![], 0),
            library_page(vec![], 0),
            library_page(later, 5),
        ]);
        let client = client(url);
        let query = AlbumQuery {
            search: "match".into(),
            ..Default::default()
        };
        let search = client
            .search_albums(&query, &AtomicBool::new(false))
            .unwrap();
        let page = client.search_page(100, &search).unwrap();
        assert_eq!(page.total_record_count, 105);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["100", "101", "102", "103", "104"]
        );
        assert!(client.search_page(200, &search).unwrap().items.is_empty());
        let requests = requests.join().unwrap();
        assert_eq!(params(&requests[1])["StartIndex"], "100");
        assert_eq!(params(&requests[4])["StartIndex"], "0");
        assert_eq!(params(&requests[4])["Ids"], "100,101,102,103,104");
    }

    #[test]
    fn artist_search_reads_every_page_and_batches_credits_without_duplicates() {
        let artists = |range: std::ops::Range<usize>| {
            range.map(|n| serde_json::json!({"Id":format!("artist{n:03}"), "Name":format!("Match {n:03}")})).collect()
        };
        let mut responses = vec![
            library_page(vec![], 0),
            library_page(artists(0..100), 101),
            library_page(artists(100..101), 101),
            library_page(vec![], 0),
        ];
        for id in ["a", "b", "c"] {
            responses.push(library_page(
                vec![
                    serde_json::json!({"Id":"shared","Name":"Z"}),
                    serde_json::json!({"Id":id,"Name":id}),
                ],
                2,
            ));
        }
        let (url, requests) = server(responses);
        let query = AlbumQuery {
            search: "match".into(),
            ..Default::default()
        };
        let search = client(url)
            .search_albums(&query, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(search.ids, ["a", "b", "c", "shared"]);
        let requests = requests.join().unwrap();
        assert_eq!(params(&requests[2])["StartIndex"], "100");
        for (request, count) in requests[4..].iter().zip([50, 50, 1]) {
            assert_eq!(params(request)["ArtistIds"].split(',').count(), count);
        }
    }

    #[test]
    fn search_cancellation_incomplete_results_and_expired_sessions_are_errors() {
        let query = AlbumQuery {
            search: "match".into(),
            ..Default::default()
        };
        assert!(
            client("http://127.0.0.1:9".into())
                .search_albums(&query, &AtomicBool::new(true))
                .is_err()
        );
        let (url, requests) = server(vec![library_page(vec![], 1)]);
        assert!(
            client(url)
                .search_albums(&query, &AtomicBool::new(false))
                .is_err()
        );
        requests.join().unwrap();
        for responses in [
            vec![library_page(vec![], 0), response(401, "expired")],
            vec![
                library_page(vec![], 0),
                library_page(vec![], 0),
                response(401, "expired"),
            ],
        ] {
            let (url, requests) = server(responses);
            assert!(matches!(
                client(url).search_albums(&query, &AtomicBool::new(false)),
                Err(Error::Unauthorized)
            ));
            requests.join().unwrap();
        }
        let (url, requests) = server(vec![library_page(vec![], 0)]);
        assert!(
            client(url)
                .search_page(
                    0,
                    &AlbumSearch {
                        ids: vec!["removed-album".into()]
                    }
                )
                .is_err()
        );
        requests.join().unwrap();
    }

    #[test]
    fn persisted_cover_is_reused_by_a_new_client_without_another_request() {
        let directory = crate::cover_cache::TestDirectory::new();
        let (url, requests) = server(vec![response(200, "persisted cover")]);
        let mut first = client(url.clone());
        first.cover_cache = Some(Arc::new(directory.cache(100)));
        assert_eq!(first.cover("album").unwrap().unwrap(), b"persisted cover");
        drop(first);
        let mut restored = client(url);
        restored.cover_cache = Some(Arc::new(directory.cache(100)));
        assert_eq!(
            restored.cover("album").unwrap().unwrap(),
            b"persisted cover"
        );
        assert_eq!(requests.join().unwrap().len(), 1);
    }

    #[test]
    fn an_unwritable_cache_does_not_prevent_loading_artwork() {
        let directory = crate::cover_cache::TestDirectory::new();
        let blocked = directory.0.join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let (url, requests) = server(vec![response(200, "fresh cover")]);
        let mut client = client(url);
        let cache = crate::cover_cache::TestDirectory(blocked);
        client.cover_cache = Some(Arc::new(cache.cache(100)));
        assert_eq!(client.cover("album").unwrap().unwrap(), b"fresh cover");
        requests.join().unwrap();
    }

    #[test]
    fn loads_album_art_with_authentication_and_handles_missing_art() {
        let (url, requests) = server(vec![
            response(200, "cover bytes"),
            response(404, ""),
            response(401, ""),
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let client = client(url);
        assert_eq!(client.cover("album id").unwrap().unwrap(), b"cover bytes");
        assert!(client.cover("album id").unwrap().is_none());
        assert!(matches!(client.cover("album id"), Err(Error::Unauthorized)));
        assert!(
            client
                .cover("album id")
                .err()
                .unwrap()
                .to_string()
                .contains("redirected")
        );
        for request in requests.join().unwrap() {
            assert_eq!(
                request.line,
                "GET /jellyfin/Items/album%20id/Images/Primary?maxWidth=640&maxHeight=640&format=Png HTTP/1.1\r\n"
            );
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
            assert!(!request.line.contains("test-token"));
        }
    }

    #[test]
    fn streams_original_audio_with_authentication_and_no_token_in_the_url() {
        let (url, requests) = server(vec![response(200, "original audio"), response(401, "")]);
        let client = client(url);
        let mut bytes = Vec::new();
        client
            .audio("track id")
            .unwrap()
            .into_reader()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"original audio");
        assert!(matches!(client.audio("track id"), Err(Error::Unauthorized)));
        for request in requests.join().unwrap() {
            assert_eq!(
                request.line,
                "GET /jellyfin/Audio/track%20id/stream?static=true HTTP/1.1\r\n"
            );
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
            assert!(!request.line.contains("test-token"));
        }
    }

    #[test]
    fn interrupted_audio_body_is_an_error_and_redirects_are_rejected() {
        let (url, requests) = server(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".into(),
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let client = client(url);
        assert!(
            client
                .audio("track")
                .unwrap()
                .into_reader()
                .read_to_end(&mut Vec::new())
                .is_err()
        );
        assert!(
            client
                .audio("track")
                .err()
                .unwrap()
                .to_string()
                .contains("redirected")
        );
        requests.join().unwrap();
    }

    #[test]
    fn audio_ranges_are_authenticated_and_preserve_full_file_length() {
        let (url, requests) = server(vec![
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".into(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef".into(),
            response(200, "abcdef"),
            response(401, ""),
        ]);
        let client = client(url);
        for (start, end, offset, expected) in [
            (0, Some(2), 0, "abc"),
            (3, None, 3, "def"),
            (3, None, 0, "abcdef"),
        ] {
            let stream = client.audio_range("track", start, end).unwrap();
            assert_eq!(stream.offset, offset);
            assert_eq!(stream.length, Some(6));
            assert_eq!(stream.end, Some(if end.is_some() { 3 } else { 6 }));
            let mut bytes = Vec::new();
            stream.body.into_reader().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected.as_bytes());
        }
        assert!(matches!(
            client.audio_range("track", 0, Some(2)),
            Err(Error::Unauthorized)
        ));
        let requests = requests.join().unwrap();
        assert_eq!(requests[0].header("range"), Some("bytes=0-2"));
        assert_eq!(requests[1].header("range"), Some("bytes=3-"));
        for request in requests {
            assert_eq!(request.header("accept-encoding"), Some("identity"));
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
            assert!(!request.line.contains("test-token"));
        }
    }

    #[test]
    fn invalid_or_misaligned_audio_ranges_are_rejected() {
        let cases = [
            ("", 3, None),
            ("bytes 0-2/6", 3, None),
            ("bytes 3-4/6", 3, None),
            ("bytes 0-2/6", 0, Some(1)),
            ("bytes 0-2/*", 0, Some(2)),
            ("bytes 3-2/6", 3, None),
            ("bytes 3-6/6", 3, None),
            ("bytes 3-5/6", 3, None), // Content-Length disagrees with range.
        ];
        let (url, requests) = server(cases.iter().map(|(range, _, _)| format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: {range}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nxx"
        )).collect());
        let client = client(url);
        for (_, start, end) in cases {
            assert!(client.audio_range("track", start, end).is_err());
        }
        requests.join().unwrap();
    }

    #[test]
    fn playback_reports_are_authenticated_ordered_and_continue_after_a_failure() {
        use crate::playback_reporting::{Reporter, Snapshot};
        let (url, requests) = server(vec![
            response(204, ""),
            response(500, "private-response-body"),
            response(204, ""),
            response(204, ""),
            response(204, ""),
            response(204, ""),
            response(204, ""),
        ]);
        let client = client(url);
        let reporter = Reporter::new().unwrap();
        let snapshot = |id: &str, seconds, paused, volume| Snapshot {
            item_id: id.into(),
            position: Duration::from_secs(seconds),
            paused,
            volume,
            can_seek: true,
            queue: vec![id.into(), "up-next".into()],
        };
        reporter.update(client.clone(), snapshot("a", 1, false, 0.5), false);
        reporter.update(client.clone(), snapshot("a", 90, false, 0.5), true);
        reporter.update(client.clone(), snapshot("a", 90, true, 0.), true);
        reporter.update(client.clone(), snapshot("a", 90, false, 0.37), true);
        reporter.stop(Some(Duration::from_secs(91)), false);
        reporter.update(client, snapshot("b", 1, false, 0.37), false);
        reporter.shutdown();
        let requests = requests.join().unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.line.split_whitespace().nth(1).unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "/jellyfin/Sessions/Playing",
                "/jellyfin/Sessions/Playing/Progress",
                "/jellyfin/Sessions/Playing/Progress",
                "/jellyfin/Sessions/Playing/Progress",
                "/jellyfin/Sessions/Playing/Stopped",
                "/jellyfin/Sessions/Playing",
                "/jellyfin/Sessions/Playing/Stopped",
            ]
        );
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .map(|request| {
                assert!(request.line.starts_with("POST "));
                assert!(!request.line.contains("test-token"));
                let auth = request.header("authorization").unwrap();
                assert!(auth.contains("Token=\"test-token\""));
                assert!(auth.contains("DeviceId=\"test-device\""));
                serde_json::from_str(&request.body).unwrap()
            })
            .collect();
        assert_eq!(bodies[0]["ItemId"], "a");
        assert_eq!(bodies[0]["PositionTicks"], 10_000_000);
        assert_eq!(bodies[0]["PlayMethod"], "DirectPlay");
        assert_eq!(bodies[0]["NowPlayingQueue"][1]["Id"], "up-next");
        assert_eq!(bodies[1]["PositionTicks"], 900_000_000);
        assert_eq!(bodies[2]["IsPaused"], true);
        assert_eq!(bodies[2]["IsMuted"], true);
        assert_eq!(bodies[3]["IsPaused"], false);
        assert_eq!(bodies[3]["VolumeLevel"], 37);
        assert_eq!(bodies[4]["PositionTicks"], 910_000_000);
        assert_eq!(bodies[4]["Failed"], false);
        for body in &bodies[..5] {
            assert_eq!(body["PlaySessionId"], bodies[0]["PlaySessionId"]);
        }
        assert_ne!(bodies[5]["PlaySessionId"], bodies[0]["PlaySessionId"]);
        assert_eq!(bodies[6]["ItemId"], "b");
        assert_eq!(bodies[6]["PlaySessionId"], bodies[5]["PlaySessionId"]);
    }

    #[test]
    fn playback_report_redirects_are_not_followed_and_still_allow_shutdown() {
        use crate::playback_reporting::{Reporter, Snapshot};
        let (url, requests) = server(vec![
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            response(204, ""),
        ]);
        let reporter = Reporter::new().unwrap();
        reporter.update(
            client(url),
            Snapshot {
                item_id: "track".into(),
                position: Duration::ZERO,
                paused: false,
                volume: 1.,
                can_seek: true,
                queue: vec![],
            },
            false,
        );
        reporter.shutdown();
        assert_eq!(requests.join().unwrap().len(), 2);
    }

    #[test]
    fn signs_in_and_paginates_albums_without_persisting_password() {
        let (url, requests) = server(vec![
            response(
                200,
                r#"{"AccessToken":"test-token","User":{"Id":"user-id","Name":"Listener"}}"#,
            ),
            response(200, r#"{"Items":[],"TotalRecordCount":150}"#),
        ]);
        let client = Client::sign_in(&url, "Listener", "private-password").unwrap();
        assert_eq!(
            client
                .albums(100, &AlbumQuery::default())
                .unwrap()
                .total_record_count,
            150
        );
        let saved = serde_json::to_string(&client.session).unwrap();
        assert!(!saved.contains("private-password"));
        let requests = requests.join().unwrap();
        assert_eq!(
            requests[0].line,
            "POST /jellyfin/Users/AuthenticateByName HTTP/1.1\r\n"
        );
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["Pw"], "private-password");
        assert!(requests[0].headers.contains("Client=\"Jellyui\""));
        assert!(
            !requests[0]
                .header("authorization")
                .unwrap()
                .contains("Token=")
        );
        let url = Url::parse(&format!(
            "http://localhost{}",
            requests[1].line.split_whitespace().nth(1).unwrap()
        ))
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.path(), "/jellyfin/Items");
        assert_eq!(query["UserId"], "user-id");
        assert_eq!(query["IncludeItemTypes"], "MusicAlbum");
        assert_eq!(query["StartIndex"], "100");
        assert_eq!(query["Limit"], "100");
        let authorization = requests[1].header("authorization").unwrap();
        assert!(authorization.starts_with("MediaBrowser "));
        assert!(
            authorization.contains("Token=\"test-token\""),
            "missing token in Authorization header"
        );
        assert!(requests[1].header("x-emby-token").is_none());
        assert!(!requests[1].line.contains("test-token"));
    }

    #[test]
    fn reads_all_track_pages_and_sorts_by_disc_then_track() {
        let (url, requests) = server(vec![
            response(
                200,
                r#"{"Items":[{"Id":"b","Name":"Second disc","ParentIndexNumber":2,"IndexNumber":1}],"TotalRecordCount":3}"#,
            ),
            response(
                200,
                r#"{"Items":[{"Id":"c","Name":"Second track","IndexNumber":2,"Artists":null},{"Id":"a","Name":"First track","IndexNumber":1}],"TotalRecordCount":3}"#,
            ),
        ]);
        let tracks = client(url).tracks("album id").unwrap();
        assert_eq!(
            tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        assert_eq!(tracks[1].artist(), "Unknown artist");
        let requests = requests.join().unwrap();
        for request in &requests {
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("Token=\"test-token\"")
            );
        }
        assert!(requests[0].line.contains("ParentId=album+id"));
        assert!(requests[1].line.contains("StartIndex=1"));
    }

    #[test]
    fn distinguishes_expired_tokens_from_server_failures() {
        let (url, requests) = server(vec![
            response(401, "secret diagnostic"),
            response(503, "secret diagnostic"),
        ]);
        let client = client(url);
        assert!(matches!(
            client.albums(0, &AlbumQuery::default()),
            Err(Error::Unauthorized)
        ));
        let error = client.albums(0, &AlbumQuery::default()).err().unwrap();
        assert!(matches!(error, Error::Message(_)));
        assert!(!error.to_string().contains("secret diagnostic"));
        requests.join().unwrap();
    }

    #[test]
    fn sign_in_does_not_forward_password_to_a_redirect_target() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap()
        );
        let (url, requests) = server(vec![redirect]);
        let error = Client::sign_in(&url, "Listener", "private-password")
            .err()
            .unwrap();
        assert!(error.to_string().contains("redirected"));
        requests.join().unwrap();
        assert!(matches!(target.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}
