use rust_i18n::t;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{ColumnSort, DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::batch::ResultSet;
use rb_core::dialect::Dialect;
use rb_core::metadata::MetaRequest;
use rb_core::org_config::TabKind;
use rb_core::session::Session;
use rb_core::table::{EditTarget, PendingEdits, TABLE_LIMIT, build_sql, columns_match, select_sql};

use std::collections::HashMap;

use rb_core::table_info::{ColInfo, Facts, LinNode};

use crate::dbt_store::DbtStore;
use crate::grid::{self, ResultGrid};
use crate::lang::Lang;
use crate::meta::{ConnTarget, MetaStore};
use crate::ui::Load;
use crate::{CancelQuery, CopyCells, Db, SaveEdits};

/// Opens a SQL tab with the text (Query button).
pub type OpenQuery = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Runs `run` on a new session of `conn` and hands the result to `done`; each call has its own session,
/// so two calls run in parallel (one session runs its commands one at a time).
pub(crate) fn spawn_query<V: 'static, T: 'static>(
    conn: ConnTarget,
    cx: &mut Context<V>,
    run: impl AsyncFnOnce(&Session) -> Result<T, rb_core::DbError> + 'static,
    done: impl FnOnce(&mut V, Result<T, rb_core::DbError>, &mut Context<V>) + 'static,
) {
    let engine = cx.global::<Db>().0.clone();
    cx.spawn(async move |this, cx| {
        let res = match engine.connect(&conn.cfg, conn.creds.clone()).await {
            Ok(s) => run(&s).await,
            Err(e) => Err(e),
        };
        this.update(cx, |t, cx| {
            done(t, res, cx);
            cx.notify();
        })
        .ok();
    })
    .detach();
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Data,
    Definition,
    Dbt,
    Lineage,
    /// Tab from the configuration (index into `custom`).
    Custom(usize),
}

/// Custom tab from the configuration: SQL (own grid, runs on open) or URL.
pub(crate) struct CustomSection {
    pub(crate) name: SharedString,
    pub(crate) kind: TabKind,
    pub(crate) grid: Option<Entity<TableState<ResultGrid>>>,
    pub(crate) status: SharedString,
    pub(crate) busy: bool,
    pub(crate) opened: bool,
}

/// Columns, classification and masking coming from Snowflake (dbt is merged at draw time).
#[derive(Clone)]
pub(crate) struct DefRaw {
    pub(crate) cols: Vec<ColInfo>,
    /// Column → (level, tag) of the configured classification tag.
    pub(crate) class_tags: HashMap<String, (u8, String)>,
    /// Column → masking policy.
    pub(crate) masking: HashMap<String, String>,
    /// Column → its other tags (name, value).
    pub(crate) other_tags: HashMap<String, Vec<(String, String)>>,
    /// Tags or policies didn't load (shown in the summary instead of "nothing classified").
    pub(crate) tag_error: Option<String>,
}

/// Model unique_id and its compiled SQL with when it was generated.
pub(crate) type Compiled = (String, Load<(String, Option<std::time::SystemTime>)>);

pub struct TableTab {
    focus: FocusHandle,
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) conn: ConnTarget,
    pub(crate) meta: Entity<MetaStore>,
    session: Option<Arc<Session>>,
    filter: Entity<InputState>,
    grid: Entity<TableState<ResultGrid>>,
    target: Option<EditTarget>,
    sort: Option<(usize, bool)>,
    status: SharedString,
    /// Message to show when the next load finishes (e.g. save result).
    after_load: Option<SharedString>,
    loaded_once: bool,
    /// Load in progress (turns on the grid skeleton after a moment).
    _busy: Option<Task<()>>,
    // Table view (Snowflake): Data | Definition | dbt sections.
    pub(crate) section: Section,
    pub(crate) dbt: Option<Entity<DbtStore>>,
    pub(crate) open_query: OpenQuery,
    pub(crate) open_table: crate::schema_tree::OpenTable,
    /// Created the first time the Lineage section opens.
    pub(crate) lineage: Option<Entity<crate::lineage_view::LineageView>>,
    pub(crate) facts: Option<Load<Facts>>,
    pub(crate) def_raw: Option<Load<DefRaw>>,
    pub(crate) selected_col: Option<String>,
    pub(crate) col_lineage: HashMap<String, Load<Vec<LinNode>>>,
    pub(crate) show_compiled: bool,
    /// Table comment expanded in full in the header.
    pub(crate) comment_open: bool,
    /// git blame of the model file (loaded when Blame is turned on).
    pub(crate) blame: Option<(String, Load<Vec<rb_core::dbt::BlameLine>>)>,
    pub(crate) show_blame: bool,
    /// Compiled model SQL (disk or `dbt compile`), by unique_id, with when it was generated.
    pub(crate) compiled: Option<Compiled>,
    pub(crate) compiling: bool,
    /// Last lines of the running `dbt compile`.
    pub(crate) compile_live: Vec<String>,
    /// Model SQL (read-only): editor state and which text it holds.
    pub(crate) sql_view: Option<(Entity<EditorState>, String)>,
    pub(crate) custom: Vec<CustomSection>,
    /// Data as selectable text instead of the grid.
    text_mode: bool,
    text_view: grid::TextView,
    _subs: Vec<Subscription>,
}

impl TableTab {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        schema: String,
        table: String,
        conn: ConnTarget,
        meta: Entity<MetaStore>,
        dbt: Option<Entity<DbtStore>>,
        open_query: OpenQuery,
        open_table: crate::schema_tree::OpenTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t!("table.filter_placeholder").to_string()));
        let grid = cx.new(|cx| TableState::new(ResultGrid::new(true), window, cx).cell_selectable(true));

        let weak = cx.entity().downgrade();
        let on_sort: grid::OnSort = Rc::new(move |col, sort, _, cx| {
            let weak = weak.clone();
            // The grid is mid-update now; reloading later avoids a double borrow.
            cx.defer(move |cx| {
                weak.update(cx, |tab, cx| tab.set_sort(col, sort, cx)).ok();
            });
        });
        grid.update(cx, |t, _| t.delegate_mut().on_sort = Some(on_sort));

        let subs = vec![
            // The placeholder lives in the field state: it changes along with the language.
            cx.observe_global_in::<Lang>(window, |this, window, cx| {
                this.filter.update(cx, |s, cx| s.set_placeholder(t!("table.filter_placeholder").to_string(), window, cx));
            }),
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
        let (db, sch) = schema.split_once('.').unwrap_or(("", &schema));
        let custom: Vec<CustomSection> = conn
            .org
            .tabs_for(db, sch, &table)
            .into_iter()
            .map(|t| {
                let kind = t.resolve(db, sch, &table);
                let grid = matches!(kind, TabKind::Sql(_)).then(|| cx.new(|cx| TableState::new(ResultGrid::new(false), window, cx).cell_selectable(true)));
                CustomSection { name: t.name.into(), kind, grid, status: SharedString::default(), busy: false, opened: false }
            })
            .collect();
        let mut subs = subs;
        subs.extend(custom.iter().filter_map(|c| c.grid.as_ref()).map(|g| cx.observe(g, |_, _, cx| cx.notify())));
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
            status: t!("tree.loading").to_string().into(),
            after_load: None,
            loaded_once: false,
            _busy: None,
            section: Section::Data,
            dbt: dbt.clone(),
            open_query,
            open_table,
            lineage: None,
            facts: None,
            def_raw: None,
            selected_col: None,
            col_lineage: HashMap::new(),
            show_compiled: false,
            comment_open: false,
            blame: None,
            show_blame: false,
            compiled: None,
            compiling: false,
            compile_live: Vec::new(),
            sql_view: None,
            custom,
            text_mode: false,
            text_view: grid::TextView::default(),
            _subs: subs,
        };
        if let Some(d) = &dbt {
            this._subs.push(cx.observe(d, |_, _, cx| cx.notify()));
        }
        this.on_meta(cx);
        this.ensure_facts(cx);
        this
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    pub fn title(&self) -> SharedString {
        format!("{}.{}", self.schema, self.table).into()
    }

    /// When the columns arrive: decides whether the grid is editable and does the first load.
    fn on_meta(&mut self, cx: &mut Context<Self>) {
        if self.loaded_once {
            return;
        }
        let key = (self.schema.clone(), self.table.clone());
        let Some(cols) = self.meta.read(cx).cache.columns.get(&key).cloned() else { return };
        self.loaded_once = true;
        let target = EditTarget { schema: self.schema.clone(), table: self.table.clone(), columns: cols };
        // Snowflake doesn't enforce PKs: its grid is always read-only.
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
            self.status = t!("table.save_before_reload").to_string().into();
            cx.notify();
            return;
        }
        let filter = self.filter.read(cx).value().to_string();
        let order_col = self.sort.and_then(|(c, asc)| {
            let name = self.target.as_ref()?.columns.get(c)?.name.clone();
            Some((name, asc))
        });
        let from = self.meta.read(cx).cache.dialect.qualified(&self.schema, &self.table);
        let limit = (self.meta.read(cx).cache.dialect == Dialect::Snowflake).then_some(TABLE_LIMIT);
        let sql = select_sql(&from, Some(&filter), order_col.as_ref().map(|(n, a)| (n.as_str(), *a)), limit);
        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        self.status = t!("tree.loading").to_string().into();
        self._busy = Some(grid::busy_later(&self.grid, cx));
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
                tab._busy = None;
                grid::set_idle(&tab.grid, cx);
                match result {
                    Ok((session, out)) => {
                        tab.session = Some(session.clone());
                        if tab.target.as_ref().is_some_and(|t| !columns_match(t, &out.columns)) {
                            // ALTER TABLE since the tab opened: column indices no longer match.
                            tab.target = None;
                            tab.grid.update(cx, |t, _| t.delegate_mut().edits = None);
                            let (schema, table) = (tab.schema.clone(), tab.table.clone());
                            tab.meta.update(cx, |m, _| m.cache.invalidate_table(&schema, &table));
                            tab.after_load = Some(t!("table.structure_changed").to_string().into());
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

    /// Runs custom tab `i`'s SQL on the grid's session (connects if there is none yet).
    pub(crate) fn load_custom(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(c) = self.custom.get_mut(i) else { return };
        let (TabKind::Sql(sql), Some(grid)) = (c.kind.clone(), c.grid.clone()) else { return };
        c.busy = true;
        c.status = t!("tree.loading").to_string().into();
        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
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
                let c = &mut tab.custom[i];
                c.busy = false;
                match result {
                    Ok((session, out)) => {
                        c.status = format!("{} ms", out.elapsed.as_millis()).into();
                        tab.session.get_or_insert(session.clone());
                        let exec_id = out.exec_id;
                        let rs = ResultSet::new(out.columns, out.batch, out.more);
                        grid.update(cx, |t, cx| {
                            t.delegate_mut().set_result(rs, session, exec_id);
                            t.refresh(cx);
                            cx.notify();
                        });
                    }
                    Err(e) => c.status = e.to_string().into(),
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn render_custom(&mut self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(c) = self.custom.get(i) else { return div().into_any_element() };
        let bar = h_flex().p_2().gap_2().items_center();
        match (&c.kind, &c.grid) {
            (TabKind::Url(u), _) => {
                let u = u.clone();
                v_flex()
                    .p_4()
                    .gap_2()
                    .child(div().text_sm().text_color(theme.muted_foreground).child(u.clone()))
                    .child(div().child(Button::new("tv-custom-open").label(t!("tv.open_browser").to_string()).outline().small().on_click(move |_, _, cx| cx.open_url(&u))))
                    .into_any_element()
            }
            (TabKind::Sql(sql), Some(grid)) => {
                let sql = sql.clone();
                let open_query = self.open_query.clone();
                let rows = grid.read(cx).delegate().loaded_rows();
                v_flex()
                    .size_full()
                    .child(
                        bar.child(Button::new("tv-custom-reload").label(t!("table.reload").to_string()).ghost().on_click(cx.listener(move |t, _, _, cx| t.load_custom(i, cx))))
                            .child(Button::new("tv-custom-query").label(t!("tv.query").to_string()).ghost().on_click(move |_, window, cx| open_query(sql.clone(), window, cx))),
                    )
                    .child(
                        div()
                            .key_context("ResultGrid")
                            .flex_1()
                            .min_h_0()
                            .on_action({
                                let grid = grid.clone();
                                move |_: &CopyCells, _, cx| grid::copy_to_clipboard(&grid, cx)
                            })
                            .child(DataTable::new(grid).stripe(true).bordered(true)),
                    )
                    .child(h_flex().px_2().py_1().gap_3().text_sm().when(c.busy, |d| d.child(Spinner::new().small())).child(t!("grid.rows", n = rows).to_string()).child(c.status.clone()))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
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
            self.status = t!("table.generated_column").to_string().into();
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
                    .child(Button::new("cell-null").label(t!("table.set_null").to_string()).on_click(move |_, w, cx| set_null(None, w, cx)))
                    .child(Button::new("cell-cancel").label(t!("common.cancel").to_string()).on_click(|_, w, cx| w.close_dialog(cx)))
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
            _ => Err(rb_core::DbError::Edit(t!("table.nothing_to_save").to_string())),
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
        self.status = t!("table.saving").to_string().into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = session.run_in_transaction(stmts).await;
            this.update(cx, |tab, cx| match res {
                Ok(n) => {
                    tab.grid.update(cx, |t, _| t.delegate_mut().finish_save(true));
                    tab.after_load = Some(t!("table.rows_saved", n = n).to_string().into());
                    tab.load(cx);
                }
                Err(e) => {
                    tab.grid.update(cx, |t, _| t.delegate_mut().finish_save(false));
                    tab.status = t!("table.save_failed", error = e).to_string().into();
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sf = self.meta.read(cx).cache.dialect == Dialect::Snowflake;
        if !sf {
            return self.render_data(window, cx);
        }
        let content = match self.section {
            Section::Data => self.render_data(window, cx),
            Section::Definition => self.render_definition(cx),
            Section::Dbt => self.render_dbt(window, cx),
            Section::Lineage => self.lineage.clone().map_or_else(|| div().into_any_element(), |l| l.into_any_element()),
            Section::Custom(i) => self.render_custom(i, cx),
        };
        v_flex().size_full().child(self.render_header(cx)).child(div().flex_1().min_h_0().child(content)).into_any_element()
    }
}

impl TableTab {
    fn render_data(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = self.grid.read(cx).delegate();
        let editable = g.edits.is_some();
        let pending = g.edits.as_ref().is_some_and(|e| !e.is_empty());
        let rows = g.loaded_rows();
        let more = g.rs.as_ref().is_some_and(|r| r.more);
        let spinning = self._busy.is_some() || g.loading_more();
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
                    .child(Button::new("reload").label(t!("table.reload").to_string()).ghost().on_click(cx.listener(|t, _, _, cx| t.load(cx))))
                    .when(editable, |d| {
                        d.child(Button::new("add-row").label(format!("+ {}", t!("table.row"))).ghost().on_click(cx.listener(|t, _, _, cx| t.with_edits(cx, |e, _| e.add_row()))))
                            .child(Button::new("del-row").label(t!("table.delete_row").to_string()).ghost().on_click(cx.listener(|t, _, _, cx| {
                                if let Some(r) = t.selected_row(cx) {
                                    t.with_edits(cx, |e, loaded| e.toggle_delete(r, loaded));
                                }
                            })))
                            .child(Button::new("discard").label(t!("table.discard").to_string()).ghost().on_click(cx.listener(|t, _, _, cx| {
                                t.with_edits(cx, |e, _| *e = PendingEdits::default());
                            })))
                            .child(Button::new("save").label(format!("{} ⌘S", t!("common.save"))).primary().disabled(!pending).on_click(cx.listener(|t, _, _, cx| t.save(cx))))
                    }),
            )
            .child(div().key_context("ResultGrid").flex_1().on_action(cx.listener(|t, _: &CopyCells, _, cx| grid::copy_to_clipboard(&t.grid, cx))).child(if self.text_mode {
                self.text_view.render(&self.grid, window, cx)
            } else {
                DataTable::new(&self.grid).stripe(true).bordered(true).into_any_element()
            }))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .when(spinning, |d| d.child(Spinner::new().small()))
                    .child(grid::mode_toggle(self.text_mode, |t: &mut Self, v| t.text_mode = v, cx))
                    .child(t!("grid.rows", n = format!("{rows}{}", if more { "+" } else { "" })).to_string())
                    // Hit the tab's LIMIT: the table may have more; to see everything, use SQL in the editor.
                    .when(sf && rows >= TABLE_LIMIT, |d| d.child(div().text_color(cx.theme().muted_foreground).child(t!("table.limited", n = TABLE_LIMIT).to_string())))
                    .when(pending, |d| d.child(div().text_color(cx.theme().warning).child(t!("table.pending_edits").to_string())))
                    .when(read_only, |d| {
                        let msg = if sf { t!("table.sf_read_only") } else { t!("table.read_only_no_pk") }.to_string();
                        d.child(div().text_color(cx.theme().muted_foreground).child(msg))
                    })
                    .child(self.status.clone()),
            )
            .into_any_element()
    }
}
