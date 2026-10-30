use crate::{
    domain::{Error, Item},
    infrastructure::jellyfin::Client,
};
use gpui::{Context, EventEmitter, Image, ImageFormat, Task};
use std::sync::Arc;

pub struct SessionRejected;

/// One album request lifecycle. Leaving the page releases its controller.
pub struct Album {
    client: Client,
    item: Item,
    cover: Option<Arc<Image>>,
    tracks: Vec<Item>,
    busy: bool,
    error: Option<String>,
    revision: u64,
    tracks_task: Option<Task<()>>,
    cover_task: Option<Task<()>>,
}
impl EventEmitter<SessionRejected> for Album {}
impl Album {
    pub fn new(
        client: Client,
        item: Item,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut album = Self {
            client,
            item,
            cover,
            tracks: Vec::new(),
            busy: false,
            error: None,
            revision: 0,
            tracks_task: None,
            cover_task: None,
        };
        album.load(cx);
        album
    }
    pub fn item(&self) -> &Item {
        &self.item
    }
    pub fn cover(&self) -> Option<&Arc<Image>> {
        self.cover.as_ref()
    }
    pub fn tracks(&self) -> &[Item] {
        &self.tracks
    }
    pub fn busy(&self) -> bool {
        self.busy
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        let revision = self.revision;
        self.tracks.clear();
        self.busy = true;
        self.error = None;
        let client = self.client.clone();
        let id = self.item.id.clone();
        let request = cx
            .background_executor()
            .spawn(async move { client.tracks(&id) });
        self.tracks_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |album, cx| {
                if album.revision != revision {
                    return;
                }
                album.busy = false;
                match result {
                    Ok(tracks) => album.tracks = tracks,
                    Err(error) => album.request_failed(error, cx),
                }
                cx.notify();
            });
        }));
        if self.cover.is_none() {
            self.load_cover(cx);
        }
        cx.notify();
    }

    fn request_failed(&mut self, error: Error, cx: &mut Context<Self>) {
        tracing::error!(%error, "Album request failed");
        if matches!(error, Error::Unauthorized) {
            cx.emit(SessionRejected);
        } else {
            self.error = Some(error.to_string());
        }
    }

    fn load_cover(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let id = self.item.id.clone();
        let revision = self.revision;
        let request = cx
            .background_executor()
            .spawn(async move { client.cover(&id) });
        self.cover_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |album, cx| {
                if album.revision != revision {
                    return;
                }
                match result {
                    Ok(bytes) => {
                        album.cover =
                            bytes.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                    }
                    Err(Error::Unauthorized) => cx.emit(SessionRejected),
                    Err(error) => tracing::warn!(%error, "Could not load album artwork"),
                }
                cx.notify();
            });
        }));
    }
}
