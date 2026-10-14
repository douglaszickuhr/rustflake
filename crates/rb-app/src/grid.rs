use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableSelection, TableState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::batch::ResultSet;
use rb_core::decode::one_line;
use rb_core::session::Session;
use rb_core::table::PendingEdits;

pub const MAX_CELL_CHARS: usize = 200;

type OnSort = Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)>;

pub struct ResultGrid {
    pub rs: Option<ResultSet>,
    pub edits: Option<PendingEdits>,
    pub sortable: bool,
    pub on_sort: Option<OnSort>,
    pub error: Option<String>,
    session: Option<Arc<Session>>,
    widths: Vec<f32>,
    loading_more: bool,
    /// Muda a cada resultado novo; descarta lotes que chegam de um resultado antigo.
    generation: u64,
}

impl ResultGrid {
    pub fn new(sortable: bool) -> Self {
        ResultGrid { rs: None, edits: None, sortable, on_sort: None, error: None, session: None, widths: Vec::new(), loading_more: false, generation: 0 }
    }

    pub fn set_result(&mut self, rs: ResultSet, session: Arc<Session>) {
        self.generation += 1;
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

    pub fn loaded_rows(&self) -> usize {
        self.rs.as_ref().map_or(0, ResultSet::rows)
    }

    /// (texto, é NULL, foi editada, linha marcada para excluir)
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

    /// Valor completo de uma célula carregada (para cópia e edição).
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
        div()
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

    fn has_more(&self, _: &App) -> bool {
        self.rs.as_ref().is_some_and(|r| r.more)
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
        let generation = self.generation;
        cx.spawn(async move |table, cx| {
            let res = session.fetch_more().await;
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
