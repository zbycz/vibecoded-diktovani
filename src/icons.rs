use tray_icon::Icon;

/// The idle menu-bar glyph — a microphone whose capsule carries a knocked-out
/// letter "A" — described as vector geometry in a 72x100 design box and
/// rasterised on demand. Keeping it as shapes rather than a bitmap lets the
/// glyph stay crisp at any size and be filled with an arbitrary color.
const DESIGN_WIDTH: f32 = 72.0;
const DESIGN_HEIGHT: f32 = 100.0;
const MIC_CENTER_X: f32 = 36.0;
const MIC_RENDER_HEIGHT: usize = 72;

/// `tray-icon` stretches whatever bitmap it gets to the full 18pt menu-bar
/// height, so the glyph is inset by this much design-space padding to end up
/// the same visual size as the surrounding SF Symbol icons.
const MIC_PADDING: f32 = 9.0;

pub fn load_microphone_icon(color: Option<(u8, u8, u8)>) -> Icon {
    let height = MIC_RENDER_HEIGHT;
    let box_width = DESIGN_WIDTH + 2.0 * MIC_PADDING;
    let box_height = DESIGN_HEIGHT + 2.0 * MIC_PADDING;
    let scale = box_height / height as f32;
    let width = (box_width / scale).round() as usize;
    let origin_x = (box_width - width as f32 * scale) / 2.0 - MIC_PADDING;
    let (r, g, b) = color.unwrap_or((0, 0, 0));

    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let distance = microphone_distance(
                origin_x + (x as f32 + 0.5) * scale,
                (y as f32 + 0.5) * scale - MIC_PADDING,
            );
            let coverage = (0.5 - distance / scale).clamp(0.0, 1.0);
            set_pixel(
                &mut rgba,
                width,
                x,
                y,
                r,
                g,
                b,
                (coverage * 255.0).round() as u8,
            );
        }
    }

    Icon::from_rgba(rgba, width as u32, height as u32).expect("valid microphone tray icon")
}

/// Signed distance to the microphone glyph: capsule, cradle arc, stem and base
/// unioned together, with the letter "A" subtracted out of the capsule.
fn microphone_distance(x: f32, y: f32) -> f32 {
    let p = (x, y);
    let mut distance = sd_round_box(p, (MIC_CENTER_X, 31.5), (17.0, 27.5), 17.0);
    distance = distance.min(sd_arc(p, (MIC_CENTER_X, 46.0), 28.0, 4.5, 41.0));
    distance = distance.min(sd_segment(
        p,
        (MIC_CENTER_X, 74.0),
        (MIC_CENTER_X, 90.0),
        4.5,
    ));
    distance = distance.min(sd_round_box(p, (MIC_CENTER_X, 92.0), (21.0, 4.0), 4.0));
    distance.max(-letter_a_distance(p))
}

fn letter_a_distance(p: (f32, f32)) -> f32 {
    const TOP: f32 = 14.0;
    const BOTTOM: f32 = 48.0;
    const HALF_WIDTH: f32 = 11.0;
    const STROKE: f32 = 2.7;
    const CROSSBAR: f32 = 0.62;

    let apex = (MIC_CENTER_X, TOP);
    let bar_y = TOP + (BOTTOM - TOP) * CROSSBAR;
    let bar_half_width = HALF_WIDTH * CROSSBAR;

    let mut distance = sd_segment(p, apex, (MIC_CENTER_X - HALF_WIDTH, BOTTOM), STROKE);
    distance = distance.min(sd_segment(
        p,
        apex,
        (MIC_CENTER_X + HALF_WIDTH, BOTTOM),
        STROKE,
    ));
    distance.min(sd_segment(
        p,
        (MIC_CENTER_X - bar_half_width, bar_y),
        (MIC_CENTER_X + bar_half_width, bar_y),
        STROKE,
    ))
}

fn sd_round_box(p: (f32, f32), center: (f32, f32), half_size: (f32, f32), radius: f32) -> f32 {
    let qx = (p.0 - center.0).abs() - half_size.0 + radius;
    let qy = (p.1 - center.1).abs() - half_size.1 + radius;
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

fn sd_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32), half_width: f32) -> f32 {
    let (px, py) = (p.0 - a.0, p.1 - a.1);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;
    let t = if length_squared == 0.0 {
        0.0
    } else {
        ((px * dx + py * dy) / length_squared).clamp(0.0, 1.0)
    };
    (px - dx * t).hypot(py - dy * t) - half_width
}

/// Ring of `radius` clipped to everything below `y_min`, giving the U-shaped
/// cradle the microphone hangs in.
fn sd_arc(p: (f32, f32), center: (f32, f32), radius: f32, half_stroke: f32, y_min: f32) -> f32 {
    let ring = ((p.0 - center.0).hypot(p.1 - center.1) - radius).abs() - half_stroke;
    ring.max(y_min - p.1)
}

pub fn draw_checkmark_icon() -> Icon {
    let width = 32;
    let height = 32;
    let mut rgba = vec![0u8; width * height * 4];

    for offset in 0..6 {
        let x = 8 + offset;
        let y = 17 + offset;
        draw_stroke(&mut rgba, width, x, y, 2);
    }

    for offset in 0..12 {
        let x = 13 + offset;
        let y = 22 - offset;
        draw_stroke(&mut rgba, width, x, y, 2);
    }

    Icon::from_rgba(rgba, width as u32, height as u32).expect("valid checkmark tray icon")
}

/// Draw a circular progress indicator (arc filling clockwise from 12 o'clock).
///
/// `progress` is 0–100. At 0 only the dim background ring is drawn.
/// At 100 a full bright ring is drawn.
///
/// When `submit` is true a play triangle is drawn in the center, signalling that
/// the transcript will be auto-submitted (paste + Enter) once transcription ends.
pub fn draw_progress_icon(progress: u8, submit: bool) -> Icon {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    let cx = 16.0f32;
    let cy = 16.0f32;
    let radius = 11.0f32;
    let stroke = 2.0f32;
    let start = -std::f32::consts::FRAC_PI_2; // 12 o'clock

    // Background: full dim ring
    draw_arc(
        &mut rgba,
        size,
        cx,
        cy,
        radius,
        stroke,
        start,
        std::f32::consts::TAU,
        60,
    );

    // Foreground: bright progress arc
    if progress > 0 {
        let sweep = std::f32::consts::TAU * progress as f32 / 100.0;
        draw_arc(&mut rgba, size, cx, cy, radius, stroke, start, sweep, 255);
    }

    if submit {
        draw_play_triangle(&mut rgba, size);
    }

    Icon::from_rgba(rgba, size as u32, size as u32).expect("valid progress tray icon")
}

/// Draw a right-pointing "play" triangle centered in the icon.
fn draw_play_triangle(rgba: &mut [u8], size: usize) {
    let left = 13.0f32;
    let right = 21.0f32;
    let top = 11.0f32;
    let bottom = 21.0f32;
    let mid_y = (top + bottom) / 2.0;
    let half_height = (bottom - top) / 2.0;

    let y_start = top.floor() as usize;
    let y_end = bottom.ceil() as usize;
    for y in y_start..=y_end {
        // Triangle width shrinks linearly to a point at `right`.
        let t = ((y as f32 - mid_y).abs() / half_height).min(1.0);
        let x_right = left + (right - left) * (1.0 - t);
        let x_start = left.floor() as usize;
        let x_end = x_right.round() as usize;
        for x in x_start..=x_end {
            if x < size && y < size {
                set_pixel(rgba, size, x, y, 0, 0, 0, 255);
            }
        }
    }
}

/// Draw an arc by plotting filled circles along the arc path.
///
/// `sweep` is in radians; positive = clockwise (Y-axis points down in image space).
fn draw_arc(
    rgba: &mut [u8],
    size: usize,
    cx: f32,
    cy: f32,
    radius: f32,
    stroke: f32,
    start_angle: f32,
    sweep: f32,
    alpha: u8,
) {
    // One step per degree of arc, minimum 4 steps.
    let steps = ((sweep.abs() * (180.0 / std::f32::consts::PI)).ceil() as usize).max(4);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let angle = start_angle + t * sweep;
        let x = cx + radius * angle.cos();
        let y = cy + radius * angle.sin();
        draw_filled_circle(rgba, size, size, x, y, stroke, alpha);
    }
}

fn draw_stroke(rgba: &mut [u8], width: usize, x: usize, y: usize, radius: usize) {
    let start_x = x.saturating_sub(radius);
    let start_y = y.saturating_sub(radius);
    let end_x = (x + radius).min(width - 1);
    let end_y = (y + radius).min((rgba.len() / 4 / width).saturating_sub(1));

    for py in start_y..=end_y {
        for px in start_x..=end_x {
            set_pixel(rgba, width, px, py, 0, 0, 0, 255);
        }
    }
}

fn draw_filled_circle(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    center_x: f32,
    center_y: f32,
    radius: f32,
    alpha: u8,
) {
    let min_x = (center_x - radius).floor().max(0.0) as usize;
    let max_x = (center_x + radius).ceil().min((width - 1) as f32) as usize;
    let min_y = (center_y - radius).floor().max(0.0) as usize;
    let max_y = (center_y + radius).ceil().min((height - 1) as f32) as usize;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 - center_x;
            let dy = y as f32 - center_y;
            if dx * dx + dy * dy <= radius * radius {
                set_pixel(rgba, width, x, y, 0, 0, 0, alpha);
            }
        }
    }
}

fn set_pixel(rgba: &mut [u8], width: usize, x: usize, y: usize, r: u8, g: u8, b: u8, a: u8) {
    let index = (y * width + x) * 4;
    rgba[index] = r;
    rgba[index + 1] = g;
    rgba[index + 2] = b;
    rgba[index + 3] = a;
}
