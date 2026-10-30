use super::LibraryPage;
use crate::ui::theme;
use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, canvas, div, point, prelude::*, px, rgb,
};

impl LibraryPage {
    pub(super) fn scrollbar(
        &self,
        viewport: Bounds<Pixels>,
        offset: Point<Pixels>,
        content_height: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let height = f32::from(viewport.size.height);
        let content_height = f32::from(content_height);
        let Some((thumb_top, thumb_height)) =
            scrollbar_thumb(height, content_height, -f32::from(offset.y))
        else {
            return div().into_any_element();
        };
        let entity = cx.entity().downgrade();
        div()
            .size_full()
            .relative()
            .child(
                div()
                    .id("album-scrollbar")
                    .absolute()
                    .right_0()
                    .top(-offset.y)
                    .h(viewport.size.height)
                    .w(px(10.))
                    .rounded_full()
                    .bg(rgb(theme::MANTLE))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                            let y = f32::from(event.position.y - viewport.origin.y);
                            let grab = if (thumb_top..=thumb_top + thumb_height).contains(&y) {
                                y - thumb_top
                            } else {
                                thumb_height / 2.
                            };
                            view.scroll_drag = Some(grab);
                            view.drag_scrollbar(y, grab, height, content_height, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(thumb_top))
                            .left(px(2.))
                            .w(px(6.))
                            .h(px(thumb_height))
                            .rounded_full()
                            .bg(rgb(theme::OVERLAY1))
                            .hover(|thumb| thumb.bg(rgb(theme::LAVENDER))),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, _| {
                                let moved = entity.clone();
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, _, cx| {
                                        if phase != DispatchPhase::Capture {
                                            return;
                                        }
                                        let _ = moved.update(cx, |view, cx| {
                                            if let Some(grab) = view.scroll_drag {
                                                if event.dragging() {
                                                    view.drag_scrollbar(
                                                        f32::from(
                                                            event.position.y - viewport.origin.y,
                                                        ),
                                                        grab,
                                                        height,
                                                        content_height,
                                                        cx,
                                                    );
                                                } else {
                                                    view.scroll_drag = None;
                                                }
                                            }
                                        });
                                    },
                                );
                                window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                                    let _ = entity.update(cx, |view, _| view.scroll_drag = None);
                                });
                            },
                        )
                        .size_full(),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn drag_scrollbar(
        &mut self,
        y: f32,
        grab: f32,
        height: f32,
        content_height: f32,
        cx: &mut Context<Self>,
    ) {
        let offset = scrollbar_offset(y - grab, height, content_height);
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-offset)));
        cx.notify();
    }
}

/// Thumb position uses the full content extent, never the number of cached pages.
fn scrollbar_thumb(viewport: f32, content: f32, offset: f32) -> Option<(f32, f32)> {
    if viewport <= 0. || content <= viewport {
        return None;
    }
    let thumb = (viewport * viewport / content).max(24.).min(viewport);
    let top = (offset / (content - viewport)).clamp(0., 1.) * (viewport - thumb);
    Some((top, thumb))
}

fn scrollbar_offset(thumb_top: f32, viewport: f32, content: f32) -> f32 {
    let Some((_, thumb)) = scrollbar_thumb(viewport, content, 0.) else {
        return 0.;
    };
    if viewport <= thumb {
        return 0.;
    }
    (thumb_top / (viewport - thumb)).clamp(0., 1.) * (content - viewport)
}
