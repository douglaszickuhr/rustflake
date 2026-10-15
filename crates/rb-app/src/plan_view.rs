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

/// ⌘ + scroll: zoom contínuo (trackpad manda muitos eventos pequenos); para cima aproxima.
fn wheel_zoom(z: f32, dy: f32) -> f32 {
    (z * (dy / 200.0).exp()).clamp(0.25, 2.0)
}

/// Nova rolagem (≤ 0) de um eixo para que o ponto sob o ponteiro continue sob ele ao trocar o zoom.
/// `pointer` é relativo à borda da área; o conteúdo começa depois da margem `PAD`, que não escala.
fn zoom_anchor(offset: f32, pointer: f32, z0: f32, z1: f32) -> f32 {
    let content = (pointer - offset - PAD) / z0;
    (pointer - PAD - content * z1).min(0.0)
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
    /// Área visível, medida no último quadro (ajustar à tela e zoom pelo ponteiro).
    viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
}

impl PlanView {
    pub fn new(plan: Plan, cx: &mut Context<Self>) -> Self {
        let placed = layout(&plan.root, BOX_W, BOX_H, GAP_X, GAP_Y);
        let selected = hottest(&plan.root);
        PlanView { plan, placed, selected, zoom: 1.0, fitted: false, viewport: Rc::default(), scroll: ScrollHandle::new(), focus: cx.focus_handle() }
    }

    fn content_size(&self) -> (f32, f32) {
        let w = self.placed.iter().map(|p| p.x).fold(0.0, f32::max) + BOX_W;
        let h = self.placed.iter().map(|p| p.y).fold(0.0, f32::max) + BOX_H;
        (w, h)
    }

    pub fn zoom_step(&mut self, up: bool, cx: &mut Context<Self>) {
        self.zoom = step_zoom(self.zoom, up);
        cx.notify();
    }

    pub fn zoom_fit(&mut self, cx: &mut Context<Self>) {
        self.fit();
        cx.notify();
    }

    fn fit(&mut self) {
        let size = self.viewport.get().map(|b| (f32::from(b.size.width), f32::from(b.size.height)));
        if let Some(v) = size.filter(|v| v.0 > 2.0 * PAD && v.1 > 2.0 * PAD) {
            self.zoom = fit_zoom(self.content_size(), (v.0 - 2.0 * PAD, v.1 - 2.0 * PAD));
            self.fitted = true;
        }
    }

    /// ⌘ + scroll: muda o zoom mantendo parado o ponto sob o ponteiro.
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
            .on_action(cx.listener(|v, _: &PlanZoomIn, _, cx| v.zoom_step(true, cx)))
            .on_action(cx.listener(|v, _: &PlanZoomOut, _, cx| v.zoom_step(false, cx)))
            .on_action(cx.listener(|v, _: &PlanFit, _, cx| v.zoom_fit(cx)))
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
                            .track_scroll(&self.scroll)
                            .p(px(PAD))
                            .child(canvas(move |b, _, _| viewport.set(Some(b)), |_, _, _, _| {}).absolute().size_full())
                            // ⌘ + scroll dá zoom; registrado num filho para rodar antes da rolagem da área.
                            .child(div().min_w_full().min_h_full().on_scroll_wheel(cx.listener(|v, ev: &ScrollWheelEvent, _, cx| {
                                if ev.modifiers.platform {
                                    cx.stop_propagation();
                                    v.wheel(ev, cx);
                                }
                            })).child(content)),
                    )
                    .child(div().id("plan-details").w(px(280.)).h_full().overflow_y_scroll().border_l_1().border_color(theme.border).p_3().child(self.render_details(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn wheel_zoom_is_smooth_and_clamped() {
        assert!(wheel_zoom(1.0, 10.0) > 1.0, "rolar para cima aproxima");
        assert!(wheel_zoom(1.0, -10.0) < 1.0);
        assert_eq!(wheel_zoom(1.0, 0.0), 1.0);
        assert_eq!(wheel_zoom(2.0, 1000.0), 2.0);
        assert_eq!(wheel_zoom(0.25, -1000.0), 0.25);
    }

    #[::core::prelude::v1::test]
    fn zoom_keeps_the_point_under_the_pointer() {
        // Sem rolagem, ponteiro a 116 px (16 de margem + 100 de conteúdo): ponto de conteúdo 100 no zoom 1.
        let off = zoom_anchor(0.0, 116.0, 1.0, 2.0);
        assert_eq!(off, -100.0, "no zoom 2 o ponto 100 vira 200; rola 100 para continuar sob o ponteiro");
        assert_eq!(zoom_anchor(-100.0, 116.0, 2.0, 1.0), 0.0, "volta");
        assert_eq!(zoom_anchor(0.0, 500.0, 1.0, 0.25), 0.0, "afastando no início: nunca rola para antes do começo");
    }

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
