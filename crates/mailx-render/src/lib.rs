//! mailx-render —— MIME 解析 + HTML 清洗 + cid 内联图替换。

pub mod rich;
pub mod sanitize;

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use mail_parser::{
    decoders::{base64::base64_decode, quoted_printable::quoted_printable_decode},
    Encoding, MessageParser, MessagePart, MimeHeaders, PartType,
};

/// 渲染结果：安全 HTML + cid 到字节的映射。
pub struct RenderedBody {
    pub html: String,
    pub plain_fallback: String,
    pub inline_parts: HashMap<String, InlinePart>,
    pub attachments: Vec<AttachmentPart>,
}

pub struct InlinePart {
    pub content_type: String,
    pub data: Vec<u8>,
}

pub struct AttachmentPart {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// 完整渲染：解析 MIME → 选最好的正文 part（text/html 优先）→ 清洗 HTML → 收集 cid 资源与附件。
pub fn render(raw: &[u8]) -> Result<RenderedBody> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("无法解析 MIME"))?;

    let mut inline_parts: HashMap<String, InlinePart> = HashMap::new();
    let mut attachments: Vec<AttachmentPart> = Vec::new();

    for part in message.parts.iter() {
        match &part.body {
            PartType::Binary(bytes) | PartType::InlineBinary(bytes) => {
                let ct = part
                    .content_type()
                    .and_then(|c| {
                        let t = c.ctype();
                        c.subtype().map(|s| format!("{t}/{s}"))
                    })
                    .unwrap_or_else(|| "application/octet-stream".into());

                let is_inline = matches!(&part.body, PartType::InlineBinary(_))
                    || part
                        .content_type()
                        .map(|c| c.ctype().eq_ignore_ascii_case("image"))
                        .unwrap_or(false);

                if is_inline {
                    if let Some(cid) = part.content_id() {
                        inline_parts.insert(
                            cid.trim_matches(|c| c == '<' || c == '>').to_string(),
                            InlinePart { content_type: ct.clone(), data: bytes.to_vec() },
                        );
                        continue;
                    }
                }
            }
            _ => {}
        }
    }

    let html_body = first_body_part(raw, &message.parts, &message.html_body);
    let text_body = first_body_part(raw, &message.parts, &message.text_body);
    for (n, part_id) in message.attachments.iter().copied().enumerate() {
        let Some(part) = message.parts.get(part_id) else {
            continue;
        };
        if is_inline_cid_resource(part) {
            continue;
        }
        let data = attachment_bytes(raw, part);
        let filename = part
            .attachment_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("attachment-{}.bin", n + 1));
        attachments.push(AttachmentPart {
            filename,
            content_type: content_type_string(part),
            data,
        });
    }

    let plain = text_body.clone().unwrap_or_default();
    let raw_html = html_body.unwrap_or_else(|| {
        html_escape::encode_safe(&plain)
            .replace('\n', "<br/>")
            .to_string()
    });
    let html = sanitize::clean_email_html(&raw_html, &inline_parts);
    // 纯文本路径同样要 decode HTML 实体（部分发件方会把 &nbsp;/&amp; 直接塞进 text/plain）。
    let plain_fallback = clean_mail_text(&html_escape::decode_html_entities(&plain));
    Ok(RenderedBody { html, plain_fallback, inline_parts, attachments })
}

fn first_body_part(raw: &[u8], parts: &[MessagePart<'_>], ids: &[usize]) -> Option<String> {
    ids.iter().find_map(|id| match &parts.get(*id)?.body {
        PartType::Html(fallback) | PartType::Text(fallback) => {
            Some(decode_text_part(raw, &parts[*id], fallback.as_ref()))
        }
        _ => None,
    })
}

fn is_inline_cid_resource(part: &MessagePart<'_>) -> bool {
    part.content_id().is_some()
        && (matches!(&part.body, PartType::InlineBinary(_))
            || part
                .content_type()
                .map(|c| c.ctype().eq_ignore_ascii_case("image"))
                .unwrap_or(false))
}

fn content_type_string(part: &MessagePart<'_>) -> String {
    part.content_type()
        .and_then(|c| {
            let t = c.ctype();
            c.subtype().map(|s| format!("{t}/{s}"))
        })
        .unwrap_or_else(|| "application/octet-stream".into())
}

fn attachment_bytes(raw: &[u8], part: &MessagePart<'_>) -> Vec<u8> {
    match &part.body {
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.to_vec(),
        PartType::Html(fallback) | PartType::Text(fallback) => {
            decode_transfer_bytes(raw, part, fallback.as_ref().as_bytes())
        }
        _ => raw
            .get(part.raw_body_offset()..part.raw_end_offset())
            .map(|s| s.to_vec())
            .unwrap_or_default(),
    }
}

/// 从原始报文中取出 text/* 部分的字节，按 Content-Transfer-Encoding 解码后，
/// 再按 Content-Type 声明的 charset（若无则启发式尝试 UTF-8 / GB18030 / Big5 等）转 UTF-8。
///
/// 这样可以绕开 mail-parser 在 charset 缺失/误报时产生的乱码（例如 GB2312 正文被按 Latin-1 解出）。
/// `fallback` 是 mail-parser 已经解码好的字符串，仅当我们拿不到可用 raw 字节时兜底。
fn decode_text_part(raw: &[u8], part: &MessagePart<'_>, fallback: &str) -> String {
    let cte_decoded = decode_transfer_bytes(raw, part, fallback.as_bytes());
    let declared = part
        .content_type()
        .and_then(|c| c.attribute("charset"));
    clean_mail_text(&decode_with_charset(&cte_decoded, declared, fallback))
}

fn decode_transfer_bytes(raw: &[u8], part: &MessagePart<'_>, fallback: &[u8]) -> Vec<u8> {
    let start = part.raw_body_offset();
    let end = part.raw_end_offset();
    let Some(slice) = raw.get(start..end) else {
        return fallback.to_vec();
    };
    if slice.is_empty() {
        return fallback.to_vec();
    }

    match part.encoding {
        Encoding::Base64 => base64_decode(slice).unwrap_or_else(|| slice.to_vec()),
        Encoding::QuotedPrintable => quoted_printable_decode(slice).unwrap_or_else(|| slice.to_vec()),
        Encoding::None => slice.to_vec(),
    }
}

/// 在多个候选 charset 中挑"替换字符最少"的解码结果。
/// 遇到完全无误 + 无 U+FFFD 的候选立刻返回；都失败时返回 mail-parser 的 fallback。
fn decode_with_charset(bytes: &[u8], declared: Option<&str>, fallback: &str) -> String {
    let mut tried: Vec<&'static encoding_rs::Encoding> = Vec::new();
    let mut best: Option<(String, usize)> = None;

    let mut candidates: Vec<&'static encoding_rs::Encoding> = Vec::new();
    if let Some(name) = declared {
        if let Some(enc) = encoding_rs::Encoding::for_label(name.trim().as_bytes()) {
            candidates.push(enc);
        }
    }
    // 不重复的兜底顺序：UTF-8 → GB18030（覆盖 GBK/GB2312）→ Big5 → Shift_JIS → EUC-KR → Win-1252。
    for enc in [
        encoding_rs::UTF_8,
        encoding_rs::GB18030,
        encoding_rs::BIG5,
        encoding_rs::SHIFT_JIS,
        encoding_rs::EUC_KR,
        encoding_rs::WINDOWS_1252,
    ] {
        if !candidates.iter().any(|c| std::ptr::eq(*c, enc)) {
            candidates.push(enc);
        }
    }

    for enc in candidates {
        if tried.iter().any(|e| std::ptr::eq(*e, enc)) {
            continue;
        }
        tried.push(enc);
        let (cow, _, had_errors) = enc.decode(bytes);
        let s = cow.into_owned();
        let replacements = s.chars().filter(|&c| c == '\u{FFFD}').count();
        if !had_errors && replacements == 0 {
            return s;
        }
        match best {
            Some((_, prev)) if replacements >= prev => {}
            _ => best = Some((s, replacements)),
        }
    }

    best.map(|(s, _)| s).unwrap_or_else(|| fallback.to_string())
}

fn clean_mail_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => {}
            '\u{00AD}' | '\u{034F}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => {}
            '\u{2007}' | '\u{00A0}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// 轻量封装：只要正文纯文本（UI 未接入 wry 前的 fallback）。
pub fn render_to_text(raw: &[u8]) -> Result<String> {
    let body = render(raw)?;
    if !body.plain_fallback.trim().is_empty() {
        return Ok(body.plain_fallback);
    }
    // 从 sanitized HTML 里剥离标签得到纯文本
    Ok(html_to_plaintext(&body.html))
}

/// HTML → 纯文本：
/// - 跳过 `<script>` / `<style>` 的内容；
/// - 把 `<br>`、`</p>`、`</div>`、`</li>`、`</tr>`、`<hr>` 以及标题 `</h1..6>` 替换成换行；
/// - 其他标签直接剥掉；
/// - 解码 HTML 实体；折叠连续空白行。
pub fn html_to_plaintext(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    let n = bytes.len();

    while i < n {
        if bytes[i] == b'<' {
            // 跳过 script / style 的内容块
            if starts_at(lower_bytes, i, b"<script") {
                i = skip_until(lower_bytes, i, b"</script>").unwrap_or(n);
                continue;
            }
            if starts_at(lower_bytes, i, b"<style") {
                i = skip_until(lower_bytes, i, b"</style>").unwrap_or(n);
                continue;
            }
            // 定位结束 '>'
            let end = match memchr(bytes, b'>', i + 1) {
                Some(e) => e,
                None => break,
            };
            let tag = &lower_bytes[i..=end];
            if is_line_break_tag(tag) {
                // block 级边界：把前面所有尾随空白吃掉，再补一个 \n。
                // 这样 `</p><p>` 之间即使源码里夹着大量换行+缩进，输出也只保留 **一个** 换行，
                // 不会出现 "登录地点\n\n湖北省 武汉市" 的空行。
                while matches!(out.chars().last(), Some(c) if c.is_whitespace()) {
                    out.pop();
                }
                if !out.is_empty() {
                    out.push('\n');
                }
            } else if is_cell_tag(tag) {
                // <td>/<th> 之间插入空格——保持"字段 值"在同一行，
                // 整行换行交给 <tr>/<br>/<p> 等 block 级标签。
                if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
                    out.push(' ');
                }
            }
            i = end + 1;
            continue;
        }
        // 注意：`<` 是 ASCII，UTF-8 字符边界天然安全，
        // 所以扫到下一个 `<` 然后整段 &str 拷贝即可；
        // 千万不能 `out.push(bytes[i] as char)`——那样会把多字节汉字切成 Latin-1 乱码。
        let start = i;
        while i < n && bytes[i] != b'<' {
            i += 1;
        }
        let mut chunk = &html[start..i];
        // block 边界刚换过行的话，把紧跟的源码缩进（换行+空格）吃掉，
        // 避免 `<p>\n   xxx</p>` 里那个领头换行变成一个空白行。
        if out.ends_with('\n') {
            chunk = chunk.trim_start();
        }
        out.push_str(chunk);
    }

    let decoded = html_escape::decode_html_entities(&out);
    let normalized = normalize_whitespace(&decoded);
    collapse_blank_lines(&normalized)
}

fn starts_at(hay: &[u8], pos: usize, needle: &[u8]) -> bool {
    hay.len() >= pos + needle.len() && &hay[pos..pos + needle.len()] == needle
}

fn skip_until(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut i = from;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i + needle.len());
        }
        i += 1;
    }
    None
}

fn memchr(hay: &[u8], target: u8, from: usize) -> Option<usize> {
    hay.iter().enumerate().skip(from).find(|(_, b)| **b == target).map(|(i, _)| i)
}

fn is_cell_tag(tag: &[u8]) -> bool {
    let inner = &tag[1..tag.len() - 1];
    let name_end = inner
        .iter()
        .position(|b| matches!(*b, b' ' | b'/' | b'\t'))
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    let name = name.strip_prefix(b"/").unwrap_or(name);
    matches!(name, b"td" | b"th")
}

/// 把 HTML 正文转成纯文本后遗留的连续空白（空格/制表符/不断行空格）折叠成一个空格；
/// 保留换行（后续 `collapse_blank_lines` 再处理空行）。
///
/// 邮件常用 `<table><td>&nbsp;&nbsp;&nbsp;值</td>` 做排版缩进，
/// 不折叠的话正文会被顶到屏幕中间，视觉上"格式错乱"。
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == '\n' {
            // 行末去掉尾随空白
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
            prev_space = false;
            continue;
        }
        // U+00A0 NBSP、U+2028/2029、各种 CJK 全角空格都当空格处理
        let is_space = c == ' ' || c == '\t' || c == '\u{00A0}' || c == '\u{3000}';
        if is_space {
            if !prev_space && !out.ends_with('\n') && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn is_line_break_tag(tag: &[u8]) -> bool {
    // tag 含尖括号，全小写；形如 `<br>` / `<br/>` / `</p>` / `</h3>`…
    let inner = &tag[1..tag.len() - 1];
    let name_end = inner
        .iter()
        .position(|b| matches!(*b, b' ' | b'/' | b'\t'))
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    let name = name.strip_prefix(b"/").unwrap_or(name);
    matches!(
        name,
        b"br" | b"p" | b"div" | b"li" | b"tr" | b"hr" | b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6" | b"blockquote" | b"pre"
    )
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.split('\n') {
        let trimmed = line.trim_end_matches(['\r', ' ', '\t']);
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    // 去掉首尾空白
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_attachment_is_not_used_as_body() {
        let raw = b"From: a@example.org\r\n\
To: b@example.org\r\n\
Subject: test\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Disposition: attachment; filename=\"note.txt\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Attachment=20text=20that=20should=20not=20be=20body.\r\n\
--b\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Actual body.\r\n\
--b--\r\n";

        let body = render(raw).expect("render");

        assert_eq!(body.plain_fallback.trim(), "Actual body.");
        assert_eq!(body.attachments.len(), 1);
        assert_eq!(body.attachments[0].filename, "note.txt");
        assert_eq!(
            String::from_utf8_lossy(&body.attachments[0].data).trim(),
            "Attachment text that should not be body."
        );
    }

    #[test]
    fn html_attachment_is_kept_out_of_body() {
        let raw = b"From: a@example.org\r\n\
To: b@example.org\r\n\
Subject: test\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Disposition: attachment; filename=\"page.html\"\r\n\
\r\n\
<h1>Attachment</h1>\r\n\
--b\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Visible body.\r\n\
--b--\r\n";

        let body = render(raw).expect("render");

        assert_eq!(body.plain_fallback.trim(), "Visible body.");
        assert!(!body.html.contains("Attachment"));
        assert_eq!(body.attachments.len(), 1);
        assert_eq!(body.attachments[0].filename, "page.html");
        assert_eq!(body.attachments[0].content_type, "text/html");
    }

    #[test]
    fn cid_inline_image_is_not_listed_as_attachment() {
        let raw = b"From: a@example.org\r\n\
To: b@example.org\r\n\
Subject: test\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/related; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>Hello<img src=\"cid:logo\"></p>\r\n\
--b\r\n\
Content-Type: image/png\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-ID: <logo>\r\n\
\r\n\
AQID\r\n\
--b--\r\n";

        let body = render(raw).expect("render");

        assert!(body.html.contains("data:image/png;base64,AQID"));
        assert!(body.inline_parts.contains_key("logo"));
        assert!(body.attachments.is_empty());
    }

    #[test]
    fn invisible_mail_formatting_chars_are_removed_from_text_body() {
        let raw = "From: a@example.org\r\n\
To: b@example.org\r\n\
Subject: test\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Hidden =E2=80=8C =CD=8F soft=C2=AD hyphen=C2=A0space\r\n";

        let body = render(raw.as_bytes()).expect("render");

        assert_eq!(
            body.plain_fallback.trim(),
            "Hidden   soft hyphen space"
        );
        assert!(!body.plain_fallback.contains('\u{200C}'));
        assert!(!body.plain_fallback.contains('\u{034F}'));
        assert!(!body.plain_fallback.contains('\u{00AD}'));
        assert!(!body.plain_fallback.contains('\u{00A0}'));
    }
}
