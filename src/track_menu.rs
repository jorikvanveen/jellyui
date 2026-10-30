use std::sync::Arc;

use gpui::{
    Context, EventEmitter, FocusHandle, Image, KeyDownEvent, Pixels, Point, Window, anchored,
    deferred, div, prelude::*, px, rgb,
};

use crate::{jellyfin::Item, theme};

#[derive(Clone)]
pub struct TrackTarget {
    pub track: Item,
    pub album: Arc<Item>,
    pub cover: Option<Arc<Image>>,
}

pub struct TrackMenuRequest {
    pub target: TrackTarget,
    pub position: Point<Pixels>,
}

pub struct QueueTrack(pub TrackTarget);

pub struct TrackMenu {
    target: Option<TrackTarget>,
    position: Point<Pixels>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

impl EventEmitter<QueueTrack> for TrackMenu {}

impl TrackMenu {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        cx.on_blur(&focus, window, |view, window, cx| {
            if !view.focus.is_focused(window) {
                view.clear(cx);
            }
        })
        .detach();
        Self {
            target: None,
            position: Point::default(),
            focus,
            previous_focus: None,
        }
    }

    pub fn open(&mut self, request: TrackMenuRequest, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            self.previous_focus = window.focused(cx);
        }
        self.target = Some(request.target);
        self.position = request.position;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.target.take().is_some() {
            self.previous_focus = None;
            cx.notify();
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous_focus = self.previous_focus.take();
        self.clear(cx);
        if self.focus.is_focused(window) {
            if let Some(focus) = previous_focus {
                window.focus(&focus, cx);
            } else {
                window.blur(cx);
            }
        }
    }

    fn queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.target.take() {
            self.dismiss(window, cx);
            cx.emit(QueueTrack(target));
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.dismiss(window, cx),
            "enter" | "space" => self.queue(window, cx),
            "tab" => {
                self.dismiss(window, cx);
                if event.keystroke.modifiers.shift {
                    window.focus_prev(cx);
                } else {
                    window.focus_next(cx);
                }
            }
            "up" | "down" => {}
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
    }
}

impl gpui::Render for TrackMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.target.is_none() {
            return div().absolute().into_any_element();
        }
        let menu = div()
            .id("track-context-menu")
            .track_focus(&self.focus)
            .aria_label("Queue track")
            .occlude()
            .w(px(180.))
            .p_1()
            .rounded_md()
            .bg(rgb(theme::MANTLE))
            .text_color(rgb(theme::TEXT))
            .text_sm()
            .border_1()
            .border_color(rgb(theme::SURFACE1))
            .shadow_lg()
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down_out(cx.listener(|view, _, window, cx| view.dismiss(window, cx)))
            .child(
                div()
                    .id("queue-track")
                    .px_3()
                    .py_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::SURFACE0)))
                    .child("Queue")
                    .on_click(cx.listener(|view, _, window, cx| view.queue(window, cx))),
            );
        deferred(
            anchored()
                .position(self.position)
                .snap_to_window_with_margin(px(8.))
                .child(menu),
        )
        .with_priority(2)
        .into_any_element()
    }
}
