//! Headless check that a press-drag-release over a message selects text.
//!
//! WebKit decides whether a pointer move is a drag from the button bits in
//! the move event's own modifiers; a move reporting no held button is a
//! hover, and nothing is selected. The page paints its selection pure green,
//! so the readback answers without any script: green pixels on the text line
//! mean something is selected. A double-click (which needs no drag) runs
//! first as the control that the probe can see a selection at all.
//!
//! `cargo run --release -p cce-mail --example wpe_select`

#[path = "../src/wpe/mod.rs"]
mod wpe;

use cce_ui::widget::MouseButton;

fn main() {
    let html = r##"<!doctype html>
<html><head><style>
  body { margin:0; background:#ffffff; color:#000000; font:80px/120px monospace }
  ::selection { background:#00ff00; color:#000000 }
</style></head>
<body><p style="margin:0">alpha bravo charlie</p></body></html>"##;

    let mut view = wpe::MailWebView::new((1200, 600));
    view.load_html(html);
    let mut settle = |view: &mut wpe::MailWebView, n: u32| {
        for _ in 0..n {
            view.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    settle(&mut view, 40);

    let green = |view: &wpe::MailWebView| {
        (0..120).step_by(4).flat_map(|y| (0..900).step_by(10).map(move |x| (x, y)))
            .filter(|&(x, y)| view.sample_pixel(x, y).is_some_and(|(r, g, b)| g > 200 && r < 80 && b < 80))
            .count()
    };
    println!("before: green={}", green(&view));
    // Control: a double-click selects a word without any drag.
    view.mouse_button_ui(MouseButton::Left, true, 100.0, 60.0);
    view.mouse_button_ui(MouseButton::Left, false, 100.0, 60.0);
    view.mouse_button_ui(MouseButton::Left, true, 100.0, 60.0);
    view.mouse_button_ui(MouseButton::Left, false, 100.0, 60.0);
    settle(&mut view, 10);
    println!("dblclick: green={}", green(&view));
    view.mouse_button_ui(MouseButton::Left, true, 1100.0, 400.0);
    view.mouse_button_ui(MouseButton::Left, false, 1100.0, 400.0);
    settle(&mut view, 10);
    println!("cleared: green={}", green(&view));

    view.mouse_move(2.0, 60.0);
    view.mouse_button_ui(MouseButton::Left, true, 2.0, 60.0);
    settle(&mut view, 2);
    for x in (40..=700).step_by(60) {
        view.mouse_move(x as f32, 60.0);
        settle(&mut view, 1);
    }
    view.mouse_button_ui(MouseButton::Left, false, 700.0, 60.0);
    settle(&mut view, 10);

    let after = green(&view);
    println!("after drag: green={after}");
    assert!(after > 0, "drag selected nothing");
    println!("OK: drag selected text");
}
