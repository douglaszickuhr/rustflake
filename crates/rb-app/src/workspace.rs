use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

use crate::{CloseTab, NewSqlTab};

pub struct Workspace {
    focus: FocusHandle,
}

impl Workspace {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Workspace { focus: cx.focus_handle() }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|_, _: &NewSqlTab, _, _| {}))
            .on_action(cx.listener(|_, _: &CloseTab, _, _| {}))
            .child(v_flex().w(px(260.)).h_full().border_r_1().border_color(cx.theme().border).p_2().child("Conexões"))
            .child(v_flex().flex_1().h_full().items_center().justify_center().child("Nenhuma conexão aberta"))
    }
}
