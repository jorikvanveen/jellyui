use super::{Player, TIMELINE_HEIGHT};
use crate::ui::theme;
use gpui::{
    Bounds, Context, DispatchPhase, Div, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Stateful, canvas, div, prelude::*, px, relative, rgb,
};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Slider {
    Seek,
    Volume,
}
impl Player {
    fn slider_bounds(&self, slider: Slider) -> Bounds<Pixels> {
        match slider {
            Slider::Seek => self.seek_bounds,
            Slider::Volume => self.volume_bounds,
        }
    }

    fn slider_dragging(&self, slider: Slider) -> bool {
        match slider {
            Slider::Seek => self.seek_drag.is_some(),
            Slider::Volume => self.volume_dragging,
        }
    }

    fn cancel_slider_drag(&mut self, slider: Slider, cx: &mut Context<Self>) {
        match slider {
            Slider::Seek => self.seek_drag = None,
            Slider::Volume => {
                self.volume_dragging = false;
                self.playback
                    .update(cx, |player, cx| player.commit_volume(cx));
            }
        }
    }

    fn update_slider(
        &mut self,
        slider: Slider,
        fraction: f32,
        released: bool,
        cx: &mut Context<Self>,
    ) {
        match slider {
            Slider::Seek if released => {
                self.seek_drag = None;
                self.playback
                    .update(cx, |player, cx| player.seek_to(fraction, cx));
            }
            Slider::Seek => self.seek_drag = Some(fraction),
            Slider::Volume => {
                self.playback
                    .update(cx, |player, cx| player.set_volume(fraction, released, cx));
                self.volume_dragging = !released;
            }
        }
        cx.notify();
    }

    pub(super) fn slider(
        &self,
        slider: Slider,
        progress: f32,
        enabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let entity = cx.entity().downgrade();
        div()
            .id(match slider {
                Slider::Seek => "seek-bar",
                Slider::Volume => "volume-bar",
            })
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(TIMELINE_HEIGHT))
            .flex()
            .items_center()
            .when(enabled, |bar| bar.cursor_pointer())
            .when(slider == Slider::Volume, |bar| {
                bar.focusable().tab_stop(true).on_key_down(cx.listener(
                    |view, event: &KeyDownEvent, window, cx| {
                        let level = view.playback.read(cx).volume();
                        let level = match event.keystroke.key.as_str() {
                            "left" | "down" => level - 0.05,
                            "right" | "up" => level + 0.05,
                            "home" => 0.,
                            "end" => 1.,
                            _ => return,
                        };
                        view.playback
                            .update(cx, |player, cx| player.set_volume(level, true, cx));
                        window.prevent_default();
                        cx.stop_propagation();
                        cx.notify();
                    },
                ))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                    if enabled {
                        if let Some(fraction) =
                            pointer_fraction(event.position.x, view.slider_bounds(slider))
                        {
                            view.update_slider(slider, fraction, false, cx);
                        }
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .w_full()
                    .h(px(if slider == Slider::Volume { 6. } else { 4. }))
                    .rounded_full()
                    .bg(rgb(theme::SURFACE1))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(relative(progress))
                            .rounded_full()
                            .bg(rgb(theme::TEXT)),
                    ),
            )
            .when(enabled && slider == Slider::Seek, |bar| {
                bar.child(
                    div()
                        .absolute()
                        .left(relative(progress))
                        .ml(px(-4.))
                        .top(px((TIMELINE_HEIGHT - 8.) / 2.))
                        .size(px(8.))
                        .rounded_full()
                        .bg(rgb(if self.slider_dragging(slider) {
                            theme::LAVENDER
                        } else {
                            theme::TEXT
                        })),
                )
            })
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = entity.update(cx, |view, _| match slider {
                            Slider::Seek => view.seek_bounds = bounds,
                            Slider::Volume => view.volume_bounds = bounds,
                        });
                    },
                    {
                        let entity = cx.entity().downgrade();
                        move |_, _, window, _| {
                            let moved = entity.clone();
                            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture {
                                    return;
                                }
                                let _ = moved.update(cx, |view, cx| {
                                    if view.slider_dragging(slider) {
                                        if event.dragging() {
                                            if let Some(fraction) = pointer_fraction(
                                                event.position.x,
                                                view.slider_bounds(slider),
                                            ) {
                                                view.update_slider(slider, fraction, false, cx);
                                            }
                                        } else {
                                            view.cancel_slider_drag(slider, cx);
                                        }
                                        cx.notify();
                                    }
                                });
                            });
                            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture
                                    || event.button != MouseButton::Left
                                {
                                    return;
                                }
                                let _ = entity.update(cx, |view, cx| {
                                    if view.slider_dragging(slider) {
                                        if let Some(fraction) = pointer_fraction(
                                            event.position.x,
                                            view.slider_bounds(slider),
                                        ) {
                                            view.update_slider(slider, fraction, true, cx);
                                        } else {
                                            view.cancel_slider_drag(slider, cx);
                                            cx.notify();
                                        }
                                    }
                                });
                            });
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }
}
fn pointer_fraction(x: Pixels, bounds: Bounds<Pixels>) -> Option<f32> {
    (bounds.size.width > px(0.)).then(|| ((x - bounds.origin.x) / bounds.size.width).clamp(0., 1.))
}
