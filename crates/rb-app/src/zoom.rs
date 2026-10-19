//! Zoom and fit for the diagram views (query plan, lineage).

/// Inner margin of the diagram area (`p_4`), subtracted when fitting to screen.
pub const PAD: f32 = 16.0;

pub fn step_zoom(z: f32, up: bool) -> f32 {
    (if up { z * 1.25 } else { z / 1.25 }).clamp(0.25, 2.0)
}

/// Zoom at which the whole diagram fits the area (never enlarging beyond 1).
pub fn fit_zoom(content: (f32, f32), view: (f32, f32)) -> f32 {
    (view.0 / content.0).min(view.1 / content.1).clamp(0.25, 1.0)
}

/// ⌘ + scroll: continuous zoom (trackpad sends many small events); scrolling up zooms in.
pub fn wheel_zoom(z: f32, dy: f32) -> f32 {
    (z * (dy / 200.0).exp()).clamp(0.25, 2.0)
}

/// New scroll offset (≤ 0) of an axis so the point under the pointer stays under it when zoom changes.
/// `pointer` is relative to the area's edge; content starts after the `PAD` margin, which doesn't scale.
pub fn zoom_anchor(offset: f32, pointer: f32, z0: f32, z1: f32) -> f32 {
    let content = (pointer - offset - PAD) / z0;
    (pointer - PAD - content * z1).min(0.0)
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
    fn step_and_fit_are_clamped() {
        assert_eq!(step_zoom(2.0, true), 2.0);
        assert_eq!(step_zoom(0.25, false), 0.25);
        assert_eq!(step_zoom(1.0, true), 1.25);
        assert_eq!(fit_zoom((1000.0, 400.0), (500.0, 400.0)), 0.5);
        assert_eq!(fit_zoom((100.0, 100.0), (500.0, 400.0)), 1.0, "never enlarges beyond 1");
        assert_eq!(fit_zoom((10_000.0, 100.0), (500.0, 400.0)), 0.25, "minimum");
    }
}
