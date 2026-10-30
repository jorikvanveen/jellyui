use crate::{
    application::playback::Playback,
    domain::playback::PlaybackState as State,
    ui::{
        components::track_menu::{TrackMenuRequest, TrackTarget},
        theme,
    },
};
use gpui::{
    AnimationExt, Bounds, Context, DragMoveEvent, Entity, EventEmitter, MouseButton,
    MouseDownEvent, Pixels, Point, ScrollHandle, SpringAnimation, SpringConfig, Window, div,
    prelude::*, px, relative, rgb,
};
const QUEUE_ROW_HEIGHT: f32 = 72.;
const QUEUE_ROW_PITCH: f32 = QUEUE_ROW_HEIGHT + 4.;
const QUEUE_PADDING: f32 = 8.;
const QUEUE_SPRING: SpringConfig = SpringConfig::new(700., 50., 1.);
mod reorder;
use reorder::{QueueDrag, QueueReorder, queue_drop_before, queue_preview_index};
pub struct QueueSidebar {
    playback: Entity<Playback>,
    queue_scroll: ScrollHandle,
    queue_reorder: Option<QueueReorder>,
}
impl EventEmitter<TrackMenuRequest> for QueueSidebar {}
impl QueueSidebar {
    pub fn new(playback: Entity<Playback>, cx: &mut Context<Self>) -> Self {
        cx.observe(&playback, |view, playback, cx| {
            if view
                .queue_reorder
                .is_some_and(|drag| drag.revision != playback.read(cx).queue().revision())
            {
                view.queue_reorder = None;
            }
            cx.notify();
        })
        .detach();
        Self {
            playback,
            queue_scroll: ScrollHandle::default(),
            queue_reorder: None,
        }
    }
    fn move_queue_drag(&mut self, event: &DragMoveEvent<QueueDrag>, cx: &mut Context<Self>) {
        let payload = event.drag(cx);
        if payload.owner != cx.entity_id()
            || payload.revision != self.playback.read(cx).queue().revision()
        {
            return;
        }
        self.update_queue_gap(event.event.position, event.bounds, cx);
    }

    fn update_queue_gap(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(reorder) = self.queue_reorder.as_mut() else {
            return;
        };
        if reorder.revision != self.playback.read(cx).queue().revision() {
            return;
        }
        let old = *reorder;
        reorder.inside = bounds.contains(&position);
        if reorder.inside {
            let y = f32::from(position.y - bounds.origin.y - self.queue_scroll.offset().y)
                - QUEUE_PADDING;
            reorder.before = queue_drop_before(
                reorder.from,
                reorder.before,
                y,
                self.playback.read(cx).queue().tracks.len(),
            );
        } else {
            reorder.before = reorder.from;
        }
        if *reorder != old {
            cx.notify();
        }
    }

    fn queue_view(&mut self, cx: &Context<Self>) -> impl IntoElement + use<> {
        if self.queue_reorder.is_some_and(|reorder| {
            !cx.has_active_drag() || reorder.revision != self.playback.read(cx).queue().revision()
        }) {
            self.queue_reorder = None;
        }
        let playback = self.playback.read(cx);
        let queue = playback.queue();
        let mut content = div().relative().w_full();
        if !queue.tracks.is_empty() {
            content = content.h(px(queue.tracks.len() as f32 * QUEUE_ROW_PITCH - 4.));
        }
        if queue.tracks.is_empty() {
            content = content.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(rgb(theme::SUBTEXT0))
                    .child("Play or queue an album to add tracks."),
            );
        }
        for (index, entry) in queue.tracks.iter().enumerate() {
            let entry_id = entry.id;
            let track = &entry.track;
            let current = index == queue.position && playback.state() != State::Stopped;
            let entity = cx.entity().downgrade();
            let reorder = self.queue_reorder.filter(|reorder| reorder.inside);
            let destination = reorder.map_or(index, |reorder| {
                queue_preview_index(index, reorder.from, reorder.before)
            });
            let lifted = reorder.is_some_and(|reorder| reorder.from == index);
            let row = div()
                .id("queue-track-content")
                .group("queue-row")
                .absolute()
                .left_0()
                .w_full()
                .h(px(QUEUE_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_3()
                .rounded_md()
                .cursor_pointer()
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.playback
                        .update(cx, |player, cx| player.jump_to(entry_id, cx))
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                        if let Some(entry) = view
                            .playback
                            .read(cx)
                            .queue()
                            .tracks
                            .iter()
                            .find(|entry| entry.id == entry_id)
                        {
                            cx.emit(TrackMenuRequest {
                                target: TrackTarget {
                                    track: entry.track.clone(),
                                    album: entry.album.clone(),
                                    cover: None,
                                },
                                position: event.position,
                            });
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                    }),
                )
                .on_drag(
                    QueueDrag {
                        owner: cx.entity_id(),
                        revision: queue.revision(),
                        index,
                        title: track.name.clone(),
                        subtitle: format!("{} · {}", track.artist(), entry.album.name),
                    },
                    move |drag, _, _, cx| {
                        let playback = entity.clone();
                        let _ = entity.update(cx, |view, cx| {
                            view.queue_reorder = Some(QueueReorder {
                                from: drag.index,
                                before: drag.index,
                                revision: drag.revision,
                                inside: true,
                            });
                            cx.notify();
                        });
                        cx.new(|cx| {
                            cx.on_release(move |_, cx| {
                                let _ = playback.update(cx, |view, cx| {
                                    view.queue_reorder = None;
                                    cx.notify();
                                });
                            })
                            .detach();
                            drag.clone()
                        })
                    },
                )
                .when(current, |row| {
                    row.bg(rgb(theme::SURFACE0)).text_color(rgb(theme::MAUVE))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().truncate().child(track.name.clone()))
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(theme::SUBTEXT0))
                                .child(format!("{} · {}", track.artist(), entry.album.name)),
                        ),
                )
                .child(
                    div()
                        .id("remove-queue-entry")
                        .focusable()
                        .aria_label("Remove from queue")
                        .size(px(24.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_lg()
                        .text_color(rgb(theme::SUBTEXT0))
                        .opacity(0.)
                        .group_hover("queue-row", |style| style.opacity(1.))
                        .focus(|style| style.opacity(1.).text_color(rgb(theme::RED)))
                        .hover(|style| style.text_color(rgb(theme::RED)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.playback
                                .update(cx, |player, cx| player.remove_from_queue(entry_id, cx));
                            cx.stop_propagation();
                        }))
                        .child("×"),
                )
                .with_spring(
                    "queue-position",
                    SpringAnimation::new(QUEUE_SPRING)
                        .to(px(destination as f32 * QUEUE_ROW_PITCH))
                        .with_epsilon(0.25),
                    move |row, top| row.top(top - px(index as f32 * QUEUE_ROW_PITCH)),
                )
                .with_spring(
                    "queue-lift",
                    SpringAnimation::new(QUEUE_SPRING).to(if lifted { 0. } else { 1. }),
                    |row, opacity| row.map_element(|row| row.opacity(opacity.clamp(0., 1.))),
                );
            content = content.child(
                div()
                    .id(("queue-track", entry.id))
                    .absolute()
                    .left_0()
                    .w_full()
                    .top(px(index as f32 * QUEUE_ROW_PITCH))
                    .h(px(QUEUE_ROW_HEIGHT))
                    .child(row),
            );
        }
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .when(!queue.tracks.is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .p_4()
                        .border_b_1()
                        .border_color(rgb(theme::SURFACE0))
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(theme::SUBTEXT0))
                                .child(format!("{} tracks", queue.tracks.len())),
                        ),
                )
            })
            .child(
                div()
                    .id("queue-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.queue_scroll)
                    .on_scroll(cx.listener(|view, _, window, cx| {
                        if view.queue_reorder.is_some() && cx.has_active_drag() {
                            cx.defer_in(window, |view, window, cx| {
                                view.update_queue_gap(
                                    window.mouse_position(),
                                    view.queue_scroll.bounds(),
                                    cx,
                                );
                            });
                        }
                    }))
                    .on_drag_move(
                        cx.listener(|view, event: &DragMoveEvent<QueueDrag>, _, cx| {
                            view.move_queue_drag(event, cx)
                        }),
                    )
                    .can_drop({
                        let owner = cx.entity_id();
                        let revision = queue.revision();
                        move |value, _, _| {
                            value.downcast_ref::<QueueDrag>().is_some_and(|drag| {
                                drag.owner == owner && drag.revision == revision
                            })
                        }
                    })
                    .on_drop(cx.listener(|view, drag: &QueueDrag, window, cx| {
                        view.playback.update(cx, |player, cx| {
                            player.poll_events(cx);
                        });
                        view.update_queue_gap(
                            window.mouse_position(),
                            view.queue_scroll.bounds(),
                            cx,
                        );
                        let reorder = view.queue_reorder.take();
                        if drag.owner == cx.entity_id()
                            && drag.revision == view.playback.read(cx).queue().revision()
                            && let Some(reorder) = reorder.filter(|reorder| {
                                reorder.inside && reorder.revision == drag.revision
                            })
                            && view.playback.update(cx, |player, cx| {
                                player.reorder_queue(drag.index, reorder.before, drag.revision, cx)
                            })
                        {
                            tracing::debug!(
                                from = drag.index,
                                before = reorder.before,
                                "Queue reordered"
                            );
                        }
                        cx.notify();
                    }))
                    .child(div().p_2().child(content)),
            )
    }
}
impl Render for QueueSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h_full()
            .w(px(300.))
            .max_w(relative(0.4))
            .min_w_0()
            .flex_shrink_0()
            .bg(rgb(theme::MANTLE))
            .border_l_1()
            .border_color(rgb(theme::SURFACE0))
            .child(self.queue_view(cx))
    }
}
