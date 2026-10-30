use super::{LibraryPage, SORT_OPTIONS};
use crate::domain::LibraryKind;
use crate::{domain::AlbumSort, ui::components::button, ui::theme};
use gpui::{
    Context, Div, KeyDownEvent, MouseButton, MouseDownEvent, Stateful, Window, anchored, canvas,
    deferred, div, point, prelude::*, px, relative, rgb,
};

impl LibraryPage {
    pub(super) fn toggle_sort_menu(&mut self, cx: &mut Context<Self>) {
        self.sort_open = !self.sort_open;
        if self.sort_open {
            self.sort_cursor = SORT_OPTIONS
                .iter()
                .position(|(_, _, sort)| *sort == self.query.sort)
                .unwrap_or(0);
        }
        cx.notify();
    }

    pub(super) fn choose_sort(&mut self, sort: AlbumSort, cx: &mut Context<Self>) {
        self.sort_open = false;
        self.pending_search = None;
        self.query.sort = sort;
        self.apply(cx);
    }

    pub(super) fn sort_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => self.sort_open = false,
            "enter" | "space" => {
                if self.sort_open {
                    self.choose_sort(SORT_OPTIONS[self.sort_cursor].2, cx);
                } else {
                    self.toggle_sort_menu(cx);
                }
            }
            "up" | "down" => {
                if !self.sort_open {
                    self.toggle_sort_menu(cx);
                } else if event.keystroke.key == "down" {
                    self.sort_cursor = (self.sort_cursor + 1) % SORT_OPTIONS.len();
                } else {
                    self.sort_cursor =
                        (self.sort_cursor + SORT_OPTIONS.len() - 1) % SORT_OPTIONS.len();
                }
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn sort_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let trigger_bounds = self.sort_bounds;
        let mut menu = div()
            .id("sort-menu")
            .occlude()
            .w(self.sort_bounds.size.width.max(px(160.)))
            .p_1()
            .rounded_md()
            .bg(rgb(theme::MANTLE))
            .border_1()
            .border_color(rgb(theme::SURFACE1))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_1()
            .on_mouse_down_out(cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                if !trigger_bounds.contains(&event.position) {
                    view.sort_open = false;
                    cx.notify();
                }
            }));
        for (index, (id, text, sort)) in SORT_OPTIONS.into_iter().enumerate() {
            menu = menu.child(
                choice(id, text, index == self.sort_cursor)
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .when(self.query.sort == sort, |option| option.child("✓"))
                    .on_click(cx.listener(move |view, _, _, cx| view.choose_sort(sort, cx))),
            );
        }
        deferred(
            anchored()
                .position(self.sort_bounds.bottom_left())
                .offset(point(px(0.), px(4.)))
                .snap_to_window()
                .child(menu),
        )
        .with_priority(1)
    }

    pub(super) fn filters(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = SORT_OPTIONS
            .iter()
            .find(|(_, _, sort)| *sort == self.query.sort)
            .map(|(_, label, _)| *label)
            .unwrap_or("Title");
        let entity = cx.entity().downgrade();
        let sort_controls = div()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .when(self.kind == LibraryKind::Albums, |controls| {
                controls.child(
                    div()
                        .id("sort-select")
                        .track_focus(&self.sort_focus)
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h(px(42.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .rounded_md()
                        .bg(rgb(theme::MANTLE))
                        .border_1()
                        .border_color(rgb(theme::SURFACE1))
                        .cursor_pointer()
                        .focus(|style| style.border_color(rgb(theme::MAUVE)))
                        .hover(|style| style.bg(rgb(theme::SURFACE0)))
                        .child(div().min_w_0().truncate().child(selected))
                        .child("⌄")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _, window, cx| {
                                window.focus(&view.sort_focus, cx);
                                view.toggle_sort_menu(cx);
                                cx.stop_propagation();
                            }),
                        )
                        .on_key_down(
                            cx.listener(|view, event, window, cx| view.sort_key(event, window, cx)),
                        )
                        .child(
                            canvas(
                                move |bounds, _, cx| {
                                    let _ = entity.update(cx, |view, cx| {
                                        if view.sort_bounds != bounds {
                                            view.sort_bounds = bounds;
                                            if view.sort_open {
                                                cx.notify();
                                            }
                                        }
                                    });
                                },
                                |_, _, _, _| (),
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        ),
                )
            })
            .when(self.kind == LibraryKind::Artists, |controls| {
                controls.child(div().flex_1().child("Name"))
            })
            .child(
                button(
                    "sort-direction",
                    if self.query.descending { "↓" } else { "↑" },
                    false,
                    theme::SURFACE2,
                )
                .w(px(34.))
                .h(px(42.))
                .p_0()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_xl()
                .on_click(cx.listener(|view, _, _, cx| {
                    view.pending_search = None;
                    view.query.descending = !view.query.descending;
                    view.apply(cx);
                })),
            )
            .when(self.sort_open, |controls| {
                controls.child(self.sort_menu(cx))
            });
        div()
            .id("library-filters")
            .w(px(200.))
            .max_w(relative(0.4))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .bg(rgb(theme::MANTLE))
            .border_r_1()
            .border_color(rgb(theme::SURFACE0))
            .child(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(div().text_lg().child("Browse"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("Search"))
                            .child(self.search.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("Sort by"))
                            .child(sort_controls),
                    )
                    .child(
                        button("reset-filters", "Reset", false, theme::SURFACE2)
                            .on_click(cx.listener(|view, _, _, cx| view.reset(cx))),
                    ),
            )
    }
}

fn label(text: &'static str) -> Div {
    div().text_sm().text_color(rgb(theme::SUBTEXT0)).child(text)
}

fn choice(id: &'static str, text: &'static str, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .cursor_pointer()
        .bg(rgb(if selected {
            theme::SURFACE0
        } else {
            theme::MANTLE
        }))
        .border_color(rgb(if selected {
            theme::MAUVE
        } else {
            theme::MANTLE
        }))
        .text_color(rgb(if selected {
            theme::MAUVE
        } else {
            theme::SUBTEXT1
        }))
        .focus(|style| style.border_color(rgb(theme::MAUVE)))
        .hover(|style| style.bg(rgb(theme::SURFACE0)))
        .child(div().min_w_0().truncate().child(text))
}
