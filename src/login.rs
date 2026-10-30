use gpui::{Context, Entity, EventEmitter, Focusable, Window, actions, div, prelude::*, px, rgb};

use crate::{
    button,
    input::TextInput,
    jellyfin::{self, Client, Session},
    theme,
};

pub(crate) const CREDENTIAL_KEY: &str = "jellyui://session";
actions!(login, [SignIn]);

pub struct SignedIn {
    pub client: Client,
    pub warning: Option<String>,
}

pub struct LoginPage {
    server: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    busy: bool,
    error: Option<String>,
    warning: Option<String>,
}

impl EventEmitter<SignedIn> for LoginPage {}

impl LoginPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let server = cx.new(|cx| TextInput::new("https://music.example.com", false, cx));
        window.focus(&server.focus_handle(cx), cx);
        let mut page = Self {
            server,
            username: cx.new(|cx| TextInput::new("Your Jellyfin username", false, cx)),
            password: cx.new(|cx| TextInput::new("Your Jellyfin password", true, cx)),
            busy: true,
            error: None,
            warning: None,
        };
        page.restore(cx);
        page
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.password.update(cx, |input, cx| input.clear(cx));
        self.busy = false;
        self.error = None;
        self.warning = None;
        cx.notify();
    }

    pub fn reject_session(&mut self, error: String, cx: &mut Context<Self>) {
        self.reset(cx);
        self.error = Some(error);
        self.busy = true;
        let deletion = cx.delete_credentials(CREDENTIAL_KEY);
        cx.spawn(async move |this, cx| {
            let deleted = deletion.await;
            let _ = this.update(cx, |page, cx| {
                page.busy = false;
                if let Err(error) = deleted {
                    tracing::warn!(error = %error, "Could not delete rejected login from desktop credential storage");
                    page.warning = Some("Could not remove the rejected login from desktop credential storage.".into());
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn restore(&mut self, cx: &mut Context<Self>) {
        tracing::info!("Reading saved login from desktop credential storage");
        let credentials = cx.read_credentials(CREDENTIAL_KEY);
        cx.spawn(async move |this, cx| {
            let result = credentials.await;
            let _ =
                this.update(cx, |view, cx| {
                    view.busy = false;
                    match result {
                        Ok(Some((_, secret))) => {
                            match serde_json::from_slice::<Session>(&secret)
                                .inspect_err(|error| tracing::warn!(category = ?error.classify(), line = error.line(), column = error.column(), "Saved login has invalid JSON structure"))
                                .ok()
                                .and_then(|session| Client::restore(session).inspect_err(|error| tracing::warn!(error = %error, "Saved login has an invalid server URL")).ok())
                            {
                                Some(client) => {
                                    tracing::info!("Saved login restored; loading albums");
                                    view.server.update(cx, |input, cx| {
                                        input.set_value(client.session.server.clone(), cx)
                                    });
                                    view.username.update(cx, |input, cx| {
                                        input.set_value(client.session.username.clone(), cx)
                                    });
                                    cx.emit(SignedIn { client, warning: None });
                                }
                                None => {
                                    view.warning = Some(
                                        "The saved login could not be read. Sign in to replace it."
                                            .into(),
                                    )
                                }
                            }
                        }
                        Ok(None) => { tracing::info!("No saved login found"); }
                        Err(error) => {
                            tracing::warn!(error = %error, "Could not read desktop credential storage");
                            view.warning = Some(
                            "Desktop credential storage is unavailable. Sign in for this session."
                                .into(),
                            );
                        }
                    }
                    cx.notify();
                });
        })
        .detach();
    }

    fn sign_in(&mut self, _: &SignIn, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            tracing::debug!(
                busy = self.busy,
                "Sign-in ignored while another login operation is active"
            );
            return;
        }
        let server = self.server.read(cx).value().to_owned();
        if let Err(error) = jellyfin::server_url(&server) {
            tracing::warn!(error = %error, "Sign-in rejected by server URL validation");
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let username = self.username.read(cx).value().trim().to_owned();
        if username.is_empty() {
            tracing::warn!("Sign-in requires a username");
            self.error = Some("Enter your Jellyfin username.".into());
            cx.notify();
            return;
        }
        // Empty passwords are allowed by some Jellyfin servers.
        let password = self.password.read(cx).value().to_owned();
        self.password.update(cx, |input, cx| input.clear(cx));
        self.busy = true;
        self.error = None;
        tracing::info!("Sign-in submitted");
        let request = cx
            .background_executor()
            .spawn(async move { Client::sign_in(&server, &username, &password) });
        cx.spawn(async move |this, cx| {
            match request.await {
                Ok(client) => {
                    tracing::info!("Saving Jellyfin session in desktop credential storage");
                    let secret = serde_json::to_vec(&client.session).expect("session serialization");
                        let save = cx.update(|cx| cx.write_credentials(CREDENTIAL_KEY, "session", &secret));
                    let saved = save.await;
                    match &saved {
                        Ok(()) => tracing::info!("Login saved to desktop credential storage"),
                        Err(error) => tracing::warn!(error = %error, "Could not save login; continuing for this session"),
                    }
                    let _ = this.update(cx, |view, cx| {
                        let warning = saved.err().map(|_| "Signed in, but the desktop keyring could not save your login. You will need to sign in next time.".into());
                        view.busy = false;
                        cx.emit(SignedIn { client, warning });
                        cx.notify();
                    });
                }
                Err(error) => {
                    tracing::error!(error = %error, "Sign-in failed");
                    let _ = this.update(cx, |view, cx| {
                        view.busy = false;
                        view.error = Some(error.to_string());
                        cx.notify();
                    });
                }
            }
        }).detach();
        cx.notify();
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
            .child(button("sign-in", if self.busy { "Connecting…" } else { "Sign in" }, self.busy, theme::LAVENDER)
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
            .when_some(self.warning.clone(), |content, warning| {
                content.child(div().text_color(rgb(theme::YELLOW)).child(warning))
            })
            .when_some(self.error.clone(), |content, error| {
                content.child(div().text_color(rgb(theme::RED)).child(error))
            })
            .child(self.form(cx))
    }
}
