use super::*;
use crate::domain::search::AlbumSearch;
use crate::domain::{AlbumQuery, AlbumSort, PAGE_SIZE};
use std::{collections::HashMap, sync::atomic::AtomicBool};
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
    let artist = serde_json::json!({"Id":"performer", "Name":"Guest & Friends", "AlbumCount":2});
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
    let album_artist = serde_json::json!({"Id":"z","Name":"Z catalog","AlbumArtist":"Blue artist"});
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
            .map(|n| serde_json::json!({"Id":format!("{n:03}"), "Name":format!("Match {n:03}")}))
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
    let directory = crate::infrastructure::cache::covers::TestDirectory::new();
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
    let directory = crate::infrastructure::cache::covers::TestDirectory::new();
    let blocked = directory.0.join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let (url, requests) = server(vec![response(200, "fresh cover")]);
    let mut client = client(url);
    let cache = crate::infrastructure::cache::covers::TestDirectory(blocked);
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
    use crate::application::reporting::{Reporter, Snapshot};
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
    use crate::application::reporting::{Reporter, Snapshot};
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
