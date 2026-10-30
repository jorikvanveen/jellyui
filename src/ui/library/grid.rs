use super::{LibraryEvent, LibraryPage};
use crate::domain::LibraryKind;
use crate::{
    domain::Item,
    domain::playback::AlbumAction,
    ui::components::{Tooltip, album_art_placeholder},
    ui::theme,
};
use gpui::{
    AnyElement, App, Bounds, Context, Div, Image, ObjectFit, Pixels, Point, SharedString,
    UniformListDecoration, WeakEntity, Window, div, prelude::*, px, relative, rgb, svg,
};
use std::{ops::Range, sync::Arc};

impl LibraryPage {
    pub(super) fn album_actions(
        &self,
        album: &Item,
        cover: Option<Arc<Image>>,
        width: f32,
        cx: &Context<Self>,
    ) -> Div {
        let play_size = (width * 0.3).clamp(32., 64.);
        let queue_size = (width * 0.2).clamp(24., 40.);
        let mut controls = div()
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .gap(px((width * 0.04).clamp(4., 12.)))
            .child(div().size(px(queue_size)).flex_shrink_0());
        for (action, id, label, icon, size) in [
            (
                AlbumAction::Play,
                "cover-play",
                "Play album",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/icons/filled-lucide/play.svg"
                ))
                .as_slice(),
                play_size,
            ),
            (
                AlbumAction::Queue,
                "cover-queue",
                "Queue album",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/icons/filled-lucide/list-plus.svg"
                ))
                .as_slice(),
                queue_size,
            ),
        ] {
            let album = album.clone();
            let cover = cover.clone();
            controls = controls.child(
                div()
                    .id(id)
                    .focusable()
                    .aria_label(label)
                    .size(px(size))
                    .flex_shrink_0()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(theme::TEXT))
                    .cursor_pointer()
                    .opacity(0.)
                    .group_hover("album-card", |style| style.opacity(1.))
                    .focus(|style| style.opacity(1.).bg(rgb(theme::LAVENDER)))
                    .hover(|style| style.bg(rgb(theme::LAVENDER)))
                    .tooltip(move |_, cx| cx.new(|_| Tooltip(label)).into())
                    .child(
                        svg()
                            .data(icon)
                            .size(px(size * 0.45))
                            .text_color(rgb(theme::BASE)),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(LibraryEvent::Action {
                            action,
                            album: Box::new(album.clone()),
                            cover: cover.clone(),
                        });
                        cx.stop_propagation();
                    })),
            );
        }
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(rgb(theme::BASE).opacity(0.65))
                    .opacity(0.)
                    .group_hover("album-card", |style| style.opacity(1.)),
            )
            .child(controls)
    }

    pub(super) fn rows(
        &self,
        range: Range<usize>,
        columns: usize,
        width: f32,
        cx: &Context<Self>,
    ) -> Vec<Div> {
        let mut rows = Vec::new();
        let kind = self.kind;
        for row in range {
            let mut content = div()
                .flex()
                .gap_5()
                .px_6()
                .h(px(width
                    + if kind == LibraryKind::Artists {
                        64.
                    } else {
                        100.
                    }))
                .w_full();
            let start = row * columns;
            let end = (start + columns).min(self.library.read(cx).pages().total.unwrap_or(0));
            for index in start..end {
                let Some(item) = self.library.read(cx).pages().get(index).cloned() else {
                    content = content.child(library_placeholder(width, kind));
                    continue;
                };
                let cover = self.covers.get(&item.id).cloned().flatten();
                let selected_cover = cover.clone();
                let selected = item.clone();
                let art = div()
                    .relative()
                    .w_full()
                    .aspect_ratio(1.)
                    .rounded_md()
                    .when(kind == LibraryKind::Artists, |art| art.rounded_full())
                    .overflow_hidden()
                    .bg(rgb(theme::SURFACE0))
                    .child(if let Some(cover) = cover {
                        crate::ui::components::artwork::image(cover)
                            .size_full()
                            .when(kind == LibraryKind::Artists, |avatar| avatar.rounded_full())
                            .resize_to_display(if kind == LibraryKind::Artists {
                                ObjectFit::Cover
                            } else {
                                ObjectFit::Contain
                            })
                            .with_fallback(move || library_art_placeholder(kind).into_any_element())
                            .into_any_element()
                    } else {
                        library_art_placeholder(kind).into_any_element()
                    })
                    .when(kind == LibraryKind::Albums, |art| {
                        art.child(self.album_actions(&item, selected_cover.clone(), width, cx))
                    });
                content = content.child(
                    div()
                        .id(SharedString::from(item.id.clone()))
                        .group("album-card")
                        .w(px(width))
                        .min_w_0()
                        .focusable()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::BASE))
                        .cursor_pointer()
                        .focus(|style| style.border_color(rgb(theme::MAUVE)))
                        .child(art)
                        .when(kind == LibraryKind::Artists, |card| card.text_center())
                        .child(div().mt_2().truncate().child(item.name.clone()))
                        .when(kind == LibraryKind::Albums, |card| {
                            card.child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(rgb(theme::SUBTEXT0))
                                    .child(item.artist()),
                            )
                            .child(
                                div().text_sm().text_color(rgb(theme::SUBTEXT0)).child(
                                    item.production_year
                                        .map(|year| year.to_string())
                                        .unwrap_or_default(),
                                ),
                            )
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(LibraryEvent::Open {
                                item: Box::new(selected.clone()),
                                cover: selected_cover.clone(),
                            })
                        })),
                );
            }
            rows.push(content);
        }
        rows
    }
}

pub(super) struct LibraryGridDecoration {
    pub(super) page: WeakEntity<LibraryPage>,
    pub(super) columns: usize,
}

impl UniformListDecoration for LibraryGridDecoration {
    fn compute(
        &self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        offset: Point<Pixels>,
        item_height: Pixels,
        count: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // Decorations run for the actual viewport, unlike the row callback,
        // which GPUI also calls for measuring offscreen rows.
        self.page
            .update(cx, |view, cx| {
                view.viewport(range, self.columns, cx);
                view.scrollbar(
                    Bounds::new(bounds.origin - offset, bounds.size),
                    offset,
                    item_height * count,
                    cx,
                )
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

fn library_art_placeholder(kind: LibraryKind) -> Div {
    if kind == LibraryKind::Albums {
        return album_art_placeholder();
    }
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .data(kind.icon())
                .size(px(64.))
                .text_color(rgb(theme::OVERLAY1)),
        )
}

fn library_placeholder(width: f32, kind: LibraryKind) -> Div {
    div()
        .w(px(width))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .aspect_ratio(1.)
                .rounded_md()
                .when(kind == LibraryKind::Artists, |art| art.rounded_full())
                .bg(rgb(theme::SURFACE0)),
        )
        .child(
            div()
                .mt_2()
                .w(relative(0.8))
                .h(px(15.))
                .rounded_sm()
                .bg(rgb(theme::SURFACE0)),
        )
        .child(
            div()
                .w(relative(0.6))
                .h(px(12.))
                .rounded_sm()
                .bg(rgb(theme::SURFACE0)),
        )
}

pub(super) fn grid_layout(available: f32) -> (usize, f32) {
    let available = available.max(1.);
    let columns = ((available + 20.) / 200.).floor().max(1.) as usize;
    (
        columns,
        (available - (columns - 1) as f32 * 20.) / columns as f32,
    )
}
