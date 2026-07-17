//! Step 1 demo: capture the live screen via DRM/KMS and dump it as a PNG.

use kmsrdp::capture;

fn main() -> std::io::Result<()> {
    let img = capture::capture_frame()?;
    let out_path = "/home/foo/Documents/kmsrdp/capture.png";
    img.save(out_path)
        .map_err(|e| std::io::Error::other(format!("PNG save failed: {e}")))?;
    println!("SUCCESS: dumped a live frame to {out_path}");
    Ok(())
}
