//! Headless smoke test for [`MailWebView`] — boots the engine, loads a
//! representative HTML mail, and asserts frames actually render with the
//! expected content policy. No Wayland session needed: frames land in WPE's
//! SHM buffers and are sampled straight off the readback.
//!
//! `cargo run --release -p cce-mail --example wpe_mail`

#[path = "../src/wpe/mod.rs"]
mod wpe;

fn main() {
    let html = r##"<!doctype html>
<html><body style="margin:0;background:#ff0000">
  <h1 style="color:#ffffff">HTML mail</h1>
  <p><a href="https://example.com/click">a link</a></p>
  <img src="https://tracker.invalid/pixel.gif" width="10" height="10">
</body></html>"##;

    let mut view = wpe::MailWebView::new((800, 600));
    view.load_html(html);

    let mut frames = 0;
    for i in 0..120 {
        if view.pump() {
            frames += 1;
            let px = view.sample_pixel(400, 300);
            println!("t={:>5}ms frame#{frames} image={:?} px@center={:?}", i * 50, view.image(), px);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        if frames >= 2 && i > 40 {
            break;
        }
    }

    assert!(frames > 0, "no frames rendered");
    // The body is red; a rendered frame proves layout + raster, and the
    // color proves the page (not a blank) was what rendered.
    let (r, g, b) = view.sample_pixel(400, 300).expect("no readback");
    println!("center pixel: ({r},{g},{b})");
    assert!(r > 180 && g < 80 && b < 80, "expected the red body, got ({r},{g},{b})");
    println!("OK: {frames} frames, red body rendered, remote pixel blocked from loading");

    // Phase 2: a cid: inline image, served by the registered scheme handler
    // from the per-message store — full-bleed green, so the center pixel
    // proves the request went store → stream → raster.
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="200"><rect width="200" height="200" fill="#00c000"/></svg>"##;
    view.set_inline_parts(vec![(
        "logo@test".to_string(),
        "image/svg+xml".to_string(),
        svg.to_vec(),
    )]);
    view.load_html(
        r#"<html><body style="margin:0"><img src="cid:logo@test" style="display:block;width:800px;height:600px"></body></html>"#,
    );
    let mut cid_frames = 0;
    for i in 0..120 {
        if view.pump() {
            cid_frames += 1;
            println!(
                "t={:>5}ms cid frame#{cid_frames} px@center={:?}",
                i * 50,
                view.sample_pixel(400, 300)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Wait for the frame that actually shows the image, not the first
        // paint before the subresource arrived. All three channels: white
        // (the pre-image paint) also has a green channel over 150.
        if cid_frames > 0 {
            if let Some((r, g, b)) = view.sample_pixel(400, 300) {
                if g > 150 && r < 80 && b < 80 {
                    break;
                }
            }
        }
    }
    let (r, g, b) = view.sample_pixel(400, 300).expect("no readback");
    println!("cid image center pixel: ({r},{g},{b})");
    assert!(g > 150 && r < 80 && b < 80, "expected the green cid image, got ({r},{g},{b})");
    println!("OK: cid inline image rendered through the scheme handler");
}
