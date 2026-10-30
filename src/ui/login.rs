use gpui::{Context, Entity, Focusable, Window, actions, div, prelude::*, px, rgb};

use crate::{
    application::session::{Session, SignedIn},
    ui::components::button,
    ui::components::input::TextInput,
    ui::theme,
};

actions!(login, [SignIn]);

pub struct LoginPage {
    session: Entity<Session>,
    server: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
}

impl LoginPage {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let server = cx.new(|cx| TextInput::new("https://music.example.com", false, cx));
        window.focus(&server.focus_handle(cx), cx);
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&session, |view, _, event: &SignedIn, cx| {
            view.server.update(cx, |input, cx| {
                input.set_value(event.client.session.server.clone(), cx)
            });
            view.username.update(cx, |input, cx| {
                input.set_value(event.client.session.username.clone(), cx)
            });
            view.password.update(cx, |input, cx| input.clear(cx));
        })
        .detach();
        Self {
            session,
            server,
            username: cx.new(|cx| TextInput::new("Your Jellyfin username", false, cx)),
            password: cx.new(|cx| TextInput::new("Your Jellyfin password", true, cx)),
        }
    }
    fn sign_in(&mut self, _: &SignIn, _: &mut Window, cx: &mut Context<Self>) {
        let server = self.server.read(cx).value().to_owned();
        let username = self.username.read(cx).value().to_owned();
        let password = self.password.read(cx).value().to_owned();
        if self.session.update(cx, |session, cx| {
            session.sign_in(server, username, password, cx)
        }) {
            self.password.update(cx, |input, cx| input.clear(cx));
        }
    }

    fn form(&self, cx: &Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, input: Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(theme::SUBTEXT1))
                        .child(label),
                )
                .child(input)
        };
        div().flex().flex_col().gap_5().w(px(400.)).max_w_full()
            .child(div().text_3xl().child("Your music, from Jellyfin."))
            .child(div().text_color(rgb(theme::SUBTEXT0)).child("Connect to your server to browse your albums."))
            .child(field("Server URL", self.server.clone()))
            .child(field("Username", self.username.clone()))
            .child(field("Password", self.password.clone()))
            .child(button("sign-in", if self.session.read(cx).busy() { "Connecting…" } else { "Sign in" }, self.session.read(cx).busy(), theme::LAVENDER)
                .bg(rgb(theme::MAUVE)).border_color(rgb(theme::MAUVE))
                .text_color(rgb(theme::BASE))
                .on_click(cx.listener(|view, _, window, cx| view.sign_in(&SignIn, window, cx))))
            .child(div().text_sm().text_color(rgb(theme::SUBTEXT0))
                .child("Your password is never saved. Jellyui remembers your login using the desktop keyring."))
    }
}

impl Render for LoginPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Login")
            .on_action(cx.listener(Self::sign_in))
            .flex()
            .flex_col()
            .gap_5()
            .when_some(
                self.session.read(cx).warning().map(str::to_owned),
                |content, warning| {
                    content.child(div().text_color(rgb(theme::YELLOW)).child(warning))
                },
            )
            .when_some(
                self.session.read(cx).error().map(str::to_owned),
                |content, error| content.child(div().text_color(rgb(theme::RED)).child(error)),
            )
            .child(self.form(cx))
    }
}
