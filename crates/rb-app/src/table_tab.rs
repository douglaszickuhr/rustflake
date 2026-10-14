use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::table::{ColumnSort, DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::batch::ResultSet;
use rb_core::dialect::Dialect;
use rb_core::metadata::MetaRequest;
use rb_core::session::Session;
use rb_core::table::{EditTarget, PendingEdits, build_sql, columns_match, select_sql};

use crate::grid::{self, ResultGrid};
use crate::meta::{ConnTarget, MetaStore};
use crate::{CancelQuery, CopyCells, Db, SaveEdits};

pub struct TableTab {
    focus: FocusHandle,
    schema: String,
    table: String,
    conn: ConnTarget,
    meta: Entity<MetaStore>,
    session: Option<Arc<Session>>,
    filter: Entity<InputState>,
    grid: Entity<TableState<ResultGrid>>,
    target: Option<EditTarget>,
    sort: Option<(usize, bool)>,
    status: SharedString,
    /// Mensagem a mostrar quando a próxima carga terminar (ex.: resultado da gravação).
    after_load: Option<SharedString>,
    loaded_once: bool,
    _subs: Vec<Subscription>,
}

impl TableTab {
    pub fn new(schema: String, table: String, conn: ConnTarget, meta: Entity<MetaStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("filtro WHERE, ex.: id > 10 — Enter aplica"));
        let grid = cx.new(|cx| TableState::new(ResultGrid::new(true), window, cx).cell_selectable(true));

        let weak = cx.entity().downgrade();
        let on_sort: Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)> = Rc::new(move |col, sort, _, cx| {
            let weak = weak.clone();
            // A grade está em update agora; recarregar depois evita empréstimo duplo.
            cx.defer(move |cx| {
                weak.update(cx, |tab, cx| tab.set_sort(col, sort, cx)).ok();
            });
        });
        grid.update(cx, |t, _| t.delegate_mut().on_sort = Some(on_sort));

        let subs = vec![
            cx.subscribe_in(&filter, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.load(cx);
                }
            }),
            cx.subscribe_in(&grid, window, |this, _, ev: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(r, c) = ev {
                    this.edit_cell(*r, *c, window, cx);
                }
            }),
            cx.observe(&grid, |_, _, cx| cx.notify()),
            cx.observe(&meta, |this, _, cx| this.on_meta(cx)),
        ];
        meta.update(cx, |m, cx| m.request(MetaRequest::Columns(schema.clone(), table.clone()), cx));
        let mut this = TableTab {
            focus: cx.focus_handle(),
            schema,
            table,
            conn,
            meta,
            session: None,
            filter,
            grid,
            target: None,
            sort: None,
            status: "carregando…".into(),
            after_load: None,
            loaded_once: false,
            _subs: subs,
        };
        this.on_meta(cx);
        this
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    pub fn title(&self) -> SharedString {
        format!("{}.{}", self.schema, self.table).into()
    }

    /// Quando as colunas chegam: decide se a grade é editável e faz a primeira carga.
    fn on_meta(&mut self, cx: &mut Context<Self>) {
        if self.loaded_once {
            return;
        }
        let key = (self.schema.clone(), self.table.clone());
        let Some(cols) = self.meta.read(cx).cache.columns.get(&key).cloned() else { return };
        self.loaded_once = true;
        let target = EditTarget { schema: self.schema.clone(), table: self.table.clone(), columns: cols };
        // Snowflake não garante PK: a grade dele é sempre somente leitura.
        let editable = !target.pk().is_empty() && self.meta.read(cx).cache.dialect == Dialect::Postgres;
        self.grid.update(cx, |t, _| t.delegate_mut().edits = editable.then(PendingEdits::default));
        self.target = Some(target);
        self.load(cx);
    }

    pub fn has_pending(&self, cx: &App) -> bool {
        self.grid.read(cx).delegate().edits.as_ref().is_some_and(|e| !e.is_empty())
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if self.has_pending(cx) {
            self.status = "grave (⌘S) ou descarte as edições antes de recarregar".into();
            cx.notify();
            return;
        }
        let filter = self.filter.read(cx).value().to_string();
        let order_col = self.sort.and_then(|(c, asc)| {
            let name = self.target.as_ref()?.columns.get(c)?.name.clone();
            Some((name, asc))
        });
        let from = self.meta.read(cx).cache.dialect.qualified(&self.schema, &self.table);
        let sql = select_sql(&from, Some(&filter), order_col.as_ref().map(|(n, a)| (n.as_str(), *a)));
        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        self.status = "carregando…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let session = match existing {
                    Some(s) => s,
                    None => Arc::new(engine.connect(&conn.cfg, conn.creds.clone()).await?),
                };
                let out = session.execute(sql).await?;
                Ok::<_, rb_core::DbError>((session, out))
            }
            .await;
            this.update(cx, |tab, cx| {
                match result {
                    Ok((session, out)) => {
                        tab.session = Some(session.clone());
                        if tab.target.as_ref().is_some_and(|t| !columns_match(t, &out.columns)) {
                            // ALTER TABLE desde que a aba abriu: índices de coluna não batem mais.
                            tab.target = None;
                            tab.grid.update(cx, |t, _| t.delegate_mut().edits = None);
                            let (schema, table) = (tab.schema.clone(), tab.table.clone());
                            tab.meta.update(cx, |m, _| m.cache.invalidate_table(&schema, &table));
                            tab.after_load = Some("a estrutura da tabela mudou: edição desativada; reabra a aba".into());
                        }
                        tab.status = tab.after_load.take().unwrap_or_else(|| format!("{} ms", out.elapsed.as_millis()).into());
                        let exec_id = out.exec_id;
                        let rs = ResultSet::new(out.columns, out.batch, out.more);
                        tab.grid.update(cx, |t, cx| {
                            t.delegate_mut().set_result(rs, session, exec_id);
                            t.refresh(cx);
                            cx.notify();
                        });
                    }
                    Err(e) => tab.status = e.to_string().into(),
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn set_sort(&mut self, col: usize, sort: ColumnSort, cx: &mut Context<Self>) {
        self.sort = match sort {
            ColumnSort::Ascending => Some((col, true)),
            ColumnSort::Descending => Some((col, false)),
            ColumnSort::Default => None,
        };
        self.load(cx);
    }

    fn edit_cell(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = &self.target else { return };
        let g = self.grid.read(cx).delegate();
        if g.edits.is_none() || g.saving {
            return;
        }
        if target.columns.get(col).is_some_and(|c| c.generated) {
            self.status = "coluna gerada: não editável".into();
            cx.notify();
            return;
        }
        let current = self.grid.read(cx).delegate().full_text(row, col).unwrap_or_default();
        let col_name = target.columns.get(col).map_or(String::new(), |c| format!("{} ({})", c.name, c.type_name));
        let editor = cx.new(|cx| EditorState::new(window, cx).line_number(false).default_value(current));
        let grid = self.grid.clone();
        let set = Rc::new(move |value: Option<String>, window: &mut Window, cx: &mut App| {
            grid.update(cx, |t, cx| {
                let g = t.delegate_mut();
                let loaded = g.loaded_rows();
                if let Some(e) = &mut g.edits {
                    e.set(row, col, value, loaded);
                }
                cx.notify();
            });
            window.close_dialog(cx);
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let (ed, set_null, set_ok) = (editor.clone(), set.clone(), set.clone());
            dialog.title(col_name.clone()).w(px(560.)).child(Editor::new(&editor).h(px(220.))).footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cell-null").label("Definir NULL").on_click(move |_, w, cx| set_null(None, w, cx)))
                    .child(Button::new("cell-cancel").label("Cancelar").on_click(|_, w, cx| w.close_dialog(cx)))
                    .child(Button::new("cell-ok").label("OK").primary().on_click(move |_, w, cx| {
                        let v = ed.read(cx).value().to_string();
                        set_ok(Some(v), w, cx)
                    })),
            )
        });
    }

    fn selected_row(&self, cx: &App) -> Option<usize> {
        match self.grid.read(cx).selection() {
            TableSelection::Row(r) | TableSelection::Cell(r, _) => Some(r),
            _ => None,
        }
    }

    fn with_edits(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut PendingEdits, usize)) {
        self.grid.update(cx, |t, cx| {
            let g = t.delegate_mut();
            let loaded = g.loaded_rows();
            if let (false, Some(e)) = (g.saving, &mut g.edits) {
                f(e, loaded);
            }
            cx.notify();
        });
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(session)) = (self.target.clone(), self.session.clone()) else { return };
        if !self.grid.update(cx, |t, _| t.delegate_mut().start_save()) {
            return;
        }
        let g = self.grid.read(cx).delegate();
        let built = match (&g.edits, &g.rs) {
            (Some(edits), Some(rs)) => build_sql(&target, &edits.to_row_edits(&target, |r, c| rs.text(r, c))),
            _ => Err(rb_core::DbError::Edit("nada para gravar".into())),
        };
        let stmts = match built {
            Ok(s) => s,
            Err(e) => {
                self.grid.update(cx, |t, _| t.delegate_mut().finish_save(false));
                self.status = e.to_string().into();
                cx.notify();
                return;
            }
        };
        self.status = "gravando…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = session.run_in_transaction(stmts).await;
            this.update(cx, |tab, cx| match res {
                Ok(n) => {
                    tab.grid.update(cx, |t, _| t.delegate_mut().finish_save(true));
                    tab.after_load = Some(format!("{n} linha(s) gravada(s)").into());
                    tab.load(cx);
                }
                Err(e) => {
                    tab.grid.update(cx, |t, _| t.delegate_mut().finish_save(false));
                    tab.status = format!("Erro ao gravar — nada foi alterado: {e}").into();
                    cx.notify();
                }
            })
        })
        .detach();
    }

    fn cancel(&mut self) {
        if let Some(s) = &self.session {
            s.cancel();
        }
    }
}

impl Render for TableTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.grid.read(cx).delegate();
        let editable = g.edits.is_some();
        let pending = g.edits.as_ref().is_some_and(|e| !e.is_empty());
        let rows = g.loaded_rows();
        let more = g.rs.as_ref().is_some_and(|r| r.more);
        let read_only = !editable && self.target.is_some();
        let sf = self.meta.read(cx).cache.dialect == Dialect::Snowflake;
        v_flex()
            .key_context("TableTab")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|t, _: &SaveEdits, _, cx| t.save(cx)))
            .on_action(cx.listener(|t, _: &CancelQuery, _, _| t.cancel()))
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.filter)))
                    .child(Button::new("reload").label("Recarregar").ghost().on_click(cx.listener(|t, _, _, cx| t.load(cx))))
                    .when(editable, |d| {
                        d.child(Button::new("add-row").label("+ Linha").ghost().on_click(cx.listener(|t, _, _, cx| t.with_edits(cx, |e, _| e.add_row()))))
                            .child(Button::new("del-row").label("Excluir linha").ghost().on_click(cx.listener(|t, _, _, cx| {
                                if let Some(r) = t.selected_row(cx) {
                                    t.with_edits(cx, |e, loaded| e.toggle_delete(r, loaded));
                                }
                            })))
                            .child(Button::new("discard").label("Descartar").ghost().on_click(cx.listener(|t, _, _, cx| {
                                t.with_edits(cx, |e, _| *e = PendingEdits::default());
                            })))
                            .child(Button::new("save").label("Salvar ⌘S").primary().disabled(!pending).on_click(cx.listener(|t, _, _, cx| t.save(cx))))
                    }),
            )
            .child(
                div()
                    .key_context("ResultGrid")
                    .flex_1()
                    .on_action(cx.listener(|t, _: &CopyCells, _, cx| grid::copy_to_clipboard(&t.grid, cx)))
                    .child(DataTable::new(&self.grid).stripe(true).bordered(true)),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .child(format!("{rows}{} linha(s)", if more { "+" } else { "" }))
                    .when(pending, |d| d.child(div().text_color(cx.theme().warning).child("edições pendentes")))
                    .when(read_only, |d| {
                        let msg = if sf { "Snowflake: somente leitura" } else { "somente leitura: tabela sem chave primária" };
                        d.child(div().text_color(cx.theme().muted_foreground).child(msg))
                    })
                    .child(self.status.clone()),
            )
    }
}
