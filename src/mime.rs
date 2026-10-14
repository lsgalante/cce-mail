//! Reading messages: walking a BODYSTRUCTURE for the text, HTML, inline and
//! attachment parts, decoding a part, the From/To/Date lines and plain-text
//! bodies, and `mailto:` arguments.
//! Split out of main.rs on 2026-10-10.

use crate::*;

/// The text part chosen from a BODYSTRUCTURE walk: its IMAP section path plus
/// the metadata needed to rebuild a decodable single-part MIME message.
pub(crate) struct TextPartSpec {
    pub(crate) path: Vec<u32>,
    pub(crate) subtype: String,
    pub(crate) charset: Option<String>,
    pub(crate) encoding: String,
}

/// IMAP dotted section string for a part path ("1.2").
pub(crate) fn section_str(path: &[u32]) -> String {
    path.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

impl TextPartSpec {
    /// Wrap raw (still transfer-encoded) part bytes in a minimal single-part
    /// message so mail-parser handles the decode (CTE + charset + HTML→text).
    pub(crate) fn synthesize(&self, part_bytes: &[u8]) -> Vec<u8> {
        let charset = self.charset.as_deref().unwrap_or("utf-8");
        let mut raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: text/{}; charset=\"{}\"\r\nContent-Transfer-Encoding: {}\r\n\r\n",
            self.subtype, charset, self.encoding
        )
        .into_bytes();
        raw.extend_from_slice(part_bytes);
        raw
    }
}

pub(crate) fn encoding_str(enc: &imap_proto::types::ContentEncoding) -> String {
    use imap_proto::types::ContentEncoding as E;
    match enc {
        E::SevenBit => "7bit".to_string(),
        E::EightBit => "8bit".to_string(),
        E::Binary => "binary".to_string(),
        E::Base64 => "base64".to_string(),
        E::QuotedPrintable => "quoted-printable".to_string(),
        E::Other(s) => s.to_string(),
    }
}

/// DFS over a BODYSTRUCTURE for the best displayable part: the first
/// text/plain anywhere, else the first text/html. Multipart children are
/// numbered 1.. and nest dotted (RFC 3501); a non-multipart top level is
/// section 1. None (e.g. embedded message/rfc822 only) → caller falls back
/// to a capped full-message fetch.
pub(crate) fn find_text_part(bs: &imap_proto::types::BodyStructure<'_>) -> Option<TextPartSpec> {
    use imap_proto::types::BodyStructure as B;
    fn walk(bs: &B<'_>, path: &mut Vec<u32>, best: &mut Option<(u8, TextPartSpec)>) {
        match bs {
            B::Text { common, other, .. } => {
                let sub = common.ty.subtype.to_ascii_lowercase();
                let rank = match sub.as_str() {
                    "plain" => 0u8,
                    "html" => 1u8,
                    _ => return,
                };
                if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                    let charset = common.ty.params.as_ref().and_then(|ps| {
                        ps.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("charset"))
                            .map(|(_, v)| v.to_string())
                    });
                    *best = Some((
                        rank,
                        TextPartSpec {
                            path: if path.is_empty() { vec![1] } else { path.clone() },
                            subtype: sub,
                            charset,
                            encoding: encoding_str(&other.transfer_encoding),
                        },
                    ));
                }
            }
            B::Multipart { bodies, .. } => {
                for (i, b) in bodies.iter().enumerate() {
                    path.push(i as u32 + 1);
                    walk(b, path, best);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(bs, &mut Vec::new(), &mut best);
    best.map(|(_, spec)| spec)
}

/// DFS over a BODYSTRUCTURE for the first text/html part, for the HTML
/// view. Separate from [`find_text_part`] because the two walks want
/// opposite parts of a multipart/alternative: the sync wants the cheap
/// plain preview, the HTML view wants the real thing.
#[cfg(feature = "wpe")]
pub(crate) fn find_html_part(bs: &imap_proto::types::BodyStructure<'_>) -> Option<TextPartSpec> {
    use imap_proto::types::BodyStructure as B;
    fn walk(bs: &B<'_>, path: &mut Vec<u32>, best: &mut Option<TextPartSpec>) {
        if best.is_some() {
            return;
        }
        match bs {
            B::Text { common, other, .. } if common.ty.subtype.eq_ignore_ascii_case("html") => {
                let charset = common.ty.params.as_ref().and_then(|ps| {
                    ps.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("charset"))
                        .map(|(_, v)| v.to_string())
                });
                *best = Some(TextPartSpec {
                    path: if path.is_empty() { vec![1] } else { path.clone() },
                    subtype: "html".to_string(),
                    charset,
                    encoding: encoding_str(&other.transfer_encoding),
                });
            }
            B::Multipart { bodies, .. } => {
                for (i, b) in bodies.iter().enumerate() {
                    path.push(i as u32 + 1);
                    walk(b, path, best);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(bs, &mut Vec::new(), &mut best);
    best
}

/// A part carrying a Content-ID — a candidate for the HTML body's `cid:`
/// references. Whether it is actually fetched depends on the HTML: only
/// referenced parts are worth the round trip.
#[cfg(feature = "wpe")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InlinePartSpec {
    /// Content-ID with the RFC 2392 angle brackets stripped — the form a
    /// `cid:` URI names it by.
    pub(crate) cid: String,
    pub(crate) section: Vec<u32>,
    pub(crate) encoding: String,
    pub(crate) mime: String,
}

/// DFS over a BODYSTRUCTURE for every part with a Content-ID, in the same
/// section-numbering scheme as the other walks.
#[cfg(feature = "wpe")]
pub(crate) fn find_inline_parts(bs: &imap_proto::types::BodyStructure<'_>) -> Vec<InlinePartSpec> {
    use imap_proto::types::BodyStructure as B;
    fn push(
        common: &imap_proto::types::BodyContentCommon<'_>,
        other: &imap_proto::types::BodyContentSinglePart<'_>,
        path: &[u32],
        out: &mut Vec<InlinePartSpec>,
    ) {
        let Some(id) = &other.id else { return };
        let cid = id.trim().trim_start_matches('<').trim_end_matches('>').to_string();
        if cid.is_empty() {
            return;
        }
        out.push(InlinePartSpec {
            cid,
            section: if path.is_empty() { vec![1] } else { path.to_vec() },
            encoding: encoding_str(&other.transfer_encoding),
            mime: format!(
                "{}/{}",
                common.ty.ty.to_ascii_lowercase(),
                common.ty.subtype.to_ascii_lowercase()
            ),
        });
    }
    fn walk(bs: &B<'_>, path: &mut Vec<u32>, out: &mut Vec<InlinePartSpec>) {
        match bs {
            B::Basic { common, other, .. } | B::Text { common, other, .. } => {
                push(common, other, path, out)
            }
            B::Multipart { bodies, .. } => {
                for (i, b) in bodies.iter().enumerate() {
                    path.push(i as u32 + 1);
                    walk(b, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(bs, &mut Vec::new(), &mut out);
    out
}

/// One attachment as the server describes it: everything needed to list it
/// in the detail pane and to fetch exactly that part on demand.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RemoteAttachment {
    pub(crate) name: String,
    /// BODYSTRUCTURE part path (dotted section = section_str(&section)).
    pub(crate) section: Vec<u32>,
    /// Content-Transfer-Encoding, for the decode after the fetch.
    pub(crate) encoding: String,
    pub(crate) mime: String,
    /// Transfer-encoded size in octets, as reported by the server.
    pub(crate) size: u32,
}

/// DFS over a BODYSTRUCTURE for the parts that are attachments: anything
/// with an `attachment` disposition, or any part carrying a filename (many
/// senders attach with only a Content-Type `name` param). The filename falls
/// back through disposition `filename` → type `name` → a synthesized
/// `attachment.<subtype>`. Same path numbering as [`find_text_part`].
pub(crate) fn find_attachment_parts(bs: &imap_proto::types::BodyStructure<'_>) -> Vec<RemoteAttachment> {
    use imap_proto::types::BodyStructure as B;

    fn param(params: &imap_proto::types::BodyParams<'_>, key: &str) -> Option<String> {
        params.as_ref().and_then(|ps| {
            ps.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.to_string())
        })
    }

    fn single(
        common: &imap_proto::types::BodyContentCommon<'_>,
        other: &imap_proto::types::BodyContentSinglePart<'_>,
        path: &[u32],
        out: &mut Vec<RemoteAttachment>,
    ) {
        let disp_attachment = common
            .disposition
            .as_ref()
            .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"));
        let filename = common
            .disposition
            .as_ref()
            .and_then(|d| param(&d.params, "filename"))
            .or_else(|| param(&common.ty.params, "name"));
        if !disp_attachment && filename.is_none() {
            return;
        }
        let mime = format!(
            "{}/{}",
            common.ty.ty.to_ascii_lowercase(),
            common.ty.subtype.to_ascii_lowercase()
        );
        out.push(RemoteAttachment {
            name: filename
                .unwrap_or_else(|| format!("attachment.{}", common.ty.subtype.to_ascii_lowercase())),
            section: if path.is_empty() { vec![1] } else { path.to_vec() },
            encoding: encoding_str(&other.transfer_encoding),
            mime,
            size: other.octets,
        });
    }

    fn walk(bs: &B<'_>, path: &mut Vec<u32>, out: &mut Vec<RemoteAttachment>) {
        match bs {
            B::Basic { common, other, .. }
            | B::Text { common, other, .. }
            | B::Message { common, other, .. } => single(common, other, path, out),
            B::Multipart { bodies, .. } => {
                for (i, b) in bodies.iter().enumerate() {
                    path.push(i as u32 + 1);
                    walk(b, path, out);
                    path.pop();
                }
            }
        }
    }

    let mut out = Vec::new();
    walk(bs, &mut Vec::new(), &mut out);
    out
}

/// Decode fetched part bytes per their Content-Transfer-Encoding, by the
/// same trick [`TextPartSpec::synthesize`] uses for bodies: wrap the raw
/// bytes as a minimal single-part message and let mail-parser do the decode
/// (it already handles base64/quoted-printable and their whitespace forms).
/// Content-Type is forced to application/octet-stream so the part parses as
/// an opaque attachment — the real MIME type only matters to the opener.
pub(crate) fn decode_part_bytes(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    match encoding.to_ascii_lowercase().as_str() {
        "7bit" | "8bit" | "binary" | "" => bytes.to_vec(),
        enc => {
            let mut raw = format!(
                "MIME-Version: 1.0\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: {}\r\nContent-Disposition: attachment\r\n\r\n",
                enc
            )
            .into_bytes();
            raw.extend_from_slice(bytes);
            mail_parser::MessageParser::default()
                .parse(&raw)
                .and_then(|m| m.attachments().next().map(|a| a.contents().to_vec()))
                .unwrap_or_else(|| bytes.to_vec())
        }
    }
}

/// Char-boundary-safe ellipsized truncation. Byte slicing (`&s[..n]`) panics
/// mid-UTF-8, and real-world mail headers/bodies are full of multi-byte chars.
pub(crate) fn ellipsize(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...", &s[..idx]),
        None => s.to_string(),
    }
}

/// Display form of the From address: "Name <addr>", falling back through the
/// parts. mail-parser has already decoded any RFC 2047 encoded-words.
pub(crate) fn format_from(msg: &mail_parser::Message) -> String {
    if let Some(addr) = msg.from().and_then(|a| a.first()) {
        match (addr.name(), addr.address()) {
            (Some(n), Some(a)) => format!("{} <{}>", n, a),
            (None, Some(a)) => a.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => "Unknown".to_string(),
        }
    } else {
        "Unknown".to_string()
    }
}

/// Display form of the To address; sent-folder rows lead with this. Falls
/// back to the account's own address (inbox mail is addressed to us anyway).
pub(crate) fn format_to(msg: &mail_parser::Message, fallback: &str) -> String {
    if let Some(addr) = msg.to().and_then(|a| a.first()) {
        match (addr.name(), addr.address()) {
            (Some(n), Some(a)) => format!("{} <{}>", n, a),
            (None, Some(a)) => a.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => fallback.to_string(),
        }
    } else {
        fallback.to_string()
    }
}

/// "Jul 23 10:50" from the parsed Date header (raw header strings previously
/// showed as e.g. "Wed, 23 Jul 2026 10:50:12 +0200 (CEST)").
pub(crate) fn format_date(dt: Option<&mail_parser::DateTime>) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match dt {
        Some(d) => {
            let month = MONTHS
                .get((d.month as usize).wrapping_sub(1))
                .copied()
                .unwrap_or("?");
            format!("{} {} {:02}:{:02}", month, d.day, d.hour, d.minute)
        }
        None => "Unknown".to_string(),
    }
}

/// Readable body text: the first text part (mail-parser decodes transfer
/// encoding + charset, and converts an HTML-only message to text), trimmed
/// and capped for the 1-message display model.
pub(crate) fn extract_body(msg: &mail_parser::Message) -> String {
    let mut body = msg
        .body_text(0)
        .map(|t| t.to_string())
        .or_else(|| msg.body_html(0).map(|h| strip_html(&h)))
        .unwrap_or_default();
    body = body.trim_end().to_string();
    if let Some((idx, _)) = body.char_indices().nth(1200) {
        body.truncate(idx);
        body.push_str("...");
    }
    body
}

/// Last-resort HTML-to-text: drop tags, decode the common entities, collapse
/// blank-line runs. Only reached if mail-parser yields no text conversion.
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line.trim_end());
        collapsed.push('\n');
    }
    collapsed
}

/// What a `mailto:` argv prefills into the compose dialog.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MailtoPrefill {
    pub(crate) to: String,
    pub(crate) cc: String,
    pub(crate) bcc: String,
    pub(crate) subject: String,
    pub(crate) body: String,
}

/// Parse a `mailto:` URL (RFC 6068) into compose-dialog prefills.
///
/// Recipients come from the path *and* any `to` query key; `cc` and `bcc`
/// keep their own header fields — the compose dialog has all three, and bcc
/// folded anywhere visible would defeat what bcc is for. Header names are
/// case-insensitive; unknown ones are ignored per spec.
pub(crate) fn parse_mailto(arg: &str) -> Option<MailtoPrefill> {
    let rest = arg.strip_prefix("mailto:").or_else(|| arg.strip_prefix("MAILTO:"))?;
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    fn addr_list(raw: &str) -> Vec<String> {
        percent_decode(raw)
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect()
    }
    let mut to = addr_list(path);
    let mut cc = Vec::new();
    let mut bcc = Vec::new();
    let mut prefill = MailtoPrefill::default();
    if let Some(q) = query {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k.to_ascii_lowercase().as_str() {
                "to" => to.extend(addr_list(v)),
                "cc" => cc.extend(addr_list(v)),
                "bcc" => bcc.extend(addr_list(v)),
                "subject" => prefill.subject = percent_decode(v),
                "body" => prefill.body = percent_decode(v),
                _ => {}
            }
        }
    }
    prefill.to = to.join(", ");
    prefill.cc = cc.join(", ");
    prefill.bcc = bcc.join(", ");
    Some(prefill)
}
