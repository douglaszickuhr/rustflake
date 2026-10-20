//! Database filter dialog (Snowflake): each pattern becomes a chip; `!pattern` excludes (red chip).

use rust_i18n::t;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::metadata::parse_db_filter;

pub struct DbFilterEditor {
    input: Entity<InputState>,
    tags: Vec<String>,
    /// Position of the chip being edited (double click): the text goes back there.
    editing: Option<usize>,
    _sub: Subscription,
}

/// Merges new patterns into the chips, without repeats (case-insensitive): at `at` (edited chip) or at the end.
fn merge(tags: &mut Vec<String>, at: Option<usize>, new: Vec<String>) {
    let mut pos = at.map_or(tags.len(), |i| i.min(tags.len()));
    for p in new {
        if !tags.iter().any(|t| t.eq_ignore_ascii_case(&p)) {
            tags.insert(pos, p);
            pos += 1;
        }
    }
}

impl DbFilterEditor {
    fn new(tags: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("filter.placeholder").to_string()));
        let sub = cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, window, cx| {
            let typed_comma = matches!(ev, InputEvent::Change) && input.read(cx).value().contains(',');
            if typed_comma || matches!(ev, InputEvent::PressEnter { .. }) {
                this.absorb(window, cx);
            }
        });
        DbFilterEditor { input, tags, editing: None, _sub: sub }
    }

    /// Turns the typed text into chips (no repeats, case-insensitive) and clears the field.
    fn absorb(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        merge(&mut self.tags, self.editing.take(), parse_db_filter(&text));
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// Double click on a chip: its text goes to the field (whatever was already typed becomes a chip first).
    fn edit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.absorb(window, cx);
        if i >= self.tags.len() {
            return;
        }
        let text = self.tags.remove(i);
        self.editing = Some(i);
        self.input.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// Chips plus whatever was typed without Enter.
    fn patterns(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<String> {
        self.absorb(window, cx);
        self.tags.clone()
    }
}

impl Render for DbFilterEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chips = self.tags.iter().enumerate().map(|(i, t)| {
            let tag = crate::ui::badge(if t.starts_with('!') { crate::ui::Tone::Danger } else { crate::ui::Tone::Neutral }, cx);
            let tag = tag.child(h_flex().gap_1().child(t.clone()).child(div().id(("dbf-rm", i)).cursor_pointer().child("×").on_click(cx.listener(move |this, _, _, cx| {
                this.tags.remove(i);
                cx.notify();
            }))));
            div().id(("dbf-tag", i)).child(tag).on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if ev.click_count() == 2 {
                    this.edit(i, window, cx);
                }
            }))
        });
        v_flex()
            .gap_2()
            // Backspace with an empty field deletes the last chip (before the field handles the key).
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "backspace" && this.input.read(cx).value().is_empty() && this.tags.pop().is_some() {
                    cx.notify();
                }
            }))
            .when(!self.tags.is_empty(), |d| d.child(h_flex().flex_wrap().gap_1().children(chips)))
            .child(Input::new(&self.input))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(t!("filter.hint").to_string()))
    }
}

/// Opens the dialog with the current patterns; `apply` receives the new list (empty = no filter).
pub fn open(current: Vec<String>, apply: impl Fn(Vec<String>, &mut Window, &mut App) + 'static, window: &mut Window, cx: &mut App) {
    let editor = cx.new(|cx| DbFilterEditor::new(current, window, cx));
    let input = editor.read(cx).input.clone();
    let apply = Rc::new(apply);
    window.open_dialog(cx, move |dialog, _, _| {
        let (clear, save, ed) = (apply.clone(), apply.clone(), editor.clone());
        dialog.title(t!("filter.title").to_string()).w(px(480.)).child(editor.clone()).footer(
            h_flex()
                .justify_end()
                .gap_2()
                .child(Button::new("dbf-clear").label(t!("filter.clear").to_string()).on_click(move |_, w, cx| {
                    w.close_dialog(cx);
                    clear(Vec::new(), w, cx)
                }))
                .child(Button::new("dbf-cancel").label(t!("common.cancel").to_string()).on_click(|_, w, cx| w.close_dialog(cx)))
                .child(Button::new("dbf-ok").label(t!("filter.apply").to_string()).primary().on_click(move |_, w, cx| {
                    let patterns = ed.update(cx, |e, cx| e.patterns(w, cx));
                    w.close_dialog(cx);
                    save(patterns, w, cx)
                })),
        )
    });
    // After opening: the dialog focuses its own handle and stores the previous focus to restore on close.
    input.update(cx, |s, cx| s.focus(window, cx));
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` brings GPUI\'s own `test` attribute; this one wins over the glob.
    use core::prelude::v1::test;

    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merge_appends_or_returns_to_the_edited_slot_without_duplicates() {
        let mut tags = v(&["A*", "C*"]);
        merge(&mut tags, None, v(&["b*", "a*"]));
        assert_eq!(tags, v(&["A*", "C*", "b*"]));
        // Edited chip (was index 1) goes back to the same place, even when it becomes two.
        merge(&mut tags, Some(1), v(&["X*", "Y*"]));
        assert_eq!(tags, v(&["A*", "X*", "Y*", "C*", "b*"]));
        // Index past the end (chips removed during editing): goes to the end.
        merge(&mut tags, Some(99), v(&["Z"]));
        assert_eq!(tags.last().map(String::as_str), Some("Z"));
    }
}
