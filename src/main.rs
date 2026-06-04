use wayland_client::QueueHandle;
use glyphon::{FontSystem, Buffer, Metrics, Attrs};
use clear_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use clear_ui::widget::{
    MouseButton, ElementState, MouseScrollDelta, KeyEvent, TextItem, Widget,
    TextBox, Button, TextLabel, Key, ScrollingList, Paginator
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Folder {
    Inbox,
    Sent,
    Trash,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Email {
    id: usize,
    from: String,
    to: String,
    subject: String,
    body: String,
    date: String,
    read: bool,
    folder: String, // "inbox", "sent", "trash"
}

#[derive(Debug, Clone)]
enum AppMessage {
    SwitchFolder(Folder),
    SelectEmail(usize),
    SearchChanged,
    ComposeNew,
    ComposeCancel,
    ComposeSend,
    Reply,
    DeleteSelected,
    ToggleUnread,
}

struct ClearEmailApp {
    // Navigation / Sidebar
    btn_compose: Button,
    paginator: Paginator,

    // Search and List View
    search_box: TextBox,
    email_list: ScrollingList,
    email_buttons: Vec<Button>,

    // Details View
    btn_reply: Button,
    btn_delete: Button,
    btn_unread: Button,
    detail_body: TextBox,

    // Compose Dialog
    compose_to: TextBox,
    compose_subject: TextBox,
    compose_body: TextBox,
    btn_compose_send: Button,
    btn_compose_cancel: Button,

    // Application state
    emails: Vec<Email>,
    current_folder: Folder,
    selected_email_id: Option<usize>,
    compose_open: bool,
    status_message: Option<(String, f32)>, // (message, timer)

    // UI state
    width: u32,
    height: u32,
    scale_factor: f64,
    text_items: Vec<TextItem>,
    font_system: FontSystem,
    needs_rebuild: bool,
}

fn get_config_path() -> std::path::PathBuf {
    let p = std::path::PathBuf::from("/home/lsgalante/.config/ccec");
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
    }
    p.join("emails.json")
}

fn load_emails() -> Vec<Email> {
    let path = get_config_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(emails) = serde_json::from_str(&content) {
                return emails;
            }
        }
    }
    get_default_mock_emails()
}

fn save_emails(emails: &[Email]) {
    let path = get_config_path();
    if let Ok(content) = serde_json::to_string_pretty(emails) {
        let _ = std::fs::write(&path, content);
    }
}

fn get_default_mock_emails() -> Vec<Email> {
    vec![
        Email {
            id: 1,
            from: "System Daemon <daemon@clear-ui.org>".to_string(),
            to: "lsgalante@clear-ui.org".to_string(),
            subject: "Welcome to Clear Mail".to_string(),
            body: "Welcome to the Clear Computing Environment (CCE) email client!\n\nThis application is built entirely using the clear-ui Rust framework, running on top of Wayland and wgpu. It provides a premium, responsive dark slate interface.\n\nEnjoy the clean lines and micro-animations!\n\nBest,\nSystem Daemon".to_string(),
            date: "12:15 PM".to_string(),
            read: false,
            folder: "inbox".to_string(),
        },
        Email {
            id: 2,
            from: "Design Team <design@clear-ui.org>".to_string(),
            to: "lsgalante@clear-ui.org".to_string(),
            subject: "Slate Dark Theme Palette Guidelines".to_string(),
            body: "Hi Lucas,\n\nWe have finalized the core Slate Dark styling color values for CCE application builders:\n- Window Background: [0.05, 0.05, 0.07, 1.0] (deep slate blue)\n- Sidebar Background: [0.08, 0.08, 0.12, 1.0]\n- Selection Highlight: [0.20, 0.45, 0.85, 0.50] (high contrast premium blue)\n- Text Bright: [0.88, 0.88, 0.92, 1.0]\n- Text Muted: [0.51, 0.51, 0.54, 1.0]\n\nPlease align all interface panels and layouts to use these values.\n\nThanks,\nDesign Team".to_string(),
            date: "Yesterday".to_string(),
            read: false,
            folder: "inbox".to_string(),
        },
        Email {
            id: 3,
            from: "Codeberg CI <ci@codeberg.org>".to_string(),
            to: "lsgalante@clear-ui.org".to_string(),
            subject: "Build Success: clear-email-interface (main)".to_string(),
            body: "Repository: lsgalante/clear-email-interface\nBranch: main\nCommit: da8cf20fcb2c993c1c048ced4020\nStatus: SUCCESS\n\nAll unit tests passed. Binary compiled in 48.2s.\n\n---\nCodeberg Actions".to_string(),
            date: "June 3".to_string(),
            read: true,
            folder: "inbox".to_string(),
        },
        Email {
            id: 4,
            from: "lsgalante@clear-ui.org".to_string(),
            to: "Design Team <design@clear-ui.org>".to_string(),
            subject: "Re: Slate Dark Theme Palette Guidelines".to_string(),
            body: "Thanks for sending the palette! I am implementing the email client right now using these specifications. The three-pane layout feels very premium.\n\n- Lucas".to_string(),
            date: "Yesterday".to_string(),
            read: true,
            folder: "sent".to_string(),
        },
    ]
}

impl ClearEmailApp {
    fn rebuild_text_items(&mut self) {
        self.text_items.clear();
        let mut labels = Vec::new();
        let font_system = &mut self.font_system;

        let w_f32 = self.width as f32;
        let h_f32 = self.height as f32;

        // 1. Sidebar Buttons text labels
        labels.extend(self.btn_compose.text_labels());
        labels.extend(self.paginator.text_labels());

        // Sidebar Folder Badges
        let inbox_unread = self.emails.iter().filter(|e| e.folder == "inbox" && !e.read).count();
        if inbox_unread > 0 {
            let badge_text = inbox_unread.to_string();
            let est_w = TextLabel::estimate_width(&badge_text, 10.0);
            labels.push(TextLabel {
                text: badge_text,
                x: 8.0 + (40.0 - est_w) / 2.0,
                y: 97.0,
                font_size: 10.0,
                color: [0xff, 0xff, 0xff],
            });
        }

        // 2. Search box labels
        self.search_box.prepare_text(font_system);
        for (label, bounds) in self.search_box.text_labels_with_bounds() {
            let metrics = Metrics::new(label.font_size, label.font_size * 1.4);
            let mut buf = Buffer::new(font_system, metrics);
            buf.set_text(font_system, &label.text, Attrs::new(), glyphon::Shaping::Advanced);
            buf.shape_until_scroll(font_system, true);
            self.text_items.push(TextItem {
                buffer: buf,
                x: label.x,
                y: label.y,
                color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                bounds,
            });
        }

        // 3. Email List Labels
        let current_folder_str = match self.current_folder {
            Folder::Inbox => "inbox",
            Folder::Sent => "sent",
            Folder::Trash => "trash",
        };
        let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
        let search_lower = search_text.to_lowercase();
        let filtered: Vec<&Email> = self.emails.iter()
            .filter(|e| e.folder == current_folder_str)
            .filter(|e| {
                if search_lower.is_empty() {
                    true
                } else {
                    e.from.to_lowercase().contains(&search_lower) ||
                    e.subject.to_lowercase().contains(&search_lower) ||
                    e.body.to_lowercase().contains(&search_lower)
                }
            })
            .collect();

        for (idx, email) in filtered.iter().enumerate() {
            if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                // Sender name
                labels.push(TextLabel {
                    text: if email.from.len() > 24 { format!("{}...", &email.from[..21]) } else { email.from.clone() },
                    x: 86.0,
                    y: draw_y + 6.0,
                    font_size: 11.0,
                    color: if !email.read { [0xff, 0xff, 0xff] } else { [0xb0, 0xb0, 0xb8] },
                });

                // Date
                labels.push(TextLabel {
                    text: email.date.clone(),
                    x: 316.0,
                    y: draw_y + 7.0,
                    font_size: 9.0,
                    color: [0x70, 0x70, 0x75],
                });

                // Subject
                labels.push(TextLabel {
                    text: if email.subject.len() > 32 { format!("{}...", &email.subject[..29]) } else { email.subject.clone() },
                    x: 86.0,
                    y: draw_y + 20.0,
                    font_size: 10.0,
                    color: if !email.read { [0x3a, 0x9a, 0xff] } else { [0x83, 0x83, 0x8a] },
                });

                // Snippet
                let snippet_raw = email.body.replace('\n', " ");
                let snippet = if snippet_raw.len() > 40 { format!("{}...", &snippet_raw[..37]) } else { snippet_raw };
                labels.push(TextLabel {
                    text: snippet,
                    x: 86.0,
                    y: draw_y + 34.0,
                    font_size: 9.0,
                    color: [0x60, 0x60, 0x65],
                });
            }
        }

        // 4. Detail View Content
        if let Some(selected_id) = self.selected_email_id {
            if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                labels.extend(self.btn_reply.text_labels());
                labels.extend(self.btn_delete.text_labels());
                labels.extend(self.btn_unread.text_labels());

                // Subject Header
                labels.push(TextLabel {
                    text: email.subject.clone(),
                    x: 391.0,
                    y: 60.0,
                    font_size: 15.0,
                    color: [0xff, 0xff, 0xff],
                });

                // Metadata
                labels.push(TextLabel { text: format!("From: {}", email.from), x: 391.0, y: 85.0, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                labels.push(TextLabel { text: format!("To:   {}", email.to), x: 391.0, y: 105.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
                labels.push(TextLabel { text: format!("Date: {}", email.date), x: 391.0, y: 125.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

                // Body rendering
                self.detail_body.prepare_text(font_system);
                for (label, bounds) in self.detail_body.text_labels_with_bounds() {
                    let metrics = Metrics::new(label.font_size, label.font_size * 1.4);
                    let mut buf = Buffer::new(font_system, metrics);
                    buf.set_text(font_system, &label.text, Attrs::new(), glyphon::Shaping::Advanced);
                    buf.shape_until_scroll(font_system, true);
                    self.text_items.push(TextItem {
                        buffer: buf,
                        x: label.x,
                        y: label.y,
                        color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                        bounds,
                    });
                }
            }
        } else {
            let placeholder = "Select an email to view its content".to_string();
            let est_w = TextLabel::estimate_width(&placeholder, 13.0);
            let px = 376.0 + ((w_f32 - 376.0) - est_w) / 2.0;
            labels.push(TextLabel {
                text: placeholder,
                x: px.max(380.0),
                y: h_f32 / 2.0 - 10.0,
                font_size: 13.0,
                color: [0x60, 0x60, 0x65],
            });
        }

        // 5. Status Banner Message
        if let Some((ref msg, _)) = self.status_message {
            labels.push(TextLabel {
                text: msg.clone(),
                x: 200.0,
                y: h_f32 - 25.0,
                font_size: 11.0,
                color: [0x3a, 0xff, 0x80],
            });
        }

        // 6. Compose Dialog Content
        if self.compose_open {
            let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
            let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

            labels.push(TextLabel {
                text: "New Message".to_string(),
                x: modal_x + 15.0,
                y: modal_y + 16.0,
                font_size: 13.0,
                color: [0xff, 0xff, 0xff],
            });

            labels.push(TextLabel { text: "To:".to_string(), x: modal_x + 15.0, y: modal_y + 54.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Subject:".to_string(), x: modal_x + 15.0, y: modal_y + 94.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

            labels.extend(self.btn_compose_send.text_labels());
            labels.extend(self.btn_compose_cancel.text_labels());

            // Compose inputs text labels
            self.compose_to.prepare_text(font_system);
            for (label, bounds) in self.compose_to.text_labels_with_bounds() {
                let metrics = Metrics::new(label.font_size, label.font_size * 1.4);
                let mut buf = Buffer::new(font_system, metrics);
                buf.set_text(font_system, &label.text, Attrs::new(), glyphon::Shaping::Advanced);
                buf.shape_until_scroll(font_system, true);
                self.text_items.push(TextItem {
                    buffer: buf,
                    x: label.x,
                    y: label.y,
                    color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                    bounds,
                });
            }

            self.compose_subject.prepare_text(font_system);
            for (label, bounds) in self.compose_subject.text_labels_with_bounds() {
                let metrics = Metrics::new(label.font_size, label.font_size * 1.4);
                let mut buf = Buffer::new(font_system, metrics);
                buf.set_text(font_system, &label.text, Attrs::new(), glyphon::Shaping::Advanced);
                buf.shape_until_scroll(font_system, true);
                self.text_items.push(TextItem {
                    buffer: buf,
                    x: label.x,
                    y: label.y,
                    color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                    bounds,
                });
            }

            self.compose_body.prepare_text(font_system);
            for (label, bounds) in self.compose_body.text_labels_with_bounds() {
                let metrics = Metrics::new(label.font_size, label.font_size * 1.4);
                let mut buf = Buffer::new(font_system, metrics);
                buf.set_text(font_system, &label.text, Attrs::new(), glyphon::Shaping::Advanced);
                buf.shape_until_scroll(font_system, true);
                self.text_items.push(TextItem {
                    buffer: buf,
                    x: label.x,
                    y: label.y,
                    color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                    bounds,
                });
            }
        }

        // Shape and append static text items
        for label in labels {
            let metrics = Metrics::new(label.font_size, label.font_size * 1.4);
            let mut buf = Buffer::new(font_system, metrics);
            buf.set_text(font_system, &label.text, Attrs::new(), glyphon::Shaping::Advanced);
            buf.shape_until_scroll(font_system, true);
            self.text_items.push(TextItem {
                buffer: buf,
                x: label.x,
                y: label.y,
                color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                bounds: None,
            });
        }
    }
}

impl Application for ClearEmailApp {
    type Message = AppMessage;

    fn new(_qh: &QueueHandle<EngineState<Self>>, _sender: calloop::channel::Sender<Self::Message>) -> Self {
        let btn_compose = Button::new(10.0, 15.0, 36.0, 36.0).with_label("+");
        let mut paginator = Paginator::new(56.0, vec![
            "📥 Inbox".to_string(),
            "📤 Sent".to_string(),
            "🗑️ Trash".to_string(),
        ]);
        paginator.tabs_rotated = true;
        paginator.tabs_at_top = false;
        paginator.tab_y_offset = 70.0;

        let mut search_box = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        search_box.font_size = 11.0;

        let email_list = ScrollingList::new(54.0, 4.0);

        let btn_reply = Button::new(391.0, 8.0, 70.0, 26.0).with_label("Reply");
        let btn_delete = Button::new_reset(471.0, 8.0, 80.0, 26.0).with_label("Delete");
        let btn_unread = Button::new(561.0, 8.0, 110.0, 26.0).with_label("Mark Unread");

        let mut detail_body = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(false);
        detail_body.font_size = 12.0;
        detail_body.font_family = "sans-serif".to_string();

        let mut compose_to = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_to.font_size = 12.0;
        let mut compose_subject = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_subject.font_size = 12.0;
        let mut compose_body = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(true);
        compose_body.font_size = 12.0;
        compose_body.font_family = "sans-serif".to_string();

        let btn_compose_send = Button::new(0.0, 0.0, 75.0, 28.0).with_label("Send");
        let btn_compose_cancel = Button::new_reset(0.0, 0.0, 75.0, 28.0).with_label("Cancel");

        let emails = load_emails();

        Self {
            btn_compose,
            paginator,
            search_box,
            email_list,
            email_buttons: Vec::new(),
            btn_reply,
            btn_delete,
            btn_unread,
            detail_body,
            compose_to,
            compose_subject,
            compose_body,
            btn_compose_send,
            btn_compose_cancel,
            emails,
            current_folder: Folder::Inbox,
            selected_email_id: None,
            compose_open: false,
            status_message: None,
            width: 1000,
            height: 600,
            scale_factor: 1.0,
            text_items: Vec::new(),
            font_system: FontSystem::new(),
            needs_rebuild: true,
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Clear Email Client".to_string(),
            app_id: "clear-email-interface".to_string(),
            width: 1000,
            height: 600,
            fullscreen: false,
            min_size: Some((800, 500)),
        }
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            AppMessage::SwitchFolder(f) => {
                self.current_folder = f;
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectEmail(id) => {
                self.selected_email_id = Some(id);
                if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                    if !email.read {
                        email.read = true;
                        save_emails(&self.emails);
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SearchChanged => {
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeNew => {
                self.compose_to.text = String::new();
                self.compose_to.edit_buffer = String::new();
                self.compose_subject.text = String::new();
                self.compose_subject.edit_buffer = String::new();
                self.compose_body.text = String::new();
                self.compose_body.edit_buffer = String::new();
                self.compose_open = true;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeCancel => {
                self.compose_open = false;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeSend => {
                let to = if self.compose_to.editing { &self.compose_to.edit_buffer } else { &self.compose_to.text };
                let subject = if self.compose_subject.editing { &self.compose_subject.edit_buffer } else { &self.compose_subject.text };
                let body = if self.compose_body.editing { &self.compose_body.edit_buffer } else { &self.compose_body.text };

                if !to.trim().is_empty() {
                    let new_id = self.emails.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                    let new_email = Email {
                        id: new_id,
                        from: "lsgalante@clear-ui.org".to_string(),
                        to: to.clone(),
                        subject: if subject.trim().is_empty() { "(No Subject)".to_string() } else { subject.clone() },
                        body: body.clone(),
                        date: "Just now".to_string(),
                        read: true,
                        folder: "sent".to_string(),
                    };
                    self.emails.push(new_email);
                    save_emails(&self.emails);
                    self.compose_open = false;
                    self.status_message = Some(("Email Sent Successfully".to_string(), 4.0));
                } else {
                    self.status_message = Some(("Recipient is required".to_string(), 4.0));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::Reply => {
                if let Some(id) = self.selected_email_id {
                    if let Some(email) = self.emails.iter().find(|e| e.id == id) {
                        self.compose_to.text = email.from.clone();
                        self.compose_to.edit_buffer = email.from.clone();
                        self.compose_subject.text = if email.subject.starts_with("Re:") {
                            email.subject.clone()
                        } else {
                            format!("Re: {}", email.subject)
                        };
                        self.compose_subject.edit_buffer = self.compose_subject.text.clone();

                        let reply_intro = format!("\n\nOn {}, {} wrote:\n> {}", email.date, email.from, email.body.replace('\n', "\n> "));
                        self.compose_body.text = reply_intro.clone();
                        self.compose_body.edit_buffer = reply_intro;
                        self.compose_body.cursor_idx = 0;
                        self.compose_open = true;
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::DeleteSelected => {
                if let Some(id) = self.selected_email_id {
                    let mut permanently_deleted = false;
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        if email.folder == "trash" {
                            permanently_deleted = true;
                        } else {
                            email.folder = "trash".to_string();
                        }
                    }
                    if permanently_deleted {
                        self.emails.retain(|e| e.id != id);
                    }
                    save_emails(&self.emails);
                    self.selected_email_id = None;
                    self.status_message = Some((
                        if permanently_deleted { "Email Deleted Permanently" } else { "Moved to Trash" }.to_string(),
                        4.0
                    ));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ToggleUnread => {
                if let Some(id) = self.selected_email_id {
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        email.read = !email.read;
                    }
                    save_emails(&self.emails);
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if let Some((_, ref mut timer)) = self.status_message {
            *timer -= dt;
            if *timer <= 0.0 {
                self.status_message = None;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
        }
        if self.paginator.tick(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn view(&mut self, quads: &mut Vec<(f32, f32, f32, f32, [f32; 4])>, size: LogicalSize, scale: f64) {
        let size_changed = self.width != size.width as u32 || self.height != size.height as u32 || self.scale_factor != scale;
        if self.needs_rebuild || size_changed {
            self.width = size.width as u32;
            self.height = size.height as u32;
            self.scale_factor = scale;
        }

        let w_f32 = self.width as f32;
        let h_f32 = self.height as f32;

        let current_folder_str = match self.current_folder {
            Folder::Inbox => "inbox",
            Folder::Sent => "sent",
            Folder::Trash => "trash",
        };

        if self.needs_rebuild || size_changed {
            // Sidebar buttons layout
            self.btn_compose.set_rect(10.0, 15.0, 36.0, 36.0);

            // Set paginator layout
            self.paginator.set_scale_factor(scale as f32);
            self.paginator.set_rect(0.0, 0.0, 56.0, h_f32);
            let folder_idx = match self.current_folder {
                Folder::Inbox => 0,
                Folder::Sent => 1,
                Folder::Trash => 2,
            };
            self.paginator.set_selected_page(folder_idx);

            // Search box and Scrolling list
            self.search_box.set_rect(66.0, 15.0, 300.0, 26.0);

            // Get filtered emails count for bounds setup
            let filtered_count = {
                let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
                let search_lower = search_text.to_lowercase();
                self.emails.iter()
                    .filter(|e| e.folder == current_folder_str)
                    .filter(|e| {
                        if search_lower.is_empty() {
                            true
                        } else {
                            e.from.to_lowercase().contains(&search_lower) ||
                            e.subject.to_lowercase().contains(&search_lower) ||
                            e.body.to_lowercase().contains(&search_lower)
                        }
                    })
                    .count()
            };

            self.email_list.set_rect(66.0, 55.0, 300.0, h_f32 - 70.0);
            self.email_list.update_bounds(filtered_count, 55.0, h_f32 - 70.0);

            if self.email_buttons.len() != filtered_count {
                self.email_buttons = (0..filtered_count)
                    .map(|_| Button::new_list_row(0.0, 0.0, 0.0, 0.0))
                    .collect();
            }

            // Get selected email state to avoid borrowing self while mutating
            let selected_email_state = if let Some(selected_id) = self.selected_email_id {
                self.emails.iter().find(|e| e.id == selected_id).map(|e| (e.read, e.body.clone()))
            } else {
                None
            };

            // Get filtered email IDs and selection states
            let filtered_email_ids: Vec<(usize, bool)> = {
                let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
                let search_lower = search_text.to_lowercase();
                self.emails.iter()
                    .filter(|e| e.folder == current_folder_str)
                    .filter(|e| {
                        if search_lower.is_empty() {
                            true
                        } else {
                            e.from.to_lowercase().contains(&search_lower) ||
                            e.subject.to_lowercase().contains(&search_lower) ||
                            e.body.to_lowercase().contains(&search_lower)
                        }
                    })
                    .map(|e| (e.id, Some(e.id) == self.selected_email_id))
                    .collect()
            };

            for (idx, &(_email_id, is_selected)) in filtered_email_ids.iter().enumerate() {
                self.email_buttons[idx].selected = is_selected;
                if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                    self.email_buttons[idx].set_rect(66.0, draw_y, 300.0, 54.0);
                } else {
                    self.email_buttons[idx].set_rect(-9999.0, -9999.0, 0.0, 0.0);
                }
            }

            // Detail View
            if let Some((read, body)) = selected_email_state {
                self.btn_reply.set_rect(391.0, 8.0, 70.0, 26.0);
                self.btn_delete.set_rect(471.0, 8.0, 80.0, 26.0);
                self.btn_unread.set_rect(561.0, 8.0, 110.0, 26.0);
                if let Some(base) = self.btn_unread.base_mut() {
                    base.label = Some((if read { "Mark Unread" } else { "Mark Read" }).to_string());
                }

                let detail_w = (w_f32 - 406.0).max(100.0);
                self.detail_body.set_rect(391.0, 170.0, detail_w, (h_f32 - 190.0).max(100.0));
                self.detail_body.text = body;
            }

            // Compose inputs layout
            if self.compose_open {
                let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
                let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

                self.compose_to.set_rect(modal_x + 80.0, modal_y + 50.0, 400.0, 26.0);
                self.compose_subject.set_rect(modal_x + 80.0, modal_y + 90.0, 400.0, 26.0);
                self.compose_body.set_rect(modal_x + 15.0, modal_y + 130.0, 470.0, 230.0);

                self.btn_compose_send.set_rect(modal_x + 320.0, modal_y + 375.0, 75.0, 28.0);
                self.btn_compose_cancel.set_rect(modal_x + 410.0, modal_y + 375.0, 75.0, 28.0);
            }

            self.rebuild_text_items();
            self.needs_rebuild = false;
        }

        // Now compute `filtered` only for rendering (immutable borrow of self)
        let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
        let search_lower = search_text.to_lowercase();
        let filtered: Vec<&Email> = self.emails.iter()
            .filter(|e| e.folder == current_folder_str)
            .filter(|e| {
                if search_lower.is_empty() {
                    true
                } else {
                    e.from.to_lowercase().contains(&search_lower) ||
                    e.subject.to_lowercase().contains(&search_lower) ||
                    e.body.to_lowercase().contains(&search_lower)
                }
            })
            .collect();

        // 1. General window background (deep slate blue)
        quads.push((0.0, 0.0, w_f32, h_f32, [0.05, 0.05, 0.07, 1.0]));

        // 2. Sidebar background panel
        quads.push((0.0, 0.0, 56.0, h_f32, [0.08, 0.08, 0.12, 1.0]));
        quads.push((56.0, 0.0, 1.0, h_f32, [0.18, 0.18, 0.22, 1.0])); // sidebar separator

        // Compose Button and Folders Graphics
        quads.extend(self.btn_compose.extra_quads());
        quads.extend(self.paginator.extra_quads());

        // Draw badge pill for inbox unread (centered on vertical rotated tab)
        let inbox_unread = self.emails.iter().filter(|e| e.folder == "inbox" && !e.read).count();
        if inbox_unread > 0 {
            let bx = 8.0;
            let by = 70.0;
            quads.push((bx + (40.0 - 22.0) / 2.0, by + 25.0, 22.0, 16.0, [0.20, 0.45, 0.85, 0.8]));
        }

        // 3. Email List Panel Separator
        quads.push((376.0, 0.0, 1.0, h_f32, [0.18, 0.18, 0.22, 1.0]));

        // Search box and List
        quads.extend(self.search_box.extra_quads());
        quads.extend(self.email_list.extra_quads());

        // Visible Email List Item Buttons
        for (idx, _) in filtered.iter().enumerate() {
            if self.email_list.get_item_draw_y(idx, 0.0).is_some() {
                quads.extend(self.email_buttons[idx].extra_quads());

                // Blue dot/unread indicator for this row
                if !filtered[idx].read {
                    if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                        quads.push((74.0, draw_y + 12.0, 6.0, 6.0, [0.20, 0.45, 0.85, 1.0]));
                    }
                }
            }
        }

        // 4. Detail View Area
        if let Some(selected_id) = self.selected_email_id {
            if self.emails.iter().any(|e| e.id == selected_id) {
                // Top action toolbar background
                quads.push((377.0, 0.0, w_f32 - 377.0, 42.0, [0.08, 0.08, 0.12, 1.0]));
                quads.push((377.0, 42.0, w_f32 - 377.0, 1.0, [0.18, 0.18, 0.22, 1.0]));

                quads.extend(self.btn_reply.extra_quads());
                quads.extend(self.btn_delete.extra_quads());
                quads.extend(self.btn_unread.extra_quads());

                // Detail body textbox graphics
                quads.extend(self.detail_body.extra_quads());
            }
        }

        // 5. Compose Dialog Overlay
        if self.compose_open {
            let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
            let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

            // Semitransparent modal backdrop
            quads.push((0.0, 0.0, w_f32, h_f32, [0.0, 0.0, 0.0, 0.6]));

            // Modal dialog container
            quads.push((modal_x, modal_y, 500.0, 420.0, [0.08, 0.08, 0.12, 1.0]));
            quads.push((modal_x, modal_y, 500.0, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y + 419.0, 500.0, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y, 1.0, 420.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x + 499.0, modal_y, 1.0, 420.0, [0.25, 0.35, 0.50, 0.40]));

            quads.extend(self.compose_to.extra_quads());
            quads.extend(self.compose_subject.extra_quads());
            quads.extend(self.compose_body.extra_quads());
            quads.extend(self.btn_compose_send.extra_quads());
            quads.extend(self.btn_compose_cancel.extra_quads());
        }
    }

    fn text_items(&self) -> &[TextItem] {
        &self.text_items
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let mut changed = false;
        let px = pos.x as f32;
        let py = pos.y as f32;

        if self.compose_open {
            if self.compose_to.on_cursor_moved(px, py) { changed = true; }
            if self.compose_subject.on_cursor_moved(px, py) { changed = true; }
            if self.compose_body.on_cursor_moved(px, py) { changed = true; }
            if self.btn_compose_send.on_cursor_moved(px, py) { changed = true; }
            if self.btn_compose_cancel.on_cursor_moved(px, py) { changed = true; }
        } else {
            // Sidebar buttons
            if self.btn_compose.on_cursor_moved(px, py) { changed = true; }
            if px < 56.0 {
                if self.paginator.on_cursor_moved(px, py) { changed = true; }
            }

            // Search and lists
            if self.search_box.on_cursor_moved(px, py) { changed = true; }
            if self.email_list.on_cursor_moved(px, py) { changed = true; }

            for btn in &mut self.email_buttons {
                if btn.rect().0 > -9000.0 {
                    if btn.on_cursor_moved(px, py) { changed = true; }
                }
            }

            // Detail view buttons
            if self.selected_email_id.is_some() {
                if self.btn_reply.on_cursor_moved(px, py) { changed = true; }
                if self.btn_delete.on_cursor_moved(px, py) { changed = true; }
                if self.btn_unread.on_cursor_moved(px, py) { changed = true; }
                if self.detail_body.on_cursor_moved(px, py) { changed = true; }
            }
        }

        if changed {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn handle_mouse_input(&mut self, button: MouseButton, state: ElementState, pos: LogicalPosition, needs_rebuild: &mut bool) -> Option<Self::Message> {
        let mut changed = false;
        let mut msg_out = None;
        let px = pos.x as f32;
        let py = pos.y as f32;

        if self.compose_open {
            if self.compose_to.mouse_input(button, state, px, py) { changed = true; }
            if self.compose_subject.mouse_input(button, state, px, py) { changed = true; }
            if self.compose_body.mouse_input(button, state, px, py) { changed = true; }

            if self.btn_compose_send.mouse_input(button, state, px, py) {
                changed = true;
                if state == ElementState::Released && self.btn_compose_send.take_click() {
                    msg_out = Some(AppMessage::ComposeSend);
                }
            }
            if self.btn_compose_cancel.mouse_input(button, state, px, py) {
                changed = true;
                if state == ElementState::Released && self.btn_compose_cancel.take_click() {
                    msg_out = Some(AppMessage::ComposeCancel);
                }
            }

            // Click outside the modal clears focus or behaves neutrally
            if !changed && state == ElementState::Pressed && button == MouseButton::Left {
                let w_f32 = self.width as f32;
                let h_f32 = self.height as f32;
                let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
                let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

                if px < modal_x || px > modal_x + 500.0 || py < modal_y || py > modal_y + 420.0 {
                    self.compose_to.unfocus();
                    self.compose_subject.unfocus();
                    self.compose_body.unfocus();
                    changed = true;
                }
            }
        } else {
            // Sidebar buttons
            if self.btn_compose.mouse_input(button, state, px, py) {
                changed = true;
                if state == ElementState::Released && self.btn_compose.take_click() {
                    msg_out = Some(AppMessage::ComposeNew);
                }
            }
            if px < 56.0 {
                if self.paginator.mouse_input(button, state, px, py) {
                    changed = true;
                    if self.paginator.take_click() {
                        let page = self.paginator.selected_page();
                        let folder = match page {
                            0 => Folder::Inbox,
                            1 => Folder::Sent,
                            2 => Folder::Trash,
                            _ => Folder::Inbox,
                        };
                        msg_out = Some(AppMessage::SwitchFolder(folder));
                    }
                }
            }

            // Search input
            if self.search_box.mouse_input(button, state, px, py) {
                changed = true;
                if self.search_box.take_change() {
                    msg_out = Some(AppMessage::SearchChanged);
                }
            } else if state == ElementState::Pressed && button == MouseButton::Left {
                self.search_box.unfocus();
                changed = true;
            }

            // Scrolling list items
            let current_folder_str = match self.current_folder {
                Folder::Inbox => "inbox",
                Folder::Sent => "sent",
                Folder::Trash => "trash",
            };
            let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
            let search_lower = search_text.to_lowercase();
            let filtered: Vec<&Email> = self.emails.iter()
                .filter(|e| e.folder == current_folder_str)
                .filter(|e| {
                    if search_lower.is_empty() {
                        true
                    } else {
                        e.from.to_lowercase().contains(&search_lower) ||
                        e.subject.to_lowercase().contains(&search_lower) ||
                        e.body.to_lowercase().contains(&search_lower)
                    }
                })
                .collect();

            if self.email_list.mouse_input(button, state, px, py) {
                changed = true;
            }

            for (idx, email) in filtered.iter().enumerate() {
                if idx < self.email_buttons.len() {
                    let btn = &mut self.email_buttons[idx];
                    if btn.rect().0 > -9000.0 {
                        if btn.mouse_input(button, state, px, py) {
                            changed = true;
                            if state == ElementState::Released && btn.take_click() {
                                msg_out = Some(AppMessage::SelectEmail(email.id));
                            }
                        }
                    }
                }
            }

            // Detail View action buttons
            if self.selected_email_id.is_some() {
                if self.btn_reply.mouse_input(button, state, px, py) {
                    changed = true;
                    if state == ElementState::Released && self.btn_reply.take_click() {
                        msg_out = Some(AppMessage::Reply);
                    }
                }
                if self.btn_delete.mouse_input(button, state, px, py) {
                    changed = true;
                    if state == ElementState::Released && self.btn_delete.take_click() {
                        msg_out = Some(AppMessage::DeleteSelected);
                    }
                }
                if self.btn_unread.mouse_input(button, state, px, py) {
                    changed = true;
                    if state == ElementState::Released && self.btn_unread.take_click() {
                        msg_out = Some(AppMessage::ToggleUnread);
                    }
                }
                if self.detail_body.mouse_input(button, state, px, py) {
                    changed = true;
                }
            }
        }

        if changed {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }

        msg_out
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let mut changed = false;
        let px = pos.x as f32;
        let py = pos.y as f32;

        if !self.compose_open {
            if px >= 66.0 && px <= 366.0 {
                if self.email_list.mouse_wheel(delta, px, py) {
                    changed = true;
                }
            }
        }

        if changed {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        let mut handled = false;
        let mut msg_out = None;

        if self.compose_open {
            if self.compose_to.editing {
                if self.compose_to.keyboard_input(event) { handled = true; }
            } else if self.compose_subject.editing {
                if self.compose_subject.keyboard_input(event) { handled = true; }
            } else if self.compose_body.editing {
                if self.compose_body.keyboard_input(event) { handled = true; }
            }

            // Escape closes compose dialog
            if !handled && event.state == ElementState::Pressed && event.logical_key == Key::Named(clear_ui::widget::NamedKey::Escape) {
                msg_out = Some(AppMessage::ComposeCancel);
                handled = true;
            }
        } else {
            // General keyboard shortcuts
            if event.ctrl && event.state == ElementState::Pressed {
                if let Key::Character(ref ch) = event.logical_key {
                    match ch.to_lowercase().as_str() {
                        "n" => {
                            msg_out = Some(AppMessage::ComposeNew);
                            handled = true;
                        }
                        "f" => {
                            self.search_box.focus();
                            handled = true;
                        }
                        _ => {}
                    }
                }
            }

            if !handled && self.search_box.editing {
                if self.search_box.keyboard_input(event) {
                    handled = true;
                    if self.search_box.take_change() {
                        msg_out = Some(AppMessage::SearchChanged);
                    }
                }
            }

            // Escape unfocuses search
            if !handled && event.state == ElementState::Pressed && event.logical_key == Key::Named(clear_ui::widget::NamedKey::Escape) {
                if self.search_box.editing {
                    self.search_box.unfocus();
                    handled = true;
                }
            }
        }

        if handled {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }

        msg_out
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    clear_ui::engine::run::<ClearEmailApp>();
}
