//! Window and pane metrics, and the compose modal's and detail chips' geometry.
//! Split out of main.rs on 2026-10-10.

use crate::*;

/// Inset from the WINDOW's outer edge to anything standing on the root
/// plate: the toolkit's [`cce_ui::layout::root_plate_inset`], the plate's
/// rolled rim plus one [`pane_pad`]. The padding is a run of flat plate
/// face — the same run [`pane_gap`] leaves between the two panes — and the
/// face only begins where the roll ends, so measured from the window edge
/// the inset carries the roll too. That is what makes every gap in the
/// window read the same: rim to bar items, items to plates, plates to rim,
/// and the split between the panes.
///
/// History: while this window's base was a flat quad with no rim (until
/// 2026-09-24) the roll compensation alone was a 20-against-12 discrepancy,
/// and the inset was briefly the bare padding. The base is the standard
/// root plate now, and the roll is real again.
pub(crate) fn window_pad() -> f32 {
    cce_ui::layout::root_plate_inset()
}

/// Height of the menubar's row of controls: the toolkit's control height
/// (the mail button at `button_height`, the switchers at `dropdown_height`;
/// one number unless a config sets them apart).
pub(crate) fn bar_item_h() -> f32 {
    cce_ui::layout::button_height().max(cce_ui::layout::dropdown_height())
}

/// The menubar band's height. One padding above its items, which then sit
/// flush with the band's bottom edge, so the gap from an item down to the
/// plates below is that same padding again (`list_geom` adds it). That is
/// what makes every vertical gap in the window one number: window rim to
/// items, items to plates, plates to rim.
pub(crate) fn menubar_h() -> f32 {
    window_pad() + bar_item_h()
}

/// Widths of the two switchers in the bar; their heights and every gap
/// around them come from [`bar_item_h`] and the DE spacings.
pub(crate) const FOLDER_W: f32 = 134.0;
pub(crate) const ACCOUNT_W: f32 = 210.0;

// List/detail split geometry. `list_w` (the list plate's width) is the one
// draggable value — everything else derives from it through `split_geom`,
// against the two DE-wide spacings below.

/// The gutter between the window plate's rim and what sits on it
/// (`style.surface.plate.root.padding`, DE-wide). EVERY inset around the
/// panes is this one value — left, right, under the menubar, bottom — so the
/// frame around them is uniform. cce-mail used to carry its own 10 for the
/// sides and 15 for top and bottom.
pub(crate) fn pane_pad() -> f32 {
    cce_ui::layout::root_plate_padding()
}

/// The gap between sibling objects on the window plate
/// (`style.surface.plate.root.gap`, DE-wide) — its own docs name pane splits
/// as the case. One gap separates the two panes, with the drag band on its
/// centre line.
pub(crate) fn pane_gap() -> f32 {
    cce_ui::layout::root_plate_gap()
}

/// The pane plates' fill: the DE's list colour carrying the blur-behind
/// sentinel — a NEGATIVE alpha, which is how shader2d is told to sample the
/// frosted backdrop beneath this surface rather than compositing flatly over
/// it. The plate is then transparent AND blurred, which is what lets a
/// scrollbar parked underneath read as genuinely behind it instead of as a
/// stripe drawn through it.
pub(crate) fn pane_fill() -> [f32; 4] {
    let mut c = cce_ui::color::list_bg_color();
    c[3] = -c[3].abs();
    c
}



/// The padding INSIDE a pane plate, between its rim and its content
/// (`style.surface.plate.padding`, DE-wide — the pane rung's own value, 20
/// here, where the root rung's [`pane_pad`] is 12). Uniform on all four
/// sides: the detail pane used to carry 15 at the sides, 18 above its
/// subject and 20 under its body.
pub(crate) fn pane_inner_pad() -> f32 {
    cce_ui::layout::plate_padding()
}
/// The unread mark on a list row (the `circle` glyph), linear like a prim.
pub(crate) const UNREAD_DOT: [f32; 4] = [0.20, 0.45, 0.85, 1.0];
pub(crate) const LIST_W_DEFAULT: f32 = 300.0;
pub(crate) const LIST_W_MIN: f32 = 180.0;
/// The detail pane never gets squeezed below this by a drag or a narrow window.
pub(crate) const DETAIL_W_MIN: f32 = 220.0;
/// Half-width of the separator's grab band (±, matching ScrollRegion's slop).
pub(crate) const SPLIT_GRAB_SLOP: f32 = 4.0;

/// How long to wait before re-reading the keyring after a sync found the
/// account had no password. Short enough that unlocking the vault feels
/// self-healing, long enough that a vault left locked costs nothing.
pub(crate) const SECRET_RETRY_SECS: u64 = 5;

/// How much larger than the DE's menu font a card's right-click menu is
/// drawn (`context_menu::show_scaled`): at the stock 12px it read small
/// for a menu of three or so actions. Hit-testing scales with it inside
/// the toolkit; nothing here steps rows by hand.
pub(crate) const CARD_MENU_SCALE: f32 = 1.25;

// The search band. It is not permanent chrome: the `open_search` chord
// ("/" by default) reveals and focuses it, and closing hands its strip back
// to the list, so `list_geom` is the single source for where the rows start.
// Its height is the toolkit's `textbox_height`.

// Detail header layout, every offset measured from the HEADER plate's
// top-left of its CONTENT — the plate's rim taken in by `pane_inner_pad`
// (`detail_origin_y`). The header (subject, From/To/Date, attachment chips
// and the HTML-view buttons) is a plate of its own above the preview
// plate, so the body no longer has an offset in this list: it starts one
// padding inside the preview plate (`detail_body_geom`). The plate is as
// tall as what it holds (`detail_header_content_h`): the date line's
// bottom, or the chip row's when the message has attachments. The
// HTML-view buttons add no height — they ride the right edge with their
// bottoms on the date line.
pub(crate) const DETAIL_SUBJECT_Y: f32 = 0.0;
pub(crate) const DETAIL_FROM_Y: f32 = 25.0;
pub(crate) const DETAIL_TO_Y: f32 = 45.0;
pub(crate) const DETAIL_DATE_Y: f32 = 65.0;
/// The 11px metadata lines' box height: where the date line's bottom is,
/// which the buttons align to and the plate ends at without chips.
pub(crate) const DETAIL_LINE_H: f32 = 14.0;
pub(crate) const DETAIL_LINES_BOTTOM: f32 = DETAIL_DATE_Y + DETAIL_LINE_H;
pub(crate) const DETAIL_CHIPS_Y: f32 = 80.0;

// Compose modal geometry. One source of truth: the background quads, the
// input rects, the labels, the chip row and the outside-click test all
// derive from these — the old duplicated 500.0/420.0 literals meant growing
// the dialog required finding every site by hand.
pub(crate) const COMPOSE_W: f32 = 500.0;
// The dialog's SIZES — what is not spacing. Every inset and gap in it is
// the pane rung of the DE spacing ladder (`pane_inner_pad`, `plate_gap`),
// and the dialog's height falls out of the stack rather than being a
// number of its own.
pub(crate) const COMPOSE_TITLE_H: f32 = 22.0;
pub(crate) const COMPOSE_LABEL_W: f32 = 65.0;
pub(crate) const COMPOSE_BODY_H: f32 = 195.0;
pub(crate) const COMPOSE_BTN_W: f32 = 75.0;
pub(crate) const COMPOSE_ATTACH_W: f32 = 80.0;

/// The compose dialog laid out: one source for paint, the labels, the
/// widget rects and every hit-test. A raised popover plate on the root
/// plate, its content inset by the pane padding, its rows a pane gap apart.
pub(crate) struct ComposeLayout {
    pub(crate) x: f32,
    pub(crate) pad: f32,
    pub(crate) gap: f32,
    pub(crate) title_y: f32,
    pub(crate) label_x: f32,
    pub(crate) field_x: f32,
    pub(crate) field_w: f32,
    /// To, Cc, Bcc, Subject — the top of each field.
    pub(crate) rows_y: [f32; 4],
    pub(crate) body: (f32, f32, f32, f32),
    pub(crate) chips_y: f32,
    pub(crate) buttons_y: f32,
    pub(crate) attach_x: f32,
    pub(crate) send_x: f32,
    pub(crate) cancel_x: f32,
}

pub(crate) fn compose_layout(modal_x: f32, modal_y: f32) -> ComposeLayout {
    let pad = pane_inner_pad();
    let gap = cce_ui::layout::plate_gap();
    let title_y = modal_y + pad;
    let rows0 = title_y + COMPOSE_TITLE_H + gap;
    let pitch = cce_ui::layout::textbox_height() + gap;
    let body_y = rows0 + 4.0 * pitch;
    let chips_y = body_y + COMPOSE_BODY_H + gap;
    let buttons_y = chips_y + cce_ui::layout::button_height() + gap;
    let cancel_x = modal_x + COMPOSE_W - pad - COMPOSE_BTN_W;
    ComposeLayout {
        x: modal_x,
        pad,
        gap,
        title_y,
        label_x: modal_x + pad,
        field_x: modal_x + pad + COMPOSE_LABEL_W,
        field_w: COMPOSE_W - 2.0 * pad - COMPOSE_LABEL_W,
        rows_y: [rows0, rows0 + pitch, rows0 + 2.0 * pitch, rows0 + 3.0 * pitch],
        body: (modal_x + pad, body_y, COMPOSE_W - 2.0 * pad, COMPOSE_BODY_H),
        chips_y,
        buttons_y,
        attach_x: modal_x + pad,
        send_x: cancel_x - gap - COMPOSE_BTN_W,
        cancel_x,
    }
}

/// The dialog's height: the bottom of its button row plus the padding.
pub(crate) fn compose_h() -> f32 {
    let l = compose_layout(0.0, 0.0);
    l.buttons_y + cce_ui::layout::button_height() + l.pad
}

pub(crate) fn compose_modal_origin(w: f32, h: f32) -> (f32, f32) {
    (((w - COMPOSE_W) / 2.0).max(0.0), ((h - compose_h()) / 2.0).max(0.0))
}

/// Rects of the attachment chips (one per queued file), in the row between
/// the body and the buttons. Paint and hit-test both call this, so a click
/// lands exactly on what was drawn.
pub(crate) fn compose_chip_rects(attachments: &[String], modal_x: f32, modal_y: f32) -> Vec<(f32, f32, f32, f32)> {
    let l = compose_layout(modal_x, modal_y);
    let mut rects = Vec::with_capacity(attachments.len());
    let mut x = l.x + l.pad;
    let y = l.chips_y;
    for path in attachments {
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        let shown = ellipsize(name, 22);
        // Estimated glyph advance at font_size 10 — the chip is a painted
        // quad, not a widget, so an estimate only has to be consistent
        // between paint and hit-test (it is: both use this fn). The 10 past
        // the name is the room the remove mark (the `x` glyph) is drawn in.
        let w = shown.chars().count() as f32 * 6.0 + 2.0 * cce_ui::layout::CONTROL_TEXT_INSET + 10.0;
        rects.push((x, y, w, cce_ui::layout::button_height()));
        x += w + l.gap;
    }
    rects
}

/// Chip label for a server attachment: name plus a humanized size.
pub(crate) fn detail_chip_label(att: &RemoteAttachment) -> String {
    let size = if att.size >= 1_048_576 {
        format!("{:.1} MB", att.size as f32 / 1_048_576.0)
    } else if att.size >= 1024 {
        format!("{} KB", att.size / 1024)
    } else {
        format!("{} B", att.size)
    };
    format!("{} \u{00b7} {}", ellipsize(&att.name, 26), size)
}

/// Rects of the detail pane's attachment chips, one per server attachment,
/// in the fixed header band between Date and the body (the body top never
/// moves). Paint and hit-test both call this — the compose-chip convention.
pub(crate) fn detail_chip_rects(atts: &[RemoteAttachment], detail_x: f32, origin_y: f32) -> Vec<(f32, f32, f32, f32)> {
    let mut rects = Vec::with_capacity(atts.len());
    let mut x = detail_x;
    let y = origin_y + DETAIL_CHIPS_Y;
    for att in atts {
        let w = detail_chip_label(att).chars().count() as f32 * 6.0 + 2.0 * cce_ui::layout::CONTROL_TEXT_INSET;
        rects.push((x, y, w, cce_ui::layout::button_height()));
        x += w + cce_ui::layout::plate_gap();
    }
    rects
}
