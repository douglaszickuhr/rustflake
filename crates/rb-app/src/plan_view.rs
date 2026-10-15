//! Plano de execução desenhado como diagrama: caixas por nó, setas filho → pai, zoom e detalhes.

use rust_i18n::t;
use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::explain::{Placed, Plan, PlanNode, edge_width, hottest, layout, node_at, short_count};

use crate::{PlanFit, PlanZoomIn, PlanZoomOut};

const BOX_W: f32 = 220.0;
const BOX_H: f32 = 76.0;
/// Margem interna da área do diagrama (`p_4`), descontada no ajustar à tela.
const PAD: f32 = 16.0;
const GAP_X: f32 = 32.0;
const GAP_Y: f32 = 56.0;

/// 0 = neutro, 1 = vermelho forte; satura em 50%.
fn heat(pct: f64) -> f32 {
    (pct / 50.0).clamp(0.0, 1.0) as f32
}

fn step_zoom(z: f32, up: bool) -> f32 {
    (if up { z * 1.25 } else { z / 1.25 }).clamp(0.25, 2.0)
}

/// Zoom que cabe o diagrama inteiro na área (sem ampliar além de 1).
fn fit_zoom(content: (f32, f32), view: (f32, f32)) -> f32 {
    (view.0 / content.0).min(view.1 / content.1).clamp(0.25, 1.0)
}

fn rows_of(n: &PlanNode) -> Option<f64> {
    n.act_rows.or(n.est_rows)
}

pub struct PlanView {
    plan: Plan,
    placed: Vec<Placed>,
    selected: Vec<usize>,
    zoom: f32,
    fitted: bool,
    /// Tamanho visível da área, medido no último quadro (para o "ajustar à tela").
    viewport: Rc<Cell<Option<(f32, f32)>>>,
    focus: FocusHandle,
}

impl PlanView {
    pub fn new(plan: Plan, cx: &mut Context<Self>) -> Self {
        let placed = layout(&plan.root, BOX_W, BOX_H, GAP_X, GAP_Y);
        let selected = hottest(&plan.root);
        PlanView { plan, placed, selected, zoom: 1.0, fitted: false, viewport: Rc::default(), focus: cx.focus_handle() }
    }

    fn content_size(&self) -> (f32, f32) {
        let w = self.placed.iter().map(|p| p.x).fold(0.0, f32::max) + BOX_W;
        let h = self.placed.iter().map(|p| p.y).fold(0.0, f32::max) + BOX_H;
        (w, h)
    }

    fn fit(&mut self) {
        if let Some(v) = self.viewport.get().filter(|v| v.0 > 2.0 * PAD && v.1 > 2.0 * PAD) {
            self.zoom = fit_zoom(self.content_size(), (v.0 - 2.0 * PAD, v.1 - 2.0 * PAD));
            self.fitted = true;
        }
    }

    fn render_details(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let Some(n) = node_at(&self.plan.root, &self.selected) else { return v_flex() };
        let mut nums: Vec<(String, String)> = Vec::new();
        if let Some(r) = n.est_rows {
            nums.push((t!("plan.est_rows").to_string(), short_count(r)));
        }
        if let Some(r) = n.act_rows {
            nums.push((t!("plan.act_rows").to_string(), short_count(r)));
        }
        if let Some(ms) = n.time_ms {
            nums.push((t!("plan.time").to_string(), format!("{ms:.2} ms")));
        }
        if let Some(c) = n.cost {
            nums.push((t!("plan.cost").to_string(), format!("{c:.1}")));
        }
        nums.push(("%".into(), format!("{:.1}%", n.pct)));
        let kv = |(k, v): (String, String)| v_flex().child(div().text_xs().text_color(theme.muted_foreground).child(k)).child(div().text_sm().font_family("monospace").child(v));
        v_flex()
            .gap_2()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(n.op.clone()))
            .when_some(n.object.clone(), |d, o| d.child(div().text_sm().text_color(theme.muted_foreground).child(o)))
            .children(n.warn.iter().map(|w| div().text_sm().text_color(theme.warning).child(format!("⚠ {w}"))))
            .children(nums.into_iter().map(kv))
            .children(n.detail.iter().cloned().map(kv))
    }
}

impl Render for PlanView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.fitted {
            self.fit();
            if !self.fitted {
                // A área ainda não foi medida: ajusta no próximo quadro.
                window.request_animation_frame();
            }
        }
        let z = self.zoom;
        let theme = cx.theme();
        let (w, h) = self.content_size();
        let nodes: Vec<&PlanNode> = self.placed.iter().map(|p| node_at(&self.plan.root, &p.path).expect("layout só tem caminhos válidos")).collect();

        // Setas filho (topo) → pai (base), em coordenadas do conteúdo já com zoom.
        let edges: Vec<((f32, f32), (f32, f32), f32)> = self
            .placed
            .iter()
            .zip(&nodes)
            .filter_map(|(p, n)| {
                let parent = &self.placed[p.parent?];
                Some((((p.x + BOX_W / 2.0) * z, p.y * z), ((parent.x + BOX_W / 2.0) * z, (parent.y + BOX_H) * z), edge_width(rows_of(n)) * z.max(0.5)))
            })
            .collect();
        let edge_color = theme.muted_foreground.opacity(0.6);
        let arrows = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                for (from, to, width) in &edges {
                    let mut b = PathBuilder::stroke(px(*width));
                    b.move_to(bounds.origin + point(px(from.0), px(from.1)));
                    b.line_to(bounds.origin + point(px(to.0), px(to.1)));
                    if let Ok(path) = b.build() {
                        window.paint_path(path, edge_color);
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let labels = self.placed.iter().zip(&nodes).filter_map(|(p, n)| {
            let parent = &self.placed[p.parent?];
            let rows = rows_of(n)?;
            let (x, y) = ((p.x + parent.x + BOX_W) / 2.0 * z, (p.y + parent.y + BOX_H) / 2.0 * z);
            Some(div().absolute().left(px(x + 4.0)).top(px(y - 8.0)).text_xs().text_color(theme.muted_foreground).child(short_count(rows)))
        });

        let boxes = self.placed.iter().zip(&nodes).enumerate().map(|(i, (p, n))| {
            let selected = p.path == self.selected;
            let path = p.path.clone();
            let figure = match (n.time_ms, n.cost) {
                (Some(ms), _) => format!("{ms:.1} ms"),
                (None, Some(c)) if self.plan.analyzed || c < 1e6 => format!("{} {}", t!("plan.cost"), short_count(c)),
                (None, Some(b)) => format!("{}B", short_count(b)),
                (None, None) => String::new(),
            };
            let rows = rows_of(n).map(|r| format!(" · {} {}", short_count(r), t!("plan.rows"))).unwrap_or_default();
            v_flex()
                .id(("plan-node", i))
                .absolute()
                .left(px(p.x * z))
                .top(px(p.y * z))
                .w(px(BOX_W * z))
                .h(px(BOX_H * z))
                .px(px(8.0 * z))
                .py(px(4.0 * z))
                .overflow_hidden()
                .rounded(theme.radius)
                .border_1()
                .when(selected, |d| d.border_2())
                .border_color(if selected { theme.primary } else { theme.border })
                .bg(theme.background.blend(theme.danger.opacity(heat(n.pct) * 0.6)))
                .text_size(px(12.0 * z))
                .cursor_pointer()
                .child(
                    h_flex()
                        .gap_1()
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_weight(FontWeight::SEMIBOLD).child(n.op.clone()))
                        .child(format!("{:.0}%", n.pct)),
                )
                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.muted_foreground).child(n.object.clone().unwrap_or_default()))
                .child(
                    h_flex()
                        .gap_1()
                        .child(div().flex_1().overflow_hidden().text_ellipsis().whitespace_nowrap().child(format!("{figure}{rows}")))
                        .when(!n.warn.is_empty(), |d| d.child(div().text_color(theme.warning).child("⚠"))),
                )
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.selected = path.clone();
                    v.focus.focus(window, cx);
                    cx.notify();
                }))
        });

        let header = if self.plan.analyzed {
            t!("plan.real", time = self.plan.total_ms.map_or(String::new(), |ms| format!("{ms:.1} ms"))).to_string()
        } else {
            t!("plan.estimated").to_string()
        };
        let viewport = self.viewport.clone();
        let content = div().relative().w(px(w * z)).h(px(h * z)).child(arrows).children(labels).children(boxes);
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
            .child(h_flex().px_2().py_1().gap_3().text_sm().child(header).child(div().text_color(theme.muted_foreground).child(t!("plan.zoom_hint").to_string())))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("plan-area")
                            .relative()
                            .flex_1()
                            .h_full()
                            .overflow_scroll()
                            .p(px(PAD))
                            .child(canvas(move |b, _, _| viewport.set(Some((f32::from(b.size.width), f32::from(b.size.height)))), |_, _, _, _| {}).absolute().size_full())
                            .child(content),
                    )
                    .child(div().id("plan-details").w(px(280.)).h_full().overflow_y_scroll().border_l_1().border_color(theme.border).p_3().child(self.render_details(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn heat_and_zoom_are_clamped() {
        assert_eq!(heat(0.0), 0.0);
        assert_eq!(heat(25.0), 0.5);
        assert_eq!(heat(90.0), 1.0);
        assert_eq!(step_zoom(2.0, true), 2.0);
        assert_eq!(step_zoom(0.25, false), 0.25);
        assert_eq!(step_zoom(1.0, true), 1.25);
        assert_eq!(fit_zoom((1000.0, 400.0), (500.0, 400.0)), 0.5);
        assert_eq!(fit_zoom((100.0, 100.0), (500.0, 400.0)), 1.0, "não amplia além de 1");
        assert_eq!(fit_zoom((10_000.0, 100.0), (500.0, 400.0)), 0.25, "mínimo");
    }
}
