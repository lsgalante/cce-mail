//! Sending: the outgoing message, its MIME build (attachments included) and SMTP.
//! Split out of main.rs on 2026-10-10.

use crate::*;

/// One outgoing message, exactly as composed. This travels to the send
/// thread and comes back in [`AppMessage::SendResult`] so the outcome
/// handler still holds the full content — a success records it in Sent, a
/// failure lands it in Drafts instead of losing it.
#[derive(Debug, Clone)]
pub(crate) struct OutgoingMail {
    pub(crate) to: String,
    pub(crate) cc: String,
    pub(crate) bcc: String,
    pub(crate) subject: String,
    pub(crate) body: String,
    /// Absolute paths; read at build time on the send thread.
    pub(crate) attachments: Vec<String>,
}

/// Content type for an attachment, by extension. A tiny map beats a mime
/// dependency: anything unlisted is application/octet-stream, which every
/// receiver treats as "download it".
pub(crate) fn attachment_content_type(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") | Some("log") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        Some("gz") | Some("tgz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// Build the lettre message for one [`OutgoingMail`].
///
/// To and Cc become headers; **Bcc deliberately never becomes a header** —
/// bcc recipients ride only in the SMTP envelope, so no copy of the message
/// can name them regardless of how the library formats headers. Attachments
/// are read here (the send thread), each as one part of a multipart/mixed.
pub(crate) fn build_outgoing(from: &str, mail: &OutgoingMail) -> Result<lettre::Message, String> {
    use lettre::message::{Attachment, MultiPart, SinglePart, header::ContentType};

    fn mailboxes(field: &str, label: &str) -> Result<Vec<lettre::message::Mailbox>, String> {
        field
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| a.parse().map_err(|e| format!("Invalid {} address {}: {}", label, a, e)))
            .collect()
    }

    let from_mb: lettre::message::Mailbox =
        from.parse().map_err(|e| format!("Invalid From address: {}", e))?;
    let to = mailboxes(&mail.to, "To")?;
    let cc = mailboxes(&mail.cc, "Cc")?;
    let bcc = mailboxes(&mail.bcc, "Bcc")?;
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err("No recipients".to_string());
    }

    // Envelope = actual delivery list: To + Cc + Bcc.
    let rcpts: Vec<lettre::Address> =
        to.iter().chain(cc.iter()).chain(bcc.iter()).map(|m| m.email.clone()).collect();
    let envelope = lettre::address::Envelope::new(Some(from_mb.email.clone()), rcpts)
        .map_err(|e| format!("Invalid envelope: {}", e))?;

    let mut builder = Message::builder().from(from_mb).envelope(envelope).subject(&mail.subject);
    for mb in to {
        builder = builder.to(mb);
    }
    for mb in cc {
        builder = builder.cc(mb);
    }

    let msg = if mail.attachments.is_empty() {
        builder.body(mail.body.clone())
    } else {
        let mut mp = MultiPart::mixed().singlepart(SinglePart::plain(mail.body.clone()));
        for path in &mail.attachments {
            let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {}", path, e))?;
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_string();
            let ct = ContentType::parse(attachment_content_type(path))
                .map_err(|e| format!("Bad content type: {}", e))?;
            mp = mp.singlepart(Attachment::new(name).body(bytes, ct));
        }
        builder.multipart(mp)
    };
    msg.map_err(|e| format!("Failed to build message: {}", e))
}

pub(crate) fn send_smtp(mut account: AccountInfo, mail: OutgoingMail, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        // Every exit reports through SendResult so the app can file the
        // message (Sent on success, Drafts on failure) — a bare Status would
        // discard the composed content.
        let fail = |sender: &calloop::channel::Sender<AppMessage>, mail: OutgoingMail, err: String| {
            let _ = sender.send(AppMessage::SendResult(mail, Some(err)));
        };

        if account.password == "mock_password" {
            let _ = sender.send(AppMessage::SendResult(mail, None));
            return;
        }

        let mut access_token = account.password.clone();
        if account.is_oauth {
            let mut acc = account.clone();
            match block_on_worker(refresh_access_token(&mut acc)).and_then(|r| r) {
                Ok(token) => {
                    access_token = token;
                    let _ = sender.send(AppMessage::UpdateAccountTokens(
                        acc.email.clone(),
                        acc.access_token.clone(),
                        acc.token_expiry,
                    ));
                    account.access_token = acc.access_token;
                    account.token_expiry = acc.token_expiry;
                }
                Err(e) => {
                    fail(&sender, mail, format!("OAuth Refresh Failed: {}", e));
                    return;
                }
            }
        }

        let domain = match account.smtp.split(':').next() {
            Some(d) => d,
            None => {
                fail(&sender, mail, "Invalid SMTP hostname".to_string());
                return;
            }
        };
        let port = account.smtp.split(':').nth(1).and_then(|p| p.parse::<u16>().ok()).unwrap_or(465);

        let _ = sender.send(AppMessage::Status("Sending SMTP mail...".to_string()));

        let email = match build_outgoing(&account.email, &mail) {
            Ok(m) => m,
            Err(e) => {
                fail(&sender, mail, e);
                return;
            }
        };

        let creds = Credentials::new(account.email.clone(), access_token);

        let mut mailer_builder = if port == 465 {
            SmtpTransport::relay(domain).unwrap().port(port)
        } else {
            SmtpTransport::starttls_relay(domain).unwrap().port(port)
        };

        if account.is_oauth {
            mailer_builder = mailer_builder.authentication(vec![lettre::transport::smtp::authentication::Mechanism::Xoauth2]);
        }

        let mailer = mailer_builder.credentials(creds).build();

        match mailer.send(&email) {
            Ok(_) => {
                let _ = sender.send(AppMessage::SendResult(mail, None));
            }
            Err(e) => {
                fail(&sender, mail, format!("SMTP send failed: {}", e));
            }
        }
    });
}
