mod navigation;
mod view;

use crate::application::playback::{Playback, QueueReplacementRequested, SessionRejected};
use crate::domain::{Artist, Error};
use crate::infrastructure::jellyfin::Client;
use crate::ui::components::track_menu::{QueueTrack, TrackMenu, TrackMenuRequest};
use crate::ui::library::{LibraryEvent, LibraryKind, LibraryPage};
use crate::ui::login::LoginPage;
use crate::ui::{
    album::AlbumPage,
    player::{PlaybackNavigation, Player},
    queue::QueueSidebar,
};
use gpui::{Context, Entity, FocusHandle, PromptButton, PromptLevel, Window, prelude::*};

use crate::application::session::{Session, SignedIn};

pub(crate) struct Jellyui {
    session: Entity<Session>,
    focus: FocusHandle,
    login: Entity<LoginPage>,
    playback: Entity<Playback>,
    player: Entity<Player>,
    queue_sidebar: Entity<QueueSidebar>,
    track_menu: Entity<TrackMenu>,
    window: gpui::AnyWindowHandle,
    client: Option<Client>,
    albums: Entity<LibraryPage>,
    artists: Entity<LibraryPage>,
    artist_albums: Entity<LibraryPage>,
    section: LibraryKind,
    artist: Option<Artist>,
    detail_revision: u64,
    album: Option<Entity<AlbumPage>>,
    error: Option<String>,
    warning: Option<String>,
}

impl Jellyui {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session = cx.new(Session::new);
        let login = cx.new(|cx| LoginPage::new(session.clone(), window, cx));
        let playback = cx.new(Playback::new);
        let player = cx.new(|cx| Player::new(playback.clone(), cx));
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
            &queue_sidebar,
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
            let target = request.0.clone();
            view.playback.update(cx, |player, cx| {
                player.enqueue(
                    target.album.as_ref().clone(),
                    vec![target.track],
                    target.cover,
                    cx,
                )
            });
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
                        view.playback.update(cx, |player, cx| {
                            player.load_album(*action, (**album).clone(), cover.clone(), cx)
                        });
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
            &player,
            |view, _, event: &PlaybackNavigation, cx| match event {
                PlaybackNavigation::Album { album, cover } => {
                    view.switch_library(LibraryKind::Albums, cx);
                    view.open_album((**album).clone(), cover.clone(), cx);
                }
                PlaybackNavigation::Artist(artist) => view.open_artist(artist.clone(), cx),
            },
        )
        .detach();
        cx.subscribe(&session, |view, _, event: &SignedIn, cx| {
            view.client = Some(event.client.clone());
            view.playback
                .update(cx, |player, cx| player.connect(event.client.clone(), cx));
            view.warning = event.warning.clone();
            view.error = None;
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
            session,
            playback,
            player,
            queue_sidebar,
            track_menu,
            window: window.window_handle(),
            client: None,
            albums,
            artists,
            artist_albums,
            section: LibraryKind::Albums,
            artist: None,
            detail_revision: 0,
            album: None,
            error: None,
            warning: None,
        }
    }

    fn request_failed(&mut self, error: Error, cx: &mut Context<Self>) {
        tracing::error!(error = %error, "Library request failed");
        if matches!(error, Error::Unauthorized) {
            tracing::warn!("Saved login rejected; returning to sign-in");
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
            self.error = None;
            self.warning = None;
            self.session.update(cx, |session, cx| {
                session.reject_session(error.to_string(), cx)
            });
        } else {
            self.error = Some(error.to_string());
        }
    }
}
