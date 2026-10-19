//! Object lineage (Snowflake `GET_LINEAGE`): SQL, rows, graph, visibility and layout for the lineage view.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::Value;

use crate::lexer::{qualify, quote_always, sql_str};
use crate::snowflake::Named;

/// Rows fetched per query; one more comes back when the result was cut.
pub const LIMIT: usize = 5000;
/// `GET_LINEAGE`'s maximum (and default) distance.
pub const MAX_DISTANCE: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dir {
    Up,
    Down,
}

impl Dir {
    pub fn sql(self) -> &'static str {
        match self {
            Dir::Up => "UPSTREAM",
            Dir::Down => "DOWNSTREAM",
        }
    }

    fn ix(self) -> usize {
        self as usize
    }

    /// Edges toward the other end: an upstream node reaches the focus through its children.
    fn toward_focus(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
        }
    }
}

/// The domain is part of the key: a stage and a table may share a name in one schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjKey {
    pub domain: String,
    pub db: String,
    pub schema: String,
    pub name: String,
}

impl ObjKey {
    pub fn table(db: &str, schema: &str, name: &str) -> Self {
        ObjKey { domain: "TABLE".into(), db: db.into(), schema: schema.into(), name: name.into() }
    }

    /// Integrations and external objects have no database/schema: they can't be expanded or opened.
    pub fn is_qualified(&self) -> bool {
        !self.db.is_empty() && !self.schema.is_empty()
    }

    pub fn fq(&self) -> String {
        if self.is_qualified() { qualify(&[&self.db, &self.schema, &self.name]) } else { quote_always(&self.name) }
    }

    pub fn label(&self) -> String {
        if self.is_qualified() { format!("{}.{}.{}", self.db, self.schema, self.name) } else { self.name.clone() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Obj {
    pub key: ObjKey,
    /// `dataset_type` from the details (VIEW, DYNAMIC TABLE, …), upper case with spaces.
    pub kind: Option<String>,
    /// No privilege on it (status MASKED): drawn gray, never expanded or opened.
    pub masked: bool,
}

impl Obj {
    /// Tables, views and dynamic tables: they open as a table tab and have columns.
    pub fn is_table_like(&self) -> bool {
        self.key.domain == "TABLE" && !self.masked && self.key.is_qualified()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub src: Obj,
    pub dst: Obj,
    pub src_col: Option<String>,
    pub dst_col: Option<String>,
    pub distance: u32,
    /// `PROCESS` as JSON text.
    pub process: Option<String>,
    /// Only on column queries (`UP`/`DOWN` column), which fetch both directions at once.
    pub dir: Option<Dir>,
}

const COLS: &str = "source_object_database, source_object_schema, source_object_name, source_object_domain, source_status, \
source_details:dataset_type::string as source_type, source_column_name, target_object_database, target_object_schema, target_object_name, \
target_object_domain, target_status, target_details:dataset_type::string as target_type, target_column_name, distance, to_json(process) as process";

/// Ordered by distance so a cut result keeps the levels next to the object.
pub fn lineage_sql(key: &ObjKey, dir: Dir, distance: u32) -> String {
    format!("select {COLS} from table(snowflake.core.get_lineage({}, {}, {}, {distance})) order by distance limit {}", sql_str(&key.fq()), sql_str(&key.domain), sql_str(dir.sql()), LIMIT + 1)
}

/// Both directions in one statement (one session per column click).
pub fn column_lineage_sql(key: &ObjKey, column: &str) -> String {
    let arg = sql_str(&format!("{}.{}", key.fq(), quote_always(column)));
    let one = |d: Dir, tag: &str| format!("select '{tag}' as dir, {COLS} from table(snowflake.core.get_lineage({arg}, 'COLUMN', '{}', {MAX_DISTANCE}))", d.sql());
    format!("{} union all {} order by distance limit {}", one(Dir::Up, "UP"), one(Dir::Down, "DOWN"), LIMIT + 1)
}

fn opt(names: &[String], r: &[Option<String>], col: &str) -> Option<String> {
    Named::new(names, r).text(col)
}

/// Domains of table-like objects become TABLE (the kind keeps the specific one); any other domain stays as
/// Snowflake wrote it, because "Center here" passes it back to `GET_LINEAGE`.
fn normalize(domain: String, kind: Option<String>) -> (String, Option<String>) {
    let kind = kind.map(|k| k.to_uppercase().replace('_', " "));
    let spaced = domain.replace('_', " ");
    match spaced.as_str() {
        "VIEW" | "MATERIALIZED VIEW" | "DYNAMIC TABLE" | "EXTERNAL TABLE" | "ICEBERG TABLE" | "EVENT TABLE" | "HYBRID TABLE" => ("TABLE".into(), kind.or(Some(spaced))),
        _ => (domain, kind),
    }
}

/// Unique across fetches: masked objects without names must never merge.
static MASKED_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn parse_obj(names: &[String], r: &[Option<String>], side: &str) -> (Obj, Option<String>) {
    let g = |c: &str| opt(names, r, &format!("{side}_{c}"));
    let masked = g("status").is_some_and(|s| s.eq_ignore_ascii_case("MASKED"));
    let (domain, kind) = normalize(g("object_domain").unwrap_or_default().to_uppercase(), g("type"));
    // Masked objects may come without names: a unique name keeps them apart (never merged).
    let name = g("object_name").unwrap_or_else(|| format!("#{}", MASKED_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let key = ObjKey { domain, db: g("object_database").unwrap_or_default(), schema: g("object_schema").unwrap_or_default(), name };
    (Obj { key, kind, masked }, g("column_name"))
}

/// Rows without the edge from an object (or column) to itself (incremental MERGE).
pub fn parse_rows(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<Row> {
    let mut out = Vec::new();
    for r in rows {
        let (src, src_col) = parse_obj(names, r, "source");
        let (dst, dst_col) = parse_obj(names, r, "target");
        if src.key == dst.key && src_col == dst_col {
            continue;
        }
        let dir = match opt(names, r, "dir").as_deref() {
            Some("UP") => Some(Dir::Up),
            Some("DOWN") => Some(Dir::Down),
            _ => None,
        };
        let distance = opt(names, r, "distance").and_then(|d| d.parse().ok()).unwrap_or(1);
        out.push(Row { src, dst, src_col, dst_col, distance, process: opt(names, r, "process"), dir });
    }
    out
}

/// Lineage around one object: upstream objects on one side, downstream on the other.
#[derive(Debug, Clone)]
pub struct Graph {
    pub focus: ObjKey,
    pub objs: HashMap<ObjKey, Obj>,
    /// Which fetch found the object (first wins: a cycle through the focus becomes a back edge).
    pub side: HashMap<ObjKey, Dir>,
    parents: HashMap<ObjKey, BTreeSet<ObjKey>>,
    children: HashMap<ObjKey, BTreeSet<ObjKey>>,
    processes: HashMap<(ObjKey, ObjKey), Vec<String>>,
    /// Objects at the edge of what was fetched: there may be more beyond them.
    pub frontier: HashSet<(ObjKey, Dir)>,
    /// Per side (`Dir as usize`): the fetch hit `LIMIT`.
    pub truncated: [bool; 2],
}

impl Graph {
    pub fn new(focus: Obj) -> Self {
        let key = focus.key.clone();
        Graph {
            focus: key.clone(),
            objs: HashMap::from([(key, focus)]),
            side: HashMap::new(),
            parents: HashMap::new(),
            children: HashMap::new(),
            processes: HashMap::new(),
            frontier: HashSet::new(),
            truncated: [false; 2],
        }
    }

    /// Merges one side fetched from the focus (`lineage_sql`, ordered by distance).
    // ponytail: a cut result is detected after self edges were dropped; a result cut by exactly that many is missed.
    pub fn add(&mut self, dir: Dir, mut rows: Vec<Row>) {
        let cut = rows.len() > LIMIT;
        rows.truncate(LIMIT);
        self.truncated[dir.ix()] = cut;
        let deepest = rows.last().map_or(0, |r| r.distance);
        for r in rows {
            let far = match dir {
                Dir::Up => &r.src,
                Dir::Down => &r.dst,
            };
            if far.key != self.focus && (r.distance >= MAX_DISTANCE || (cut && r.distance == deepest)) {
                self.frontier.insert((far.key.clone(), dir));
            }
            for o in [&r.src, &r.dst] {
                if o.key != self.focus {
                    self.side.entry(o.key.clone()).or_insert(dir);
                }
                // The focus starts without a kind when the tab didn't know it yet (e.g. a dynamic table).
                self.objs.entry(o.key.clone()).and_modify(|e| e.kind = e.kind.take().or_else(|| o.kind.clone())).or_insert_with(|| o.clone());
            }
            let (s, d) = (r.src.key, r.dst.key);
            self.parents.entry(d.clone()).or_default().insert(s.clone());
            self.children.entry(s.clone()).or_default().insert(d.clone());
            let p = self.processes.entry((s, d)).or_default();
            if let Some(proc_) = r.process
                && !p.contains(&proc_)
            {
                p.push(proc_);
            }
        }
    }

    /// Sorted by key: sources (`Up`) or targets (`Down`) of `k`.
    pub fn neighbors(&self, k: &ObjKey, dir: Dir) -> Vec<ObjKey> {
        let m = match dir {
            Dir::Up => &self.parents,
            Dir::Down => &self.children,
        };
        m.get(k).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    /// Neighbors away from the focus (same side), the ones a `+` on `k` reveals.
    pub fn outward(&self, k: &ObjKey, dir: Dir) -> Vec<ObjKey> {
        self.neighbors(k, dir).into_iter().filter(|n| self.side.get(n) == Some(&dir)).collect()
    }

    /// `PROCESS` values (JSON) of the edge `src → dst`.
    pub fn processes(&self, src: &ObjKey, dst: &ObjKey) -> &[String] {
        self.processes.get(&(src.clone(), dst.clone())).map_or(&[], Vec::as_slice)
    }

    pub fn count(&self, dir: Dir) -> usize {
        self.side.values().filter(|d| **d == dir).count()
    }

    /// The side `k` is drawn on; `None` for the focus.
    pub fn side_of(&self, k: &ObjKey) -> Option<Dir> {
        self.side.get(k).copied()
    }

    /// From the focus through the focus and `expanded` objects, outward only; more than `cap` children of one
    /// object → the first `cap` (by key) and a `More`. `pinned` objects show with one chain back to the focus.
    pub fn visible(&self, expanded: &HashSet<ObjKey>, pinned: &HashSet<ObjKey>, cap: usize) -> Visible {
        let mut seen: HashSet<ObjKey> = HashSet::from([self.focus.clone()]);
        let mut nodes = vec![self.focus.clone()];
        let mut more: Vec<More> = Vec::new();
        let mut queue = VecDeque::from([self.focus.clone()]);
        let reveal = |n: ObjKey, seen: &mut HashSet<ObjKey>, nodes: &mut Vec<ObjKey>, queue: &mut VecDeque<ObjKey>| {
            if seen.insert(n.clone()) {
                nodes.push(n.clone());
                queue.push_back(n);
            }
        };
        while let Some(k) = queue.pop_front() {
            let dirs: Vec<Dir> = match self.side_of(&k) {
                None => vec![Dir::Up, Dir::Down],
                Some(d) if expanded.contains(&k) => vec![d],
                Some(_) => vec![],
            };
            for d in dirs {
                let kids = self.outward(&k, d);
                let mut hidden = Vec::new();
                for (i, n) in kids.into_iter().enumerate() {
                    if i < cap || pinned.contains(&n) {
                        reveal(n, &mut seen, &mut nodes, &mut queue);
                    } else {
                        hidden.push(n);
                    }
                }
                if !hidden.is_empty() {
                    more.push(More { parent: k.clone(), dir: d, hidden });
                }
            }
        }
        for p in pinned {
            if seen.contains(p) {
                continue;
            }
            for n in self.chain(p) {
                if seen.insert(n.clone()) {
                    nodes.push(n);
                }
            }
        }
        for m in &mut more {
            m.hidden.retain(|h| !seen.contains(h));
        }
        more.retain(|m| !m.hidden.is_empty());
        Visible { nodes, more }
    }

    /// One shortest cached path from the focus to `target` (without the focus); empty if unreachable.
    fn chain(&self, target: &ObjKey) -> Vec<ObjKey> {
        let Some(dir) = self.side_of(target) else { return Vec::new() };
        let mut prev: HashMap<ObjKey, ObjKey> = HashMap::new();
        let mut queue = VecDeque::from([self.focus.clone()]);
        while let Some(k) = queue.pop_front() {
            if &k == target {
                let mut out = vec![k.clone()];
                let mut cur = k;
                while let Some(p) = prev.get(&cur) {
                    if *p == self.focus {
                        break;
                    }
                    out.push(p.clone());
                    cur = p.clone();
                }
                return out;
            }
            for n in self.outward(&k, dir) {
                if n != self.focus && !prev.contains_key(&n) {
                    prev.insert(n.clone(), k.clone());
                    queue.push_back(n);
                }
            }
        }
        Vec::new()
    }

    /// Column rows (`column_lineage_sql`) onto the objects of this graph.
    pub fn column_path(&self, rows: &[Row]) -> ColumnPath {
        let index: HashMap<(&str, &str, &str), &ObjKey> =
            self.objs.values().filter(|o| o.key.domain == "TABLE" && !o.masked).map(|o| ((o.key.db.as_str(), o.key.schema.as_str(), o.key.name.as_str()), &o.key)).collect();
        let find = |o: &Obj| index.get(&(o.key.db.as_str(), o.key.schema.as_str(), o.key.name.as_str())).map(|k| (*k).clone());
        let mut p = ColumnPath::default();
        let mut outside: HashSet<(String, String, String)> = HashSet::new();
        for r in rows {
            let (s, d) = (find(&r.src), find(&r.dst));
            for (o, key, col) in [(&r.src, &s, &r.src_col), (&r.dst, &d, &r.dst_col)] {
                match key {
                    Some(k) => {
                        let cols = p.cols.entry(k.clone()).or_default();
                        if let Some(c) = col
                            && !cols.contains(c)
                        {
                            cols.push(c.clone());
                        }
                    }
                    None => {
                        outside.insert((o.key.db.clone(), o.key.schema.clone(), o.key.name.clone()));
                    }
                }
            }
            if let (Some(a), Some(b)) = (s, d)
                && a != b
            {
                p.edges.insert((a, b));
            }
            let dir = r.dir.unwrap_or(Dir::Up);
            let (o, col) = match dir {
                Dir::Up => (&r.src, &r.src_col),
                Dir::Down => (&r.dst, &r.dst_col),
            };
            let step = (dir, r.distance, format!("{}.{}", o.key.label(), col.as_deref().unwrap_or("?")));
            if !p.steps.contains(&step) {
                p.steps.push(step);
            }
        }
        p.outside = outside.len();
        p.steps.sort();
        p
    }
}

/// Children of `parent` left out because there were more than the cap.
#[derive(Debug, Clone, PartialEq)]
pub struct More {
    pub parent: ObjKey,
    pub dir: Dir,
    pub hidden: Vec<ObjKey>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Visible {
    pub nodes: Vec<ObjKey>,
    pub more: Vec<More>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Slot {
    Node(ObjKey),
    /// Index into `Visible::more`.
    More(usize),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    /// (slot, column, row): column 0 is the focus; rows are centered on 0 in every column.
    pub pos: Vec<(Slot, i32, f32)>,
    /// (source, target, back): a back edge goes right → left (cycle through the focus).
    pub edges: Vec<(ObjKey, ObjKey, bool)>,
    pub more_edges: Vec<(ObjKey, usize)>,
}

/// Longest path from `k` to the focus within its side, ignoring edges that close a cycle.
fn depth(g: &Graph, vis: &HashSet<&ObjKey>, k: &ObjKey, dir: Dir, memo: &mut HashMap<ObjKey, i32>, stack: &mut HashSet<ObjKey>) -> i32 {
    if *k == g.focus {
        return 0;
    }
    if let Some(d) = memo.get(k) {
        return *d;
    }
    stack.insert(k.clone());
    let mut best = None;
    for n in g.neighbors(k, dir.toward_focus()) {
        if vis.contains(&n) && (n == g.focus || g.side_of(&n) == Some(dir)) && !stack.contains(&n) {
            let d = depth(g, vis, &n, dir, memo, stack) + 1;
            best = Some(best.map_or(d, |b: i32| b.max(d)));
        }
    }
    stack.remove(k);
    // Reached only through a cycle: next to the focus.
    let d = best.unwrap_or(1);
    memo.insert(k.clone(), d);
    d
}

/// Columns by longest path (every forward edge goes strictly left → right), then 4 barycenter sweeps.
pub fn layout(g: &Graph, v: &Visible) -> Layout {
    let vis: HashSet<&ObjKey> = v.nodes.iter().collect();
    let mut col: HashMap<Slot, i32> = HashMap::new();
    let mut memo = HashMap::new();
    for k in &v.nodes {
        let c = match g.side_of(k) {
            None => 0,
            Some(d) => {
                let n = depth(g, &vis, k, d, &mut memo, &mut HashSet::new());
                if d == Dir::Up { -n } else { n }
            }
        };
        col.insert(Slot::Node(k.clone()), c);
    }
    let mut edges = Vec::new();
    for k in &v.nodes {
        for c in g.neighbors(k, Dir::Down) {
            if vis.contains(&c) {
                let back = col[&Slot::Node(k.clone())] >= col[&Slot::Node(c.clone())];
                edges.push((k.clone(), c, back));
            }
        }
    }
    let mut more_edges = Vec::new();
    for (i, m) in v.more.iter().enumerate() {
        let pc = col.get(&Slot::Node(m.parent.clone())).copied().unwrap_or(0);
        col.insert(Slot::More(i), if m.dir == Dir::Up { pc - 1 } else { pc + 1 });
        more_edges.push((m.parent.clone(), i));
    }

    let mut columns: BTreeMap<i32, Vec<Slot>> = BTreeMap::new();
    for k in &v.nodes {
        columns.entry(col[&Slot::Node(k.clone())]).or_default().push(Slot::Node(k.clone()));
    }
    for i in 0..v.more.len() {
        columns.entry(col[&Slot::More(i)]).or_default().push(Slot::More(i));
    }
    let mut adj: HashMap<Slot, Vec<Slot>> = HashMap::new();
    let mut link = |a: Slot, b: Slot| {
        adj.entry(a.clone()).or_default().push(b.clone());
        adj.entry(b).or_default().push(a);
    };
    for (a, b, back) in &edges {
        if !back {
            link(Slot::Node(a.clone()), Slot::Node(b.clone()));
        }
    }
    for (p, i) in &more_edges {
        link(Slot::Node(p.clone()), Slot::More(*i));
    }

    let centered = |i: usize, n: usize| i as f32 - (n as f32 - 1.0) / 2.0;
    let keys: Vec<i32> = columns.keys().copied().collect();
    for sweep in 0..4 {
        let order: Vec<i32> = if sweep % 2 == 0 { keys.clone() } else { keys.iter().rev().copied().collect() };
        for c in order {
            if c == 0 {
                continue;
            }
            let row: HashMap<&Slot, f32> = columns.values().flat_map(|s| s.iter().enumerate().map(move |(i, x)| (x, centered(i, s.len())))).collect();
            let here = &columns[&c];
            let mut keyed: Vec<(f32, usize, Slot)> = here
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let refs: Vec<f32> = adj.get(s).into_iter().flatten().filter(|n| if sweep % 2 == 0 { col[*n] < c } else { col[*n] > c }).filter_map(|n| row.get(n).copied()).collect();
                    let v = if refs.is_empty() { centered(i, here.len()) } else { refs.iter().sum::<f32>() / refs.len() as f32 };
                    (v, i, s.clone())
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            columns.insert(c, keyed.into_iter().map(|(_, _, s)| s).collect());
        }
    }
    let pos = columns.iter().flat_map(|(c, s)| s.iter().enumerate().map(move |(i, x)| (x.clone(), *c, centered(i, s.len())))).collect();
    Layout { pos, edges, more_edges }
}

/// One column's lineage over the graph: highlighted objects and edges, and the full step list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnPath {
    pub cols: HashMap<ObjKey, Vec<String>>,
    pub edges: HashSet<(ObjKey, ObjKey)>,
    /// Objects on the path that aren't in the graph (beyond what was fetched, or masked).
    pub outside: usize,
    /// (direction, distance, `db.schema.object.column`).
    pub steps: Vec<(Dir, u32, String)>,
}

/// Query ids anywhere in a `PROCESS` value (`queryId`, `query_id`, … at any depth); unknown shape → none.
pub fn query_ids(process: &str) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    match v.as_str() {
                        Some(s) if k.to_ascii_lowercase().replace('_', "") == "queryid" => {
                            if !out.iter().any(|o| o == s) {
                                out.push(s.to_string());
                            }
                        }
                        _ => walk(v, out),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<Value>(process) {
        walk(&v, &mut out);
    }
    out
}

/// The definition of views and dynamic tables (`None` for anything else).
pub fn ddl_sql(o: &Obj) -> Option<String> {
    let typ = match o.kind.as_deref()? {
        "VIEW" | "MATERIALIZED VIEW" | "SECURE VIEW" => "VIEW",
        "DYNAMIC TABLE" => "TABLE",
        _ => return None,
    };
    Some(format!("select get_ddl('{typ}', {})", sql_str(&o.key.fq())))
}

/// Looks up a query's text; never run automatically (ACCOUNT_USAGE is slow and needs a privilege).
pub fn history_sql(query_id: &str) -> String {
    format!(
        "-- ACCOUNT_USAGE needs IMPORTED PRIVILEGES on the SNOWFLAKE database and lags up to 45 min.\n\
-- Narrowing start_time makes the lookup much faster.\n\
select query_text, user_name, role_name, start_time\n\
from snowflake.account_usage.query_history\n\
where query_id = {}\n\
  -- and start_time >= dateadd(day, -30, current_timestamp())\n;",
        sql_str(query_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 17] = [
        "dir",
        "source_object_database",
        "source_object_schema",
        "source_object_name",
        "source_object_domain",
        "source_status",
        "source_type",
        "source_column_name",
        "target_object_database",
        "target_object_schema",
        "target_object_name",
        "target_object_domain",
        "target_status",
        "target_type",
        "target_column_name",
        "distance",
        "process",
    ];

    fn names() -> Vec<String> {
        NAMES.iter().map(|s| s.to_string()).collect()
    }

    /// One side: "DB.S.NAME" or "DB.S.NAME.COL" (column row); domain TABLE unless it's a column.
    fn side(spec: &str) -> Vec<Option<String>> {
        let p: Vec<&str> = spec.split('.').collect();
        let col = p.get(3).map(|c| c.to_string());
        let domain = if col.is_some() { "COLUMN" } else { "TABLE" };
        vec![Some(p[0].into()), Some(p[1].into()), Some(p[2].into()), Some(domain.into()), Some("ACTIVE".into()), None, col]
    }

    fn row(src: &str, dst: &str, d: u32) -> Vec<Option<String>> {
        let mut r = vec![None];
        r.extend(side(src));
        r.extend(side(dst));
        r.push(Some(d.to_string()));
        r.push(None);
        r
    }

    fn rows(edges: &[(&str, &str, u32)]) -> Vec<Row> {
        let raw: Vec<_> = edges.iter().map(|(s, t, d)| row(s, t, *d)).collect();
        parse_rows(&names(), &raw)
    }

    fn k(spec: &str) -> ObjKey {
        let p: Vec<&str> = spec.split('.').collect();
        ObjKey::table(p[0], p[1], p[2])
    }

    fn focus() -> Graph {
        Graph::new(Obj { key: k("D.S.T"), kind: None, masked: false })
    }

    fn set(v: &[&str]) -> HashSet<ObjKey> {
        v.iter().map(|s| k(s)).collect()
    }

    fn shown(v: &Visible) -> HashSet<ObjKey> {
        v.nodes.iter().cloned().collect()
    }

    fn pos(l: &Layout, key: &ObjKey) -> (i32, f32) {
        l.pos.iter().find(|(s, _, _)| *s == Slot::Node(key.clone())).map(|(_, c, r)| (*c, *r)).expect("placed")
    }

    // Task 1: SQL and parsing.

    #[test]
    fn sql_quotes_and_orders() {
        let s = lineage_sql(&ObjKey::table("D", "S", "o'b"), Dir::Up, 5);
        assert!(s.contains(r#"get_lineage('"D"."S"."o''b"', 'TABLE', 'UPSTREAM', 5)"#), "{s}");
        assert!(s.contains("order by distance limit 5001"), "{s}");
        assert!(s.contains("to_json(process)"), "{s}");
        assert!(lineage_sql(&k("D.S.T"), Dir::Down, 5).contains("'DOWNSTREAM'"));
    }

    #[test]
    fn column_sql_unions_both_directions() {
        let s = column_lineage_sql(&k("D.S.T"), "C");
        assert!(s.contains(r#"'"D"."S"."T"."C"', 'COLUMN', 'UPSTREAM', 5"#), "{s}");
        assert!(s.contains("'COLUMN', 'DOWNSTREAM', 5"), "{s}");
        assert!(s.contains("union all"), "{s}");
    }

    #[test]
    fn parse_drops_self_edges_and_reads_kind() {
        let mut a = row("D.S.A", "D.S.T", 1);
        a[6] = Some("view".into());
        let mut v = row("D.S.V", "D.S.T", 1);
        v[4] = Some("VIEW".into());
        let r = parse_rows(&names(), &[a, row("D.S.T", "D.S.T", 1), v]);
        assert_eq!(r.len(), 2, "self edge dropped");
        assert_eq!(r[0].src.kind.as_deref(), Some("VIEW"));
        assert_eq!(r[0].src.key.domain, "TABLE");
        assert_eq!(r[1].src.key.domain, "TABLE", "a VIEW domain is table-like");
        assert_eq!(r[1].src.kind.as_deref(), Some("VIEW"));
        assert!(r[0].src.is_table_like());
    }

    #[test]
    fn masked_rows_without_names_never_merge() {
        let masked = |d: u32| {
            let mut r = row("D.S.X", "D.S.T", d);
            (r[1], r[2], r[3], r[5]) = (None, None, None, Some("MASKED".into()));
            r
        };
        let r = parse_rows(&names(), &[masked(1), masked(1)]);
        assert_eq!(r.len(), 2);
        assert!(r[0].src.masked && r[1].src.masked);
        assert_ne!(r[0].src.key, r[1].src.key);
        assert!(!r[0].src.is_table_like());
    }

    #[test]
    fn masked_names_are_unique_across_fetches() {
        let masked = || {
            let mut r = row("D.S.X", "D.S.T", 2);
            (r[1], r[2], r[3], r[5]) = (None, None, None, Some("MASKED".into()));
            r
        };
        let (a, b) = (parse_rows(&names(), &[masked()]), parse_rows(&names(), &[masked()]));
        assert_ne!(a[0].src.key, b[0].src.key, "same row index in two fetches must not merge");
    }

    #[test]
    fn other_domains_keep_their_raw_name() {
        let mut r = row("D.S.F", "D.S.T", 1);
        r[4] = Some("FEATURE_VIEW".into());
        r[6] = Some("feature_view".into());
        let rows = parse_rows(&names(), &[r]);
        assert_eq!(rows[0].src.key.domain, "FEATURE_VIEW", "passed back to GET_LINEAGE as is");
        assert_eq!(rows[0].src.kind.as_deref(), Some("FEATURE VIEW"));
    }

    #[test]
    fn focus_kind_comes_from_the_rows_when_unknown() {
        let mut r = row("D.S.A", "D.S.T", 1);
        r[13] = Some("VIEW".into());
        let mut g = focus();
        g.add(Dir::Up, parse_rows(&names(), &[r]));
        assert_eq!(g.objs[&k("D.S.T")].kind.as_deref(), Some("VIEW"));
    }

    #[test]
    fn masked_nodes_never_match_column_rows() {
        let mut r = row("D.S.X", "D.S.T", 1);
        (r[1], r[2], r[3], r[5]) = (None, None, None, Some("MASKED".into()));
        let mut g = focus();
        g.add(Dir::Up, parse_rows(&names(), &[r]));
        let masked = g.objs.values().find(|o| o.masked).expect("masked node").key.clone();
        let col = Row {
            src: Obj { key: masked.clone(), kind: None, masked: true },
            dst: Obj { key: k("D.S.T"), kind: None, masked: false },
            src_col: Some("C".into()),
            dst_col: Some("Y".into()),
            distance: 1,
            process: None,
            dir: Some(Dir::Up),
        };
        let p = g.column_path(&[col]);
        assert!(!p.cols.contains_key(&masked));
        assert_eq!(p.outside, 1);
    }

    // Task 2: graph and visibility.

    #[test]
    fn distance_five_is_frontier() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.A", "D.S.T", 1), ("D.S.B", "D.S.A", 2), ("D.S.C", "D.S.B", 3), ("D.S.E", "D.S.C", 4), ("D.S.F", "D.S.E", 5)]));
        assert!(g.frontier.contains(&(k("D.S.F"), Dir::Up)));
        assert!(!g.frontier.contains(&(k("D.S.E"), Dir::Up)));
        assert!(!g.truncated[0]);
        assert_eq!(g.count(Dir::Up), 5);
    }

    #[test]
    fn truncated_marks_deepest_level_and_flag() {
        let mut e: Vec<(String, String, u32)> = (0..3000).map(|i| (format!("D.S.X{i}"), "D.S.T".to_string(), 1)).collect();
        e.extend((0..2001).map(|i| (format!("D.S.Y{i}"), "D.S.X0".to_string(), 2)));
        let raw: Vec<_> = e.iter().map(|(s, t, d)| row(s, t, *d)).collect();
        let mut g = focus();
        g.add(Dir::Up, parse_rows(&names(), &raw));
        assert!(g.truncated[0]);
        assert_eq!(g.count(Dir::Up), LIMIT);
        assert!(g.frontier.contains(&(k("D.S.Y0"), Dir::Up)));
        assert!(!g.frontier.contains(&(k("D.S.X5"), Dir::Up)));
    }

    #[test]
    fn cycle_through_focus_keeps_focus_on_no_side() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.X", "D.S.T", 1), ("D.S.T", "D.S.X", 2)]));
        assert!(!g.side.contains_key(&k("D.S.T")));
        assert_eq!(g.side.get(&k("D.S.X")), Some(&Dir::Up));
        let v = g.visible(&HashSet::new(), &HashSet::new(), 30);
        assert_eq!(shown(&v), set(&["D.S.T", "D.S.X"]));
        let l = layout(&g, &v);
        assert_eq!(pos(&l, &k("D.S.T")), (0, 0.0));
        assert_eq!(pos(&l, &k("D.S.X")).0, -1);
        assert!(l.edges.contains(&(k("D.S.T"), k("D.S.X"), true)), "T → X is a back edge");
        assert!(l.edges.contains(&(k("D.S.X"), k("D.S.T"), false)));
    }

    #[test]
    fn visible_starts_one_level_each_side() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.A", "D.S.T", 1), ("D.S.B", "D.S.A", 2)]));
        g.add(Dir::Down, rows(&[("D.S.T", "D.S.C", 1), ("D.S.C", "D.S.E", 2)]));
        let v = g.visible(&HashSet::new(), &HashSet::new(), 30);
        assert_eq!(shown(&v), set(&["D.S.T", "D.S.A", "D.S.C"]));
        assert!(v.more.is_empty());
        let v = g.visible(&set(&["D.S.C"]), &HashSet::new(), 30);
        assert_eq!(shown(&v), set(&["D.S.T", "D.S.A", "D.S.C", "D.S.E"]));
    }

    #[test]
    fn expand_reveals_outward_and_collapse_hides_only_unreachable() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.A", "D.S.T", 1), ("D.S.B", "D.S.T", 1), ("D.S.S", "D.S.A", 2), ("D.S.S", "D.S.B", 2)]));
        let vis = |e: &[&str]| shown(&g.visible(&set(e), &HashSet::new(), 30));
        assert!(!vis(&[]).contains(&k("D.S.S")));
        assert!(vis(&["D.S.A"]).contains(&k("D.S.S")));
        assert!(vis(&["D.S.B"]).contains(&k("D.S.S")), "still reachable through the other parent");
        assert!(vis(&["D.S.A", "D.S.B"]).contains(&k("D.S.S")));
    }

    #[test]
    fn hub_over_cap_makes_more_and_pinned_survive() {
        let mut g = focus();
        let e: Vec<(String, u32)> = (0..40).map(|i| (format!("D.S.C{i:02}"), 1)).collect();
        let raw: Vec<_> = e.iter().map(|(t, d)| row("D.S.T", t, *d)).collect();
        g.add(Dir::Down, parse_rows(&names(), &raw));
        let v = g.visible(&HashSet::new(), &HashSet::new(), 30);
        assert_eq!(v.nodes.len(), 31);
        assert_eq!(v.more.len(), 1);
        assert_eq!(v.more[0].hidden.len(), 10);
        assert_eq!(v.more[0].dir, Dir::Down);
        let v = g.visible(&HashSet::new(), &set(&["D.S.C39"]), 30);
        assert!(shown(&v).contains(&k("D.S.C39")));
        assert_eq!(v.more[0].hidden.len(), 9);
        let l = layout(&g, &v);
        let mut cells: Vec<(i32, i64)> = l.pos.iter().map(|(_, c, r)| (*c, (*r * 2.0) as i64)).collect();
        let n = cells.len();
        cells.sort();
        cells.dedup();
        assert_eq!(cells.len(), n, "no two slots share a cell");
        assert_eq!(l.more_edges, vec![(k("D.S.T"), 0)]);
    }

    #[test]
    fn pinned_deep_node_reveals_its_chain() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.A", "D.S.T", 1), ("D.S.B", "D.S.A", 2), ("D.S.C", "D.S.B", 3)]));
        let v = g.visible(&HashSet::new(), &set(&["D.S.C"]), 30);
        assert_eq!(shown(&v), set(&["D.S.T", "D.S.A", "D.S.B", "D.S.C"]));
    }

    // Task 3: layout.

    #[test]
    fn diamond_is_strictly_left_to_right() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.A", "D.S.T", 1), ("D.S.S", "D.S.T", 1), ("D.S.S", "D.S.A", 2)]));
        let v = g.visible(&HashSet::new(), &HashSet::new(), 30);
        let l = layout(&g, &v);
        assert_eq!(pos(&l, &k("D.S.S")).0, -2);
        assert_eq!(pos(&l, &k("D.S.A")).0, -1);
        for (a, b, back) in &l.edges {
            assert!(!back);
            assert!(pos(&l, a).0 < pos(&l, b).0, "{a:?} → {b:?}");
        }
    }

    #[test]
    fn focus_at_origin_and_columns_centered() {
        let mut g = focus();
        g.add(Dir::Down, rows(&[("D.S.T", "D.S.C1", 1), ("D.S.T", "D.S.C2", 1), ("D.S.T", "D.S.C3", 1)]));
        let l = layout(&g, &g.visible(&HashSet::new(), &HashSet::new(), 30));
        assert_eq!(pos(&l, &k("D.S.T")), (0, 0.0));
        let mut r: Vec<f32> = ["D.S.C1", "D.S.C2", "D.S.C3"].iter().map(|s| pos(&l, &k(s)).1).collect();
        r.sort_by(f32::total_cmp);
        assert_eq!(r, vec![-1.0, 0.0, 1.0]);
    }

    #[test]
    fn barycenter_uncrosses_simple_case() {
        // C2 expanded reveals E first; F (pinned) comes after: E–C2 and F–C1 start crossed.
        let mut g = focus();
        g.add(Dir::Down, rows(&[("D.S.T", "D.S.C1", 1), ("D.S.T", "D.S.C2", 1), ("D.S.C1", "D.S.F", 2), ("D.S.C2", "D.S.E", 2)]));
        let l = layout(&g, &g.visible(&set(&["D.S.C2"]), &set(&["D.S.F"]), 30));
        let r = |s: &str| pos(&l, &k(s)).1;
        assert!((r("D.S.E") - r("D.S.F")) * (r("D.S.C2") - r("D.S.C1")) > 0.0, "E–C2 and F–C1 must not cross");
    }

    // Task 4: column paths and SQL helpers.

    fn col_rows(edges: &[(&str, &str, &str, u32)]) -> Vec<Row> {
        let raw: Vec<_> = edges
            .iter()
            .map(|(dir, s, t, d)| {
                let mut r = row(s, t, *d);
                r[0] = Some(dir.to_string());
                r
            })
            .collect();
        parse_rows(&names(), &raw)
    }

    #[test]
    fn column_rows_map_onto_table_nodes() {
        let mut g = focus();
        g.add(Dir::Up, rows(&[("D.S.A", "D.S.T", 1)]));
        g.add(Dir::Down, rows(&[("D.S.T", "D.S.C", 1)]));
        let p = g.column_path(&col_rows(&[("UP", "D.S.A.X", "D.S.T.Y", 1), ("DOWN", "D.S.T.Y", "D.S.C.Z", 1)]));
        assert_eq!(p.cols.get(&k("D.S.A")), Some(&vec!["X".to_string()]));
        assert_eq!(p.cols.get(&k("D.S.T")), Some(&vec!["Y".to_string()]));
        assert!(p.edges.contains(&(k("D.S.A"), k("D.S.T"))) && p.edges.contains(&(k("D.S.T"), k("D.S.C"))));
        assert_eq!(p.outside, 0);
        assert_eq!(p.steps, vec![(Dir::Up, 1, "D.S.A.X".to_string()), (Dir::Down, 1, "D.S.C.Z".to_string())]);
    }

    #[test]
    fn column_outside_graph_counted() {
        let g = focus();
        let p = g.column_path(&col_rows(&[("UP", "D.S.Z.C", "D.S.T.Y", 1), ("UP", "D.S.Z.C2", "D.S.T.Y2", 1)]));
        assert_eq!(p.outside, 1, "one object outside, however many columns");
        assert_eq!(p.cols.get(&k("D.S.T")), Some(&vec!["Y".to_string(), "Y2".to_string()]));
        assert!(p.edges.is_empty());
    }

    #[test]
    fn query_ids_found_in_nested_shapes() {
        assert_eq!(query_ids(r#"{"queryId":"01a"}"#), vec!["01a"]);
        assert_eq!(query_ids(r#"{"x":{"query_id":"01b"}}"#), vec!["01b"]);
        assert_eq!(query_ids(r#"[{"QUERY_ID":"01c"},{"QUERY_ID":"01c"}]"#), vec!["01c"]);
        assert!(query_ids(r#"{"procedure":"P"}"#).is_empty());
        assert!(query_ids("not json").is_empty());
    }

    #[test]
    fn ddl_only_for_views_and_dynamic_tables() {
        let o = |kind: Option<&str>| Obj { key: k("D.S.V"), kind: kind.map(String::from), masked: false };
        assert_eq!(ddl_sql(&o(Some("VIEW"))).as_deref(), Some(r#"select get_ddl('VIEW', '"D"."S"."V"')"#));
        assert!(ddl_sql(&o(Some("MATERIALIZED VIEW"))).is_some_and(|s| s.contains("'VIEW'")));
        assert!(ddl_sql(&o(Some("DYNAMIC TABLE"))).is_some_and(|s| s.contains("'TABLE'")));
        assert_eq!(ddl_sql(&o(Some("TABLE"))), None);
        assert_eq!(ddl_sql(&o(None)), None);
    }

    #[test]
    fn history_sql_escapes_id() {
        let s = history_sql("a'b");
        assert!(s.contains("query_id = 'a''b'"), "{s}");
        assert!(s.contains("snowflake.account_usage.query_history"));
    }
}
