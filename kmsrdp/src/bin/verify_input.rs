//! Step 2 demo: create the virtual uinput mouse, move it to four corners of
//! the screen, and capture (via Step 1's DRM code) a PNG at each stop so we
//! can visually confirm the pointer actually moved on the live desktop.

use std::thread::sleep;
use std::time::Duration;

use kmsrdp::{capture, uinput::VirtualInput};

const CORNERS: &[(&str, f64, f64)] = &[
    ("top_left", 0.05, 0.05),
    ("top_right", 0.95, 0.05),
    ("bottom_right", 0.95, 0.95),
    ("bottom_left", 0.05, 0.95),
];

fn main() -> std::io::Result<()> {
    let input = VirtualInput::create()?;
    println!("virtual input device created");

    for (name, fx, fy) in CORNERS {
        input.move_abs(*fx, *fy)?;
        // Let libinput/the compositor actually process the motion and
        // redraw the cursor before we capture.
        sleep(Duration::from_millis(300));

        let img = capture::capture_frame()?;
        let out_path = format!("verify_{name}.png");
        img.save(&out_path)
            .map_err(|e| std::io::Error::other(format!("PNG save failed: {e}")))?;
        println!("moved to {name} ({fx}, {fy}) -> {out_path}");
    }

    input.click_left()?;
    println!("\nSUCCESS: uinput pointer motion + click injected and captured.");
    Ok(())
}
