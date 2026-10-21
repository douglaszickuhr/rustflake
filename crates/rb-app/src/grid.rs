use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableSelection, TableState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::batch::ResultSet;
use rb_core::decode::one_line;
use rb_core::query_edit::Filter;
use rb_core::session::Session;
use rb_core::table::PendingEdits;
use rust_i18n::t;

pub const MAX_CELL_CHARS: usize = 200;
/// A query faster than this doesn't show the skeleton: the old result stays until the new one arrives.
const BUSY_DELAY: Duration = Duration::from_millis(150);

pub type OnSort = Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)>;

/// A query asked for from a cell's right-click menu; the grid's owner knows the SQL behind it.
#[derive(Clone)]
pub enum Derive {
    CountBy(Vec<String>),
    Distinct(String),
    Range(String),
    Filter(String, Filter),
}

fn menu_item(f: &OnDerive, label: String, d: Derive) -> PopupMenuItem {
    let f = f.clone();
    PopupMenuItem::new(label).on_click(move |_, window, cx| f(d.clone(), window, cx))
}

/// min/max make sense here. Snowflake results come as TEXT (types aren't decoded), so it's always offered there.
fn orderable(t: &rb_core::Type) -> bool {
    use rb_core::Type as T;
    [T::TEXT, T::VARCHAR, T::BPCHAR, T::NAME, T::INT2, T::INT4, T::INT8, T::NUMERIC, T::FLOAT4, T::FLOAT8, T::MONEY, T::DATE, T::TIME, T::TIMESTAMP, T::TIMESTAMPTZ, T::INTERVAL].contains(t)
}
pub type OnDerive = Rc<dyn Fn(Derive, &mut Window, &mut App)>;

/// The grid as drawn everywhere, with the right-click menu. The table's own menu only knows rows,
/// and with selectable cells it never gets one, so the menu lives on a wrapper around it.
pub fn view(grid: &Entity<TableState<ResultGrid>>) -> impl IntoElement {
    let g = grid.clone();
    // The grid's real width: when it changes, the last column is stretched again to fill it.
    let measure = canvas(
        {
            let g = grid.clone();
            move |b, _, cx| {
                let now = f32::from(b.size.width);
                if (g.read(cx).delegate().avail - now).abs() > 0.5 {
                    // Not while the table is being drawn: right after this frame.
                    let g = g.clone();
                    cx.defer(move |cx| {
                        g.update(cx, |t, cx| {
                            t.delegate_mut().avail = now;
                            t.refresh(cx);
                            cx.notify();
                        })
                    });
                }
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full();
    div()
        .id("grid-menu")
        .relative()
        .size_full()
        .child(measure)
        // Capture runs parent first: forget the last header here, the header's own listener sets it again.
        .capture_any_mouse_down({
            let g = grid.clone();
            move |_, _, cx| g.update(cx, |t, _| t.delegate_mut().menu_header = None)
        })
        .child(DataTable::new(grid).stripe(true).bordered(true))
        .context_menu(move |menu, _, cx| {
            let t = g.read(cx);
            if let Some(col) = t.delegate().menu_header {
                return t.delegate().header_menu(menu, col);
            }
            // The table records the right-clicked cell itself (column 0 is the row number).
            match t.right_clicked_cell().and_then(|(r, c)| Some((r, c.checked_sub(ROW_NO)?))) {
                Some(cell) => t.delegate().derive_menu(menu, cell),
                None => menu,
            }
        })
}

/// Widest a column starts, in characters: 60 when there are many, more when few columns share the
/// window (one JSON column shouldn't stop at 60). ponytail: ~1400px budget, not the real grid width.
fn width_cap(columns: usize) -> usize {
    (160 / columns.max(1)).clamp(60, MAX_CELL_CHARS)
}

pub struct ResultGrid {
    pub rs: Option<ResultSet>,
    pub edits: Option<PendingEdits>,
    pub sortable: bool,
    pub on_sort: Option<OnSort>,
    /// Without it the right-click menu stays empty.
    pub on_derive: Option<OnDerive>,
    /// Width the grid had in the last frame (0 until measured); the last column fills what's left.
    avail: f32,
    /// Header (data column) under the current right click; the table only records cells.
    menu_header: Option<usize>,
    /// Columns marked with ⌘-click on the header, for a count grouped by all of them.
    group_cols: Vec<usize>,
    pub error: Option<String>,
    /// Save in progress: blocks new saves, edits and paging.
    pub saving: bool,
    session: Option<Arc<Session>>,
    exec_id: u64,
    widths: Vec<f32>,
    loading_more: bool,
    /// Query in progress: the `DataTable` swaps the rows for the animated skeleton.
    busy: bool,
    /// Changes with each new result; discards batches arriving from an old result.
    generation: u64,
}

impl ResultGrid {
    pub fn new(sortable: bool) -> Self {
        ResultGrid {
            rs: None,
            edits: None,
            sortable,
            on_sort: None,
            on_derive: None,
            menu_header: None,
            avail: 0.,
            group_cols: Vec::new(),
            error: None,
            saving: false,
            session: None,
            exec_id: 0,
            widths: Vec::new(),
            loading_more: false,
            busy: false,
            generation: 0,
        }
    }

    /// Row-number column: wide enough for the biggest number shown.
    fn row_no_width(&self, cx: &App) -> f32 {
        self.rows_count(cx).max(1).to_string().len() as f32 * 8. + 20.
    }

    /// Width a data column is drawn at: its estimate, the last one stretched over the free space.
    fn shown_width(&self, col: usize, cx: &App) -> f32 {
        let est = self.widths.get(col).copied().unwrap_or(120.);
        if col + 1 != self.widths.len() {
            return est;
        }
        // Room for the vertical scrollbar, so the fill doesn't add a horizontal one.
        let used: f32 = self.row_no_width(cx) + self.widths[..col].iter().sum::<f32>() + 16.;
        est.max(self.avail - used)
    }

    /// Right-click menu for a header: what applies to the whole column.
    fn header_menu(&self, menu: PopupMenu, col: usize) -> PopupMenu {
        let (Some(f), Some(rs)) = (self.on_derive.as_ref(), self.rs.as_ref()) else { return menu };
        let Some(info) = rs.columns.get(col) else { return menu };
        let name = info.name.clone();
        // Columns marked with ⌘-click, plus this one, in table order.
        let mut group = self.group_cols.clone();
        if !group.contains(&col) {
            group.push(col);
        }
        group.sort_unstable();
        let names: Vec<String> = group.iter().filter_map(|c| rs.columns.get(*c)).map(|c| c.name.clone()).collect();
        let menu = if names.len() > 1 { menu.item(menu_item(f, t!("grid.count_by", col = names.join(", ")).to_string(), Derive::CountBy(names))) } else { menu };
        menu.item(menu_item(f, t!("grid.count_by", col = name).to_string(), Derive::CountBy(vec![name.clone()])))
            .item(menu_item(f, t!("grid.distinct", col = name).to_string(), Derive::Distinct(name.clone())))
            .when(orderable(&info.type_), |m| m.item(menu_item(f, t!("grid.range", col = name).to_string(), Derive::Range(name.clone()))))
    }

    /// Right-click menu for a cell (data column): count by its column, filter by its value.
    fn derive_menu(&self, menu: PopupMenu, (row, col): (usize, usize)) -> PopupMenu {
        let Some(f) = self.on_derive.clone() else { return menu };
        let Some(name) = self.rs.as_ref().and_then(|rs| rs.columns.get(col)).map(|c| c.name.clone()) else { return menu };
        let item = |label: String, d: Derive| menu_item(&f, label, d);
        let menu = menu.item(item(t!("grid.count_by", col = name).to_string(), Derive::CountBy(vec![name.clone()]))).separator();
        match self.full_text(row, col) {
            Some(v) => {
                let shown = one_line(&v, 40);
                menu.item(item(t!("grid.only", col = name, v = shown).to_string(), Derive::Filter(name.clone(), Filter::Eq(v.clone()))))
                    .item(item(t!("grid.exclude", col = name, v = shown).to_string(), Derive::Filter(name.clone(), Filter::Ne(v))))
                    .item(item(t!("grid.not_null", col = name).to_string(), Derive::Filter(name, Filter::NotNull)))
            }
            None => menu
                .item(item(t!("grid.null", col = name).to_string(), Derive::Filter(name.clone(), Filter::IsNull)))
                .item(item(t!("grid.not_null", col = name).to_string(), Derive::Filter(name, Filter::NotNull))),
        }
    }

    pub fn set_result(&mut self, rs: ResultSet, session: Arc<Session>, exec_id: u64) {
        self.generation += 1;
        self.exec_id = exec_id;
        self.group_cols.clear();
        let cap = width_cap(rs.columns.len());
        self.widths = (0..rs.columns.len())
            .map(|c| {
                let sample = (0..rs.rows().min(100)).map(|r| rs.display(r, c, cap).map_or(4, |s| s.chars().count())).max().unwrap_or(0);
                let chars = sample.max(rs.columns[c].name.chars().count()).clamp(4, cap);
                chars as f32 * 8.5 + 24.
            })
            .collect();
        self.rs = Some(rs);
        self.session = Some(session);
        self.loading_more = false;
        self.error = None;
    }

    pub fn loading_more(&self) -> bool {
        self.loading_more
    }

    /// The session cursor is about to close (another command will run): stops paging. Returns whether there was more.
    pub fn close_cursor(&mut self) -> bool {
        self.rs.as_mut().is_some_and(|rs| std::mem::replace(&mut rs.more, false))
    }

    /// Can it fetch more rows? Not while there are inserted rows (they are addressed
    /// right after the loaded ones and would change index) nor during a save.
    pub fn can_page(&self) -> bool {
        self.rs.as_ref().is_some_and(|r| r.more) && !self.saving && self.edits.as_ref().is_none_or(|e| e.inserted_rows() == 0)
    }

    /// Starts a save if there are edits and none other in progress.
    pub fn start_save(&mut self) -> bool {
        if self.saving || self.edits.as_ref().is_none_or(PendingEdits::is_empty) {
            return false;
        }
        self.saving = true;
        true
    }

    /// Ends the save; pending edits are cleared only on success.
    pub fn finish_save(&mut self, ok: bool) {
        self.saving = false;
        if ok {
            self.edits = Some(PendingEdits::default());
        }
    }

    pub fn loaded_rows(&self) -> usize {
        self.rs.as_ref().map_or(0, ResultSet::rows)
    }

    /// (text, is NULL, was edited, row marked for deletion)
    /// Text drawn in a cell, cut at `max` characters (enough to fill its width; the rest is clipped).
    fn cell(&self, row: usize, col: usize, max: usize) -> (SharedString, bool, bool, bool) {
        let loaded = self.loaded_rows();
        let deleted = self.edits.as_ref().is_some_and(|e| e.is_deleted(row));
        if let Some(v) = self.edits.as_ref().and_then(|e| e.get(row, col, loaded)) {
            return match v {
                Some(s) => (one_line(s, max).into(), false, true, deleted),
                None => ("NULL".into(), true, true, deleted),
            };
        }
        if row >= loaded {
            return ("DEFAULT".into(), true, false, false);
        }
        match self.rs.as_ref().and_then(|rs| rs.display(row, col, max)) {
            Some(s) => (s.into(), false, false, deleted),
            None => ("NULL".into(), true, false, deleted),
        }
    }

    /// Full value of a loaded cell (for copy and edit).
    pub fn full_text(&self, row: usize, col: usize) -> Option<String> {
        let loaded = self.loaded_rows();
        if let Some(v) = self.edits.as_ref().and_then(|e| e.get(row, col, loaded)) {
            return v.clone();
        }
        self.rs.as_ref().filter(|_| row < loaded).and_then(|rs| rs.text(row, col))
    }

    /// `sel` in table columns: column 0 is the row number (see [`ROW_NO`]).
    pub fn copy_selection(&self, sel: TableSelection) -> Option<String> {
        let rs = self.rs.as_ref()?;
        let all: Vec<usize> = (0..rs.columns.len()).collect();
        match sel {
            TableSelection::Cell(r, 0) => Some((r + 1).to_string()),
            TableSelection::Cell(r, c) => Some(self.full_text(r, c - ROW_NO).unwrap_or_default()),
            TableSelection::Row(r) if r < rs.rows() => Some(rs.to_tsv(r..r + 1, &all)),
            TableSelection::Column(c) if c >= ROW_NO => Some(rs.to_tsv(0..rs.rows(), &[c - ROW_NO])),
            _ => None,
        }
    }
}

/// Columns before the data in the table: the pinned row number. Table column `c` is data column `c - ROW_NO`.
pub const ROW_NO: usize = 1;

pub fn copy_to_clipboard(table: &Entity<TableState<ResultGrid>>, cx: &mut App) {
    let t = table.read(cx);
    if let Some(text) = t.delegate().copy_selection(t.selection()) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.rs.as_ref().map_or(0, |rs| rs.columns.len() + ROW_NO)
    }

    fn rows_count(&self, _: &App) -> usize {
        self.loaded_rows() + self.edits.as_ref().map_or(0, PendingEdits::inserted_rows)
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let name = self.column(col_ix, cx).name.clone();
        let marked = col_ix.checked_sub(ROW_NO).is_some_and(|c| self.group_cols.contains(&c));
        div()
            .size_full()
            .when(marked, |d| d.text_color(cx.theme().primary).font_weight(FontWeight::SEMIBOLD))
            .when_some(col_ix.checked_sub(ROW_NO), |d, col| {
                d.capture_any_mouse_down(cx.listener(move |t, e: &MouseDownEvent, _, cx| {
                    if e.button == MouseButton::Right {
                        t.delegate_mut().menu_header = Some(col);
                    } else if e.button == MouseButton::Left && e.modifiers.secondary() {
                        // ⌘-click (Ctrl elsewhere) marks the column for group by, instead of sorting or selecting it.
                        let g = &mut t.delegate_mut().group_cols;
                        match g.iter().position(|c| *c == col) {
                            Some(i) => _ = g.remove(i),
                            None => g.push(col),
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
            })
            .child(name)
    }

    fn column(&self, col_ix: usize, cx: &App) -> Column {
        if col_ix == 0 {
            return Column::new("row-no", "#").width(px(self.row_no_width(cx))).fixed_left().resizable(false).movable(false).text_right();
        }
        let col_ix = col_ix - ROW_NO;
        let name = self.rs.as_ref().map_or(String::new(), |rs| rs.columns[col_ix].name.clone());
        let col = Column::new(format!("c{col_ix}"), name).width(px(self.shown_width(col_ix, cx)));
        if self.sortable { col.sortable() } else { col }
    }

    fn render_td(&mut self, row: usize, col: usize, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        if col == 0 {
            return div().size_full().text_right().text_color(cx.theme().muted_foreground).child((row + 1).to_string()).into_any_element();
        }
        let id_col = col;
        let col = col - ROW_NO;
        // ponytail: width estimated at ~7px per character; measure the text if the guess is way off.
        let width = self.shown_width(col, cx);
        // Enough characters to reach the right edge (narrow ones like `i` included); the rest is clipped.
        let (text, null, edited, deleted) = self.cell(row, col, ((width / 5.) as usize).max(MAX_CELL_CHARS));
        let clipped = text.chars().count() as f32 * 7. > width - 12.;
        // The full value (and its pretty JSON) is built on hover, not for every clipped cell every frame.
        let table = (!null && clipped).then(|| cx.entity().downgrade());
        let cols = self.columns_count(cx);
        div()
            .id(("cell", row * cols + id_col))
            .when_some(table, |d, table| {
                // Hoverable: the pointer can move into the card to scroll it or copy the value.
                d.hoverable_tooltip(move |window, cx| {
                    let full = table.upgrade().and_then(|t| t.read(cx).delegate().full_text(row, col)).unwrap_or_default();
                    cx.new(|cx| crate::cell_detail::CellDetail::new(full, window, cx)).into()
                })
            })
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .when(null, |d| d.italic().text_color(cx.theme().muted_foreground))
            .when(edited, |d| d.bg(cx.theme().accent))
            .when(deleted, |d| d.line_through().text_color(cx.theme().danger))
            .child(text)
            .into_any_element()
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        if col == 0 { (row + 1).to_string() } else { self.full_text(row, col - ROW_NO).unwrap_or_default() }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        if let (Some(f), Some(c)) = (self.on_sort.clone(), col_ix.checked_sub(ROW_NO)) {
            f(c, sort, window, cx);
        }
    }

    fn loading(&self, _: &App) -> bool {
        self.busy
    }

    fn has_more(&self, _: &App) -> bool {
        self.can_page()
    }

    fn load_more_threshold(&self) -> usize {
        200
    }

    fn load_more(&mut self, _window: &mut Window, cx: &mut Context<TableState<Self>>) {
        if self.loading_more {
            return;
        }
        let Some(session) = self.session.clone() else { return };
        self.loading_more = true;
        let (generation, exec_id) = (self.generation, self.exec_id);
        cx.spawn(async move |table, cx| {
            let res = session.fetch_more(exec_id).await;
            table.update(cx, |t, cx| {
                let g = t.delegate_mut();
                if g.generation != generation {
                    return;
                }
                g.loading_more = false;
                match res {
                    Ok(f) => {
                        if let Some(rs) = &mut g.rs {
                            rs.push(f.batch, f.more);
                        }
                    }
                    Err(e) => {
                        if let Some(rs) = &mut g.rs {
                            rs.more = false;
                        }
                        g.error = Some(e.to_string());
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` brings GPUI\'s own `test` attribute; this one wins over the glob.
    use core::prelude::v1::test;

    use super::*;
    use rb_core::Type;
    use rb_core::batch::{Batch, ColumnInfo};

    fn grid_with_more() -> ResultGrid {
        let cols: Arc<[ColumnInfo]> = Arc::from(vec![ColumnInfo { name: "id".into(), type_: Type::INT4 }]);
        let mut b = Batch::empty(1);
        b.push_row(&[Some(&1i32.to_be_bytes())]).unwrap();
        let mut g = ResultGrid::new(true);
        g.rs = Some(ResultSet::new(cols, b, true));
        g.edits = Some(PendingEdits::default());
        g
    }

    #[test]
    fn few_columns_may_grow_wider() {
        assert_eq!((super::width_cap(1), super::width_cap(2), super::width_cap(3), super::width_cap(40)), (160, 80, 60, 60));
    }

    #[test]
    fn row_number_column_shifts_the_data() {
        let g = grid_with_more();
        assert_eq!(g.copy_selection(TableSelection::Cell(0, 0)).as_deref(), Some("1"), "column 0 is the row number");
        assert_eq!(g.copy_selection(TableSelection::Cell(0, 1)).as_deref(), Some("1"), "column 1 is the first data column (id = 1)");
        assert_eq!(g.copy_selection(TableSelection::Column(0)), None, "the number column has nothing to copy");
        assert_eq!(g.copy_selection(TableSelection::Column(1)).as_deref(), Some("1\n"), "TSV of the first data column");
    }

    #[test]
    fn no_paging_while_rows_are_inserted() {
        let mut g = grid_with_more();
        assert!(g.can_page());
        g.edits.as_mut().unwrap().add_row();
        assert!(!g.can_page());
    }

    #[test]
    fn closing_the_cursor_stops_paging_and_reports_it() {
        let mut g = grid_with_more();
        assert!(g.close_cursor(), "had more rows");
        assert!(!g.can_page());
        assert!(!g.close_cursor(), "was already closed");
    }

    #[test]
    fn save_is_single_flight() {
        let mut g = grid_with_more();
        assert!(!g.start_save(), "nothing pending");
        g.edits.as_mut().unwrap().set(0, 0, Some("2".into()), 1);
        assert!(g.start_save());
        assert!(!g.start_save(), "second ⌘S during the save");
        assert!(!g.can_page());
        g.finish_save(false);
        assert!(!g.edits.as_ref().unwrap().is_empty(), "error keeps the edits");
        assert!(g.start_save());
        g.finish_save(true);
        assert!(g.edits.as_ref().unwrap().is_empty());
    }
}

/// Turns on the grid skeleton after `BUSY_DELAY`. Keep the task: dropping it cancels the timer.
pub fn busy_later<T: 'static>(grid: &Entity<TableState<ResultGrid>>, cx: &mut Context<T>) -> Task<()> {
    let grid = grid.downgrade();
    cx.spawn(async move |_, cx| {
        cx.background_executor().timer(BUSY_DELAY).await;
        let _ = grid.update(cx, |t, cx| {
            t.delegate_mut().busy = true;
            cx.notify();
        });
    })
}

pub fn set_idle(grid: &Entity<TableState<ResultGrid>>, cx: &mut App) {
    grid.update(cx, |t, cx| {
        t.delegate_mut().busy = false;
        cx.notify();
    });
}

/// Text mode: the result as a text table in a read-only editor (select, copy, ⌘F).
/// Rebuilt when another result or more rows arrive.
#[derive(Default)]
pub struct TextView(Option<(Entity<EditorState>, (EntityId, u64, usize))>);

/// Per-cell cut: a huge JSON doesn't push the other columns off screen.
const TEXT_CELL_MAX: usize = 80;
/// The whole text lives in one editor buffer: past this, the grid is the way to read.
const TEXT_MAX_ROWS: usize = 10_000;

impl TextView {
    pub fn render(&mut self, grid: &Entity<TableState<ResultGrid>>, window: &mut Window, cx: &mut App) -> AnyElement {
        let g = grid.read(cx).delegate();
        let Some(rs) = g.rs.as_ref() else { return div().into_any_element() };
        // Grid, result (generation) and loaded rows up to the cap: any change rebuilds the text.
        let key = (grid.entity_id(), g.generation, rs.rows().min(TEXT_MAX_ROWS + 1));
        let editor = match &self.0 {
            Some((e, k)) if *k == key => e.clone(),
            _ => {
                let mut text = rs.to_text_table(TEXT_CELL_MAX, TEXT_MAX_ROWS);
                if rs.rows() > TEXT_MAX_ROWS {
                    text += &format!("\n{}\n", rust_i18n::t!("grid.text_capped", n = TEXT_MAX_ROWS));
                }
                let e = cx.new(|cx| EditorState::new(window, cx).soft_wrap(false).line_number(false).searchable(true).default_value(text));
                self.0 = Some((e.clone(), key));
                e
            }
        };
        let font = cx.theme().mono_font_family.clone();
        div().size_full().font_family(font).child(Editor::new(&editor).readonly(true).h_full()).into_any_element()
    }
}

/// Grid | Text footer buttons; `set` stores the mode on the tab.
pub fn mode_toggle<T: 'static>(text: bool, set: fn(&mut T, bool), cx: &mut Context<T>) -> impl IntoElement {
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::{Selectable as _, Sizable as _};
    let b = |id: &'static str, label: &str, value: bool, cx: &mut Context<T>| {
        Button::new(id).label(label.to_string()).ghost().xsmall().selected(text == value).on_click(cx.listener(move |t, _, _, cx| {
            set(t, value);
            cx.notify();
        }))
    };
    gpui_kit::component::h_flex().child(b("mode-grid", "Grid", false, cx)).child(b("mode-text", "Text", true, cx))
}
