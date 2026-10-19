use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::dialect::Dialect;
use rb_core::metadata::{MetaCache, MetaRequest};

use rust_i18n::t;

use crate::RefreshNode;
use crate::lang::Lang;
use crate::meta::MetaStore;

const SEP: &str = "\u{1f}";

fn id(parts: &[&str]) -> SharedString {
    parts.join(SEP).into()
}

/// Tree ids from the parts of `Hit::tree_path`.
pub fn path_ids(path: &[Vec<String>]) -> Vec<SharedString> {
    path.iter().map(|p| id(&p.iter().map(String::as_str).collect::<Vec<_>>())).collect()
}

/// Node to select after `set_items` (which clears the selection): the target of a pending search,
/// otherwise whatever was already selected. `true` = it is the search target (scroll to it).
fn selection_after_rebuild(pending: Option<SharedString>, previous: Option<SharedString>) -> Option<(SharedString, bool)> {
    pending.map(|p| (p, true)).or(previous.map(|p| (p, false)))
}

fn loading(parent: &SharedString) -> TreeItem {
    TreeItem::new(format!("{parent}{SEP}loading"), t!("tree.loading").to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Database,
    Schema,
    Group,
    Table,
    View,
    MatView,
    Function,
    Columns,
    Column,
    PkColumn,
    Indexes,
    Index,
    Loading,
}

impl Kind {
    fn icon(self, expanded: bool) -> Option<IconName> {
        Some(match self {
            Kind::Database => IconName::Database,
            Kind::Schema => IconName::Layers,
            Kind::Group if expanded => IconName::FolderOpen,
            Kind::Group => IconName::Folder,
            Kind::Table => IconName::Table,
            Kind::View => IconName::Eye,
            Kind::MatView => IconName::Sheet,
            Kind::Function => IconName::SquareFunction,
            Kind::Columns => IconName::Columns3,
            Kind::Column => IconName::Hash,
            Kind::PkColumn => IconName::KeyRound,
            Kind::Indexes | Kind::Index => IconName::ListTree,
            Kind::Loading => return None,
        })
    }
}

/// Node kind and the detail shown in gray (column type), from the id and the cache.
fn describe(node: &str, c: &MetaCache) -> (Kind, Option<String>) {
    let parts: Vec<&str> = node.split(SEP).collect();
    let kind = match parts.as_slice() {
        [.., "loading"] => Kind::Loading,
        ["d", _] => Kind::Database,
        ["s", _] => Kind::Schema,
        ["g", ..] => Kind::Group,
        ["r", s, rel] => match c.objects.get(*s) {
            Some(o) if o.views.iter().any(|v| v == rel) => Kind::View,
            Some(o) if o.matviews.iter().any(|v| v == rel) => Kind::MatView,
            _ => Kind::Table,
        },
        ["f", ..] => Kind::Function,
        ["cg", ..] => Kind::Columns,
        ["c", s, rel, col] => {
            let meta = c.columns.get(&(s.to_string(), rel.to_string())).and_then(|cols| cols.iter().find(|x| x.name == *col));
            let kind = if meta.is_some_and(|m| m.is_pk) { Kind::PkColumn } else { Kind::Column };
            return (kind, meta.map(|m| m.type_name.clone()));
        }
        ["ig", ..] => Kind::Indexes,
        _ => Kind::Index,
    };
    (kind, None)
}

pub type OpenTable = Rc<dyn Fn(String, String, &mut Window, &mut App)>;

pub struct SchemaTree {
    meta: Entity<MetaStore>,
    state: Entity<TreeState>,
    expanded: HashSet<SharedString>,
    /// Node to select as soon as it appears (quick search); ancestors may still be loading.
    pending_reveal: Option<SharedString>,
    on_open_table: OpenTable,
    _subs: Vec<Subscription>,
}

impl SchemaTree {
    pub fn new(meta: Entity<MetaStore>, on_open_table: OpenTable, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| TreeState::new(cx));
        let subs = vec![
            // Labels ("Tables (n)", "loading…") live in the items: new language, new items.
            cx.observe_global::<Lang>(|this, cx| this.rebuild(cx)),
            cx.observe(&meta, |this, _, cx| this.rebuild(cx)),
            cx.subscribe(&state, |this, _, ev: &TreeEvent, cx| this.on_tree_event(ev, cx)),
        ];
        let mut this = SchemaTree { meta, state, expanded: HashSet::new(), pending_reveal: None, on_open_table, _subs: subs };
        this.rebuild(cx);
        this
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let items = build_items(&self.meta.read(cx).cache, &self.expanded);
        let pending = self.pending_reveal.clone();
        let revealed = self.state.update(cx, |st, cx| {
            let previous = st.selected_item().map(|i| i.id.clone());
            st.set_items(items, cx);
            let (target, is_reveal) = selection_after_rebuild(pending, previous)?;
            let ix = st.index_of(&target)?;
            st.set_selected_index(Some(ix), cx);
            if is_reveal {
                st.reveal_item(&target, ScrollStrategy::Center, cx);
            }
            is_reveal.then_some(())
        });
        if revealed.is_some() {
            self.pending_reveal = None;
        }
        cx.notify();
    }

    /// Expands from the top down to the target (requesting what is still missing) and selects the target once it appears.
    pub fn reveal(&mut self, path: Vec<Vec<String>>, cx: &mut Context<Self>) {
        let ids = path_ids(&path);
        let Some((target, ancestors)) = ids.split_last() else { return };
        for node in ancestors {
            self.expanded.insert(node.clone());
            self.load_children(node, cx);
        }
        self.pending_reveal = Some(target.clone());
        self.rebuild(cx);
    }

    fn on_tree_event(&mut self, ev: &TreeEvent, cx: &mut Context<Self>) {
        match ev {
            TreeEvent::Expanded(node) => {
                self.expanded.insert(node.clone());
                self.load_children(node, cx);
            }
            TreeEvent::Collapsed(node) => {
                self.expanded.remove(node);
            }
        }
    }

    /// Asks the cache for the children of a node that was just opened.
    fn load_children(&mut self, node: &SharedString, cx: &mut Context<Self>) {
        let parts: Vec<&str> = node.split(SEP).collect();
        self.meta.update(cx, |m, cx| match parts.as_slice() {
            ["d", db] => m.request(MetaRequest::Schemas(db.to_string()), cx),
            ["s", schema] => m.request(MetaRequest::Objects(schema.to_string()), cx),
            ["r", schema, rel] => {
                m.request(MetaRequest::Columns(schema.to_string(), rel.to_string()), cx);
                if m.cache.dialect == Dialect::Postgres {
                    m.request(MetaRequest::Indexes(schema.to_string(), rel.to_string()), cx);
                }
            }
            _ => {}
        });
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.state.read(cx).selected_item().map(|i| i.id.clone()) else {
            self.meta.update(cx, |m, cx| m.reload_roots(cx));
            return;
        };
        let parts: Vec<String> = node.split(SEP).map(String::from).collect();
        self.meta.update(cx, |m, cx| match parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["d", db, ..] => {
                m.cache.loaded_dbs.remove(*db);
                m.request(MetaRequest::Schemas(db.to_string()), cx);
            }
            ["s", schema, ..] | ["g", schema, ..] | ["f", schema, ..] => {
                m.cache.invalidate_schema(schema);
                m.request(MetaRequest::Objects(schema.to_string()), cx);
            }
            [_, schema, rel, ..] => {
                m.cache.invalidate_table(schema, rel);
                m.request(MetaRequest::Columns(schema.to_string(), rel.to_string()), cx);
                m.request(MetaRequest::Indexes(schema.to_string(), rel.to_string()), cx);
            }
            _ => m.reload_roots(cx),
        });
    }
}

fn build_items(c: &MetaCache, expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
    let open = |id: &SharedString| expanded.contains(id);
    if c.dialect == Dialect::Snowflake {
        return c
            .visible_databases()
            .map(|db| {
                let did = id(&["d", db]);
                let prefix = format!("{db}.");
                let children = if c.loaded_dbs.contains(db) { c.schemas.iter().filter_map(|k| k.strip_prefix(&prefix).map(|s| schema_item(c, k, s, expanded))).collect() } else { vec![loading(&did)] };
                TreeItem::new(did.clone(), db.clone()).expanded(open(&did)).children(children)
            })
            .collect();
    }
    c.schemas.iter().map(|s| schema_item(c, s, s, expanded)).collect()
}

/// Schema node: `key` is the cache key (on Snowflake, `DB.SCHEMA`), `label` is what is shown.
fn schema_item(c: &MetaCache, s: &str, label: &str, expanded: &HashSet<SharedString>) -> TreeItem {
    let open = |id: &SharedString| expanded.contains(id);
    let sid = id(&["s", s]);
    let children = match c.objects.get(s) {
        None => vec![loading(&sid)],
        Some(o) => {
            let rel_group = |key: &str, label: &str, names: &[String]| {
                let gid = id(&["g", s, key]);
                let rels = names.iter().map(|n| relation(c, s, n, expanded)).collect::<Vec<_>>();
                TreeItem::new(gid.clone(), format!("{label} ({})", names.len())).expanded(open(&gid)).children(rels)
            };
            let fid = id(&["g", s, "functions"]);
            vec![
                rel_group("tables", &t!("tree.tables"), &o.tables),
                rel_group("views", &t!("tree.views"), &o.views),
                rel_group("matviews", &t!("tree.matviews"), &o.matviews),
                TreeItem::new(fid.clone(), format!("{} ({})", t!("tree.functions"), o.functions.len()))
                    .expanded(open(&fid))
                    .children(o.functions.iter().map(|f| TreeItem::new(id(&["f", s, f]), f.clone()))),
            ]
        }
    };
    TreeItem::new(sid.clone(), label.to_string()).expanded(open(&sid)).children(children)
}

fn relation(c: &MetaCache, schema: &str, rel: &str, expanded: &HashSet<SharedString>) -> TreeItem {
    let rid = id(&["r", schema, rel]);
    let key = (schema.to_string(), rel.to_string());
    let children = match (c.columns.get(&key), c.indexes.get(&key)) {
        (Some(cols), idx) => {
            let cg = id(&["cg", schema, rel]);
            let mut groups = vec![
                TreeItem::new(cg.clone(), format!("{} ({})", t!("tree.columns"), cols.len()))
                    .expanded(expanded.contains(&cg))
                    .children(cols.iter().map(|col| TreeItem::new(id(&["c", schema, rel, &col.name]), col.name.clone()))),
            ];
            if c.dialect == Dialect::Postgres
                && let Some(idx) = idx
            {
                let ig = id(&["ig", schema, rel]);
                groups.push(
                    TreeItem::new(ig.clone(), format!("{} ({})", t!("tree.indexes"), idx.len()))
                        .expanded(expanded.contains(&ig))
                        .children(idx.iter().map(|i| TreeItem::new(id(&["i", schema, rel, &i.name]), i.name.clone()))),
                );
            }
            groups
        }
        (None, _) => vec![loading(&rid)],
    };
    TreeItem::new(rid.clone(), rel.to_string()).expanded(expanded.contains(&rid)).children(children)
}

impl Render for SchemaTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.on_open_table.clone();
        let meta = self.meta.clone();
        div()
            .key_context("SchemaTree")
            .size_full()
            .on_action(cx.listener(|t, _: &RefreshNode, _, cx| t.refresh(cx)))
            // The tree does not focus itself on click; without focus F5 never gets here.
            .on_mouse_down(MouseButton::Left, cx.listener(|t, _, window, cx| t.state.update(cx, |s, cx| s.focus(window, cx))))
            .child(tree(&self.state, move |ix, entry, selected, _window, cx| {
                let node = entry.item().id.clone();
                let open = open.clone();
                let (kind, detail) = describe(&node, &meta.read(cx).cache);
                let theme = cx.theme();
                let icon_color = if kind == Kind::PkColumn { theme.warning } else { theme.muted_foreground };
                // Fixed height per row (virtualized list): never wrap, truncate with "…".
                let row = h_flex()
                    .w_full()
                    .gap_1p5()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .when_some(kind.icon(entry.is_expanded()), |r, icon| r.child(Icon::new(icon).small().text_color(icon_color)))
                    // With a detail (column type), the name stays whole and the detail is what gets truncated.
                    .child(div().when(detail.is_some(), |d| d.flex_shrink_0()).when(detail.is_none(), |d| d.min_w_0().overflow_hidden().text_ellipsis()).child(entry.item().label.clone()))
                    .when_some(detail, |r, d| r.child(div().min_w_0().overflow_hidden().text_ellipsis().text_color(theme.muted_foreground).child(d)));
                let is_rel = node.starts_with(&format!("r{SEP}"));
                ListItem::new(ix)
                    .selected(selected)
                    .pl(px(8. + 14. * entry.depth() as f32))
                    .child(row)
                    // The tree expands/collapses on every mouse_down: on a double click the second one would collapse
                    // the table that was just expanded. It is left only for opening the tab.
                    .when(is_rel, |li| {
                        li.on_mouse_down(MouseButton::Left, |ev, _, cx| {
                            if ev.click_count >= 2 {
                                cx.stop_propagation()
                            }
                        })
                    })
                    .on_click(move |ev, window, cx| {
                        let parts: Vec<&str> = node.split(SEP).collect();
                        if ev.click_count() == 2
                            && let ["r", schema, rel] = parts.as_slice()
                        {
                            open(schema.to_string(), rel.to_string(), window, cx);
                        }
                    })
            }))
    }
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` brings GPUI\'s own `test` attribute; this one wins over the glob.
    use core::prelude::v1::test;

    use rb_core::metadata::{ColumnMeta, SchemaObjects};

    use super::*;

    fn cache() -> MetaCache {
        let mut c = MetaCache::default();
        c.objects.insert("public".into(), SchemaObjects { tables: vec!["t".into()], views: vec!["v".into()], matviews: vec!["m".into()], functions: vec!["f()".into()] });
        let col = |name: &str, pk: bool| ColumnMeta { name: name.into(), type_name: "integer".into(), cast_type: "integer".into(), nullable: !pk, default: None, is_pk: pk, generated: false };
        c.columns.insert(("public".into(), "t".into()), vec![col("id", true), col("n", false)]);
        c
    }

    #[test]
    fn snowflake_tree_starts_at_databases() {
        let mut c = cache();
        c.dialect = rb_core::dialect::Dialect::Snowflake;
        c.databases = vec!["DB".into(), "OTHER".into()];
        c.schemas = vec!["DB.PUBLIC".into()];
        c.loaded_dbs.insert("DB".into());
        let items = build_items(&c, &HashSet::new());
        assert_eq!(items.iter().map(|i| i.label.to_string()).collect::<Vec<_>>(), vec!["DB", "OTHER"]);
        assert_eq!(items[0].children.iter().map(|i| i.label.to_string()).collect::<Vec<_>>(), vec!["PUBLIC"]);
        assert_eq!(describe(&id(&["d", "DB"]), &c).0, Kind::Database);
        assert_eq!(items[1].children[0].label.to_string(), t!("tree.loading"));
        c.db_filter = vec!["oth*".into()];
        let items = build_items(&c, &HashSet::new());
        assert_eq!(items.iter().map(|i| i.label.to_string()).collect::<Vec<_>>(), vec!["OTHER"]);
    }

    #[test]
    fn each_node_gets_its_kind() {
        let c = cache();
        let k = |parts: &[&str]| describe(&id(parts), &c).0;
        assert_eq!(k(&["s", "public"]), Kind::Schema);
        assert_eq!(k(&["s", "public", "loading"]), Kind::Loading);
        assert_eq!(k(&["g", "public", "tables"]), Kind::Group);
        assert_eq!(k(&["r", "public", "t"]), Kind::Table);
        assert_eq!(k(&["r", "public", "v"]), Kind::View);
        assert_eq!(k(&["r", "public", "m"]), Kind::MatView);
        assert_eq!(k(&["f", "public", "f()"]), Kind::Function);
        assert_eq!(k(&["cg", "public", "t"]), Kind::Columns);
        assert_eq!(k(&["c", "public", "t", "id"]), Kind::PkColumn);
        assert_eq!(k(&["c", "public", "t", "n"]), Kind::Column);
        assert_eq!(k(&["ig", "public", "t"]), Kind::Indexes);
        assert_eq!(k(&["i", "public", "t", "t_pkey"]), Kind::Index);
    }

    #[test]
    fn rebuild_keeps_selection_unless_a_reveal_is_pending() {
        let (a, b): (SharedString, SharedString) = ("a".into(), "b".into());
        assert_eq!(selection_after_rebuild(None, Some(a.clone())), Some((a.clone(), false)));
        assert_eq!(selection_after_rebuild(Some(b.clone()), Some(a)), Some((b, true)));
        assert_eq!(selection_after_rebuild(None, None), None);
    }

    #[test]
    fn revealed_path_is_built_expanded() {
        let mut c = cache();
        c.schemas = vec!["public".into()];
        let hit = rb_core::quick_open::Hit { kind: rb_core::quick_open::HitKind::Table, matview: false, database: None, schema: Some("public".into()), name: "t".into() };
        let ids = path_ids(&hit.tree_path());
        let expanded: HashSet<SharedString> = ids[..ids.len() - 1].iter().cloned().collect();
        let items = build_items(&c, &expanded);
        let schema = items.iter().find(|i| i.id == ids[0]).unwrap();
        assert!(schema.is_expanded());
        let group = schema.children.iter().find(|i| i.id == ids[1]).unwrap();
        assert!(group.is_expanded() && group.children.iter().any(|i| i.id == ids[2]));
    }

    #[test]
    fn columns_carry_their_type_as_detail() {
        assert_eq!(describe(&id(&["c", "public", "t", "n"]), &cache()).1.as_deref(), Some("integer"));
        assert_eq!(describe(&id(&["r", "public", "t"]), &cache()).1, None);
    }
}
