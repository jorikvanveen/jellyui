mod album_search;
mod artwork;
mod audio;
mod audio_file;
mod cover_cache;
mod dialog;
mod input;
mod jellyfin;
mod library;
mod library_pages;
mod logging;
mod login;
mod mpris;
mod playback;
mod playback_reporting;
mod queue;
mod settings;
mod theme;
mod track_menu;

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Div, Entity, FocusHandle, FontWeight, Image, ImageFormat, KeyBinding,
    MouseButton, MouseDownEvent, ObjectFit, Pixels, Point, PromptButton, PromptLevel, SharedString,
    Stateful, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, prelude::*, px,
    rgb, size, svg,
};
use jellyfin::{Artist, Client, Error, Item};
use library::{LibraryEvent, LibraryKind, LibraryPage, NAV_WIDTH};
use login::{LoginPage, SignIn, SignedIn};
use playback::{
    Playback, PlaybackNavigation, QueueReplacementRequested, QueueSidebar, SessionRejected,
};
use track_menu::{QueueTrack, TrackMenu, TrackMenuRequest, TrackTarget};

actions!(jellyui, [Tab, BackTab, Quit]);

const APP_ID: &str = "jellyui";

struct Jellyui {
    focus: FocusHandle,
    login: Entity<LoginPage>,
    playback: Entity<Playback>,
    queue_sidebar: Entity<QueueSidebar>,
    track_menu: Entity<TrackMenu>,
    client: Option<Client>,
    session_revision: u64,
    albums: Entity<LibraryPage>,
    artists: Entity<LibraryPage>,
    artist_albums: Entity<LibraryPage>,
    section: LibraryKind,
    artist: Option<Artist>,
    detail_revision: u64,
    album: Option<Item>,
    album_cover: Option<Arc<Image>>,
    tracks: Vec<Item>,
    busy: bool,
    error: Option<String>,
    warning: Option<String>,
}

impl Jellyui {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let login = cx.new(|cx| LoginPage::new(window, cx));
        let playback = cx.new(Playback::new);
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &playback,
            window,
            |view, _, request: &QueueReplacementRequested, window, cx| {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    "Replace the current queue?",
                    Some(&format!(
                        "Playing \"{}\" will discard your current queue.",
                        request.album
                    )),
                    &[
                        PromptButton::cancel("Keep queue"),
                        PromptButton::ok("Replace queue"),
                    ],
                    cx,
                );
                let playback = view.playback.downgrade();
                cx.spawn(async move |_, cx| {
                    let confirmed = matches!(answer.await, Ok(1));
                    let _ = playback.update(cx, |player, cx| {
                        player.confirm_queue_replacement(confirmed, cx)
                    });
                })
                .detach();
            },
        )
        .detach();
        let queue_sidebar = cx.new(|cx| QueueSidebar::new(playback.clone(), cx));
        let track_menu = cx.new(|cx| TrackMenu::new(window, cx));
        cx.subscribe_in(
            &playback,
            window,
            |view, _, request: &TrackMenuRequest, window, cx| {
                view.track_menu.update(cx, |menu, cx| {
                    menu.open(
                        TrackMenuRequest {
                            target: request.target.clone(),
                            position: request.position,
                        },
                        window,
                        cx,
                    )
                });
            },
        )
        .detach();
        cx.subscribe(&track_menu, |view, _, request: &QueueTrack, cx| {
            if let Some(client) = view.client.clone() {
                let target = request.0.clone();
                view.playback.update(cx, |player, cx| {
                    player.enqueue(
                        client,
                        target.album.as_ref().clone(),
                        vec![target.track],
                        target.cover,
                        cx,
                    )
                });
            }
        })
        .detach();
        let albums = cx.new(|cx| LibraryPage::new(LibraryKind::Albums, window, cx));
        let artists = cx.new(|cx| LibraryPage::new(LibraryKind::Artists, window, cx));
        let artist_albums = cx.new(|cx| LibraryPage::new(LibraryKind::Albums, window, cx));
        for page in [&albums, &artist_albums] {
            cx.subscribe(page, |view, _, event: &LibraryEvent, cx| {
                match event {
                    LibraryEvent::Open { item, cover } => {
                        view.open_album((**item).clone(), cover.clone(), cx)
                    }
                    LibraryEvent::Action {
                        action,
                        album,
                        cover,
                    } => {
                        if let Some(client) = view.client.clone() {
                            view.playback.update(cx, |player, cx| {
                                player.load_album(
                                    *action,
                                    client,
                                    (**album).clone(),
                                    cover.clone(),
                                    cx,
                                )
                            });
                        }
                    }
                    LibraryEvent::BackToArtists => view.artist = None,
                    LibraryEvent::Unauthorized => view.request_failed(Error::Unauthorized, cx),
                }
                cx.notify();
            })
            .detach();
        }
        cx.subscribe(&artists, |view, _, event: &LibraryEvent, cx| {
            match event {
                LibraryEvent::Open { item, .. } => view.open_artist(
                    Artist {
                        id: Some(item.id.clone()),
                        name: item.name.clone(),
                    },
                    cx,
                ),
                LibraryEvent::Unauthorized => view.request_failed(Error::Unauthorized, cx),
                LibraryEvent::BackToArtists => {}
                LibraryEvent::Action { .. } => {}
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&playback, |view, _, _: &SessionRejected, cx| {
            view.request_failed(Error::Unauthorized, cx);
            cx.notify();
        })
        .detach();
        cx.subscribe(
            &playback,
            |view, _, event: &PlaybackNavigation, cx| match event {
                PlaybackNavigation::Album { album, cover } => {
                    view.switch_library(LibraryKind::Albums, cx);
                    view.open_album((**album).clone(), cover.clone(), cx);
                }
                PlaybackNavigation::Artist(artist) => view.open_artist(artist.clone(), cx),
            },
        )
        .detach();
        cx.subscribe(&login, |view, _, event: &SignedIn, cx| {
            view.session_revision += 1;
            view.client = Some(event.client.clone());
            view.warning = event.warning.clone();
            view.error = None;
            view.busy = false;
            view.albums
                .update(cx, |page, cx| page.connect(event.client.clone(), cx));
            view.artists
                .update(cx, |page, cx| page.connect(event.client.clone(), cx));
            cx.notify();
        })
        .detach();
        Self {
            focus: cx.focus_handle(),
            login,
            playback,
            queue_sidebar,
            track_menu,
            client: None,
            session_revision: 0,
            albums,
            artists,
            artist_albums,
            section: LibraryKind::Albums,
            artist: None,
            detail_revision: 0,
            album: None,
            album_cover: None,
            tracks: Vec::new(),
            busy: false,
            error: None,
            warning: None,
        }
    }

    fn open_album(&mut self, album: Item, cover: Option<Arc<Image>>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let id = album.id.clone();
        let revision = self.session_revision;
        self.detail_revision += 1;
        let detail_revision = self.detail_revision;
        if cover.is_none() {
            self.load_cover(client.clone(), id.clone(), cx);
        }
        self.album = Some(album);
        self.album_cover = cover;
        self.tracks.clear();
        self.error = None;
        self.busy = true;
        let request = cx
            .background_executor()
            .spawn(async move { client.tracks(&id) });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |view, cx| {
                if view.session_revision != revision || view.detail_revision != detail_revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(tracks) => view.tracks = tracks,
                    Err(error) => view.request_failed(error, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn leave_album(&mut self, cx: &mut Context<Self>) {
        self.detail_revision += 1;
        self.album = None;
        self.album_cover = None;
        self.tracks.clear();
        self.busy = false;
        self.error = None;
        cx.notify();
    }

    fn open_artist(&mut self, artist: Artist, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.leave_album(cx);
        self.section = LibraryKind::Artists;
        if artist.id.as_deref().is_none_or(str::is_empty) {
            self.artist = None;
            let revision = self.detail_revision;
            let request = cx
                .background_executor()
                .spawn(async move { client.artist_by_name(&artist.name) });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |view, cx| {
                    if view.detail_revision != revision {
                        return;
                    }
                    match result {
                        Ok(artist) => view.open_artist(artist, cx),
                        Err(error) => view.request_failed(error, cx),
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        self.artist = Some(artist.clone());
        self.artist_albums
            .update(cx, |page, cx| page.connect_artist(client, artist, cx));
    }

    fn switch_library(&mut self, section: LibraryKind, cx: &mut Context<Self>) {
        self.leave_album(cx);
        self.section = section;
        self.artist = None;
    }

    fn navigation(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w(px(NAV_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .py_3()
            .bg(rgb(theme::MANTLE))
            .border_r_1()
            .border_color(rgb(theme::SURFACE0))
            .children(
                [LibraryKind::Albums, LibraryKind::Artists]
                    .into_iter()
                    .map(|section| {
                        let selected = section == self.section;
                        let color = if selected {
                            theme::MAUVE
                        } else {
                            theme::SUBTEXT0
                        };
                        div()
                            .id(section.title())
                            .focusable()
                            .aria_label(section.title())
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |button| button.bg(rgb(theme::SURFACE0)))
                            .hover(|style| style.bg(rgb(theme::SURFACE0)))
                            .focus(|style| style.bg(rgb(theme::SURFACE1)))
                            .tooltip(move |_, cx| cx.new(|_| Tooltip(section.title())).into())
                            .child(
                                svg()
                                    .data(section.icon())
                                    .size(px(20.))
                                    .text_color(rgb(color)),
                            )
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.switch_library(section, cx)),
                            )
                    }),
            )
    }

    fn request_failed(&mut self, error: Error, cx: &mut Context<Self>) {
        tracing::error!(error = %error, "Library request failed");
        if matches!(error, Error::Unauthorized) {
            tracing::warn!("Saved login rejected; returning to sign-in");
            self.session_revision += 1;
            self.client = None;
            self.playback.update(cx, |player, cx| player.clear(cx));
            self.albums.update(cx, |page, cx| page.clear(cx));
            self.artists.update(cx, |page, cx| page.clear(cx));
            self.artist_albums.update(cx, |page, cx| page.clear(cx));
            self.track_menu.update(cx, |menu, cx| menu.clear(cx));
            self.detail_revision += 1;
            self.section = LibraryKind::Albums;
            self.artist = None;
            self.album = None;
            self.album_cover = None;
            self.tracks.clear();
            self.busy = false;
            self.error = None;
            self.warning = None;
            self.login
                .update(cx, |login, cx| login.reject_session(error.to_string(), cx));
        } else {
            self.error = Some(error.to_string());
        }
    }

    fn play_album(&mut self, start: usize, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(client), Some(album)) = (self.client.clone(), self.album.clone()) else {
            return;
        };
        self.playback.update(cx, |player, cx| {
            player.play_album(
                client,
                album,
                self.tracks.clone(),
                start,
                self.album_cover.clone(),
                cx,
            )
        });
    }

    fn queue_album(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(client), Some(album)) = (self.client.clone(), self.album.clone()) else {
            return;
        };
        self.playback.update(cx, |player, cx| {
            player.enqueue(
                client,
                album,
                self.tracks.clone(),
                self.album_cover.clone(),
                cx,
            )
        });
    }

    fn open_track_menu(
        &self,
        index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(track), Some(album)) = (self.tracks.get(index), self.album.as_ref()) else {
            return;
        };
        let request = TrackMenuRequest {
            target: TrackTarget {
                track: track.clone(),
                album: Arc::new(album.clone()),
                cover: self.album_cover.clone(),
            },
            position,
        };
        self.track_menu
            .update(cx, |menu, cx| menu.open(request, window, cx));
    }

    fn load_cover(&self, client: Client, id: String, cx: &mut Context<Self>) {
        let revision = self.session_revision;
        let detail_revision = self.detail_revision;
        let requested_id = id.clone();
        let request = cx
            .background_executor()
            .spawn(async move { client.cover(&requested_id) });
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |view, cx| {
                if view.session_revision != revision
                    || view.detail_revision != detail_revision
                    || view.album.as_ref().is_none_or(|album| album.id != id)
                {
                    return;
                }
                match result {
                    Ok(bytes) => {
                        view.album_cover =
                            bytes.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                    }
                    Err(Error::Unauthorized) => view.request_failed(Error::Unauthorized, cx),
                    Err(error) => tracing::warn!(%error, "Could not load album artwork"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn album_artist_links(&self, album: &Item, cx: &Context<Self>) -> Div {
        let mut artists = album
            .album_artists
            .as_ref()
            .filter(|artists| !artists.is_empty())
            .cloned()
            .unwrap_or_else(|| playback::artist_credits(album, album));
        artists.retain(|artist| !artist.name.trim().is_empty());
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .text_lg()
            .text_color(rgb(theme::SUBTEXT1));
        if artists.is_empty() {
            return row.child("Unknown artist");
        }
        for (index, artist) in artists.into_iter().enumerate() {
            if index > 0 {
                row = row.child(div().child(",\u{a0}"));
            }
            row = row.child(
                div()
                    .id(("album-artist", index))
                    .focusable()
                    .min_w_0()
                    .max_w_full()
                    .truncate()
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme::MAUVE)).underline())
                    .focus(|style| style.text_color(rgb(theme::MAUVE)).underline())
                    .child(artist.name.clone())
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.open_artist(artist.clone(), cx)),
                    ),
            );
        }
        row
    }

    fn album_header(&self, album: &Item, cx: &Context<Self>) -> impl IntoElement {
        let mut metadata = Vec::new();
        if let Some(date) = album
            .premiere_date
            .as_deref()
            .and_then(|date| date.split('T').next())
            .filter(|date| !date.is_empty())
        {
            metadata.push(date.to_owned());
        } else if let Some(year) = album.production_year {
            metadata.push(year.to_string());
        }
        let count = if self.busy {
            album.child_count
        } else {
            Some(self.tracks.len())
        };
        if let Some(count) = count {
            metadata.push(format!(
                "{count} {}",
                if count == 1 { "track" } else { "tracks" }
            ));
        }
        let ticks = if self.tracks.is_empty() {
            album.run_time_ticks
        } else {
            self.tracks.iter().try_fold(0u64, |total, track| {
                total.checked_add(track.run_time_ticks?)
            })
        };
        if let Some(ticks) = ticks {
            let seconds = ticks / 10_000_000;
            metadata.push(if seconds >= 3600 {
                format!(
                    "{}h {}m {}s",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                )
            } else {
                format!("{}m {}s", seconds / 60, seconds % 60)
            });
        }
        let artwork = div()
            .w(px(280.))
            .max_w_full()
            .aspect_ratio(1.)
            .flex_shrink_0()
            .rounded_lg()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE0))
            .child(if let Some(cover) = &self.album_cover {
                crate::artwork::image(cover.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .with_fallback(|| album_art_placeholder().into_any_element())
                    .into_any_element()
            } else {
                album_art_placeholder().into_any_element()
            });
        let disabled = self.busy || self.tracks.is_empty();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_6()
            .w_full()
            .pb_6()
            .child(artwork)
            .child(
                div()
                    .flex_1()
                    .min_w(px(240.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_4()
                    .child(
                        div()
                            .w_full()
                            .text_size(px(44.))
                            .line_height(gpui::relative(1.1))
                            .font_weight(FontWeight::BOLD)
                            .child(album.name.clone()),
                    )
                    .when(!metadata.is_empty(), |details| {
                        details.child(
                            div()
                                .text_color(rgb(theme::SUBTEXT0))
                                .child(metadata.join(" · ")),
                        )
                    })
                    .child(self.album_artist_links(album, cx))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .mt_2()
                            .child(
                                album_button("play-album", "Play", disabled, true)
                                    .on_click(cx.listener(|view, _, _, cx| view.play_album(0, cx))),
                            )
                            .child(
                                album_button("queue-album", "Queue", disabled, false)
                                    .on_click(cx.listener(|view, _, _, cx| view.queue_album(cx))),
                            ),
                    ),
            )
    }

    fn library(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut content = div().flex().flex_col().gap_4().w_full();
        if let Some(album) = &self.album {
            content = content
                .child(
                    div()
                        .id("back")
                        .focusable()
                        .flex()
                        .items_center()
                        .self_start()
                        .gap_2()
                        .cursor_pointer()
                        .text_color(rgb(theme::SUBTEXT0))
                        .focus(|style| style.text_color(rgb(theme::MAUVE)))
                        .hover(|style| style.text_color(rgb(theme::MAUVE)))
                        .child(div().text_xl().child("←"))
                        .child(
                            self.artist
                                .as_ref()
                                .map(|artist| format!("Back to {}", artist.name))
                                .unwrap_or_else(|| "Back to albums".into()),
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.leave_album(cx);
                        })),
                )
                .child(self.album_header(album, cx));
            let mut track_list = div().flex().flex_col().gap_2().w_full();
            for (index, track) in self.tracks.iter().enumerate() {
                let current = self.playback.read(cx).is_current_track(&track.id);
                if index > 0 {
                    track_list = track_list.child(div().h(px(1.)).mx_3().bg(rgb(theme::SURFACE0)));
                }
                track_list = track_list.child(
                    div()
                        .id(SharedString::from(track.id.clone()))
                        .focusable()
                        .tab_stop(!self.busy)
                        .flex()
                        .gap_4()
                        .items_center()
                        .px_3()
                        .py_3()
                        .rounded_md()
                        .when(current, |row| {
                            row.bg(rgb(theme::SURFACE0)).text_color(rgb(theme::MAUVE))
                        })
                        .focus(|style| style.bg(rgb(theme::SURFACE0).opacity(0.55)))
                        .hover(|style| style.bg(rgb(theme::SURFACE0).opacity(0.35)))
                        .cursor_pointer()
                        .on_click(cx.listener(move |view, _, _, cx| view.play_album(index, cx)))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                                view.open_track_menu(index, event.position, window, cx);
                                window.prevent_default();
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .w(px(50.))
                                .text_color(rgb(if current {
                                    theme::MAUVE
                                } else {
                                    theme::SUBTEXT0
                                }))
                                .child(format!(
                                    "{}.{}",
                                    track.parent_index_number.unwrap_or(1),
                                    track
                                        .index_number
                                        .map(|n| n.to_string())
                                        .unwrap_or_else(|| "–".into())
                                )),
                        )
                        .child(div().flex_1().child(track.name.clone()))
                        .child(
                            div()
                                .text_color(rgb(if current {
                                    theme::MAUVE
                                } else {
                                    theme::SUBTEXT0
                                }))
                                .child(duration(track.run_time_ticks)),
                        ),
                );
            }
            if !self.tracks.is_empty() {
                content = content.child(track_list);
            }
            if !self.busy && self.error.is_none() && self.tracks.is_empty() {
                content = content.child("This album has no tracks available to your account.");
            }
        }
        if self.busy {
            content = content.child(div().text_color(rgb(theme::SUBTEXT0)).child("Loading…"));
        } else if self.error.is_some() {
            content = content.child(button("retry", "Retry", false, theme::SURFACE2).on_click(
                cx.listener(|view, _, _, cx| {
                    if let Some(album) = view.album.clone() {
                        view.open_album(album, view.album_cover.clone(), cx);
                    }
                }),
            ));
        }
        content
    }
}

fn album_art_placeholder() -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(64.))
        .text_color(rgb(theme::OVERLAY1))
        .child("♪")
}

fn album_button(
    id: &'static str,
    label: &'static str,
    disabled: bool,
    primary: bool,
) -> Stateful<Div> {
    let hover = if primary {
        theme::LAVENDER
    } else {
        theme::SURFACE1
    };
    div()
        .id(id)
        .focusable()
        .tab_stop(!disabled)
        .flex()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .py_3()
        .rounded_full()
        .bg(rgb(if primary {
            theme::TEXT
        } else {
            theme::SURFACE0
        }))
        .text_color(rgb(if primary { theme::BASE } else { theme::TEXT }))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .focus(move |style| style.bg(rgb(hover)))
        .when(disabled, |button| button.opacity(0.5))
        .when(!disabled, |button| {
            button.hover(move |style| style.bg(rgb(hover)))
        })
        .when(primary, |button| button.child("▶"))
        .child(label)
}

fn button(
    id: &'static str,
    label: &'static str,
    disabled: bool,
    hover_color: u32,
) -> Stateful<Div> {
    div()
        .id(id)
        .focusable()
        .tab_stop(!disabled)
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(theme::SURFACE1))
        .border_1()
        .border_color(rgb(theme::SURFACE1))
        .focus(|style| style.border_color(rgb(theme::MAUVE)))
        .cursor_pointer()
        .when(disabled, |div| div.opacity(0.5))
        .when(!disabled, |div| {
            div.hover(|style| style.bg(rgb(hover_color)))
        })
        .child(label)
}

fn duration(ticks: Option<u64>) -> String {
    match ticks {
        Some(ticks) => {
            let seconds = ticks / 10_000_000;
            format!("{}:{:02}", seconds / 60, seconds % 60)
        }
        None => String::new(),
    }
}

impl Render for Jellyui {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let overview = self.client.is_some() && self.album.is_none();
        for (page, active) in [
            (
                self.albums.clone(),
                overview && self.section == LibraryKind::Albums,
            ),
            (
                self.artists.clone(),
                overview && self.section == LibraryKind::Artists && self.artist.is_none(),
            ),
            (
                self.artist_albums.clone(),
                overview && self.section == LibraryKind::Artists && self.artist.is_some(),
            ),
        ] {
            page.update(cx, |page, cx| page.set_active(active, cx));
        }
        let content = if self.client.is_some() && self.album.is_none() {
            match self.section {
                LibraryKind::Albums => self.albums.clone(),
                LibraryKind::Artists if self.artist.is_some() => self.artist_albums.clone(),
                LibraryKind::Artists => self.artists.clone(),
            }
            .into_any_element()
        } else {
            div()
                .id("content")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().p_8().child(if self.client.is_some() {
                    self.library(cx).into_any_element()
                } else {
                    self.login.clone().into_any_element()
                }))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .font_family("sans-serif")
            .text_size(px(15.))
            .key_context("Jellyui")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Tab, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &BackTab, window, cx| window.focus_prev(cx)))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.client.is_some(), |body| {
                        body.child(self.navigation(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .when_some(self.warning.clone(), |body, warning| {
                                body.child(
                                    div()
                                        .px_6()
                                        .py_3()
                                        .text_color(rgb(theme::YELLOW))
                                        .child(warning),
                                )
                            })
                            .when_some(self.error.clone(), |body, error| {
                                body.child(
                                    div().px_6().py_3().text_color(rgb(theme::RED)).child(error),
                                )
                            })
                            .child(content),
                    )
                    .when(self.client.is_some(), |body| {
                        body.child(self.queue_sidebar.clone())
                    }),
            )
            .when(self.client.is_some(), |view| {
                view.child(self.playback.clone())
            })
            .child(self.track_menu.clone())
    }
}

pub(crate) struct Tooltip(pub &'static str);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(theme::SURFACE0))
            .text_color(rgb(theme::TEXT))
            .text_xs()
            .child(self.0)
    }
}

fn main() {
    logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Starting Jellyui");
    gpui_platform::application().run(|cx: &mut App| {
        dialog::init(cx);
        input::bind_keys(cx);
        cx.bind_keys([
            KeyBinding::new("enter", SignIn, Some("Login")),
            KeyBinding::new("tab", Tab, None),
            KeyBinding::new("shift-tab", BackTab, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                tracing::info!("Last window closed; quitting");
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(560.), px(480.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Jellyui".into()),
                    ..Default::default()
                }),
                app_id: Some(APP_ID.into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Jellyui::new(window, cx)),
        )
        .expect("could not open the Jellyui window");
        tracing::info!("Application window opened");
        cx.activate(true);
    });
    tracing::info!("Jellyui stopped");
}
