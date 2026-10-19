//! Lineage graph of a Snowflake object (`GET_LINEAGE`): upstream to the left, downstream to the right,
//! the selection's details on the right.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::dbt::DbtProject;
use rb_core::lineage::{self, ColumnPath, Dir, Graph, Layout, MAX_DISTANCE, Obj, ObjKey, Row, Slot, Visible};
use rb_core::table_info;
use rust_i18n::t;

use crate::admin_tab::Load;
use crate::dbt_store::DbtStore;
use crate::meta::ConnTarget;
use crate::schema_tree::OpenTable;
use crate::table_tab::sections::kind_dot;
use crate::table_tab::{OpenQuery, spawn_query};
use crate::zoom::{PAD, fit_zoom, step_zoom, wheel_zoom, zoom_anchor};
use crate::{ClearLineage, PlanFit, PlanZoomIn, PlanZoomOut};

const BOX_W: f32 = 220.0;
const BOX_H: f32 = 64.0;
const GAP_X: f32 = 72.0;
const GAP_Y: f32 = 18.0;
/// Room for the +/− of the outermost boxes and for back edges dipping below.
const MARGIN: f32 = 28.0;
/// Children of one object shown before the rest goes into a "+N more" box.
const CAP: usize = 30;

/// (from, to, back edge, color, width), zoomed content coordinates.
type Line = ((f32, f32), (f32, f32), bool, Hsla, f32);

fn place((c0, r0): (i32, f32), col: i32, row: f32) -> (f32, f32) {
    (MARGIN + (col - c0) as f32 * (BOX_W + GAP_X), (row - r0) * (BOX_H + GAP_Y))
}

#[derive(Clone, PartialEq)]
enum Sel {
    Node(ObjKey),
    /// Parent and direction: indices into `Visible::more` change on every relayout.
    More(ObjKey, Dir),
}

pub struct LineageView {
    conn: ConnTarget,
    dbt: Option<Entity<DbtStore>>,
    open_table: OpenTable,
    open_query: OpenQuery,
    /// Previous focuses (Back).
    back: Vec<Obj>,
    graph: Graph,
    /// Results of an older focus (or from before Reload) are dropped.
    generation: u64,
    /// Upstream and downstream fetches (`Dir as usize`).
    sides: [Load<()>; 2],
    expanded: HashSet<ObjKey>,
    /// Revealed one by one from a "+N more" box.
    pinned: HashSet<ObjKey>,
    selected: Option<Sel>,
    /// Drag to pan: where the mouse went down and the scroll offset at that moment.
    pan: Option<(Point<Pixels>, Point<Pixels>)>,
    /// Pushing past the left/right edge (drag or scroll) expands that side: amount pushed so far
    /// with the wheel, and when it last fired (a flick shouldn't open several levels at once).
    pull: f32,
    pulled_at: Option<std::time::Instant>,
    visible: Visible,
    layout: Layout,
    columns: HashMap<ObjKey, Load<Vec<String>>>,
    /// "How it's built": (label, SQL), `None` when there's no definition to show.
    defs: HashMap<ObjKey, Load<Option<(String, String)>>>,
    col_rows: HashMap<(ObjKey, String), Load<Vec<Row>>>,
    /// Column whose lineage is highlighted, and its path over the current graph.
    column: Option<(ObjKey, String)>,
    path: Option<ColumnPath>,
    zoom: f32,
    fitted: bool,
    viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl LineageView {
    pub fn new(focus: Obj, conn: ConnTarget, dbt: Option<Entity<DbtStore>>, open_table: OpenTable, open_query: OpenQuery, cx: &mut Context<Self>) -> Self {
        let subs = dbt.iter().map(|d| cx.observe(d, |_, _, cx| cx.notify())).collect();
        let mut v = LineageView {
            conn,
            dbt,
            open_table,
            open_query,
            back: Vec::new(),
            graph: Graph::new(focus),
            generation: 0,
            sides: [Load::Loading, Load::Loading],
            expanded: HashSet::new(),
            pinned: HashSet::new(),
            selected: None,
            pan: None,
            pull: 0.0,
            pulled_at: None,
            visible: Visible::default(),
            layout: Layout::default(),
            columns: HashMap::new(),
            defs: HashMap::new(),
            col_rows: HashMap::new(),
            column: None,
            path: None,
            zoom: 1.0,
            fitted: false,
            viewport: Rc::default(),
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            _subs: subs,
        };
        v.relayout();
        v.fetch(cx);
        v
    }

    /// Both sides at once, each on its own session: one side failing never hides the other.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        for dir in [Dir::Up, Dir::Down] {
            self.sides[dir as usize] = Load::Loading;
            let key = self.graph.focus.clone();
            spawn_query(
                self.conn.clone(),
                cx,
                async move |s| {
                    let (n, r) = s.query_named(&lineage::lineage_sql(&key, dir, MAX_DISTANCE)).await?;
                    Ok(lineage::parse_rows(&n, &r))
                },
                move |v: &mut Self, res, cx| {
                    if v.generation != generation {
                        return;
                    }
                    v.sides[dir as usize] = match res {
                        Ok(rows) => {
                            v.graph.add(dir, rows);
                            Load::Ready(())
                        }
                        Err(e) => Load::Failed(e.to_string()),
                    };
                    // Fit once both sides are in (not again when the second one arrives after a zoom).
                    v.fitted = v.sides.iter().any(|s| matches!(s, Load::Loading));
                    v.relayout();
                    // After Reload the selected object wasn't in the graph yet: its panel can load now.
                    if let Some(Sel::Node(k)) = v.selected.clone() {
                        v.ensure_def(&k, cx);
                        v.ensure_columns(&k, cx);
                    }
                },
            );
        }
    }

    fn reset(&mut self, focus: Obj, cx: &mut Context<Self>) {
        self.graph = Graph::new(focus);
        self.expanded.clear();
        self.pinned.clear();
        self.selected = None;
        self.column = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.relayout();
        self.fetch(cx);
        cx.notify();
    }

    fn center_on(&mut self, k: &ObjKey, cx: &mut Context<Self>) {
        let Some(o) = self.graph.objs.get(k).cloned() else { return };
        if let Some(cur) = self.graph.objs.get(&self.graph.focus).cloned() {
            self.back.push(cur);
        }
        self.reset(o, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(o) = self.back.pop() {
            self.reset(o, cx);
        }
    }

    /// Fetches again; keeps what was expanded and selected (by key), drops the column highlight.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(focus) = self.graph.objs.get(&self.graph.focus).cloned() else { return };
        self.graph = Graph::new(focus);
        self.column = None;
        self.col_rows.clear();
        self.defs.clear();
        self.columns.clear();
        self.relayout();
        self.fetch(cx);
        if let Some(Sel::Node(k)) = self.selected.clone() {
            self.select(k, cx);
        }
        cx.notify();
    }

    fn origin(&self) -> (i32, f32) {
        let c = self.layout.pos.iter().map(|p| p.1).min().unwrap_or(0);
        let r = self.layout.pos.iter().map(|p| p.2).fold(0.0, f32::min);
        (c, r)
    }

    /// Top-left corner of a box in content coordinates (unzoomed).
    fn xy(&self, col: i32, row: f32) -> (f32, f32) {
        place(self.origin(), col, row)
    }

    fn content_size(&self) -> (f32, f32) {
        let c1 = self.layout.pos.iter().map(|p| p.1).max().unwrap_or(0);
        let r1 = self.layout.pos.iter().map(|p| p.2).fold(0.0, f32::max);
        let (x, y) = self.xy(c1, r1);
        (x + BOX_W + MARGIN, y + BOX_H + MARGIN * 2.0)
    }

    /// Visibility and positions; the focus keeps its spot on screen.
    fn relayout(&mut self) {
        let before = self.xy(0, 0.0);
        self.path = self.column.as_ref().and_then(|c| match self.col_rows.get(c) {
            Some(Load::Ready(rows)) => Some(self.graph.column_path(rows)),
            _ => None,
        });
        let mut pins = self.pinned.clone();
        pins.extend(self.path.iter().flat_map(|p| p.cols.keys().cloned()));
        self.visible = self.graph.visible(&self.expanded, &pins, CAP);
        self.layout = lineage::layout(&self.graph, &self.visible);
        let after = self.xy(0, 0.0);
        let (dx, dy) = ((after.0 - before.0) * self.zoom, (after.1 - before.1) * self.zoom);
        if dx != 0.0 || dy != 0.0 {
            let o = self.scroll.offset();
            self.scroll.set_offset(point(px((f32::from(o.x) - dx).min(0.0)), px((f32::from(o.y) - dy).min(0.0))));
        }
    }

    /// `+`/`−` on the outer side: reveals or hides what's cached; at the edge of what was fetched, centers there.
    fn toggle(&mut self, k: ObjKey, dir: Dir, cx: &mut Context<Self>) {
        if !self.graph.outward(&k, dir).is_empty() {
            if !self.expanded.remove(&k) {
                self.expanded.insert(k);
            }
            self.relayout();
            cx.notify();
        } else if self.graph.frontier.contains(&(k.clone(), dir)) {
            self.center_on(&k, cx);
        }
    }

    /// Horizontal scroll range: offsets go from `min` (right edge) to 0 (left edge).
    fn min_offset_x(&self) -> Option<f32> {
        let area = self.viewport.get()?;
        let (w, _) = self.content_size();
        Some((f32::from(area.size.width) - (w * self.zoom + 2.0 * PAD)).min(0.0))
    }

    /// Opens one more level on `dir`'s side: the outermost visible nodes with something cached outward.
    fn expand_edge(&mut self, dir: Dir, cx: &mut Context<Self>) {
        if self.pulled_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(700)) {
            return;
        }
        let side = |c: i32| if dir == Dir::Up { -c } else { c };
        let nodes: Vec<(ObjKey, i32)> = self.layout.pos.iter().filter_map(|(s, c, _)| if let Slot::Node(k) = s { Some((k.clone(), side(*c))) } else { None }).collect();
        let Some(outer) = nodes.iter().map(|n| n.1).max().filter(|c| *c >= 0) else { return };
        let open: Vec<ObjKey> = nodes
            .into_iter()
            .filter(|(k, c)| *c == outer && self.graph.side_of(k).is_none_or(|d| d == dir) && !self.expanded.contains(k) && !self.graph.outward(k, dir).is_empty())
            .map(|(k, _)| k)
            .collect();
        self.pulled_at = Some(std::time::Instant::now());
        if open.is_empty() {
            return;
        }
        self.expanded.extend(open);
        self.relayout();
        cx.notify();
    }

    fn project(&self, cx: &App) -> Option<Arc<DbtProject>> {
        match &self.dbt.as_ref()?.read(cx).project {
            Load::Ready(p) => Some(p.clone()),
            _ => None,
        }
    }

    fn select(&mut self, k: ObjKey, cx: &mut Context<Self>) {
        self.selected = Some(Sel::Node(k.clone()));
        self.ensure_def(&k, cx);
        self.ensure_columns(&k, cx);
        cx.notify();
    }

    /// dbt model SQL (exact match only) → `GET_DDL` of views and dynamic tables → nothing.
    fn ensure_def(&mut self, k: &ObjKey, cx: &mut Context<Self>) {
        if self.defs.contains_key(k) {
            return;
        }
        let Some(o) = self.graph.objs.get(k).cloned() else { return };
        if !o.masked
            && let Some(p) = self.project(cx)
            && let Some(n) = p.manifest.by_table(&k.db, &k.schema, &k.name)
        {
            let sql = rb_core::dbt::compiled_on_disk(&p.dir, n).map(|(s, _)| s).or_else(|| n.compiled_code.clone()).unwrap_or_else(|| n.raw_code.clone());
            self.defs.insert(k.clone(), Load::Ready(Some((t!("lin.model_def", name = n.name).to_string(), sql))));
            return;
        }
        let Some(sql) = lineage::ddl_sql(&o).filter(|_| !o.masked) else {
            self.defs.insert(k.clone(), Load::Ready(None));
            return;
        };
        self.defs.insert(k.clone(), Load::Loading);
        let key = k.clone();
        spawn_query(
            self.conn.clone(),
            cx,
            async move |s| {
                let (_, r) = s.query_named(&sql).await?;
                Ok(r.first().and_then(|r| r.first().cloned().flatten()))
            },
            move |v: &mut Self, res, _| {
                v.defs.insert(key, res.map_or_else(|e| Load::Failed(e.to_string()), |s| Load::Ready(s.map(|s| ("DDL".to_string(), s)))));
            },
        );
    }

    fn ensure_columns(&mut self, k: &ObjKey, cx: &mut Context<Self>) {
        if self.columns.contains_key(k) || !self.graph.objs.get(k).is_some_and(Obj::is_table_like) {
            return;
        }
        self.columns.insert(k.clone(), Load::Loading);
        let key = k.clone();
        let sql = table_info::columns_sql(&k.db, &k.schema, &k.name);
        spawn_query(
            self.conn.clone(),
            cx,
            async move |s| {
                let (n, r) = s.query_named(&sql).await?;
                Ok(table_info::parse_columns(&n, &r).into_iter().map(|c| c.name).collect())
            },
            move |v: &mut Self, res, _| {
                v.columns.insert(key, res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready));
            },
        );
    }

    fn select_column(&mut self, k: ObjKey, col: String, cx: &mut Context<Self>) {
        let id = (k.clone(), col.clone());
        self.column = Some(id.clone());
        if !self.col_rows.contains_key(&id) {
            self.col_rows.insert(id.clone(), Load::Loading);
            spawn_query(
                self.conn.clone(),
                cx,
                async move |s| {
                    let (n, r) = s.query_named(&lineage::column_lineage_sql(&k, &col)).await?;
                    Ok(lineage::parse_rows(&n, &r))
                },
                move |v: &mut Self, res, _| {
                    v.col_rows.insert(id, res.map_or_else(|e| Load::Failed(e.to_string()), Load::Ready));
                    v.relayout();
                },
            );
        }
        self.relayout();
        cx.notify();
    }

    fn clear_column(&mut self, cx: &mut Context<Self>) {
        if self.column.take().is_some() {
            self.relayout();
            cx.notify();
        }
    }

    fn open(&self, k: &ObjKey, window: &mut Window, cx: &mut App) {
        if self.graph.objs.get(k).is_some_and(Obj::is_table_like) {
            (self.open_table)(format!("{}.{}", k.db, k.schema), k.name.clone(), window, cx);
        }
    }

    fn fit(&mut self) {
        let Some(v) = self.viewport.get().map(|b| (f32::from(b.size.width), f32::from(b.size.height))) else { return };
        // Measured but too small to fit (narrow window): keep the zoom instead of retrying every frame.
        if v.0 > 2.0 * PAD && v.1 > 2.0 * PAD {
            self.zoom = fit_zoom(self.content_size(), (v.0 - 2.0 * PAD, v.1 - 2.0 * PAD));
        }
        self.fitted = true;
    }

    fn wheel(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(area) = self.viewport.get() else { return };
        let dy = f32::from(ev.delta.pixel_delta(px(16.0)).y);
        let z1 = wheel_zoom(self.zoom, dy);
        if z1 == self.zoom {
            return;
        }
        let pointer = ev.position - area.origin;
        let off = self.scroll.offset();
        let x = zoom_anchor(f32::from(off.x), f32::from(pointer.x), self.zoom, z1);
        let y = zoom_anchor(f32::from(off.y), f32::from(pointer.y), self.zoom, z1);
        self.zoom = z1;
        self.scroll.set_offset(point(px(x), px(y)));
        cx.notify();
    }

    fn side_status(&self, dir: Dir, theme: &gpui_kit::component::Theme) -> Div {
        let d = h_flex().gap_1().items_center();
        match &self.sides[dir as usize] {
            Load::Loading => d.child(Spinner::new().xsmall()).child(if dir == Dir::Up { t!("lin.up_loading") } else { t!("lin.down_loading") }.to_string()),
            Load::Ready(()) => {
                let n = self.graph.count(dir);
                d.child(if dir == Dir::Up { t!("lin.up_count", n = n) } else { t!("lin.down_count", n = n) }.to_string())
                    .when(self.graph.truncated[dir as usize], |d| d.child(div().text_color(theme.warning).child(t!("lin.truncated", n = lineage::LIMIT).to_string())))
            }
            Load::Failed(m) => d.child(div().max_w(px(360.)).overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.danger).child(m.clone())),
        }
    }
}

/// Snowflake's message; the edition/privilege hint only when it isn't a missing object.
fn error_text(m: &str) -> String {
    let lower = m.to_lowercase();
    if lower.contains("does not exist") || lower.contains("not authorized") { m.to_string() } else { format!("{m}\n{}", t!("tv.hint_lineage")) }
}

impl Render for LineageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.fitted {
            self.fit();
            if !self.fitted {
                window.request_animation_frame();
            }
        }
        let theme = cx.theme().clone();
        let z = self.zoom;
        let project = self.project(cx);
        let path = self.path.clone();
        let dim = path.is_some();

        // Edges, in content coordinates already zoomed: source's right middle → target's left middle;
        // back edges (cycle through the focus) dip below, dashed.
        let origin = self.origin();
        let placed: HashMap<&Slot, (f32, f32)> = self.layout.pos.iter().map(|p| (&p.0, place(origin, p.1, p.2))).collect();
        let at = |s: &Slot| placed.get(s).copied();
        let mut lines: Vec<Line> = Vec::new();
        for (a, b, back) in &self.layout.edges {
            let (Some(pa), Some(pb)) = (at(&Slot::Node(a.clone())), at(&Slot::Node(b.clone()))) else { continue };
            let on_path = path.as_ref().is_some_and(|p| p.edges.contains(&(a.clone(), b.clone())));
            let (color, w) = match (on_path, dim) {
                (true, _) => (theme.primary, 2.5),
                (false, true) => (theme.muted_foreground.opacity(0.2), 1.2),
                _ => (theme.muted_foreground.opacity(0.6), 1.2),
            };
            let (from, to) = if *back { ((pa.0 + BOX_W / 2.0, pa.1 + BOX_H), (pb.0 + BOX_W / 2.0, pb.1 + BOX_H)) } else { ((pa.0 + BOX_W, pa.1 + BOX_H / 2.0), (pb.0, pb.1 + BOX_H / 2.0)) };
            lines.push(((from.0 * z, from.1 * z), (to.0 * z, to.1 * z), *back, color, w * z.max(0.5)));
        }
        for (p, i) in &self.layout.more_edges {
            let (Some(pa), Some(pb)) = (at(&Slot::Node(p.clone())), at(&Slot::More(*i))) else { continue };
            let up = self.visible.more.get(*i).is_some_and(|m| m.dir == Dir::Up);
            let (from, to) = if up { ((pb.0 + BOX_W, pb.1 + BOX_H / 2.0), (pa.0, pa.1 + BOX_H / 2.0)) } else { ((pa.0 + BOX_W, pa.1 + BOX_H / 2.0), (pb.0, pb.1 + BOX_H / 2.0)) };
            lines.push(((from.0 * z, from.1 * z), (to.0 * z, to.1 * z), false, theme.muted_foreground.opacity(0.4), 1.2 * z.max(0.5)));
        }
        let arrows = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                for (from, to, back, color, width) in &lines {
                    let mut b = PathBuilder::stroke(px(*width));
                    if *back {
                        b = b.dash_array(&[px(4.), px(3.)]);
                    }
                    let (p0, p1) = (bounds.origin + point(px(from.0), px(from.1)), bounds.origin + point(px(to.0), px(to.1)));
                    if *back {
                        let dip = px(40. * z);
                        b.move_to(p0);
                        b.cubic_bezier_to(p1, point(p0.x, p0.y + dip), point(p1.x, p1.y + dip));
                    } else {
                        let dx = px(((to.0 - from.0) / 2.0).max(24.0 * z));
                        b.move_to(p0);
                        b.cubic_bezier_to(p1, point(p0.x + dx, p0.y), point(p1.x - dx, p1.y));
                    }
                    if let Ok(path) = b.build() {
                        window.paint_path(path, *color);
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let mut boxes: Vec<AnyElement> = Vec::new();
        for (i, (slot, col, row)) in self.layout.pos.iter().enumerate() {
            let (x, y) = place(origin, *col, *row);
            let shell =
                div().absolute().left(px(x * z)).top(px(y * z)).w(px(BOX_W * z)).h(px(BOX_H * z)).px(px(8.0 * z)).py(px(4.0 * z)).overflow_hidden().rounded(theme.radius).text_size(px(12.0 * z));
            match slot {
                Slot::More(m) => {
                    let Some(more) = self.visible.more.get(*m) else { continue };
                    let n = more.hidden.len();
                    let id = Sel::More(more.parent.clone(), more.dir);
                    let selected = self.selected.as_ref() == Some(&id);
                    boxes.push(
                        shell
                            .id(("lin-more", i))
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_1()
                            .border_dashed()
                            .border_color(if selected { theme.primary } else { theme.border })
                            .bg(theme.secondary)
                            .cursor_pointer()
                            .text_color(theme.muted_foreground)
                            .child(t!("lin.more", n = n).to_string())
                            .on_click(cx.listener(move |v, _, window, cx| {
                                v.selected = Some(id.clone());
                                v.focus.focus(window, cx);
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
                Slot::Node(k) => {
                    let Some(o) = self.graph.objs.get(k) else { continue };
                    let is_focus = *k == self.graph.focus;
                    let selected = self.selected == Some(Sel::Node(k.clone()));
                    let cols = path.as_ref().and_then(|p| p.cols.get(k));
                    let faded = dim && cols.is_none();
                    let dbt = project.as_deref().and_then(|p| p.manifest.by_table(&k.db, &k.schema, &k.name));
                    let dot = dbt.map_or(theme.muted_foreground, |n| kind_dot(&n.resource_type, &theme));
                    let badge = o.kind.clone().or_else(|| (k.domain != "TABLE").then(|| k.domain.clone())).filter(|b| b != "TABLE");
                    let border = if cols.is_some() || is_focus || selected { theme.primary } else { theme.border };
                    let key = k.clone();
                    let mut b = shell
                        .id(("lin-node", i))
                        .flex()
                        .flex_col()
                        .border_1()
                        .when(is_focus || selected || cols.is_some(), |d| d.border_2())
                        .border_color(border)
                        .bg(if o.masked {
                            theme.secondary
                        } else if selected {
                            theme.background.blend(theme.primary.opacity(0.08))
                        } else {
                            theme.background
                        })
                        .when(faded, |d| d.opacity(0.4))
                        .cursor_pointer()
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(div().size(px(7.0 * z)).flex_shrink_0().rounded_full().bg(dot))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .font_family(theme.mono_font_family.clone())
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(if o.masked { t!("lin.no_access").to_string() } else { k.name.clone() }),
                                )
                                .when_some(badge, |d, b| d.child(div().flex_shrink_0().text_size(px(10.0 * z)).text_color(theme.muted_foreground).child(b))),
                        )
                        .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.muted_foreground).child(if k.is_qualified() {
                            format!("{}.{}", k.db, k.schema)
                        } else {
                            k.domain.to_lowercase()
                        }));
                    if let Some(c) = cols {
                        let shown = if c.len() > 2 { format!("→ {}, {} +{}", c[0], c[1], c.len() - 2) } else { format!("→ {}", c.join(", ")) };
                        b = b.child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family(theme.mono_font_family.clone()).text_color(theme.primary).child(shown));
                    }
                    boxes.push(
                        b.on_click(cx.listener(move |v, ev: &ClickEvent, window, cx| {
                            if ev.click_count() >= 2 {
                                v.open(&key, window, cx);
                            } else {
                                v.select(key.clone(), cx);
                                v.focus.focus(window, cx);
                            }
                        }))
                        .into_any_element(),
                    );
                    // +/− in the gap on the outer side (a sibling of the box: its click never selects the box).
                    if let Some(dir) = self.graph.side_of(k).filter(|_| !o.masked && k.is_qualified()) {
                        let has = !self.graph.outward(k, dir).is_empty();
                        let edge = self.graph.frontier.contains(&(k.clone(), dir));
                        if has || edge {
                            let open = has && self.expanded.contains(k);
                            let bx = if dir == Dir::Up { x - 22.0 } else { x + BOX_W + 4.0 };
                            let key = k.clone();
                            let tip = if has { None } else { Some(t!("lin.center_more").to_string()) };
                            boxes.push(
                                div()
                                    .id(("lin-toggle", i))
                                    .absolute()
                                    .left(px(bx * z))
                                    .top(px((y + BOX_H / 2.0 - 9.0) * z))
                                    .size(px(18.0 * z))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .text_size(px(12.0 * z))
                                    .cursor_pointer()
                                    .hover(|d| d.bg(theme.secondary_hover))
                                    .child(if open { "−" } else { "+" })
                                    .when_some(tip, |d, tip| d.tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(w, cx)))
                                    .on_click(cx.listener(move |v, _, _, cx| v.toggle(key.clone(), dir, cx)))
                                    .into_any_element(),
                            );
                        }
                    }
                }
            }
        }

        let failed: Vec<String> = self.sides.iter().filter_map(|s| if let Load::Failed(m) = s { Some(m.clone()) } else { None }).collect();
        let done = self.sides.iter().all(|s| matches!(s, Load::Ready(())));
        let center: Option<AnyElement> = if failed.len() == 2 {
            Some(div().p_4().text_sm().text_color(theme.danger).whitespace_normal().child(error_text(&failed[0])).into_any_element())
        } else if done && self.graph.count(Dir::Up) + self.graph.count(Dir::Down) == 0 {
            Some(v_flex().p_4().gap_1().text_sm().child(t!("tv.no_lineage").to_string()).child(div().text_color(theme.muted_foreground).child(t!("tv.lineage_lag").to_string())).into_any_element())
        } else {
            None
        };

        let (w, h) = self.content_size();
        let viewport = self.viewport.clone();
        let content = div().relative().w(px(w * z)).h(px(h * z)).child(arrows).children(boxes);
        let col_label = self.column.as_ref().map(|(k, c)| format!("{}.{c}", k.name));
        let bar = h_flex()
            .px_2()
            .py_1()
            .gap_3()
            .flex_wrap()
            .items_center()
            .text_sm()
            .when(!self.back.is_empty(), |d| d.child(Button::new("lin-back").label(t!("lin.back").to_string()).ghost().xsmall().on_click(cx.listener(|v, _, _, cx| v.go_back(cx)))))
            // The table view header already names the table; only after moving to another object.
            .when(!self.back.is_empty(), |d| d.child(div().font_family(theme.mono_font_family.clone()).child(self.graph.focus.label())))
            .child(self.side_status(Dir::Up, &theme))
            .child(self.side_status(Dir::Down, &theme))
            .when_some(col_label, |d, c| {
                d.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(Tag::info().small().child(c))
                        .when(matches!(self.column.as_ref().and_then(|c| self.col_rows.get(c)), Some(Load::Loading)), |d| d.child(Spinner::new().xsmall()))
                        .child(Button::new("lin-clear").label(t!("lin.clear_column").to_string()).ghost().xsmall().on_click(cx.listener(|v, _, _, cx| v.clear_column(cx)))),
                )
            })
            .child(div().flex_1())
            .child(div().text_xs().text_color(theme.muted_foreground).child(t!("tv.lineage_lag").to_string()))
            .child(div().text_xs().text_color(theme.muted_foreground).child(t!("plan.zoom_hint").to_string()))
            .child(Button::new("lin-reload").label(t!("table.reload").to_string()).ghost().xsmall().on_click(cx.listener(|v, _, _, cx| v.reload(cx))));

        v_flex()
            .key_context("PlanView")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|v, _: &PlanZoomIn, _, cx| {
                v.zoom = step_zoom(v.zoom, true);
                cx.notify();
            }))
            .on_action(cx.listener(|v, _: &PlanZoomOut, _, cx| {
                v.zoom = step_zoom(v.zoom, false);
                cx.notify();
            }))
            .on_action(cx.listener(|v, _: &PlanFit, _, cx| {
                v.fit();
                cx.notify();
            }))
            .on_action(cx.listener(|v, _: &ClearLineage, _, cx| v.clear_column(cx)))
            .child(bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(match center {
                        Some(c) => div().flex_1().h_full().child(c).into_any_element(),
                        None => div()
                            .id("lin-area")
                            .relative()
                            .flex_1()
                            .h_full()
                            .overflow_scroll()
                            .track_scroll(&self.scroll)
                            .p(px(PAD))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|v, ev: &MouseDownEvent, window, cx| {
                                    v.focus.focus(window, cx);
                                    v.pan = Some((ev.position, v.scroll.offset()));
                                    v.pulled_at = None;
                                }),
                            )
                            .on_mouse_move(cx.listener(|v, ev: &MouseMoveEvent, _, cx| {
                                let Some((start, offset)) = v.pan else { return };
                                if ev.pressed_button != Some(MouseButton::Left) {
                                    v.pan = None;
                                    return;
                                }
                                let want = offset + (ev.position - start);
                                v.scroll.set_offset(want);
                                if let Some(dir) = v.min_offset_x().and_then(|min| edge_pull(f32::from(want.x), min, PULL)) {
                                    v.expand_edge(dir, cx);
                                }
                                cx.notify();
                            }))
                            .on_mouse_up(MouseButton::Left, cx.listener(|v, _, _, _| v.pan = None))
                            .when(self.pan.is_some(), |d| d.cursor_grabbing())
                            .when(self.pan.is_none(), |d| d.cursor_grab())
                            .child(canvas(move |b, _, _| viewport.set(Some(b)), |_, _, _, _| {}).absolute().size_full())
                            // ⌘ + scroll zooms; registered on a child to run before the area's scrolling.
                            .child(
                                div()
                                    .min_w_full()
                                    .min_h_full()
                                    .on_scroll_wheel(cx.listener(|v, ev: &ScrollWheelEvent, _, cx| {
                                        if ev.modifiers.platform {
                                            cx.stop_propagation();
                                            v.wheel(ev, cx);
                                            return;
                                        }
                                        // At an edge, keep pushing the same way: adds up until it opens that side.
                                        let dx = f32::from(ev.delta.pixel_delta(px(16.0)).x);
                                        let (x, min) = (f32::from(v.scroll.offset().x), v.min_offset_x().unwrap_or(0.0));
                                        let at_edge = (dx > 0.0 && x >= -1.0) || (dx < 0.0 && x <= min + 1.0);
                                        v.pull = if at_edge && v.pull.signum() != -dx.signum() { v.pull + dx } else { 0.0 };
                                        if let Some(dir) = edge_pull(v.pull, 0.0, PULL) {
                                            v.pull = 0.0;
                                            v.expand_edge(dir, cx);
                                        }
                                    }))
                                    .child(content),
                            )
                            .into_any_element(),
                    })
                    .when(self.selected.is_some(), |d| {
                        let close = cx.listener(|v, _, _, cx| {
                            v.selected = None;
                            cx.notify();
                        });
                        d.child(crate::frame::side_panel("lin-details", 320., close, self.render_details(window, cx), cx))
                    }),
            )
    }
}

impl LineageView {
    fn render_details(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let head = |s: String| div().pt_3().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(theme.muted_foreground).child(s.to_uppercase());
        let mono = theme.mono_font_family.clone();
        match self.selected.clone() {
            None => v_flex(),
            Some(Sel::More(parent, dir)) => {
                let Some(m) = self.visible.more.iter().find(|m| m.parent == parent && m.dir == dir).cloned() else { return v_flex() };
                v_flex().gap_1().child(div().font_weight(FontWeight::SEMIBOLD).child(t!("lin.hidden", n = m.hidden.len()).to_string())).children(m.hidden.into_iter().enumerate().map(|(j, k)| {
                    let key = k.clone();
                    h_flex().gap_2().items_center().text_sm().child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family(mono.clone()).child(k.label())).child(
                        Button::new(("lin-show", j)).label(t!("lin.show").to_string()).ghost().xsmall().on_click(cx.listener(move |v, _, _, cx| {
                            v.pinned.insert(key.clone());
                            v.relayout();
                            cx.notify();
                        })),
                    )
                }))
            }
            Some(Sel::Node(k)) => {
                let Some(o) = self.graph.objs.get(&k).cloned() else { return v_flex() };
                let project = self.project(cx);
                let dbt = project.as_deref().and_then(|p| p.manifest.by_table(&k.db, &k.schema, &k.name).map(|n| (n.clone(), p.manifest.layer(&n.unique_id))));
                let mut out = v_flex()
                    .gap_1()
                    .child(div().font_family(mono.clone()).font_weight(FontWeight::SEMIBOLD).whitespace_normal().child(if o.masked { t!("lin.no_access").to_string() } else { k.name.clone() }))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(if k.is_qualified() { format!("{}.{}", k.db, k.schema) } else { String::new() }))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Tag::secondary().small().child(o.kind.clone().unwrap_or_else(|| k.domain.clone())))
                            .when_some(dbt.clone(), |d, (n, layer)| d.child(Tag::info().small().child(format!("dbt {} · {layer}", n.resource_type)))),
                    )
                    .when_some(dbt.as_ref().map(|(n, _)| n.description.clone()).filter(|d| !d.is_empty()), |d, desc| {
                        d.child(div().text_sm().whitespace_normal().text_color(theme.muted_foreground).child(desc))
                    });
                let table_like = o.is_table_like();
                let can_center = !o.masked && k.is_qualified() && k != self.graph.focus;
                if table_like || can_center {
                    let (k1, k2) = (k.clone(), k.clone());
                    out = out.child(
                        h_flex()
                            .pt_2()
                            .gap_2()
                            .when(table_like, |d| {
                                d.child(Button::new("lin-open").label(t!("lin.open_table").to_string()).outline().xsmall().on_click(cx.listener(move |v, _, window, cx| v.open(&k1, window, cx))))
                            })
                            .when(can_center, |d| {
                                d.child(Button::new("lin-center").label(t!("lin.center").to_string()).ghost().xsmall().on_click(cx.listener(move |v, _, _, cx| v.center_on(&k2, cx))))
                            }),
                    );
                }

                // How it's built: the object's definition (not necessarily the statement behind each edge).
                out = out.child(head(t!("lin.built").to_string()));
                out = match self.defs.get(&k) {
                    Some(Load::Ready(Some((label, sql)))) => {
                        let (open_query, sql2) = (self.open_query.clone(), sql.clone());
                        out.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().flex_1().text_sm().child(label.clone()))
                                .child(Button::new("lin-def-open").label(t!("lin.open_sql").to_string()).ghost().xsmall().on_click(move |_, w, cx| open_query(sql2.clone(), w, cx))),
                        )
                        .child(
                            div()
                                .id("lin-def")
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .text_xs()
                                .font_family(mono.clone())
                                .whitespace_normal()
                                .child(sql.clone()),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).whitespace_normal().child(t!("lin.def_note").to_string()))
                    }
                    Some(Load::Ready(None)) => out.child(div().text_sm().text_color(theme.muted_foreground).whitespace_normal().child(t!("lin.no_definition").to_string())),
                    Some(Load::Failed(m)) => out.child(div().text_xs().text_color(theme.danger).whitespace_normal().child(m.clone())),
                    _ => out.child(Spinner::new().xsmall()),
                };

                // Inputs: each source in the graph and the queries that moved data into this object.
                out = out.child(head(t!("lin.inputs").to_string()));
                let parents = self.graph.neighbors(&k, Dir::Up);
                if parents.is_empty() {
                    out = out.child(div().text_sm().text_color(theme.muted_foreground).child(t!("lin.no_inputs").to_string()));
                }
                for (pi, p) in parents.iter().enumerate() {
                    let masked = self.graph.objs.get(p).is_some_and(|o| o.masked);
                    let mut row = v_flex().gap_1().child(div().text_sm().font_family(mono.clone()).overflow_hidden().text_ellipsis().whitespace_nowrap().child(if masked {
                        t!("lin.no_access").to_string()
                    } else {
                        p.label()
                    }));
                    for (qi, proc_) in self.graph.processes(p, &k).iter().enumerate() {
                        let ids = lineage::query_ids(proc_);
                        if ids.is_empty() {
                            let raw: String = proc_.chars().take(160).collect();
                            row = row.child(div().pl_2().text_xs().font_family(mono.clone()).text_color(theme.muted_foreground).whitespace_normal().child(raw));
                        }
                        for (ii, id) in ids.into_iter().enumerate() {
                            let (copy, open_query, id2) = (id.clone(), self.open_query.clone(), id.clone());
                            let n = pi * 10_000 + qi * 100 + ii;
                            row = row.child(
                                h_flex()
                                    .pl_2()
                                    .gap_1()
                                    .items_center()
                                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_xs().font_family(mono.clone()).child(id))
                                    .child(
                                        Button::new(("lin-copy", n))
                                            .label(t!("lin.copy").to_string())
                                            .ghost()
                                            .xsmall()
                                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                                    )
                                    .child(Button::new(("lin-hist", n)).label(t!("lin.open_sql").to_string()).ghost().xsmall().on_click(move |_, w, cx| open_query(lineage::history_sql(&id2), w, cx))),
                            );
                        }
                    }
                    out = out.child(row);
                }

                // Columns: click one to highlight its lineage on the graph.
                if table_like {
                    out = out.child(head(t!("lin.columns").to_string()));
                    let cur = self.column.clone();
                    if let Some((ck, cc)) = &cur
                        && *ck == k
                    {
                        match self.col_rows.get(&(ck.clone(), cc.clone())) {
                            Some(Load::Failed(m)) => out = out.child(div().text_xs().text_color(theme.danger).whitespace_normal().child(error_text(m))),
                            Some(Load::Ready(_)) => {
                                if let Some(p) = &self.path {
                                    if p.outside > 0 {
                                        out = out.child(div().text_xs().text_color(theme.muted_foreground).child(t!("lin.col_outside", n = p.outside).to_string()));
                                    }
                                    if p.steps.is_empty() {
                                        out = out.child(div().text_xs().text_color(theme.muted_foreground).child(t!("tv.no_lineage").to_string()));
                                    }
                                    for (dir, dist, label) in &p.steps {
                                        let arrow = if *dir == Dir::Up { "←" } else { "→" };
                                        out = out.child(div().text_xs().font_family(mono.clone()).overflow_hidden().text_ellipsis().whitespace_nowrap().child(format!("{arrow} {dist} · {label}")));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    out = match self.columns.get(&k) {
                        Some(Load::Ready(cols)) => out.children(cols.iter().enumerate().map(|(ci, c)| {
                            let on = cur.as_ref().is_some_and(|(ck, cc)| *ck == k && cc == c);
                            let (key, col) = (k.clone(), c.clone());
                            // In a row: stretched to the panel width, the label ended up centered.
                            h_flex().child(Button::new(("lin-col", ci)).label(c.clone()).ghost().xsmall().selected(on).on_click(cx.listener(move |v, _, _, cx| {
                                if on {
                                    v.clear_column(cx);
                                } else {
                                    v.select_column(key.clone(), col.clone(), cx);
                                }
                            })))
                        })),
                        Some(Load::Failed(m)) => out.child(div().text_xs().text_color(theme.danger).whitespace_normal().child(m.clone())),
                        _ => out.child(Spinner::new().xsmall()),
                    };
                }
                out
            }
        }
    }
}

/// How far past an edge the user has to push before that side opens.
const PULL: f32 = 120.0;

/// `x` past 0 (left edge) by more than `pull` → upstream; below `min` (right edge) → downstream.
fn edge_pull(x: f32, min: f32, pull: f32) -> Option<Dir> {
    if x > pull {
        Some(Dir::Up)
    } else if x < min - pull {
        Some(Dir::Down)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Dir, edge_pull};

    #[test]
    fn pushing_past_an_edge_picks_that_side() {
        assert_eq!(edge_pull(121.0, -500.0, 120.0), Some(Dir::Up), "past the left edge");
        assert_eq!(edge_pull(-621.0, -500.0, 120.0), Some(Dir::Down), "past the right edge");
        assert_eq!(edge_pull(100.0, -500.0, 120.0), None, "not far enough");
        assert_eq!(edge_pull(-300.0, -500.0, 120.0), None, "inside the range");
        assert_eq!(edge_pull(-121.0, 0.0, 120.0), Some(Dir::Down), "graph fits: either side can still be pulled");
    }
}
