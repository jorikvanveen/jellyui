use super::{Playback, SessionRejected};
use crate::{domain::Error, infrastructure::jellyfin::Client};
use gpui::{Context, Image, ImageFormat};
use std::sync::Arc;

impl Playback {
    pub(super) fn reset_cover(&mut self, cover: Option<Arc<Image>>, cx: &mut Context<Self>) {
        self.cover_task = None;
        self.cover = cover;
        self.cover_album_id = self.queue.current_album().map(|album| album.id.clone());
        if self.cover.is_none()
            && let Some(client) = self.client.clone()
        {
            self.load_cover(client, cx);
        }
    }

    pub(super) fn load_cover(&mut self, client: Client, cx: &mut Context<Self>) {
        let Some(album) = self.queue.current_album() else {
            return;
        };
        let id = album.id.clone();
        let requested_id = id.clone();
        let request = cx
            .background_executor()
            .spawn(async move { client.cover(&requested_id) });
        self.cover_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |view, cx| {
                if view
                    .queue
                    .current_album()
                    .is_none_or(|album| album.id != id)
                {
                    return;
                }
                match result {
                    Ok(bytes) => {
                        view.cover =
                            bytes.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                    }
                    Err(Error::Unauthorized) => cx.emit(SessionRejected),
                    Err(error) => tracing::warn!(%error, "Could not load player artwork"),
                }
                cx.notify();
            });
        }));
    }
}
