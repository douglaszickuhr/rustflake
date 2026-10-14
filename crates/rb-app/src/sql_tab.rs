use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{RopeExt as _, WindowExt as _, h_flex, resizable_panel, v_flex, v_resizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::error::error_byte_offset;
use rb_core::session::{ExecOutcome, Session};
use rb_core::split::{split_statements, statement_at};

use crate::completion::SqlCompletion;
use crate::grid::{self, ResultGrid};
use crate::meta::{ConnTarget, MetaStore};
use crate::{CancelQuery, CopyCells, Db, RunScript, RunStatement};

pub struct SqlTab {
    focus: FocusHandle,
    pub path: PathBuf,
    pub editor: Entity<EditorState>,
    table: Entity<TableState<ResultGrid>>,
    conn: ConnTarget,
    session: Option<Arc<Session>>,
    running: bool,
    status: SharedString,
    _save: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl SqlTab {
    pub fn new(path: PathBuf, conn: ConnTarget, meta: Entity<MetaStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let editor = cx.new(|cx| EditorState::new(window, cx).language("sql").default_value(text));
        editor.update(cx, |ed, _| ed.lsp_mut().completion_provider = Some(std::rc::Rc::new(SqlCompletion::new(meta))));
        let table = cx.new(|cx| TableState::new(ResultGrid::new(false), window, cx).cell_selectable(true));
        let subs = vec![
            cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.schedule_save(cx);
                }
            }),
            cx.observe(&table, |_, _, cx| cx.notify()),
        ];
        SqlTab {
            focus: cx.focus_handle(),
            path,
            editor,
            table,
            conn,
            session: None,
            running: false,
            status: "".into(),
            _save: None,
            _subs: subs,
        }
    }

    pub fn title(&self) -> SharedString {
        self.path.file_stem().map_or("script".into(), |s| s.to_string_lossy().to_string().into())
    }

    /// Salva 1 s depois da última alteração. Trocar a task cancela a anterior.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let editor = self.editor.clone();
        self._save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let text = cx.update(|cx| editor.read(cx).value().to_string());
            let res = cx.background_spawn(async move { std::fs::write(&path, text) }).await;
            if let Err(e) = res {
                tracing::warn!("falha ao salvar script: {e}");
            }
        }));
    }

    /// Gravação síncrona ao fechar a aba (arquivo pequeno).
    pub fn save_now(&self, cx: &App) {
        if let Err(e) = std::fs::write(&self.path, self.editor.read(cx).value().as_ref()) {
            tracing::warn!("falha ao salvar script: {e}");
        }
    }

    fn run(&mut self, whole_script: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let state = self.editor.read(cx);
        let src = state.value().to_string();
        let cursor = state.cursor();
        let ranges: Vec<Range<usize>> = if whole_script { split_statements(&src) } else { statement_at(&src, cursor).into_iter().collect() };
        if ranges.is_empty() {
            return;
        }
        self.running = true;
        self.status = "executando…".into();
        cx.notify();

        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        cx.spawn_in(window, async move |this, cx| {
            let session = match existing {
                Some(s) => s,
                None => match engine.connect(&conn.cfg, conn.password.clone()).await {
                    Ok(s) => Arc::new(s),
                    Err(e) => return this.update_in(cx, |t, w, cx| t.finish_error(&src, None, e, w, cx)),
                },
            };
            this.update(cx, |t, _| t.session = Some(session.clone()))?;
            let mut last = None;
            for r in ranges {
                match session.execute(src[r.clone()].to_string()).await {
                    Ok(out) => last = Some(out),
                    Err(e) => return this.update_in(cx, |t, w, cx| t.finish_error(&src, Some(r), e, w, cx)),
                }
            }
            this.update(cx, |t, cx| t.finish_ok(last, session, cx))
        })
        .detach();
    }

    fn finish_ok(&mut self, last: Option<ExecOutcome>, session: Arc<Session>, cx: &mut Context<Self>) {
        self.running = false;
        let Some(out) = last else { return };
        let ms = out.elapsed.as_millis();
        self.status = match out.affected {
            Some(n) => format!("{n} linha(s) afetada(s) · {ms} ms"),
            None => format!("{ms} ms"),
        }
        .into();
        let rs = ResultSet::new(out.columns, out.batch, out.more);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_result(rs, session);
            t.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn finish_error(&mut self, src: &str, range: Option<Range<usize>>, e: DbError, window: &mut Window, cx: &mut Context<Self>) {
        self.running = false;
        self.status = "erro".into();
        if let (Some(r), DbError::Postgres { position: Some(p), .. }) = (&range, &e) {
            let at = r.start + error_byte_offset(&src[r.clone()], *p);
            let word_end = src[at..].find(|c: char| c.is_whitespace() || c == ',' || c == ')' || c == ';').map_or(src.len(), |n| at + n);
            let end = if word_end > at { word_end } else { src[at..].chars().next().map_or(at, |c| at + c.len_utf8()) };
            let msg = e.to_string();
            self.editor.update(cx, |ed, cx| {
                let range = ed.text().offset_to_position(at)..ed.text().offset_to_position(end);
                if let Some(d) = ed.diagnostics_mut() {
                    d.clear();
                    d.push(Diagnostic::new(range, msg).with_severity(DiagnosticSeverity::Error));
                }
                cx.notify();
            });
        }
        window.push_notification(Notification::error(e.to_string()), cx);
        cx.notify();
    }

    fn cancel(&mut self) {
        if let Some(s) = &self.session {
            s.cancel();
        }
    }
}

impl Render for SqlTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let grid = self.table.read(cx).delegate();
        let rows = grid.loaded_rows();
        let more = grid.rs.as_ref().is_some_and(|r| r.more);
        let grid_error = grid.error.clone();
        v_flex()
            .key_context("SqlTab")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|t, _: &RunStatement, w, cx| t.run(false, w, cx)))
            .on_action(cx.listener(|t, _: &RunScript, w, cx| t.run(true, w, cx)))
            .on_action(cx.listener(|t, _: &CancelQuery, _, _| t.cancel()))
            .child(
                v_resizable("sql-split")
                    .child(resizable_panel().size(px(280.)).child(Editor::new(&self.editor).h_full()))
                    .child(resizable_panel().child(
                        div()
                            .key_context("ResultGrid")
                            .size_full()
                            .on_action(cx.listener(|t, _: &CopyCells, _, cx| grid::copy_to_clipboard(&t.table, cx)))
                            .child(DataTable::new(&self.table).stripe(true).bordered(true)),
                    )),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .child(format!("{rows}{} linha(s)", if more { "+" } else { "" }))
                    .child(self.status.clone())
                    .when_some(grid_error, |d, e| d.child(e))
                    .when(self.running, |d| {
                        d.child(Button::new("cancel").label("Cancelar ⌘.").ghost().on_click(cx.listener(|t, _, _, _| t.cancel())))
                    }),
            )
    }
}
