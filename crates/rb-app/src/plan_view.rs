//! Execution plan drawn as a diagram: a box per node, child → parent arrows, zoom and details.

use rust_i18n::t;
use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::explain::{Placed, Plan, PlanNode, PlanNote, Profile, edge_width, hottest, layout, node_at, short_count, top_nodes};

use crate::{PlanFit, PlanZoomIn, PlanZoomOut};

const BOX_W: f32 = 220.0;
const BOX_H: f32 = 84.0;
/// Inner margin of the diagram area (`p_4`), subtracted when fitting to screen.
const PAD: f32 = 16.0;
const GAP_X: f32 = 32.0;
const GAP_Y: f32 = 56.0;

/// 0 = neutral, 1 = strong red; saturates at 50%.
fn heat(pct: f64) -> f32 {
    (pct / 50.0).clamp(0.0, 1.0) as f32
}

fn step_zoom(z: f32, up: bool) -> f32 {
    (if up { z * 1.25 } else { z / 1.25 }).clamp(0.25, 2.0)
}

/// Zoom at which the whole diagram fits the area (never enlarging beyond 1).
fn fit_zoom(content: (f32, f32), view: (f32, f32)) -> f32 {
    (view.0 / content.0).min(view.1 / content.1).clamp(0.25, 1.0)
}

/// ⌘ + scroll: continuous zoom (trackpad sends many small events); scrolling up zooms in.
fn wheel_zoom(z: f32, dy: f32) -> f32 {
    (z * (dy / 200.0).exp()).clamp(0.25, 2.0)
}

/// New scroll offset (≤ 0) of an axis so the point under the pointer stays under it when zoom changes.
/// `pointer` is relative to the area's edge; content starts after the `PAD` margin, which doesn't scale.
fn zoom_anchor(offset: f32, pointer: f32, z0: f32, z1: f32) -> f32 {
    let content = (pointer - offset - PAD) / z0;
    (pointer - PAD - content * z1).min(0.0)
}

fn rows_of(n: &PlanNode) -> Option<f64> {
    n.act_rows.or(n.est_rows)
}

fn human(ms: f64) -> String {
    if ms < 1000.0 { format!("{ms:.0} ms") } else { format!("{:.1} s", ms / 1000.0) }
}

/// EXECUTION_TIME_BREAKDOWN categories, with the Query Profile's names.
fn cat_label(cat: &str) -> String {
    match cat {
        "processing" => t!("plan.cat_processing").to_string(),
        "synchronization" => t!("plan.cat_sync").to_string(),
        "initialization" => t!("plan.cat_init").to_string(),
        "local_disk_io" => t!("plan.cat_local_io").to_string(),
        "remote_disk_io" => t!("plan.cat_remote_io").to_string(),
        "network_communication" => t!("plan.cat_network").to_string(),
        other => other.replace('_', " "),
    }
}

fn cat_color(cat: &str, theme: &gpui_kit::component::Theme) -> Hsla {
    match cat {
        "processing" => theme.primary,
        "synchronization" => theme.info,
        "initialization" => theme.warning,
        "local_disk_io" => theme.success,
        "remote_disk_io" => theme.danger,
        _ => theme.muted_foreground,
    }
}

pub struct PlanView {
    plan: Plan,
    placed: Vec<Placed>,
    selected: Vec<usize>,
    /// Right panel in the Query Profile summary (nothing selected). Only on Snowflake's real plan.
    overview: bool,
    zoom: f32,
    fitted: bool,
    /// Visible area, measured in the last frame (fit to screen and zoom at pointer).
    viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
}

impl PlanView {
    pub fn new(plan: Plan, cx: &mut Context<Self>) -> Self {
        let placed = layout(&plan.root, BOX_W, BOX_H, GAP_X, GAP_Y);
        let selected = hottest(&plan.root);
        let overview = plan.profile.is_some();
        PlanView { plan, placed, selected, overview, zoom: 1.0, fitted: false, viewport: Rc::default(), scroll: ScrollHandle::new(), focus: cx.focus_handle() }
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

    /// ⌘ + scroll: changes zoom keeping the point under the pointer fixed.
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

    /// Like the Query Profile panel: most expensive nodes, where the time went and what was read.
    fn render_overview(&self, p: &Profile, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let head = |s: String| div().pt_2().font_weight(FontWeight::SEMIBOLD).child(s);
        let row = |k: String, v: String| h_flex().justify_between().gap_2().text_sm().child(div().text_color(theme.muted_foreground).child(k)).child(div().font_family(theme.mono_font_family.clone()).child(v));
        let mut out = v_flex().gap_2();

        let top = top_nodes(&self.plan.root, 5);
        out = out.child(head(t!("plan.most_expensive").to_string()));
        for (i, path) in top.into_iter().enumerate() {
            let Some(n) = node_at(&self.plan.root, &path) else { continue };
            let label = match n.id {
                Some(id) => format!("{} [{id}]", n.op),
                None => n.op.clone(),
            };
            out = out.child(
                h_flex()
                    .id(("top", i))
                    .justify_between()
                    .px_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .text_sm()
                    .hover(|d| d.bg(theme.secondary_hover))
                    .child(label)
                    .child(div().font_family(theme.mono_font_family.clone()).child(format!("{:.1}%", n.pct)))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selected = path.clone();
                        v.overview = false;
                        cx.notify();
                    })),
            );
        }

        out = out.child(head(t!("plan.overview").to_string()));
        if !p.breakdown.is_empty() {
            let total: f64 = p.breakdown.iter().map(|(_, v)| v).sum::<f64>().max(1e-9);
            let mut bar = h_flex().h(px(6.)).w_full().rounded(px(3.)).overflow_hidden();
            for (cat, v) in &p.breakdown {
                bar = bar.child(div().h_full().w(relative((*v / total) as f32)).bg(cat_color(cat, &theme)));
            }
            out = out.child(bar);
            for (cat, v) in &p.breakdown {
                out = out.child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().size(px(6.)).rounded_full().bg(cat_color(cat, &theme)))
                        .child(div().flex_1().text_color(theme.muted_foreground).child(cat_label(cat)))
                        .child(div().font_family(theme.mono_font_family.clone()).child(format!("{:.1}%", v / total * 100.0))),
                );
            }
        }
        if let Some(t) = p.timing {
            out = out
                .child(row(t!("plan.total_time").to_string(), human(t.total_ms)))
                .child(row(t!("plan.compile_time").to_string(), human(t.compile_ms)))
                .when(t.queued_ms > 0.0, |d| d.child(row(t!("plan.queued_time").to_string(), human(t.queued_ms))))
                .child(row(t!("plan.exec_time").to_string(), human(t.exec_ms)));
        }

        out = out.child(head(t!("plan.statistics").to_string()));
        if let Some(sp) = p.scan_progress {
            out = out.child(row(t!("plan.scan_progress").to_string(), format!("{:.2}%", sp * 100.0)));
        }
        out = out.child(row(t!("plan.bytes_scanned").to_string(), format!("{}B", short_count(p.bytes_scanned))));
        if let Some(c) = p.from_cache {
            out = out.child(row(t!("plan.from_cache").to_string(), format!("{:.2}%", c * 100.0)));
        }
        if p.partitions_total > 0.0 {
            out = out.child(row(t!("plan.partitions_scanned").to_string(), short_count(p.partitions_scanned))).child(row(t!("plan.partitions_total").to_string(), short_count(p.partitions_total)));
        }
        if p.spilled_local > 0.0 {
            out = out.child(row(t!("plan.spilled_local").to_string(), format!("{}B", short_count(p.spilled_local))));
        }
        if p.spilled_remote > 0.0 {
            out = out.child(row(t!("plan.spilled_remote").to_string(), format!("{}B", short_count(p.spilled_remote))));
        }
        if p.network_bytes > 0.0 {
            out = out.child(row(t!("plan.network").to_string(), format!("{}B", short_count(p.network_bytes))));
        }
        out
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Div {
        if self.overview
            && let Some(p) = self.plan.profile.clone()
        {
            return self.render_overview(&p, cx);
        }
        let back = self.plan.profile.is_some().then(|| {
            Button::new("plan-overview").label(t!("plan.back_overview").to_string()).ghost().xsmall().on_click(cx.listener(|v, _, _, cx| {
                v.overview = true;
                cx.notify();
            }))
        });
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
            .children(back)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(match n.id {
                Some(id) => format!("{} [{id}]", n.op),
                None => n.op.clone(),
            }))
            .when_some(n.summary.clone().filter(|s| Some(s) != n.object.as_ref()), |d, s| d.child(div().text_sm().font_family("monospace").child(s)))
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
                // The area hasn't been measured yet: fits on the next frame.
                window.request_animation_frame();
            }
        }
        let z = self.zoom;
        let theme = cx.theme();
        let (w, h) = self.content_size();
        let nodes: Vec<&PlanNode> = self.placed.iter().map(|p| node_at(&self.plan.root, &p.path).expect("layout only has valid paths")).collect();

        // Child (top) → parent (bottom) arrows, in content coordinates already zoomed.
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

        let no_pct = self.plan.note;
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
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_weight(FontWeight::SEMIBOLD).child(match n.id {
                            Some(id) => format!("{} [{id}]", n.op),
                            None => n.op.clone(),
                        }))
                        // With no data at all, "0%" everywhere is misleading: the notice at the top explains.
                        .when(no_pct.is_none(), |d| d.child(format!("{:.0}%", n.pct))),
                )
                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.muted_foreground).child(n.summary.clone().or_else(|| n.object.clone()).unwrap_or_default()))
                // % bar as in the Query Profile (only when there are numbers).
                .when(no_pct.is_none() && self.plan.analyzed, |d| {
                    d.child(div().h(px(3.0 * z)).w_full().rounded_full().bg(theme.secondary).child(div().h_full().rounded_full().bg(theme.primary).w(relative((n.pct / 100.0) as f32))))
                })
                .child(
                    h_flex()
                        .gap_1()
                        .child(div().flex_1().overflow_hidden().text_ellipsis().whitespace_nowrap().child(format!("{figure}{rows}")))
                        .when(!n.warn.is_empty(), |d| d.child(div().text_color(theme.warning).child("⚠"))),
                )
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.selected = path.clone();
                    v.overview = false;
                    v.focus.focus(window, cx);
                    cx.notify();
                }))
        });

        let note = self.plan.note.map(|n| match n {
            PlanNote::NoCost { secure_views: true } if self.plan.analyzed => t!("plan.note_no_cost_secure_real").to_string(),
            PlanNote::NoCost { secure_views: true } => t!("plan.note_no_cost_secure").to_string(),
            PlanNote::NoCost { .. } if self.plan.analyzed => t!("plan.note_no_cost_real").to_string(),
            PlanNote::NoCost { .. } => t!("plan.note_no_cost").to_string(),
            PlanNote::Cached => t!("plan.note_cached").to_string(),
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
            .when_some(note, |d, n| d.child(div().mx_2().mb_1().px_2().py_1().rounded(theme.radius).bg(theme.secondary).text_sm().text_color(theme.muted_foreground).child(n)))
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
                            // ⌘ + scroll zooms; registered on a child to run before the area's scrolling.
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
        assert!(wheel_zoom(1.0, 10.0) > 1.0, "scrolling up zooms in");
        assert!(wheel_zoom(1.0, -10.0) < 1.0);
        assert_eq!(wheel_zoom(1.0, 0.0), 1.0);
        assert_eq!(wheel_zoom(2.0, 1000.0), 2.0);
        assert_eq!(wheel_zoom(0.25, -1000.0), 0.25);
    }

    #[::core::prelude::v1::test]
    fn zoom_keeps_the_point_under_the_pointer() {
        // No scroll, pointer at 116 px (16 margin + 100 content): content point 100 at zoom 1.
        let off = zoom_anchor(0.0, 116.0, 1.0, 2.0);
        assert_eq!(off, -100.0, "at zoom 2 point 100 becomes 200; scroll 100 to stay under the pointer");
        assert_eq!(zoom_anchor(-100.0, 116.0, 2.0, 1.0), 0.0, "back");
        assert_eq!(zoom_anchor(0.0, 500.0, 1.0, 0.25), 0.0, "zooming out at the start: never scrolls before the beginning");
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
        assert_eq!(fit_zoom((100.0, 100.0), (500.0, 400.0)), 1.0, "never enlarges beyond 1");
        assert_eq!(fit_zoom((10_000.0, 100.0), (500.0, 400.0)), 0.25, "minimum");
    }
}
