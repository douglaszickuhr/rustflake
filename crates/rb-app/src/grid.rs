use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableSelection, TableState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::batch::ResultSet;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::input::{Editor, EditorState};
use rb_core::decode::{detail_text, one_line};
use rb_core::session::Session;
use rb_core::table::PendingEdits;

pub const MAX_CELL_CHARS: usize = 200;
/// Text limit in the detail tooltip; the full value remains in ⌘C.
const MAX_DETAIL_CHARS: usize = 6_000;
/// A query faster than this doesn't show the skeleton: the old result stays until the new one arrives.
const BUSY_DELAY: Duration = Duration::from_millis(150);

pub type OnSort = Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)>;

pub struct ResultGrid {
    pub rs: Option<ResultSet>,
    pub edits: Option<PendingEdits>,
    pub sortable: bool,
    pub on_sort: Option<OnSort>,
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

    pub fn set_result(&mut self, rs: ResultSet, session: Arc<Session>, exec_id: u64) {
        self.generation += 1;
        self.exec_id = exec_id;
        self.widths = (0..rs.columns.len())
            .map(|c| {
                let sample = (0..rs.rows().min(100)).map(|r| rs.display(r, c, 60).map_or(4, |s| s.chars().count())).max().unwrap_or(0);
                let chars = sample.max(rs.columns[c].name.chars().count()).clamp(4, 60);
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
    fn cell(&self, row: usize, col: usize) -> (SharedString, bool, bool, bool) {
        let loaded = self.loaded_rows();
        let deleted = self.edits.as_ref().is_some_and(|e| e.is_deleted(row));
        if let Some(v) = self.edits.as_ref().and_then(|e| e.get(row, col, loaded)) {
            return match v {
                Some(s) => (one_line(s, MAX_CELL_CHARS).into(), false, true, deleted),
                None => ("NULL".into(), true, true, deleted),
            };
        }
        if row >= loaded {
            return ("DEFAULT".into(), true, false, false);
        }
        match self.rs.as_ref().and_then(|rs| rs.display(row, col, MAX_CELL_CHARS)) {
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

    pub fn copy_selection(&self, sel: TableSelection) -> Option<String> {
        let rs = self.rs.as_ref()?;
        let all: Vec<usize> = (0..rs.columns.len()).collect();
        match sel {
            TableSelection::Cell(r, c) => Some(self.full_text(r, c).unwrap_or_default()),
            TableSelection::Row(r) if r < rs.rows() => Some(rs.to_tsv(r..r + 1, &all)),
            TableSelection::Column(c) => Some(rs.to_tsv(0..rs.rows(), &[c])),
            _ => None,
        }
    }
}

pub fn copy_to_clipboard(table: &Entity<TableState<ResultGrid>>, cx: &mut App) {
    let t = table.read(cx);
    if let Some(text) = t.delegate().copy_selection(t.selection()) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.rs.as_ref().map_or(0, |rs| rs.columns.len())
    }

    fn rows_count(&self, _: &App) -> usize {
        self.loaded_rows() + self.edits.as_ref().map_or(0, PendingEdits::inserted_rows)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let name = self.rs.as_ref().map_or(String::new(), |rs| rs.columns[col_ix].name.clone());
        let col = Column::new(format!("c{col_ix}"), name).width(px(self.widths.get(col_ix).copied().unwrap_or(120.)));
        if self.sortable { col.sortable() } else { col }
    }

    fn render_td(&mut self, row: usize, col: usize, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let (text, null, edited, deleted) = self.cell(row, col);
        // ponytail: width estimated at ~7px per character; measure the text if the guess is way off.
        let width = self.widths.get(col).copied().unwrap_or(120.);
        let clipped = text.chars().count() as f32 * 7. > width - 12.;
        // The full value (and its pretty JSON) is built on hover, not for every clipped cell every frame.
        let table = (!null && clipped).then(|| cx.entity().downgrade());
        let cols = self.columns_count(cx);
        div()
            .id(("cell", row * cols + col))
            .when_some(table, |d, table| {
                d.tooltip(move |window, cx| {
                    let full = table.upgrade().and_then(|t| t.read(cx).delegate().full_text(row, col)).unwrap_or_default();
                    let detail = SharedString::from(detail_text(&full, MAX_DETAIL_CHARS));
                    Tooltip::element(move |_, cx| {
                        div()
                            .max_w(px(720.))
                            .max_h(px(480.))
                            .overflow_hidden()
                            .py_1()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .child(detail.clone())
                    })
                    .build(window, cx)
                })
            })
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .when(null, |d| d.italic().text_color(cx.theme().muted_foreground))
            .when(edited, |d| d.bg(cx.theme().accent))
            .when(deleted, |d| d.line_through().text_color(cx.theme().danger))
            .child(text)
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        self.full_text(row, col).unwrap_or_default()
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        if let Some(f) = self.on_sort.clone() {
            f(col_ix, sort, window, cx);
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
