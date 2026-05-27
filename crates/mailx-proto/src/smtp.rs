//! SMTP 发送。使用 lettre 的 tokio1 异步客户端。
//!
//! 单个附件实际上限受内存约束：当前实现整体加载附件到内存中构建 MIME，
//! 建议单封邮件总附件 ≤ 200 MB，更大的请通过云盘链接发送。
//! （lettre 0.11 的 Body 是 Cow<'static, [u8]>，尚不支持 AsyncRead 流式。）

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use lettre::address::Address;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::presets::{AuthKind, ServerSettings};

pub struct SmtpCredentials {
    pub email: String,
    pub secret: String,
    pub auth: AuthKind,
}

pub struct SmtpSendRequest<'a> {
    pub from_name: Option<&'a str>,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub subject: &'a str,
    pub body_html: &'a str,
    pub attachments: &'a [&'a Path],
}

pub async fn send(
    preset: &ServerSettings<'_>,
    creds: &SmtpCredentials,
    req: SmtpSendRequest<'_>,
) -> Result<()> {
    // 构造信封
    let from_addr: Address = creds
        .email
        .parse()
        .context("发件人地址解析失败")?;
    let from = if let Some(name) = req.from_name {
        Mailbox::new(Some(name.to_string()), from_addr)
    } else {
        Mailbox::new(None, from_addr)
    };

    let mut builder = Message::builder().from(from.clone());
    for t in req.to {
        let m: Mailbox = t.parse().with_context(|| format!("收件人 {t} 无效"))?;
        builder = builder.to(m);
    }
    for c in req.cc {
        let m: Mailbox = c.parse().with_context(|| format!("抄送 {c} 无效"))?;
        builder = builder.cc(m);
    }
    builder = builder.subject(req.subject);

    // 正文：HTML + plain fallback。plain 需要保留换行，否则纯文本客户端会把正文挤成一行。
    let html_part = SinglePart::builder()
        .header(ContentType::TEXT_HTML)
        .body(req.body_html.to_string());
    let plain_part = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(html_to_plain_text(req.body_html));
    let alt = MultiPart::alternative()
        .singlepart(plain_part)
        .singlepart(html_part);

    let email = if req.attachments.is_empty() {
        builder.multipart(alt).context("构建邮件失败")?
    } else {
        let mut mixed = MultiPart::mixed().multipart(alt);
        for path in req.attachments {
            let data = tokio::fs::read(path)
                .await
                .with_context(|| format!("读取附件 {} 失败", path.display()))?;
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment.bin".into());
            let ct = guess_mime(path);
            mixed = mixed.singlepart(
                Attachment::new(filename).body(data, ct.parse().unwrap_or(ContentType::parse("application/octet-stream").unwrap())),
            );
        }
        builder.multipart(mixed).context("构建多部分邮件失败")?
    };

    // SMTP 客户端：465 走 implicit TLS（smtps），587 走 STARTTLS
    let mut transport_builder = if preset.smtp_port == 587 {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(preset.smtp_host.as_ref())?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(preset.smtp_host.as_ref())?
    }
    .port(preset.smtp_port);

    transport_builder = match creds.auth {
        AuthKind::AppPassword => transport_builder
            .credentials(Credentials::new(creds.email.clone(), creds.secret.clone()))
            .authentication(vec![Mechanism::Plain, Mechanism::Login]),
        AuthKind::OAuth2 => transport_builder
            .credentials(Credentials::new(creds.email.clone(), creds.secret.clone()))
            .authentication(vec![Mechanism::Xoauth2]),
    };

    let transport = transport_builder.build();
    transport
        .send(email)
        .await
        .map_err(|e| anyhow!("SMTP 发送失败: {e}"))?;
    Ok(())
}

fn guess_mime(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "zip" => "application/zip",
        "txt" | "md" | "log" => "text/plain",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn html_to_plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match c {
            '<' if !in_tag => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                if is_plain_break_tag(&tag) {
                    trim_trailing_spaces(&mut out);
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                } else if is_plain_cell_tag(&tag)
                    && !out.ends_with([' ', '\n'])
                    && !out.is_empty()
                {
                    out.push(' ');
                }
                in_tag = false;
            }
            _ if in_tag => tag.push(c),
            _ if out.ends_with('\n') && (c == '\n' || c == '\r') => {}
            _ => out.push(c),
        }
    }
    decode_basic_html_entities(&out).trim().to_string()
}

fn is_plain_break_tag(tag: &str) -> bool {
    matches!(
        tag_name(tag).as_str(),
        "br" | "/p" | "/div" | "/li" | "/tr" | "/h1" | "/h2" | "/h3" | "/h4" | "/h5" | "/h6"
    )
}

fn is_plain_cell_tag(tag: &str) -> bool {
    matches!(tag_name(tag).as_str(), "td" | "th" | "/td" | "/th")
}

fn tag_name(tag: &str) -> String {
    let s = tag.trim_start();
    let (prefix, body) = if let Some(rest) = s.strip_prefix('/') {
        ("/", rest)
    } else {
        ("", s)
    };
    let name = body
        .trim_start()
        .split(|c: char| c.is_ascii_whitespace() || c == '/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    format!("{prefix}{name}")
}

fn trim_trailing_spaces(s: &mut String) {
    while s.ends_with(' ') || s.ends_with('\t') {
        s.pop();
    }
}

fn decode_basic_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + 1..];
        let Some(end) = tail.find(';') else {
            out.push('&');
            rest = tail;
            continue;
        };
        let entity = &tail[..end];
        match decode_entity(entity) {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => decode_numeric_entity(entity),
    }
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let n = if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else if let Some(dec) = entity.strip_prefix('#') {
        dec.parse::<u32>().ok()?
    } else {
        return None;
    };
    char::from_u32(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_fallback_preserves_breaks_and_decodes_entities() {
        assert_eq!(
            html_to_plain_text("Hello &lt;mail&gt;<br>\nA&amp;B"),
            "Hello <mail>\nA&B"
        );
    }

    #[test]
    fn plain_text_fallback_keeps_block_and_cell_boundaries() {
        assert_eq!(
            html_to_plain_text("<p>One</p><p>Two</p><table><tr><td>A</td><td>B</td></tr></table>"),
            "One\nTwo\nA B"
        );
    }
}
