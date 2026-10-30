use super::Jellyui;
use crate::ui::actions::{BackTab, Tab};
use crate::ui::library::{LibraryKind, NAV_WIDTH};
use crate::ui::{components::Tooltip, theme};
use gpui::{Context, Window, div, prelude::*, px, rgb, svg};

impl Jellyui {
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
                    self.album.as_ref().unwrap().clone().into_any_element()
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
                view.child(self.player.clone())
            })
            .child(self.track_menu.clone())
    }
}
