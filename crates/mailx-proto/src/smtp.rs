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

use crate::presets::{AuthKind, ProviderPreset};

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
    preset: &ProviderPreset,
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

    // 正文：HTML（plain 自动生成 - 简单剥标签）
    let html_part = SinglePart::builder()
        .header(ContentType::TEXT_HTML)
        .body(req.body_html.to_string());
    let plain_part = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(strip_tags(req.body_html));
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
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(preset.smtp_host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(preset.smtp_host)?
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

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}
