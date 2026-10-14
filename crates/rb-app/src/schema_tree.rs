use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::*;
use rb_core::metadata::{MetaCache, MetaRequest};

use crate::RefreshNode;
use crate::meta::MetaStore;

const SEP: &str = "\u{1f}";

fn id(parts: &[&str]) -> SharedString {
    parts.join(SEP).into()
}

fn loading(parent: &SharedString) -> TreeItem {
    TreeItem::new(format!("{parent}{SEP}loading"), "carregando…")
}

type OpenTable = Rc<dyn Fn(String, String, &mut Window, &mut App)>;

pub struct SchemaTree {
    meta: Entity<MetaStore>,
    state: Entity<TreeState>,
    expanded: HashSet<SharedString>,
    on_open_table: OpenTable,
    _subs: Vec<Subscription>,
}

impl SchemaTree {
    pub fn new(meta: Entity<MetaStore>, on_open_table: OpenTable, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| TreeState::new(cx));
        let subs = vec![
            cx.observe(&meta, |this, _, cx| this.rebuild(cx)),
            cx.subscribe(&state, |this, _, ev: &TreeEvent, cx| this.on_tree_event(ev, cx)),
        ];
        let mut this = SchemaTree { meta, state, expanded: HashSet::new(), on_open_table, _subs: subs };
        this.rebuild(cx);
        this
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let items = build_items(&self.meta.read(cx).cache, &self.expanded);
        // ponytail: set_items zera a seleção; restaurar quando incomodar.
        self.state.update(cx, |st, cx| st.set_items(items, cx));
        cx.notify();
    }

    fn on_tree_event(&mut self, ev: &TreeEvent, cx: &mut Context<Self>) {
        match ev {
            TreeEvent::Expanded(node) => {
                self.expanded.insert(node.clone());
                let parts: Vec<&str> = node.split(SEP).collect();
                self.meta.update(cx, |m, cx| match parts.as_slice() {
                    ["s", schema] => m.request(MetaRequest::Objects(schema.to_string()), cx),
                    ["r", schema, rel] => {
                        m.request(MetaRequest::Columns(schema.to_string(), rel.to_string()), cx);
                        m.request(MetaRequest::Indexes(schema.to_string(), rel.to_string()), cx);
                    }
                    _ => {}
                });
            }
            TreeEvent::Collapsed(node) => {
                self.expanded.remove(node);
            }
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.state.read(cx).selected_item().map(|i| i.id.clone()) else {
            self.meta.update(cx, |m, cx| m.reload_roots(cx));
            return;
        };
        let parts: Vec<String> = node.split(SEP).map(String::from).collect();
        self.meta.update(cx, |m, cx| match parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
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
    c.schemas
        .iter()
        .map(|s| {
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
                        rel_group("tables", "Tabelas", &o.tables),
                        rel_group("views", "Views", &o.views),
                        rel_group("matviews", "Views materializadas", &o.matviews),
                        TreeItem::new(fid.clone(), format!("Funções ({})", o.functions.len()))
                            .expanded(open(&fid))
                            .children(o.functions.iter().map(|f| TreeItem::new(id(&["f", s, f]), f.clone()))),
                    ]
                }
            };
            TreeItem::new(sid.clone(), s.clone()).expanded(open(&sid)).children(children)
        })
        .collect()
}

fn relation(c: &MetaCache, schema: &str, rel: &str, expanded: &HashSet<SharedString>) -> TreeItem {
    let rid = id(&["r", schema, rel]);
    let key = (schema.to_string(), rel.to_string());
    let children = match (c.columns.get(&key), c.indexes.get(&key)) {
        (Some(cols), idx) => {
            let cg = id(&["cg", schema, rel]);
            let mut groups = vec![TreeItem::new(cg.clone(), format!("Colunas ({})", cols.len())).expanded(expanded.contains(&cg)).children(
                cols.iter().map(|col| {
                    let pk = if col.is_pk { " 🔑" } else { "" };
                    TreeItem::new(id(&["c", schema, rel, &col.name]), format!("{}  {}{pk}", col.name, col.type_name))
                }),
            )];
            if let Some(idx) = idx {
                let ig = id(&["ig", schema, rel]);
                groups.push(
                    TreeItem::new(ig.clone(), format!("Índices ({})", idx.len()))
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
        div()
            .key_context("SchemaTree")
            .size_full()
            .on_action(cx.listener(|t, _: &RefreshNode, _, cx| t.refresh(cx)))
            // A árvore não se foca sozinha ao clicar; sem foco o F5 não chega aqui.
            .on_mouse_down(MouseButton::Left, cx.listener(|t, _, window, cx| t.state.update(cx, |s, cx| s.focus(window, cx))))
            .child(tree(
            &self.state,
            move |ix, entry, selected, _window, _cx| {
                let node = entry.item().id.clone();
                let open = open.clone();
                ListItem::new(ix).selected(selected).pl(px(8. + 14. * entry.depth() as f32)).child(entry.item().label.clone()).on_click(
                    move |ev, window, cx| {
                        let parts: Vec<&str> = node.split(SEP).collect();
                        if ev.click_count() == 2
                            && let ["r", schema, rel] = parts.as_slice()
                        {
                            open(schema.to_string(), rel.to_string(), window, cx);
                        }
                    },
                )
            },
        ))
    }
}
