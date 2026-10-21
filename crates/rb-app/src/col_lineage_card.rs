//! Hover card on a column name in the Definition tab: where the column comes from, drawn as
//! layers (farthest source on the left, the column itself on the right). Clicking a source opens it.

use std::collections::BTreeMap;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::table_info::LinNode;
use rust_i18n::t;

use crate::table_sections::kind_dot;
use crate::table_tab::TableTab;
use crate::ui::Load;

const MAX_W: f32 = 1200.;
const MAX_H: f32 = 560.;

/// Height for the drawing: the tallest layer's boxes plus title and note, up to `MAX_H` (then it scrolls).
fn card_height(tallest: usize) -> f32 {
    (110. + tallest as f32 * 48.).clamp(170., MAX_H)
}

pub struct ColLineageCard {
    tab: WeakEntity<TableTab>,
    col: String,
    _sub: Subscription,
}

impl ColLineageCard {
    /// Starts the column's lineage query (shared with the Definition panel's "Comes from").
    pub fn new(tab: Entity<TableTab>, col: String, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&tab, |_, _, cx| cx.notify());
        let (weak, c) = (tab.downgrade(), col.clone());
        // Not while the tooltip is being built: right after.
        cx.defer(move |cx| {
            weak.update(cx, |t, cx| t.ensure_col_lineage(c, cx)).ok();
        });
        ColLineageCard { tab: tab.downgrade(), col, _sub: sub }
    }
}

/// Sources by hop, farthest first: each inner list is one column of the drawing.
fn layers(nodes: &[LinNode]) -> Vec<Vec<&LinNode>> {
    let mut by: BTreeMap<u32, Vec<&LinNode>> = BTreeMap::new();
    for n in nodes {
        by.entry(n.distance).or_default().push(n);
    }
    by.into_values().rev().collect()
}

impl Render for ColLineageCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let card = v_flex()
            .id("col-lineage-card")
            // The wheel scrolls the card, not the list behind it.
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            // Large, but never wider than the window.
            .w(px(MAX_W.min(f32::from(window.viewport_size().width) - 80.)))
            .p_3()
            .gap_2()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .text_xs();
        let Some(tab) = self.tab.upgrade() else { return card };
        let t = tab.read(cx);
        let (db, schema) = t.db_schema();
        let project = t.project(cx);
        let open_table = t.open_table.clone();
        let muted = |s: String| div().text_color(theme.muted_foreground).child(s);
        let node = |id: usize, title: String, place: String, kind: String, open: Option<(String, String)>| {
            let open_table = open_table.clone();
            h_flex()
                .id(("lin-node", id))
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if open.is_none() { theme.primary } else { theme.border })
                .bg(theme.background)
                .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(kind_dot(&kind, &theme)))
                .child(v_flex().child(div().font_family(theme.mono_font_family.clone()).child(title)).child(muted(place)))
                .when_some(open, |d, (schema, table)| d.cursor_pointer().hover(|s| s.bg(theme.secondary_hover)).on_click(move |_, window, cx| open_table(schema.clone(), table.clone(), window, cx)))
        };
        let kind_of = |db: &str, schema: &str, object: &str| project.as_ref().and_then(|p| p.manifest.find_table(db, schema, object)).map(|(f, _)| f.resource_type.clone()).unwrap_or_default();
        let card = card.child(div().font_weight(FontWeight::SEMIBOLD).child(t!("tv.lineage_of", col = self.col.clone()).to_string()));
        let body = match t.col_lineage.get(&self.col) {
            Some(Load::Ready(all)) => {
                let shown: Vec<LinNode> = all.iter().filter(|n| !t.conn.org.lineage_ignored(&n.db, &n.schema, &n.object)).cloned().collect();
                if shown.is_empty() {
                    muted(t!("tv.no_lineage").to_string()).into_any_element()
                } else {
                    let mut graph = h_flex().flex_none().items_center().gap_2().whitespace_nowrap();
                    let mut id = 0;
                    let drawn = layers(&shown);
                    let tallest = drawn.iter().map(Vec::len).max().unwrap_or(1);
                    for layer in drawn {
                        let col = v_flex().gap_1().children(layer.into_iter().map(|n| {
                            id += 1;
                            node(
                                id,
                                format!("{}.{}", n.object, n.column),
                                format!("{}.{} · {}", n.db, n.schema, kind_of(&n.db, &n.schema, &n.object)),
                                kind_of(&n.db, &n.schema, &n.object),
                                Some((format!("{}.{}", n.db, n.schema), n.object.clone())),
                            )
                        }));
                        graph = graph.child(col).child(muted("→".into()));
                    }
                    let here = node(0, format!("{}.{}", t.table, self.col), format!("{db}.{schema}"), kind_of(&db, &schema, &t.table), None);
                    // Fixed height so the drawing area can scroll both ways inside it.
                    v_flex()
                        .h(px(card_height(tallest) - 50.))
                        .gap_2()
                        .child(div().id("col-lineage-scroll").flex_1().min_h_0().min_w_0().overflow_scroll().child(graph.child(here)))
                        .child(muted(t!("tv.lineage_lag").to_string()))
                        .into_any_element()
                }
            }
            Some(Load::Failed(m)) => v_flex().child(div().text_color(theme.danger).child(m.clone())).child(muted(t!("tv.hint_lineage").to_string())).into_any_element(),
            _ => muted(t!("tree.loading").to_string()).into_any_element(),
        };
        card.child(body)
    }
}

#[cfg(test)]
mod tests {
    use rb_core::table_info::LinNode;

    #[test]
    fn taller_lineages_get_taller_cards_up_to_the_cap() {
        assert!(super::card_height(1) < super::card_height(4));
        assert_eq!(super::card_height(100), super::MAX_H);
    }

    #[test]
    fn farthest_hop_is_the_first_layer() {
        let n = |o: &str, d| LinNode { db: "D".into(), schema: "S".into(), object: o.into(), column: "C".into(), distance: d };
        let nodes = [n("STG", 1), n("RAW_A", 2), n("RAW_B", 2)];
        let got: Vec<Vec<&str>> = super::layers(&nodes).into_iter().map(|l| l.into_iter().map(|x| x.object.as_str()).collect()).collect();
        assert_eq!(got, [vec!["RAW_A", "RAW_B"], vec!["STG"]]);
    }
}
