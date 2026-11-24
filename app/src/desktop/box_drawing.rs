//! Procedural pixel-aligned rasterization for box drawing, block elements,
//! braille patterns, and geometric Powerline separators.

use super::font::FontMetrics;

/// Returns `true` if `character` is rendered procedurally instead of through
/// the font rasterizer.
pub(super) fn is_supported(character: char) -> bool {
    matches!(
        character,
        '\u{2500}'..='\u{259f}' | '\u{2800}'..='\u{28ff}' | '\u{e0b0}'..='\u{e0bf}'
    )
}

/// Rasterizes a supported geometric character into `pixels` as an RGBA8 alpha
/// mask (`[255, 255, 255, alpha]`).
///
/// Returns `Some(true)` if any pixel is visible, `Some(false)` if the glyph is
/// intentionally blank (such as `U+2800`), or `None` if `character` should be
/// handled by the font rasterizer.
pub(super) fn rasterize(
    metrics: FontMetrics,
    character: char,
    width: u32,
    height: u32,
    pixels: &mut Vec<u8>,
) -> Option<bool> {
    if !is_supported(character) || width == 0 || height == 0 {
        return None;
    }
    if character == '\u{2800}' {
        return Some(false);
    }

    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    pixels.clear();
    pixels.resize(len, 0);

    let mut canvas = Canvas {
        width,
        height,
        pixels,
    };
    let stroke = StrokeMetrics::new(metrics.size, width, height);

    let visible = match character {
        '\u{2500}'..='\u{257f}' => draw_box_drawing(&mut canvas, stroke, character),
        '\u{2580}'..='\u{259f}' => draw_block_element(&mut canvas, character),
        '\u{2801}'..='\u{28ff}' => draw_braille(&mut canvas, character as u32 - 0x2800),
        '\u{e0b0}'..='\u{e0bf}' => draw_powerline(&mut canvas, stroke, character),
        _ => return None,
    };
    Some(visible)
}

#[derive(Clone, Copy)]
struct StrokeMetrics {
    light: u32,
    /// `[start, end)` X range for a centered light vertical stroke.
    vx0: u32,
    vx1: u32,
    /// `[start, end)` Y range for a centered light horizontal stroke.
    hy0: u32,
    hy1: u32,
    /// `[start, end)` X range for a centered heavy vertical stroke.
    hvx0: u32,
    hvx1: u32,
    /// `[start, end)` Y range for a centered heavy horizontal stroke.
    hhy0: u32,
    hhy1: u32,
    /// Left and right rails `[start, end)` for a double vertical stroke.
    dvx_left: (u32, u32),
    dvx_right: (u32, u32),
    /// Top and bottom rails `[start, end)` for a double horizontal stroke.
    dhy_top: (u32, u32),
    dhy_bottom: (u32, u32),
}

impl StrokeMetrics {
    fn new(font_size: f32, width: u32, height: u32) -> Self {
        let min_dim = width.min(height).max(1);
        let light = ((font_size / 14.0).round().max(1.0) as u32).min(min_dim);
        let heavy = (light * 2).min(min_dim);

        let vx0 = (width - light) / 2;
        let vx1 = vx0 + light;
        let hy0 = (height - light) / 2;
        let hy1 = hy0 + light;

        let hvx0 = (width - heavy) / 2;
        let hvx1 = hvx0 + heavy;
        let hhy0 = (height - heavy) / 2;
        let hhy1 = hhy0 + heavy;

        let dvx = double_rails(width, light);
        let dhy = double_rails(height, light);

        Self {
            light,
            vx0,
            vx1,
            hy0,
            hy1,
            hvx0,
            hvx1,
            hhy0,
            hhy1,
            dvx_left: dvx.0,
            dvx_right: dvx.1,
            dhy_top: dhy.0,
            dhy_bottom: dhy.1,
        }
    }

    fn center_x(self) -> f32 {
        (self.vx0 + self.vx1) as f32 * 0.5
    }

    fn center_y(self) -> f32 {
        (self.hy0 + self.hy1) as f32 * 0.5
    }
}

fn double_rails(extent: u32, thickness: u32) -> ((u32, u32), (u32, u32)) {
    let t = thickness.min(extent);
    if extent <= t {
        return ((0, extent), (0, extent));
    }
    // Total span of two rails + gap, shrinking rail/gap gracefully for tiny cells.
    let gap = t.max(1);
    let (rail, total) = if t * 2 + gap <= extent {
        (t, t * 2 + gap)
    } else if extent >= 3 {
        let rail = (extent - 1) / 2;
        let gap = extent - rail * 2;
        (rail.max(1), rail * 2 + gap)
    } else {
        return ((0, 1), (extent - 1, extent));
    };
    let start = (extent - total) / 2;
    let first = (start, start + rail);
    let second = (start + total - rail, start + total);
    (first, second)
}

struct Canvas<'a> {
    width: u32,
    height: u32,
    pixels: &'a mut [u8],
}

impl Canvas<'_> {
    fn fill_rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, alpha: u8) {
        if alpha == 0 {
            return;
        }
        let x0 = x0.min(self.width);
        let x1 = x1.min(self.width);
        let y0 = y0.min(self.height);
        let y1 = y1.min(self.height);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            let row = (y as usize * self.width as usize) * 4;
            for x in x0..x1 {
                let idx = row + x as usize * 4;
                let dst = &mut self.pixels[idx..idx + 4];
                if alpha > dst[3] {
                    dst[0] = 255;
                    dst[1] = 255;
                    dst[2] = 255;
                    dst[3] = alpha;
                }
            }
        }
    }

    fn blend_pixel(&mut self, x: u32, y: u32, alpha: u8) {
        if x >= self.width || y >= self.height || alpha == 0 {
            return;
        }
        let idx = (y as usize * self.width as usize + x as usize) * 4;
        let dst = &mut self.pixels[idx..idx + 4];
        if alpha > dst[3] {
            dst[0] = 255;
            dst[1] = 255;
            dst[2] = 255;
            dst[3] = alpha;
        }
    }

    fn has_visible(&self) -> bool {
        self.pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0)
    }

    fn draw_segment(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32) {
        let half = thickness * 0.5;
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len_sq = dx * dx + dy * dy;
        if len_sq == 0.0 {
            return;
        }
        let min_x = (x0.min(x1) - half - 1.0).floor().max(0.0) as u32;
        let max_x = (x0.max(x1) + half + 1.0).ceil().min(self.width as f32) as u32;
        let min_y = (y0.min(y1) - half - 1.0).floor().max(0.0) as u32;
        let max_y = (y0.max(y1) + half + 1.0).ceil().min(self.height as f32) as u32;
        for y in min_y..max_y {
            let py = y as f32 + 0.5;
            for x in min_x..max_x {
                let px = x as f32 + 0.5;
                let t = (((px - x0) * dx + (py - y0) * dy) / len_sq).clamp(0.0, 1.0);
                let cx = x0 + t * dx;
                let cy = y0 + t * dy;
                let signed = (px - cx).hypot(py - cy) - half;
                let alpha = ((0.5 - signed).clamp(0.0, 1.0) * 255.0).round() as u8;
                self.blend_pixel(x, y, alpha);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    None,
    Light,
    Heavy,
    Double,
}

#[derive(Clone, Copy)]
struct Arms {
    left: Arm,
    right: Arm,
    up: Arm,
    down: Arm,
}

impl Arms {
    const fn new(left: Arm, right: Arm, up: Arm, down: Arm) -> Self {
        Self {
            left,
            right,
            up,
            down,
        }
    }
}

fn draw_box_drawing(canvas: &mut Canvas<'_>, s: StrokeMetrics, ch: char) -> bool {
    match ch {
        // Dashed horizontal lines
        '\u{2504}' => draw_dashed_h(canvas, s.hy0, s.hy1, 3),
        '\u{2505}' => draw_dashed_h(canvas, s.hhy0, s.hhy1, 3),
        '\u{2508}' => draw_dashed_h(canvas, s.hy0, s.hy1, 4),
        '\u{2509}' => draw_dashed_h(canvas, s.hhy0, s.hhy1, 4),
        '\u{254c}' => draw_dashed_h(canvas, s.hy0, s.hy1, 2),
        '\u{254d}' => draw_dashed_h(canvas, s.hhy0, s.hhy1, 2),
        // Dashed vertical lines
        '\u{2506}' => draw_dashed_v(canvas, s.vx0, s.vx1, 3),
        '\u{2507}' => draw_dashed_v(canvas, s.hvx0, s.hvx1, 3),
        '\u{250a}' => draw_dashed_v(canvas, s.vx0, s.vx1, 4),
        '\u{250b}' => draw_dashed_v(canvas, s.hvx0, s.hvx1, 4),
        '\u{254e}' => draw_dashed_v(canvas, s.vx0, s.vx1, 2),
        '\u{254f}' => draw_dashed_v(canvas, s.hvx0, s.hvx1, 2),
        // Rounded corners: ╭ ╮ ╯ ╰
        '\u{256d}' => draw_arc(canvas, s, true, true),
        '\u{256e}' => draw_arc(canvas, s, false, true),
        '\u{256f}' => draw_arc(canvas, s, false, false),
        '\u{2570}' => draw_arc(canvas, s, true, false),
        // Diagonals: ╱ ╲ ╳
        '\u{2571}' => {
            canvas.draw_segment(
                canvas.width as f32,
                0.0,
                0.0,
                canvas.height as f32,
                s.light as f32,
            );
        }
        '\u{2572}' => {
            canvas.draw_segment(
                0.0,
                0.0,
                canvas.width as f32,
                canvas.height as f32,
                s.light as f32,
            );
        }
        '\u{2573}' => {
            canvas.draw_segment(
                canvas.width as f32,
                0.0,
                0.0,
                canvas.height as f32,
                s.light as f32,
            );
            canvas.draw_segment(
                0.0,
                0.0,
                canvas.width as f32,
                canvas.height as f32,
                s.light as f32,
            );
        }
        _ => {
            if let Some(arms) = box_arms(ch) {
                draw_arms(canvas, s, arms);
            }
        }
    }
    canvas.has_visible()
}

fn draw_dashed_h(canvas: &mut Canvas<'_>, y0: u32, y1: u32, count: u32) {
    let w = canvas.width;
    for i in 0..count {
        let seg_start = (i * w) / count;
        let seg_end = ((i + 1) * w) / count;
        let seg_w = seg_end.saturating_sub(seg_start);
        let gap = (seg_w / 3).max(u32::from(seg_w > 1));
        let draw_end = seg_end.saturating_sub(gap).max(seg_start + 1);
        canvas.fill_rect(seg_start, y0, draw_end.min(w), y1, 255);
    }
}

fn draw_dashed_v(canvas: &mut Canvas<'_>, x0: u32, x1: u32, count: u32) {
    let h = canvas.height;
    for i in 0..count {
        let seg_start = (i * h) / count;
        let seg_end = ((i + 1) * h) / count;
        let seg_h = seg_end.saturating_sub(seg_start);
        let gap = (seg_h / 3).max(u32::from(seg_h > 1));
        let draw_end = seg_end.saturating_sub(gap).max(seg_start + 1);
        canvas.fill_rect(x0, seg_start, x1, draw_end.min(h), 255);
    }
}

/// Draws a rounded corner arc seamlessly connecting horizontal and vertical
/// cell-center strokes. `to_right` selects whether the horizontal arm goes right;
/// `to_down` selects whether the vertical arm goes down.
fn draw_arc(canvas: &mut Canvas<'_>, s: StrokeMetrics, to_right: bool, to_down: bool) {
    let cx = s.center_x();
    let cy = s.center_y();
    let w = canvas.width as f32;
    let h = canvas.height as f32;
    let half_t = s.light as f32 * 0.5;

    // Maximum radius that fits from center to both cell edges.
    let rx_space = if to_right { w - cx } else { cx };
    let ry_space = if to_down { h - cy } else { cy };
    let radius = rx_space.min(ry_space).max(1.0);

    // Center of the circular arc lies inside the corner quadrant.
    let arc_cx = if to_right { cx + radius } else { cx - radius };
    let arc_cy = if to_down { cy + radius } else { cy - radius };

    // Straight extensions beyond the arc tangent points to cell boundaries.
    if to_right {
        let start_x = arc_cx.floor() as u32;
        canvas.fill_rect(start_x, s.hy0, canvas.width, s.hy1, 255);
    } else {
        let end_x = arc_cx.ceil().max(0.0) as u32;
        canvas.fill_rect(0, s.hy0, end_x, s.hy1, 255);
    }
    if to_down {
        let start_y = arc_cy.floor() as u32;
        canvas.fill_rect(s.vx0, start_y, s.vx1, canvas.height, 255);
    } else {
        let end_y = arc_cy.ceil().max(0.0) as u32;
        canvas.fill_rect(s.vx0, 0, s.vx1, end_y, 255);
    }

    // Quarter-circle arc pixels inside the bounding box between center and arc center.
    let x_min = (if to_right { cx - half_t - 1.0 } else { arc_cx })
        .floor()
        .max(0.0) as u32;
    let x_max = (if to_right { arc_cx } else { cx + half_t + 1.0 })
        .ceil()
        .min(w) as u32;
    let y_min = (if to_down { cy - half_t - 1.0 } else { arc_cy })
        .floor()
        .max(0.0) as u32;
    let y_max = (if to_down { arc_cy } else { cy + half_t + 1.0 })
        .ceil()
        .min(h) as u32;

    for y in y_min..y_max {
        let py = y as f32 + 0.5;
        for x in x_min..x_max {
            let px = x as f32 + 0.5;
            let dist = ((px - arc_cx).hypot(py - arc_cy) - radius).abs() - half_t;
            let alpha = ((0.5 - dist).clamp(0.0, 1.0) * 255.0).round() as u8;
            canvas.blend_pixel(x, y, alpha);
        }
    }
}

fn draw_arms(canvas: &mut Canvas<'_>, s: StrokeMetrics, arms: Arms) {
    let w = canvas.width;
    let h = canvas.height;

    // Horizontal span of the vertical stem crossing the center.
    let v_outer = vertical_bounds(s, arms.up, arms.down);
    // Vertical span of the horizontal bar crossing the center.
    let h_outer = horizontal_bounds(s, arms.left, arms.right);

    // Draw horizontal arms.
    match arms.left {
        Arm::None => {}
        Arm::Light => {
            let end = match (arms.up, arms.down) {
                (Arm::Double, _) | (_, Arm::Double) => s.dvx_left.1,
                _ => v_outer.1,
            };
            canvas.fill_rect(0, s.hy0, end, s.hy1, 255);
        }
        Arm::Heavy => {
            canvas.fill_rect(0, s.hhy0, v_outer.1, s.hhy1, 255);
        }
        Arm::Double => {
            let top_end = match arms.up {
                Arm::Double => s.dvx_left.1,
                Arm::None if arms.down == Arm::Double => s.dvx_right.1,
                _ => v_outer.1,
            };
            let bot_end = match arms.down {
                Arm::Double => s.dvx_left.1,
                Arm::None if arms.up == Arm::Double => s.dvx_right.1,
                _ => v_outer.1,
            };
            canvas.fill_rect(0, s.dhy_top.0, top_end, s.dhy_top.1, 255);
            canvas.fill_rect(0, s.dhy_bottom.0, bot_end, s.dhy_bottom.1, 255);
        }
    }

    match arms.right {
        Arm::None => {}
        Arm::Light => {
            let start = match (arms.up, arms.down) {
                (Arm::Double, _) | (_, Arm::Double) => s.dvx_right.0,
                _ => v_outer.0,
            };
            canvas.fill_rect(start, s.hy0, w, s.hy1, 255);
        }
        Arm::Heavy => {
            canvas.fill_rect(v_outer.0, s.hhy0, w, s.hhy1, 255);
        }
        Arm::Double => {
            let top_start = match arms.up {
                Arm::Double => s.dvx_right.0,
                Arm::None if arms.down == Arm::Double => s.dvx_left.0,
                _ => v_outer.0,
            };
            let bot_start = match arms.down {
                Arm::Double => s.dvx_right.0,
                Arm::None if arms.up == Arm::Double => s.dvx_left.0,
                _ => v_outer.0,
            };
            canvas.fill_rect(top_start, s.dhy_top.0, w, s.dhy_top.1, 255);
            canvas.fill_rect(bot_start, s.dhy_bottom.0, w, s.dhy_bottom.1, 255);
        }
    }

    // Draw vertical arms.
    match arms.up {
        Arm::None => {}
        Arm::Light => {
            let end = match (arms.left, arms.right) {
                (Arm::Double, _) | (_, Arm::Double) => s.dhy_top.1,
                _ => h_outer.1,
            };
            canvas.fill_rect(s.vx0, 0, s.vx1, end, 255);
        }
        Arm::Heavy => {
            canvas.fill_rect(s.hvx0, 0, s.hvx1, h_outer.1, 255);
        }
        Arm::Double => {
            let left_end = match arms.left {
                Arm::Double => s.dhy_top.1,
                Arm::None if arms.right == Arm::Double => s.dhy_bottom.1,
                _ => h_outer.1,
            };
            let right_end = match arms.right {
                Arm::Double => s.dhy_top.1,
                Arm::None if arms.left == Arm::Double => s.dhy_bottom.1,
                _ => h_outer.1,
            };
            canvas.fill_rect(s.dvx_left.0, 0, s.dvx_left.1, left_end, 255);
            canvas.fill_rect(s.dvx_right.0, 0, s.dvx_right.1, right_end, 255);
        }
    }

    match arms.down {
        Arm::None => {}
        Arm::Light => {
            let start = match (arms.left, arms.right) {
                (Arm::Double, _) | (_, Arm::Double) => s.dhy_bottom.0,
                _ => h_outer.0,
            };
            canvas.fill_rect(s.vx0, start, s.vx1, h, 255);
        }
        Arm::Heavy => {
            canvas.fill_rect(s.hvx0, h_outer.0, s.hvx1, h, 255);
        }
        Arm::Double => {
            let left_start = match arms.left {
                Arm::Double => s.dhy_bottom.0,
                Arm::None if arms.right == Arm::Double => s.dhy_top.0,
                _ => h_outer.0,
            };
            let right_start = match arms.right {
                Arm::Double => s.dhy_bottom.0,
                Arm::None if arms.left == Arm::Double => s.dhy_top.0,
                _ => h_outer.0,
            };
            canvas.fill_rect(s.dvx_left.0, left_start, s.dvx_left.1, h, 255);
            canvas.fill_rect(s.dvx_right.0, right_start, s.dvx_right.1, h, 255);
        }
    }
}

fn vertical_bounds(s: StrokeMetrics, up: Arm, down: Arm) -> (u32, u32) {
    match (up, down) {
        (Arm::Double, _) | (_, Arm::Double) => (s.dvx_left.0, s.dvx_right.1),
        (Arm::Heavy, _) | (_, Arm::Heavy) => (s.hvx0, s.hvx1),
        _ => (s.vx0, s.vx1),
    }
}

fn horizontal_bounds(s: StrokeMetrics, left: Arm, right: Arm) -> (u32, u32) {
    match (left, right) {
        (Arm::Double, _) | (_, Arm::Double) => (s.dhy_top.0, s.dhy_bottom.1),
        (Arm::Heavy, _) | (_, Arm::Heavy) => (s.hhy0, s.hhy1),
        _ => (s.hy0, s.hy1),
    }
}

fn box_arms(ch: char) -> Option<Arms> {
    use Arm::{Double as D, Heavy as H, Light as L, None as N};
    let arms = match ch {
        '\u{2500}' => Arms::new(L, L, N, N),
        '\u{2501}' => Arms::new(H, H, N, N),
        '\u{2502}' => Arms::new(N, N, L, L),
        '\u{2503}' => Arms::new(N, N, H, H),
        '\u{250c}' => Arms::new(N, L, N, L),
        '\u{250d}' => Arms::new(N, H, N, L),
        '\u{250e}' => Arms::new(N, L, N, H),
        '\u{250f}' => Arms::new(N, H, N, H),
        '\u{2510}' => Arms::new(L, N, N, L),
        '\u{2511}' => Arms::new(H, N, N, L),
        '\u{2512}' => Arms::new(L, N, N, H),
        '\u{2513}' => Arms::new(H, N, N, H),
        '\u{2514}' => Arms::new(N, L, L, N),
        '\u{2515}' => Arms::new(N, H, L, N),
        '\u{2516}' => Arms::new(N, L, H, N),
        '\u{2517}' => Arms::new(N, H, H, N),
        '\u{2518}' => Arms::new(L, N, L, N),
        '\u{2519}' => Arms::new(H, N, L, N),
        '\u{251a}' => Arms::new(L, N, H, N),
        '\u{251b}' => Arms::new(H, N, H, N),
        '\u{251c}' => Arms::new(N, L, L, L),
        '\u{251d}' => Arms::new(N, H, L, L),
        '\u{251e}' => Arms::new(N, L, H, L),
        '\u{251f}' => Arms::new(N, L, L, H),
        '\u{2520}' => Arms::new(N, L, H, H),
        '\u{2521}' => Arms::new(N, H, H, L),
        '\u{2522}' => Arms::new(N, H, L, H),
        '\u{2523}' => Arms::new(N, H, H, H),
        '\u{2524}' => Arms::new(L, N, L, L),
        '\u{2525}' => Arms::new(H, N, L, L),
        '\u{2526}' => Arms::new(L, N, H, L),
        '\u{2527}' => Arms::new(L, N, L, H),
        '\u{2528}' => Arms::new(L, N, H, H),
        '\u{2529}' => Arms::new(H, N, H, L),
        '\u{252a}' => Arms::new(H, N, L, H),
        '\u{252b}' => Arms::new(H, N, H, H),
        '\u{252c}' => Arms::new(L, L, N, L),
        '\u{252d}' => Arms::new(H, L, N, L),
        '\u{252e}' => Arms::new(L, H, N, L),
        '\u{252f}' => Arms::new(H, H, N, L),
        '\u{2530}' => Arms::new(L, L, N, H),
        '\u{2531}' => Arms::new(H, L, N, H),
        '\u{2532}' => Arms::new(L, H, N, H),
        '\u{2533}' => Arms::new(H, H, N, H),
        '\u{2534}' => Arms::new(L, L, L, N),
        '\u{2535}' => Arms::new(H, L, L, N),
        '\u{2536}' => Arms::new(L, H, L, N),
        '\u{2537}' => Arms::new(H, H, L, N),
        '\u{2538}' => Arms::new(L, L, H, N),
        '\u{2539}' => Arms::new(H, L, H, N),
        '\u{253a}' => Arms::new(L, H, H, N),
        '\u{253b}' => Arms::new(H, H, H, N),
        '\u{253c}' => Arms::new(L, L, L, L),
        '\u{253d}' => Arms::new(H, L, L, L),
        '\u{253e}' => Arms::new(L, H, L, L),
        '\u{253f}' => Arms::new(H, H, L, L),
        '\u{2540}' => Arms::new(L, L, H, L),
        '\u{2541}' => Arms::new(L, L, L, H),
        '\u{2542}' => Arms::new(L, L, H, H),
        '\u{2543}' => Arms::new(H, L, H, L),
        '\u{2544}' => Arms::new(L, H, H, L),
        '\u{2545}' => Arms::new(H, L, L, H),
        '\u{2546}' => Arms::new(L, H, L, H),
        '\u{2547}' => Arms::new(H, H, H, L),
        '\u{2548}' => Arms::new(H, H, L, H),
        '\u{2549}' => Arms::new(H, L, H, H),
        '\u{254a}' => Arms::new(L, H, H, H),
        '\u{254b}' => Arms::new(H, H, H, H),
        '\u{2550}' => Arms::new(D, D, N, N),
        '\u{2551}' => Arms::new(N, N, D, D),
        '\u{2552}' => Arms::new(N, D, N, L),
        '\u{2553}' => Arms::new(N, L, N, D),
        '\u{2554}' => Arms::new(N, D, N, D),
        '\u{2555}' => Arms::new(D, N, N, L),
        '\u{2556}' => Arms::new(L, N, N, D),
        '\u{2557}' => Arms::new(D, N, N, D),
        '\u{2558}' => Arms::new(N, D, L, N),
        '\u{2559}' => Arms::new(N, L, D, N),
        '\u{255a}' => Arms::new(N, D, D, N),
        '\u{255b}' => Arms::new(D, N, L, N),
        '\u{255c}' => Arms::new(L, N, D, N),
        '\u{255d}' => Arms::new(D, N, D, N),
        '\u{255e}' => Arms::new(N, D, L, L),
        '\u{255f}' => Arms::new(N, L, D, D),
        '\u{2560}' => Arms::new(N, D, D, D),
        '\u{2561}' => Arms::new(D, N, L, L),
        '\u{2562}' => Arms::new(L, N, D, D),
        '\u{2563}' => Arms::new(D, N, D, D),
        '\u{2564}' => Arms::new(D, D, N, L),
        '\u{2565}' => Arms::new(L, L, N, D),
        '\u{2566}' => Arms::new(D, D, N, D),
        '\u{2567}' => Arms::new(D, D, L, N),
        '\u{2568}' => Arms::new(L, L, D, N),
        '\u{2569}' => Arms::new(D, D, D, N),
        '\u{256a}' => Arms::new(D, D, L, L),
        '\u{256b}' => Arms::new(L, L, D, D),
        '\u{256c}' => Arms::new(D, D, D, D),
        '\u{2574}' => Arms::new(L, N, N, N),
        '\u{2575}' => Arms::new(N, N, L, N),
        '\u{2576}' => Arms::new(N, L, N, N),
        '\u{2577}' => Arms::new(N, N, N, L),
        '\u{2578}' => Arms::new(H, N, N, N),
        '\u{2579}' => Arms::new(N, N, H, N),
        '\u{257a}' => Arms::new(N, H, N, N),
        '\u{257b}' => Arms::new(N, N, N, H),
        '\u{257c}' => Arms::new(L, H, N, N),
        '\u{257d}' => Arms::new(N, N, L, H),
        '\u{257e}' => Arms::new(H, L, N, N),
        '\u{257f}' => Arms::new(N, N, H, L),
        _ => return None,
    };
    Some(arms)
}

fn eighth(extent: u32, parts: u32) -> u32 {
    (extent * parts + 4) / 8
}

fn draw_block_element(canvas: &mut Canvas<'_>, ch: char) -> bool {
    let w = canvas.width;
    let h = canvas.height;
    let mid_x = w / 2;
    let mid_y = h / 2;
    match ch {
        '\u{2580}' => canvas.fill_rect(0, 0, w, mid_y.max(1), 255),
        '\u{2581}'..='\u{2588}' => {
            let eighths = (ch as u32 - 0x2580).min(8);
            let top = h.saturating_sub(eighth(h, eighths).max(1));
            canvas.fill_rect(0, top, w, h, 255);
        }
        '\u{2589}'..='\u{258f}' => {
            let eighths = 8 - (ch as u32 - 0x2588);
            let right = eighth(w, eighths).max(1);
            canvas.fill_rect(0, 0, right, h, 255);
        }
        '\u{2590}' => canvas.fill_rect(mid_x, 0, w, h, 255),
        '\u{2591}' => canvas.fill_rect(0, 0, w, h, 64),
        '\u{2592}' => canvas.fill_rect(0, 0, w, h, 128),
        '\u{2593}' => canvas.fill_rect(0, 0, w, h, 191),
        '\u{2594}' => canvas.fill_rect(0, 0, w, eighth(h, 1).max(1), 255),
        '\u{2595}' => {
            let left = w.saturating_sub(eighth(w, 1).max(1));
            canvas.fill_rect(left, 0, w, h, 255);
        }
        '\u{2596}'..='\u{259f}' => {
            // Bit layout: 1=TL, 2=TR, 4=BL, 8=BR
            let mask: u8 = match ch {
                '\u{2596}' => 0b0100,
                '\u{2597}' => 0b1000,
                '\u{2598}' => 0b0001,
                '\u{2599}' => 0b1101,
                '\u{259a}' => 0b1001,
                '\u{259b}' => 0b0111,
                '\u{259c}' => 0b1011,
                '\u{259d}' => 0b0010,
                '\u{259e}' => 0b0110,
                '\u{259f}' => 0b1110,
                _ => 0,
            };
            if mask & 0b0001 != 0 {
                canvas.fill_rect(0, 0, mid_x.max(1), mid_y.max(1), 255);
            }
            if mask & 0b0010 != 0 {
                canvas.fill_rect(mid_x, 0, w, mid_y.max(1), 255);
            }
            if mask & 0b0100 != 0 {
                canvas.fill_rect(0, mid_y, mid_x.max(1), h, 255);
            }
            if mask & 0b1000 != 0 {
                canvas.fill_rect(mid_x, mid_y, w, h, 255);
            }
        }
        _ => {}
    }
    canvas.has_visible()
}

fn draw_braille(canvas: &mut Canvas<'_>, bits: u32) -> bool {
    let w = canvas.width;
    let h = canvas.height;
    let dot_w = (w / 4).clamp(1, w.div_ceil(2));
    let dot_h = (h / 8).clamp(1, h.div_ceil(4));
    let dot = dot_w.min(dot_h).max(1);

    // 2 columns x 4 rows evenly distributed inside the cell.
    let col_x = [
        ((w as f32 * 0.25) - dot as f32 * 0.5).round().max(0.0) as u32,
        ((w as f32 * 0.75) - dot as f32 * 0.5)
            .round()
            .clamp(0.0, w.saturating_sub(dot) as f32) as u32,
    ];
    let row_y = [0, 1, 2, 3].map(|row| {
        let center = (row as f32 + 0.5) * (h as f32 / 4.0);
        (center - dot as f32 * 0.5)
            .round()
            .clamp(0.0, h.saturating_sub(dot) as f32) as u32
    });

    // Standard Unicode braille bit-to-(col, row) mapping.
    const DOTS: [(usize, usize); 8] = [
        (0, 0), // bit 0: dot 1
        (0, 1), // bit 1: dot 2
        (0, 2), // bit 2: dot 3
        (1, 0), // bit 3: dot 4
        (1, 1), // bit 4: dot 5
        (1, 2), // bit 5: dot 6
        (0, 3), // bit 6: dot 7
        (1, 3), // bit 7: dot 8
    ];
    for (bit, &(col, row)) in DOTS.iter().enumerate() {
        if bits & (1 << bit) != 0 {
            let x = col_x[col];
            let y = row_y[row];
            canvas.fill_rect(x, y, x + dot, y + dot, 255);
        }
    }
    canvas.has_visible()
}

fn draw_powerline(canvas: &mut Canvas<'_>, s: StrokeMetrics, ch: char) -> bool {
    let w = canvas.width as f32;
    let h = canvas.height as f32;
    let half_h = h * 0.5;
    let thickness = s.light as f32;

    match ch {
        //  solid right-pointing triangle
        '\u{e0b0}' => {
            fill_triangle_h(canvas, true);
        }
        //  thin right-pointing chevron
        '\u{e0b1}' => {
            canvas.draw_segment(0.0, 0.0, w, half_h, thickness);
            canvas.draw_segment(w, half_h, 0.0, h, thickness);
        }
        //  solid left-pointing triangle
        '\u{e0b2}' => {
            fill_triangle_h(canvas, false);
        }
        //  thin left-pointing chevron
        '\u{e0b3}' => {
            canvas.draw_segment(w, 0.0, 0.0, half_h, thickness);
            canvas.draw_segment(0.0, half_h, w, h, thickness);
        }
        //  solid right half-circle
        '\u{e0b4}' => {
            fill_semicircle(canvas, true);
        }
        //  thin right half-circle
        '\u{e0b5}' => {
            stroke_semicircle(canvas, true, thickness);
        }
        //  solid left half-circle
        '\u{e0b6}' => {
            fill_semicircle(canvas, false);
        }
        //  thin left half-circle
        '\u{e0b7}' => {
            stroke_semicircle(canvas, false, thickness);
        }
        //  solid lower-left triangle (diagonal top-left to bottom-right)
        '\u{e0b8}' => {
            fill_corner_triangle(canvas, |nx, ny| ny >= nx);
        }
        //  thin backslash diagonal
        '\u{e0b9}' => {
            canvas.draw_segment(0.0, 0.0, w, h, thickness);
        }
        //  solid lower-right triangle (diagonal bottom-left to top-right)
        '\u{e0ba}' => {
            fill_corner_triangle(canvas, |nx, ny| nx + ny >= 1.0);
        }
        //  thin slash diagonal
        '\u{e0bb}' => {
            canvas.draw_segment(0.0, h, w, 0.0, thickness);
        }
        //  solid upper-left triangle (diagonal bottom-left to top-right)
        '\u{e0bc}' => {
            fill_corner_triangle(canvas, |nx, ny| nx + ny <= 1.0);
        }
        //  thin slash diagonal
        '\u{e0bd}' => {
            canvas.draw_segment(0.0, h, w, 0.0, thickness);
        }
        //  solid upper-right triangle (diagonal top-left to bottom-right)
        '\u{e0be}' => {
            fill_corner_triangle(canvas, |nx, ny| nx >= ny);
        }
        //  thin backslash diagonal
        '\u{e0bf}' => {
            canvas.draw_segment(0.0, 0.0, w, h, thickness);
        }
        _ => {}
    }
    canvas.has_visible()
}

fn fill_triangle_h(canvas: &mut Canvas<'_>, point_right: bool) {
    let w = canvas.width as f32;
    let h = canvas.height as f32;
    let half_h = h * 0.5;
    for y in 0..canvas.height {
        let py = y as f32 + 0.5;
        let dist_mid = (py - half_h).abs();
        let edge_x = (1.0 - dist_mid / half_h).clamp(0.0, 1.0) * w;
        for x in 0..canvas.width {
            let px = x as f32 + 0.5;
            let cov = if point_right {
                (edge_x - px + 0.5).clamp(0.0, 1.0)
            } else {
                (px - (w - edge_x) + 0.5).clamp(0.0, 1.0)
            };
            canvas.blend_pixel(x, y, (cov * 255.0).round() as u8);
        }
    }
}

fn fill_semicircle(canvas: &mut Canvas<'_>, right_side: bool) {
    let w = canvas.width as f32;
    let h = canvas.height as f32;
    let rx = w;
    let ry = h * 0.5;
    let cx = if right_side { 0.0 } else { w };
    let cy = ry;
    // 4x4 supersampling for clean elliptical anti-aliasing.
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            let mut hits = 0u32;
            for sy in 0..4 {
                let py = (y as f32 + (sy as f32 + 0.5) * 0.25 - cy) / ry;
                for sx in 0..4 {
                    let px = (x as f32 + (sx as f32 + 0.5) * 0.25 - cx) / rx;
                    if px * px + py * py <= 1.0 {
                        hits += 1;
                    }
                }
            }
            if hits != 0 {
                canvas.blend_pixel(x, y, ((hits * 255 + 8) / 16) as u8);
            }
        }
    }
}

fn stroke_semicircle(canvas: &mut Canvas<'_>, right_side: bool, thickness: f32) {
    let w = canvas.width as f32;
    let h = canvas.height as f32;
    let half_t = thickness * 0.5;
    let rx = (w - half_t).max(0.5);
    let ry = (h * 0.5 - half_t).max(0.5);
    let cx = if right_side { 0.0 } else { w };
    let cy = h * 0.5;
    for y in 0..canvas.height {
        let py = y as f32 + 0.5;
        let dy = py - cy;
        for x in 0..canvas.width {
            let px = x as f32 + 0.5;
            let dx = px - cx;
            // Approximate Euclidean distance to ellipse via gradient magnitude.
            let nx = dx / rx;
            let ny = dy / ry;
            let f = (nx * nx + ny * ny).sqrt() - 1.0;
            let gx = nx / rx;
            let gy = ny / ry;
            let g = gx.hypot(gy);
            if g > 1e-5 {
                let dist = (f.abs() / g) - half_t;
                let alpha = ((0.5 - dist).clamp(0.0, 1.0) * 255.0).round() as u8;
                canvas.blend_pixel(x, y, alpha);
            }
        }
    }
}

fn fill_corner_triangle(canvas: &mut Canvas<'_>, inside: impl Fn(f32, f32) -> bool) {
    let w = canvas.width as f32;
    let h = canvas.height as f32;
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            let mut hits = 0u32;
            for sy in 0..4 {
                let ny = (y as f32 + (sy as f32 + 0.5) * 0.25) / h;
                for sx in 0..4 {
                    let nx = (x as f32 + (sx as f32 + 0.5) * 0.25) / w;
                    if inside(nx, ny) {
                        hits += 1;
                    }
                }
            }
            if hits != 0 {
                canvas.blend_pixel(x, y, ((hits * 255 + 8) / 16) as u8);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::PixelSize;

    use super::*;

    fn metrics(size: f32, width: u32, height: u32) -> FontMetrics {
        FontMetrics {
            size,
            cell: PixelSize {
                width: NonZeroU32::new(width).unwrap(),
                height: NonZeroU32::new(height).unwrap(),
            },
            baseline: (height as f32 * 0.8) as i32,
        }
    }

    fn alpha_at(pixels: &[u8], width: u32, x: u32, y: u32) -> u8 {
        pixels[(y as usize * width as usize + x as usize) * 4 + 3]
    }

    #[test]
    fn all_supported_codepoints_rasterize_without_panics() {
        let mut pixels = Vec::new();
        for (size, w, h) in [(8.0, 1, 1), (10.0, 4, 8), (15.0, 9, 18), (32.0, 20, 40)] {
            let m = metrics(size, w, h);
            for ch in ('\u{2500}'..='\u{259f}')
                .chain('\u{2800}'..='\u{28ff}')
                .chain('\u{e0b0}'..='\u{e0bf}')
            {
                let result = rasterize(m, ch, w, h, &mut pixels);
                if ch == '\u{2800}' {
                    assert_eq!(result, Some(false));
                } else {
                    assert_eq!(
                        result,
                        Some(true),
                        "expected visible glyph for U+{:04X}",
                        ch as u32
                    );
                }
            }
        }
    }

    #[test]
    fn unsupported_characters_fall_back_to_font() {
        let mut pixels = Vec::new();
        let m = metrics(15.0, 9, 18);
        for ch in [
            'A',
            '─'.to_ascii_lowercase(),
            '\u{e0a0}',
            '\u{24ff}',
            '\u{25a0}',
        ] {
            if ch == '─' {
                continue;
            }
            assert_eq!(rasterize(m, ch, 9, 18, &mut pixels), None);
        }
    }

    #[test]
    fn horizontal_and_vertical_lines_connect_across_cell_edges() {
        let m = metrics(15.0, 10, 20);
        let mut horiz = Vec::new();
        let mut corner = Vec::new();
        assert_eq!(rasterize(m, '─', 10, 20, &mut horiz), Some(true));
        assert_eq!(rasterize(m, '┌', 10, 20, &mut corner), Some(true));

        // Right edge of '┌' must match left edge of '─' pixel for pixel.
        for y in 0..20 {
            assert_eq!(
                alpha_at(&corner, 10, 9, y),
                alpha_at(&horiz, 10, 0, y),
                "horizontal seam mismatch at y={y}"
            );
        }

        let mut vert = Vec::new();
        assert_eq!(rasterize(m, '│', 10, 20, &mut vert), Some(true));
        // Bottom edge of '┌' must match top edge of '│' pixel for pixel.
        for x in 0..10 {
            assert_eq!(
                alpha_at(&corner, 10, x, 19),
                alpha_at(&vert, 10, x, 0),
                "vertical seam mismatch at x={x}"
            );
        }
    }

    #[test]
    fn double_lines_connect_seamlessly() {
        let m = metrics(15.0, 11, 22);
        let mut dbl_h = Vec::new();
        let mut dbl_v = Vec::new();
        let mut corner = Vec::new();
        assert_eq!(rasterize(m, '═', 11, 22, &mut dbl_h), Some(true));
        assert_eq!(rasterize(m, '║', 11, 22, &mut dbl_v), Some(true));
        assert_eq!(rasterize(m, '╔', 11, 22, &mut corner), Some(true));

        for y in 0..22 {
            assert_eq!(alpha_at(&corner, 11, 10, y), alpha_at(&dbl_h, 11, 0, y));
        }
        for x in 0..11 {
            assert_eq!(alpha_at(&corner, 11, x, 21), alpha_at(&dbl_v, 11, x, 0));
        }
    }

    #[test]
    fn quadrants_partition_cell_without_gaps_or_overlap() {
        let m = metrics(15.0, 9, 19);
        let mut tl = Vec::new();
        let mut tr = Vec::new();
        let mut bl = Vec::new();
        let mut br = Vec::new();
        assert_eq!(rasterize(m, '▘', 9, 19, &mut tl), Some(true));
        assert_eq!(rasterize(m, '▝', 9, 19, &mut tr), Some(true));
        assert_eq!(rasterize(m, '▖', 9, 19, &mut bl), Some(true));
        assert_eq!(rasterize(m, '▗', 9, 19, &mut br), Some(true));

        for y in 0..19 {
            for x in 0..9 {
                let count = [
                    alpha_at(&tl, 9, x, y),
                    alpha_at(&tr, 9, x, y),
                    alpha_at(&bl, 9, x, y),
                    alpha_at(&br, 9, x, y),
                ]
                .into_iter()
                .filter(|&a| a == 255)
                .count();
                assert_eq!(count, 1, "pixel ({x}, {y}) covered {count} times");
            }
        }
    }

    #[test]
    fn full_block_fills_every_pixel() {
        let m = metrics(15.0, 9, 18);
        let mut full = Vec::new();
        assert_eq!(rasterize(m, '█', 9, 18, &mut full), Some(true));
        assert!(
            full.as_chunks::<4>()
                .0
                .iter()
                .all(|px| *px == [255, 255, 255, 255])
        );
    }
}
