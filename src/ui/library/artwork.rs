use super::{LibraryEvent, LibraryPage};
use crate::domain::Error;
use gpui::{Context, Image, ImageFormat};
use std::sync::Arc;

impl LibraryPage {
    pub(super) fn request_cover(&mut self, id: &str) {
        if !self.covers.contains_key(id)
            && !self.pending_covers.iter().any(|pending| pending == id)
            && !self.loading_covers.iter().any(|loading| loading == id)
        {
            self.pending_covers.push_back(id.to_owned());
        }
    }

    pub(super) fn load_covers(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        while self.loading_covers.len() < 4 {
            let Some(id) = self.pending_covers.pop_front() else {
                break;
            };
            self.loading_covers.push(id.clone());
            let client = client.clone();
            let revision = self.session_revision;
            let requested_id = id.clone();
            let request = cx
                .background_executor()
                .spawn(async move { client.cover(&requested_id) });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |view, cx| {
                    if revision != view.session_revision {
                        return;
                    }
                    view.loading_covers.retain(|loading| loading != &id);
                    if matches!(result, Err(Error::Unauthorized)) {
                        cx.emit(LibraryEvent::Unauthorized);
                        return;
                    }
                    if !view.active || !view.visible_covers.contains(&id) {
                        // Client::cover has already persisted successful downloads.
                        view.load_covers(cx);
                        return;
                    }
                    let image = match result {
                        Ok(bytes) => {
                            bytes.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                        }
                        Err(Error::Unauthorized) => {
                            cx.emit(LibraryEvent::Unauthorized);
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Could not load library artwork");
                            None
                        }
                    };
                    view.covers.insert(id, image);
                    view.load_covers(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }
}
