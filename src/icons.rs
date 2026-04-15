use tray_icon::Icon;

pub fn load_microphone_icon() -> Icon {
    let image = image::load_from_memory_with_format(
        include_bytes!("../assets/AppIcon.appiconset/icon_32x32.png"),
        image::ImageFormat::Png,
    )
    .expect("embedded microphone icon should decode")
    .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("valid embedded tray icon")
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
pub fn draw_progress_icon(progress: u8) -> Icon {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    let cx = 16.0f32;
    let cy = 16.0f32;
    let radius = 11.0f32;
    let stroke = 2.0f32;
    let start = -std::f32::consts::FRAC_PI_2; // 12 o'clock

    // Background: full dim ring
    draw_arc(&mut rgba, size, cx, cy, radius, stroke, start, std::f32::consts::TAU, 60);

    // Foreground: bright progress arc
    if progress > 0 {
        let sweep = std::f32::consts::TAU * progress as f32 / 100.0;
        draw_arc(&mut rgba, size, cx, cy, radius, stroke, start, sweep, 255);
    }

    Icon::from_rgba(rgba, size as u32, size as u32).expect("valid progress tray icon")
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
