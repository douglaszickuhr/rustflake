use std::ops::Range;
use rust_i18n::t;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{Editor, EditorState, Enter, IndentInline, InputEvent};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, RopeExt as _, Sizable as _, WindowExt as _, h_flex, resizable_panel, v_flex, v_resizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::error::error_byte_offset;
use rb_core::history::{self, Entry};
use rb_core::session::{ExecOutcome, Session};
use rb_core::split::{split_statements, statement_at};

use crate::completion::SqlCompletion;
use crate::grid::{self, ResultGrid};
use crate::meta::{ConnTarget, MetaStore};
use crate::plan_view::PlanView;
use crate::{CancelQuery, CopyCells, Db, ExplainAnalyze, ExplainEstimate, FormatSql, RunScript, RunStatement, TabKey};

/// Aba visível na área de resultados.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Result(usize),
    Messages,
    Plan,
}

/// Uma linha da aba "Mensagens": um statement executado.
pub struct Message {
    pub sql_head: String,
    /// Linhas do primeiro lote; `more` = havia mais.
    pub rows: Option<u64>,
    pub more: bool,
    pub affected: Option<u64>,
    pub ms: u64,
    pub error: Option<String>,
}

struct ResultPane {
    title: SharedString,
    grid: Entity<TableState<ResultGrid>>,
    /// Havia mais linhas, mas o cursor fechou quando o statement seguinte rodou.
    truncated: bool,
}

/// Índices dos statements que viram aba de resultado (devolveram colunas) e se a aba
/// "Mensagens" aparece (script com mais de um statement).
fn panes(has_columns: &[bool], statements: usize) -> (Vec<usize>, bool) {
    (has_columns.iter().enumerate().filter(|(_, c)| **c).map(|(i, _)| i).collect(), statements > 1)
}

/// A sessão mantém um cursor só: qualquer statement executado depois (inclusive um que falhou)
/// fecha o do anterior.
fn cursor_closed(i: usize, executed: usize) -> bool {
    i + 1 != executed
}

/// O que o ⌘⇧F formata: a seleção, ou o script inteiro se nada estiver selecionado.
fn format_target(src: &str, selection: Range<usize>) -> Range<usize> {
    if selection.is_empty() { 0..src.len() } else { selection.start.min(selection.end)..selection.start.max(selection.end) }
}

/// Resumo de uma linha: junta as linhas (SQL formatado começa com "SELECT" sozinho) e corta em 80.
pub(crate) fn sql_head(sql: &str) -> String {
    let line = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 80 { line.chars().take(80).collect::<String>() + "…" } else { line }
}

type Outcome = (Range<usize>, Duration, Result<ExecOutcome, DbError>);

pub struct SqlTab {
    focus: FocusHandle,
    pub path: PathBuf,
    pub editor: Entity<EditorState>,
    results: Vec<ResultPane>,
    messages: Vec<Message>,
    show_messages: bool,
    plan: Option<Entity<PlanView>>,
    active: Pane,
    conn: ConnTarget,
    meta: Entity<MetaStore>,
    session: Option<Arc<Session>>,
    running: bool,
    /// O último run teve `USE …`: o contexto (database/schema/warehouse/role) mudou.
    ran_use: bool,
    status: SharedString,
    _save: Option<Task<()>>,
    _busy: Option<Task<()>>,
    _grid_subs: Vec<Subscription>,
    _subs: Vec<Subscription>,
}

impl SqlTab {
    pub fn new(path: PathBuf, conn: ConnTarget, meta: Entity<MetaStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let editor = cx.new(|cx| EditorState::new(window, cx).language("sql").default_value(text));
        editor.update(cx, |ed, _| ed.lsp_mut().completion_provider = Some(std::rc::Rc::new(SqlCompletion::new(meta.clone()))));
        let subs = vec![cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.schedule_save(cx);
            }
        })];
        let mut tab = SqlTab {
            focus: cx.focus_handle(),
            path,
            editor,
            results: Vec::new(),
            messages: Vec::new(),
            show_messages: false,
            plan: None,
            active: Pane::Result(0),
            conn,
            meta,
            session: None,
            running: false,
            ran_use: false,
            status: "".into(),
            _save: None,
            _busy: None,
            _grid_subs: Vec::new(),
            _subs: subs,
        };
        let grid = tab.new_grid(window, cx);
        tab.results.push(ResultPane { title: "".into(), grid, truncated: false });
        tab
    }

    fn new_grid(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<TableState<ResultGrid>> {
        let grid = cx.new(|cx| TableState::new(ResultGrid::new(false), window, cx).cell_selectable(true));
        self._grid_subs.push(cx.observe(&grid, |_, _, cx| cx.notify()));
        grid
    }

    fn active_grid(&self) -> Option<&Entity<TableState<ResultGrid>>> {
        match self.active {
            Pane::Result(i) => self.results.get(i).map(|p| &p.grid),
            Pane::Messages | Pane::Plan => None,
        }
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    pub fn title(&self) -> SharedString {
        self.path.file_stem().map_or("script".into(), |s| s.to_string_lossy().to_string().into())
    }

    /// Insere no cursor (histórico) e devolve o foco ao editor.
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| {
            e.insert(text.to_string(), window, cx);
            e.focus(window, cx);
        });
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
        self.ran_use = ranges.iter().any(|r| src[r.clone()].trim_start().get(..4).is_some_and(|w| w.eq_ignore_ascii_case("use ")));
        self.running = true;
        self.status = t!("sql.running").to_string().into();
        // O resultado anterior fica na tela (com skeleton se demorar) até o novo chegar.
        self._busy = self.active_grid().map(|g| grid::busy_later(g, cx));
        cx.notify();

        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        let total = ranges.len();
        cx.spawn_in(window, async move |this, cx| {
            let session = match existing {
                Some(s) => s,
                None => match engine.connect(&conn.cfg, conn.creds.clone()).await {
                    Ok(s) => Arc::new(s),
                    Err(e) => return this.update_in(cx, |t, w, cx| t.finish_error(&src, None, e, w, cx)),
                },
            };
            this.update(cx, |t, _| t.session = Some(session.clone()))?;
            let mut outcomes: Vec<Outcome> = Vec::new();
            for r in ranges {
                let started = Instant::now();
                let res = session.execute(src[r.clone()].to_string()).await;
                let failed = res.is_err();
                outcomes.push((r, started.elapsed(), res));
                if failed {
                    break;
                }
            }
            this.update_in(cx, |t, w, cx| t.finish(outcomes, total, &src, session, w, cx))
        })
        .detach();
    }

    fn finish(&mut self, mut outcomes: Vec<Outcome>, total: usize, src: &str, session: Arc<Session>, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_running(cx);
        let failure = match outcomes.last_mut() {
            Some((r, _, res)) if res.is_err() => Some((r.clone(), std::mem::replace(res, Err(DbError::Connection(String::new()))).unwrap_err())),
            _ => None,
        };
        if self.ran_use && failure.is_none() {
            self.meta.update(cx, |m, cx| m.reload_roots(cx));
        }
        let has_columns: Vec<bool> = outcomes.iter().map(|(_, _, r)| r.as_ref().is_ok_and(|o| !o.columns.is_empty())).collect();
        let (with_rows, show_messages) = panes(&has_columns, total);

        self.messages = outcomes
            .iter()
            .map(|(r, took, res)| {
                let sql_head = sql_head(&src[r.clone()]);
                match res {
                    Ok(o) => Message {
                        sql_head,
                        rows: (!o.columns.is_empty()).then(|| o.batch.rows() as u64),
                        more: o.more,
                        affected: o.affected,
                        ms: o.elapsed.as_millis() as u64,
                        error: None,
                    },
                    Err(_) => Message { sql_head, rows: None, more: false, affected: None, ms: took.as_millis() as u64, error: failure.as_ref().map(|(_, e)| e.to_string()) },
                }
            })
            .collect();
        self.record_history(&outcomes, src, cx);

        // Uma grade por statement com colunas; só o último executado mantém o cursor para paginar.
        self._grid_subs.clear();
        let executed = outcomes.len();
        let mut results = Vec::new();
        for (k, &i) in with_rows.iter().enumerate() {
            let Ok(out) = std::mem::replace(&mut outcomes[i].2, Err(DbError::Connection(String::new()))) else { continue };
            let truncated = out.more && cursor_closed(i, executed);
            let rs = ResultSet::new(out.columns, out.batch, out.more && !truncated);
            let grid = self.new_grid(window, cx);
            grid.update(cx, |t, cx| {
                t.delegate_mut().set_result(rs, session.clone(), out.exec_id);
                t.refresh(cx);
            });
            results.push(ResultPane { title: t!("results.result", n = k + 1).to_string().into(), grid, truncated });
        }
        if results.is_empty() {
            let grid = self.new_grid(window, cx);
            results.push(ResultPane { title: "".into(), grid, truncated: false });
        }
        let n = results.len();
        self.results = results;
        self.show_messages = show_messages;
        self.plan = None;
        self.active = if failure.is_some() && show_messages || with_rows.is_empty() && show_messages { Pane::Messages } else { Pane::Result(n - 1) };

        match failure {
            Some((r, e)) => self.show_error(src, Some(r), e, window, cx),
            None => {
                self.status = match self.messages.last() {
                    Some(Message { affected: Some(a), ms, .. }) => format!("{} · {ms} ms", t!("sql.rows_affected", n = a)),
                    Some(m) => format!("{} ms", m.ms),
                    None => String::new(),
                }
                .into();
            }
        }
        cx.notify();
    }

    /// ⌘E (estimado) / ⌘⇧E (real) do statement sob o cursor; o diagrama vai para a aba "Plano".
    fn explain(&mut self, analyze: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let state = self.editor.read(cx);
        let src = state.value().to_string();
        let Some(r) = statement_at(&src, state.cursor()) else { return };
        let sql = src[r].to_string();
        // O EXPLAIN roda na mesma sessão e fecha o cursor da grade aberta: ela passa a mostrar "só as primeiras".
        for p in &mut self.results {
            if p.grid.update(cx, |t, _| t.delegate_mut().close_cursor()) {
                p.truncated = true;
            }
        }
        self.running = true;
        self.status = t!("sql.running").to_string().into();
        cx.notify();
        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        cx.spawn_in(window, async move |this, cx| {
            let session = match existing {
                Some(s) => s,
                None => match engine.connect(&conn.cfg, conn.creds.clone()).await {
                    Ok(s) => Arc::new(s),
                    Err(e) => return this.update_in(cx, |t, w, cx| t.finish_error(&src, None, e, w, cx)),
                },
            };
            this.update(cx, |t, _| t.session = Some(session.clone()))?;
            let res = session.explain(sql, analyze).await;
            this.update_in(cx, |t, window, cx| {
                t.stop_running(cx);
                match res {
                    Ok(plan) => {
                        t.status = "".into();
                        t.plan = Some(cx.new(|cx| PlanView::new(plan, cx)));
                        t.active = Pane::Plan;
                        cx.notify();
                    }
                    Err(e) => t.show_error(&src, None, e, window, cx),
                }
            })
        })
        .detach();
    }

    /// Cada statement executado vai para o histórico da conexão, em segundo plano.
    fn record_history(&self, outcomes: &[Outcome], src: &str, cx: &mut Context<Self>) {
        let path = history::history_path(self.conn_id());
        let ts = history::now_ms();
        let entries: Vec<Entry> = outcomes
            .iter()
            .zip(&self.messages)
            .map(|((r, _, _), m)| Entry { ts, sql: src[r.clone()].trim().to_string(), ms: m.ms, rows: m.rows.or(m.affected), error: m.error.clone() })
            .collect();
        cx.background_spawn(async move {
            for e in &entries {
                if let Err(err) = history::append(&path, e) {
                    tracing::warn!("histórico: {err}");
                }
            }
        })
        .detach();
    }

    fn finish_error(&mut self, src: &str, range: Option<Range<usize>>, e: DbError, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_running(cx);
        self.show_error(src, range, e, window, cx);
    }

    fn show_error(&mut self, src: &str, range: Option<Range<usize>>, e: DbError, window: &mut Window, cx: &mut Context<Self>) {
        self.status = t!("sql.error").to_string().into();
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

    fn stop_running(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        self._busy = None;
        for p in &self.results {
            grid::set_idle(&p.grid, cx);
        }
    }

    /// Uma edição só (`replace`): um ⌘Z volta ao texto anterior. Texto igual não suja o desfazer.
    fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (src, sel) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.selected_range())
        };
        let range = format_target(&src, sel);
        let Some(original) = src.get(range.clone()) else { return };
        let formatted = rb_core::format::format_sql(original);
        if formatted == original {
            return;
        }
        let whole = range.len() == src.len();
        self.editor.update(cx, |e, cx| {
            if whole {
                e.select_all(window, cx);
            }
            e.replace(formatted, window, cx);
        });
    }

    fn cancel(&mut self) {
        if let Some(s) = &self.session {
            s.cancel();
        }
    }

    fn render_messages(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.messages.iter().enumerate().map(|(i, m)| {
            let what = match (m.rows, m.affected) {
                (Some(n), _) => t!("grid.rows", n = format!("{n}{}", if m.more { "+" } else { "" })).to_string(),
                (None, Some(n)) => t!("results.affected", n = n).to_string(),
                (None, None) => String::new(),
            };
            v_flex()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_3()
                        .text_sm()
                        .child(div().text_color(theme.muted_foreground).child(format!("{}.", i + 1)))
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family("monospace").child(m.sql_head.clone()))
                        .child(div().text_color(theme.muted_foreground).child(what))
                        .child(div().text_color(theme.muted_foreground).child(format!("{} ms", m.ms))),
                )
                .when_some(m.error.clone(), |d, e| d.child(div().text_sm().text_color(theme.danger).child(e)))
        });
        div().id("messages").size_full().overflow_y_scroll().child(v_flex().children(rows))
    }
}

impl Render for SqlTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let grid = self.active_grid().map(|g| g.read(cx).delegate());
        let rows = grid.map_or(0, |g| g.loaded_rows());
        let more = grid.is_some_and(|g| g.rs.as_ref().is_some_and(|r| r.more));
        let grid_error = grid.and_then(|g| g.error.clone());
        let spinning = self.running || grid.is_some_and(|g| g.loading_more());
        let truncated = matches!(self.active, Pane::Result(i) if self.results.get(i).is_some_and(|p| p.truncated));

        let tabs = (self.results.len() > 1 || self.show_messages || self.plan.is_some()).then(|| {
            // Ordem das abas: resultados, Mensagens (se houver), Plano (se houver).
            let n = self.results.len();
            let msgs = self.show_messages;
            let selected = match self.active {
                Pane::Result(i) => i,
                Pane::Messages => n,
                Pane::Plan => n + msgs as usize,
            };
            let bar = self.results.iter().fold(
                TabBar::new("result-tabs").selected_index(selected).on_click(cx.listener(move |t, ix: &usize, _, cx| {
                    t.active = match *ix {
                        i if i < n => Pane::Result(i),
                        i if i == n && msgs => Pane::Messages,
                        _ => Pane::Plan,
                    };
                    cx.notify();
                })),
                |bar, p| bar.child(Tab::new().label(p.title.clone())),
            );
            bar.when(msgs, |b| b.child(Tab::new().label(t!("results.messages").to_string())))
                .when(self.plan.is_some(), |b| b.child(Tab::new().label(t!("plan.tab").to_string())))
        });
        let content = match self.active {
            Pane::Result(i) => {
                let g = self.results.get(i).map(|p| p.grid.clone());
                div()
                    .key_context("ResultGrid")
                    .flex_1()
                    .min_h_0()
                    .on_action(cx.listener(|t, _: &CopyCells, _, cx| {
                        if let Some(g) = t.active_grid() {
                            grid::copy_to_clipboard(g, cx);
                        }
                    }))
                    .when_some(g, |d, g| d.child(DataTable::new(&g).stripe(true).bordered(true)))
                    .into_any_element()
            }
            Pane::Messages => div().flex_1().min_h_0().child(self.render_messages(cx)).into_any_element(),
            Pane::Plan => div().flex_1().min_h_0().children(self.plan.clone()).into_any_element(),
        };

        v_flex()
            .key_context("SqlTab")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|t, _: &RunStatement, w, cx| t.run(false, w, cx)))
            .on_action(cx.listener(|t, _: &RunScript, w, cx| t.run(true, w, cx)))
            .on_action(cx.listener(|t, _: &CancelQuery, _, _| t.cancel()))
            .on_action(cx.listener(|t, _: &FormatSql, w, cx| t.format(w, cx)))
            .on_action(cx.listener(|t, _: &ExplainEstimate, w, cx| t.explain(false, w, cx)))
            .on_action(cx.listener(|t, _: &ExplainAnalyze, w, cx| t.explain(true, w, cx)))
            .on_action(cx.listener(|t, _: &TabKey, window, cx| {
                // O editor só consulta o menu no Enter; o Tab dele (IndentInline) indentaria.
                if t.editor.read(cx).completion_menu_state().open {
                    window.dispatch_action(Box::new(Enter { secondary: false, shift: false }), cx);
                } else {
                    window.dispatch_action(Box::new(IndentInline), cx);
                }
            }))
            .child(
                v_resizable("sql-split")
                    .child(resizable_panel().size(px(280.)).child(Editor::new(&self.editor).h_full()))
                    .child(resizable_panel().child(v_flex().size_full().children(tabs).child(content))),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .when(spinning, |d| d.child(Spinner::new().small()))
                    .when(matches!(self.active, Pane::Result(_)), |d| d.child(t!("grid.rows", n = format!("{rows}{}", if more || truncated { "+" } else { "" })).to_string()))
                    .when(truncated, |d| d.child(div().text_color(cx.theme().muted_foreground).child(t!("results.first_rows_only").to_string())))
                    .child(self.status.clone())
                    .when_some(grid_error, |d, e| d.child(e))
                    .when(self.running, |d| {
                        d.child(Button::new("cancel").label(format!("{} ⌘.", t!("common.cancel"))).ghost().on_click(cx.listener(|t, _, _, _| t.cancel())))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn statements_become_result_tabs_and_messages() {
        assert_eq!(panes(&[true], 1), (vec![0], false), "um statement: só a grade");
        assert_eq!(panes(&[true, false, true], 3), (vec![0, 2], true));
        assert_eq!(panes(&[false, false], 2), (vec![], true), "só DML: só mensagens");
        assert_eq!(panes(&[true, false], 3), (vec![0], true), "erro no 2º de 3: o 1º fica, mensagens aparecem");
    }

    #[::core::prelude::v1::test]
    fn only_the_last_executed_statement_keeps_its_cursor() {
        assert!(cursor_closed(0, 2), "SELECT seguido de UPDATE: o UPDATE fechou o cursor");
        assert!(!cursor_closed(1, 2));
        assert!(!cursor_closed(0, 1));
        assert!(cursor_closed(0, 2), "erro no 2º: o 1º também perdeu o cursor");
    }

    #[::core::prelude::v1::test]
    fn format_target_is_selection_or_everything() {
        assert_eq!(format_target("select 1; select 2", 0..0), 0..18);
        assert_eq!(format_target("select 1; select 2", 10..18), 10..18);
    }

    #[::core::prelude::v1::test]
    fn sql_head_joins_lines_and_caps_length() {
        assert_eq!(sql_head("\n  select *\n from t"), "select * from t", "SQL formatado: junta as linhas");
        assert_eq!(sql_head("SELECT\n  1 AS a,\n  'x' AS b"), "SELECT 1 AS a, 'x' AS b");
        assert_eq!(sql_head(&"x".repeat(200)).chars().count(), 81);
    }
}
