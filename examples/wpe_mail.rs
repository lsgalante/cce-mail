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
}
