use crate::{
    domain::{
        AlbumQuery, Artist, Error, LibraryKind, PAGE_SIZE,
        pages::{LibraryPages, needed_pages},
        search::AlbumSearch,
    },
    infrastructure::jellyfin::Client,
};
use gpui::{Context, EventEmitter};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct SessionRejected;

/// Owns paged metadata requests. Views supply a query and visible item range.
pub struct Library {
    kind: LibraryKind,
    artist: Option<Artist>,
    client: Option<Client>,
    query: AlbumQuery,
    pages: LibraryPages,
    wanted_pages: Vec<usize>,
    loading_pages: HashSet<usize>,
    page_errors: HashMap<usize, String>,
    search_results: Option<Arc<AlbumSearch>>,
    search_cancelled: Arc<AtomicBool>,
    query_revision: u64,
}
impl EventEmitter<SessionRejected> for Library {}
impl Library {
    pub fn new(kind: LibraryKind) -> Self {
        Self {
            kind,
            artist: None,
            client: None,
            query: AlbumQuery::default(),
            pages: LibraryPages::default(),
            wanted_pages: vec![0],
            loading_pages: HashSet::new(),
            page_errors: HashMap::new(),
            search_results: None,
            search_cancelled: Arc::default(),
            query_revision: 0,
        }
    }
    pub fn pages(&self) -> &LibraryPages {
        &self.pages
    }
    pub fn error(&self) -> Option<&str> {
        self.wanted_pages
            .iter()
            .find_map(|page| self.page_errors.get(page))
            .map(String::as_str)
    }
    pub fn connect(
        &mut self,
        client: Client,
        artist: Option<Artist>,
        query: AlbumQuery,
        cx: &mut Context<Self>,
    ) {
        self.client = Some(client);
        self.artist = artist;
        self.apply(query, cx);
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        self.artist = None;
        self.reset_pages();
        cx.notify();
    }
    pub fn apply(&mut self, query: AlbumQuery, cx: &mut Context<Self>) {
        self.query = query;
        self.reset_pages();
        self.load(cx);
        cx.notify();
    }
    pub fn viewport(&mut self, items: Range<usize>, cx: &mut Context<Self>) {
        let wanted = needed_pages(items, self.pages.total.unwrap_or(0));
        if wanted != self.wanted_pages {
            self.wanted_pages = wanted;
            cx.notify();
        }
        for &page in &self.wanted_pages {
            self.pages.touch(page);
        }
        self.load(cx);
    }
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if !self.query.search.is_empty() {
            self.reset_pages();
        }
        for page in &self.wanted_pages {
            self.page_errors.remove(page);
        }
        self.load(cx);
        cx.notify();
    }
    fn reset_pages(&mut self) {
        self.search_cancelled.store(true, Ordering::Relaxed);
        self.search_cancelled = Arc::default();
        self.search_results = None;
        self.pages = LibraryPages::default();
        self.wanted_pages = vec![0];
        self.loading_pages.clear();
        self.page_errors.clear();
        self.query_revision = self.query_revision.wrapping_add(1);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.kind == LibraryKind::Albums
            && self.artist.is_none()
            && !self.query.search.is_empty()
            && self.search_results.is_none()
        {
            if !self.loading_pages.contains(&0) && !self.page_errors.contains_key(&0) {
                self.loading_pages.insert(0);
                let query = self.query.clone();
                let cancelled = self.search_cancelled.clone();
                let revision = self.query_revision;
                let request = cx
                    .background_executor()
                    .spawn(async move { client.search_albums(&query, &cancelled) });
                cx.spawn(async move |this, cx| {
                    let result = request.await;
                    let _ = this.update(cx, |view, cx| {
                        if revision != view.query_revision {
                            return;
                        }
                        view.loading_pages.remove(&0);
                        match result {
                            Ok(search) => {
                                tracing::debug!(total = search.ids.len(), "Album search prepared");
                                view.pages.total = Some(search.ids.len());
                                view.search_results = Some(Arc::new(search));
                                view.load(cx);
                            }
                            Err(Error::Unauthorized) => cx.emit(SessionRejected),
                            Err(error) => {
                                tracing::error!(%error, "Album search failed");
                                view.page_errors.insert(0, error.to_string());
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            return;
        }
        for page in self.wanted_pages.clone() {
            if self.loading_pages.len() >= 2 {
                break;
            }
            if self.pages.contains(page)
                || self.loading_pages.contains(&page)
                || self.page_errors.contains_key(&page)
                || self
                    .pages
                    .total
                    .is_some_and(|total| page * PAGE_SIZE >= total)
            {
                continue;
            }
            self.loading_pages.insert(page);
            let client = client.clone();
            let query = self.query.clone();
            let kind = self.kind;
            let artist = self.artist.as_ref().and_then(|artist| artist.id.clone());
            let search = self.search_results.clone();
            let revision = self.query_revision;
            let request = cx.background_executor().spawn(async move {
                if kind == LibraryKind::Artists {
                    client.artists(page * PAGE_SIZE, &query.search, query.descending)
                } else if let Some(artist) = artist {
                    client.artist_albums(page * PAGE_SIZE, &artist, &query)
                } else if let Some(search) = search {
                    client.search_page(page * PAGE_SIZE, &search)
                } else {
                    client.albums(page * PAGE_SIZE, &query)
                }
            });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |view, cx| {
                    if revision != view.query_revision {
                        return;
                    }
                    view.loading_pages.remove(&page);
                    match result {
                        Ok(result) => {
                            tracing::debug!(
                                page,
                                loaded = result.items.len(),
                                total = result.total_record_count,
                                "Library page loaded"
                            );
                            if let Err(error) = view.pages.insert(page, result) {
                                view.page_errors.insert(page, error.into());
                            }
                        }
                        Err(Error::Unauthorized) => {
                            cx.emit(SessionRejected);
                            return;
                        }
                        Err(error) => {
                            tracing::error!(page, %error, "Library request failed");
                            view.page_errors.insert(page, error.to_string());
                        }
                    }
                    view.load(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        self.search_cancelled.store(true, Ordering::Relaxed);
    }
}
