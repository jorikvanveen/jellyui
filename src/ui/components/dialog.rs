use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, PromptButton,
    PromptResponse, Window, div, prelude::*, px, rgb,
};

use crate::ui::theme;

pub fn init(cx: &mut App) {
    cx.set_prompt_builder(|_, message, detail, answers, handle, window, cx| {
        let dialog = cx.new(|cx| Dialog {
            message: message.to_owned(),
            detail: detail.map(str::to_owned),
            answers: answers.to_vec(),
            selected: answers
                .iter()
                .position(PromptButton::is_cancel)
                .unwrap_or(0),
            focus: cx.focus_handle(),
        });
        handle.with_view(dialog, window, cx)
    });
}

struct Dialog {
    message: String,
    detail: Option<String>,
    answers: Vec<PromptButton>,
    selected: usize,
    focus: FocusHandle,
}

impl EventEmitter<PromptResponse> for Dialog {}

impl Focusable for Dialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Dialog {
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                if let Some(index) = self.answers.iter().position(PromptButton::is_cancel) {
                    cx.emit(PromptResponse(index));
                }
            }
            "enter" | "space" => cx.emit(PromptResponse(self.selected)),
            "left" | "up" => {
                self.selected = (self.selected + self.answers.len() - 1) % self.answers.len()
            }
            "right" | "down" | "tab" => {
                let reverse = event.keystroke.key == "tab" && event.keystroke.modifiers.shift;
                self.selected = (self.selected + if reverse { self.answers.len() - 1 } else { 1 })
                    % self.answers.len();
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for Dialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .occlude()
            .bg(rgb(theme::BASE).opacity(0.75))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                div()
                    .id("confirmation-dialog")
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::key_down))
                    .w(px(440.))
                    .max_w_full()
                    .p_6()
                    .rounded_lg()
                    .bg(rgb(theme::MANTLE))
                    .text_color(rgb(theme::TEXT))
                    .font_family("sans-serif")
                    .text_size(px(15.))
                    .border_1()
                    .border_color(rgb(theme::SURFACE1))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.message.clone()),
                    )
                    .children(
                        self.detail
                            .clone()
                            .map(|detail| div().text_color(rgb(theme::SUBTEXT0)).child(detail)),
                    )
                    .child(div().flex().justify_end().gap_3().mt_2().children(
                        self.answers.iter().enumerate().map(|(index, answer)| {
                            div()
                                .id(index)
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(if self.selected == index {
                                    theme::MAUVE
                                } else {
                                    theme::SURFACE1
                                }))
                                .bg(rgb(theme::SURFACE0))
                                .cursor_pointer()
                                .aria_selected(self.selected == index)
                                .hover(|style| style.bg(rgb(theme::SURFACE1)))
                                .child(answer.label().clone())
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(PromptResponse(index));
                                    cx.stop_propagation();
                                }))
                        }),
                    )),
            )
    }
}
