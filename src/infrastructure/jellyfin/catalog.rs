use super::{Client, authorization, read_response, request_error};
use crate::domain::search::{AlbumSearch, search_words};
use crate::domain::{AlbumQuery, AlbumSort, Artist, Error, Item, PAGE_SIZE, Page};
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use url::Url;

impl Client {
    pub(super) fn items_url(
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

    pub(super) fn page(&self, url: Url) -> Result<Page, Error> {
        self.library_response(url)
    }

    pub(super) fn library_response<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<T, Error> {
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

    pub(super) fn album_page(
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

    pub(super) fn collect_pages(
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
