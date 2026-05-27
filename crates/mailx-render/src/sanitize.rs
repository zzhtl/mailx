//! HTML 清洗：移除脚本/事件/远程资源；把 `cid:xxx` 替换成内嵌 data: URL。

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use ammonia::Builder;

use crate::InlinePart;

pub fn clean_email_html(raw_html: &str, inline: &HashMap<String, InlinePart>) -> String {
    // 先替换 cid，再交给 ammonia；否则 `cid:` 会被 URL scheme 过滤提前移除。
    let with_inline = replace_cids(raw_html, inline);

    // 先做 ammonia 清洗：允许常见排版标签与内联样式，禁止脚本、事件、远程跟踪图
    let mut builder = Builder::new();
    builder
        .add_url_schemes(&["data"])
        .filter_style_properties(HashSet::from([
            "background-color",
            "color",
            "font-size",
            "font-style",
            "font-weight",
            "text-align",
            "text-decoration",
            "text-decoration-line",
        ]))
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("img", "src") if value.trim_start().to_ascii_lowercase().starts_with("data:") => {
                if value
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("data:image/")
                {
                    Some(Cow::Borrowed(value))
                } else {
                    None
                }
            }
            ("img", "src") => None,
            (_, "href" | "src") if value.trim_start().to_ascii_lowercase().starts_with("data:") => {
                None
            }
            _ => Some(Cow::Borrowed(value)),
        })
        .add_tags(&["font"])
        .add_tag_attributes("a", &["href", "title", "target"])
        .add_tag_attributes("font", &["color", "size", "face", "style"])
        .add_tag_attributes("img", &["src", "alt", "width", "height", "style", "align"])
        .add_tag_attributes(
            "table",
            &[
                "style",
                "cellspacing",
                "cellpadding",
                "border",
                "bgcolor",
                "align",
                "width",
            ],
        )
        .add_tag_attributes("tr", &["style", "bgcolor", "align"])
        .add_tag_attributes(
            "td",
            &[
                "style", "colspan", "rowspan", "bgcolor", "align", "valign", "width", "height",
            ],
        )
        .add_tag_attributes(
            "th",
            &[
                "style", "colspan", "rowspan", "bgcolor", "align", "valign", "width", "height",
            ],
        )
        // 块级标签普遍允许 align/bgcolor，邮件常见布局属性。
        .add_generic_attributes(&["style", "align", "bgcolor"]);
    drop_img_tags_without_src(&builder.clean(&with_inline).to_string())
}

fn replace_cids(html: &str, inline: &HashMap<String, InlinePart>) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = find_cid_scheme(rest) {
        result.push_str(&rest[..pos]);
        let tail = &rest[pos + 4..];
        let end = tail
            .find(['"', '\'', ' ', '>', ')'])
            .unwrap_or(tail.len());
        let cid = &tail[..end];
        let normalized = normalize_cid(cid);
        if let Some(part) = inline.get(cid).or_else(|| inline.get(normalized.as_str())) {
            result.push_str(&format!(
                "data:{};base64,{}",
                part.content_type,
                encode_base64(&part.data)
            ));
        } else {
            result.push_str(&rest[pos..pos + 4 + end]);
        }
        rest = &tail[end..];
    }
    result.push_str(rest);
    result
}

fn find_cid_scheme(s: &str) -> Option<usize> {
    s.as_bytes()
        .windows(4)
        .position(|w| w.eq_ignore_ascii_case(b"cid:"))
}

fn normalize_cid(cid: &str) -> String {
    percent_decode(cid)
        .trim_matches(|c| c == '<' || c == '>')
        .to_string()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn drop_img_tags_without_src(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = find_img_tag(rest) {
        out.push_str(&rest[..pos]);
        let Some(end) = rest[pos..].find('>') else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let tag = &rest[pos..pos + end + 1];
        if tag_has_attr(tag, "src") {
            out.push_str(tag);
        }
        rest = &rest[pos + end + 1..];
    }
    out.push_str(rest);
    out
}

fn find_img_tag(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if bytes[i] == b'<'
            && bytes[i + 1].eq_ignore_ascii_case(&b'i')
            && bytes[i + 2].eq_ignore_ascii_case(&b'm')
            && bytes[i + 3].eq_ignore_ascii_case(&b'g')
            && bytes
                .get(i + 4)
                .map(|b| b.is_ascii_whitespace() || *b == b'/' || *b == b'>')
                .unwrap_or(true)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn tag_has_attr(tag: &str, attr: &str) -> bool {
    let mut body = tag.trim_start_matches('<').trim_end_matches('>').trim();
    body = body.trim_end_matches('/').trim();
    let (_, attrs) = split_tag_name(body);
    attrs.split_ascii_whitespace().any(|part| {
        part.trim_start()
            .to_ascii_lowercase()
            .starts_with(&format!("{attr}="))
    })
}

fn split_tag_name(body: &str) -> (&str, &str) {
    match body.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&body[..i], body[i..].trim()),
        None => (body, ""),
    }
}

// 避免多加一个 crate：手写一个简 base64（足够邮件内嵌图用）
fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_image_survives_as_data_uri() {
        let mut inline = HashMap::new();
        inline.insert(
            "logo".to_string(),
            InlinePart {
                content_type: "image/png".to_string(),
                data: vec![1, 2, 3],
            },
        );

        let cleaned = clean_email_html(r#"<p><img src="cid:logo" alt="logo"></p>"#, &inline);

        assert!(cleaned.contains(r#"<img src="data:image/png;base64,AQID" alt="logo">"#));
    }

    #[test]
    fn cid_lookup_is_case_insensitive_for_scheme_and_decodes_url_escaped_id() {
        let mut inline = HashMap::new();
        inline.insert(
            "logo@example.org".to_string(),
            InlinePart {
                content_type: "image/png".to_string(),
                data: vec![1, 2, 3],
            },
        );

        let cleaned = clean_email_html(
            r#"<p><img src="CID:logo%40example.org"></p>"#,
            &inline,
        );

        assert!(cleaned.contains(r#"<img src="data:image/png;base64,AQID">"#));
    }

    #[test]
    fn data_href_is_removed_but_data_image_src_is_kept() {
        let cleaned = clean_email_html(
            r#"<a href="data:text/html;base64,PGgxPg==">x</a><img src="data:image/png;base64,AQID">"#,
            &HashMap::new(),
        );

        assert!(cleaned.contains("<a rel=\"noopener noreferrer\">x</a>"));
        assert!(cleaned.contains(r#"<img src="data:image/png;base64,AQID">"#));
    }

    #[test]
    fn remote_image_src_is_removed() {
        let cleaned = clean_email_html(
            r#"<p><img src="https://track.example/pixel.png" alt="pixel"></p>"#,
            &HashMap::new(),
        );

        assert!(!cleaned.contains("https://track.example"));
        assert!(!cleaned.contains("src="));
        assert!(!cleaned.contains("<img"));
    }

    #[test]
    fn legacy_font_tag_survives_for_rich_rendering() {
        let cleaned = clean_email_html(
            r##"<font color="#cc0000" size="4">警告</font>"##,
            &HashMap::new(),
        );

        assert!(cleaned.contains(r##"<font color="#cc0000" size="4">警告</font>"##));
    }

    #[test]
    fn style_filter_keeps_text_styles_and_drops_remote_css() {
        let cleaned = clean_email_html(
            r#"<p style="color:red;background-image:url(https://t.example/pixel.png);font-weight:bold">x</p>"#,
            &HashMap::new(),
        );

        assert!(cleaned.contains(r#"style="color:red;font-weight:bold""#));
        assert!(!cleaned.contains("background-image"));
        assert!(!cleaned.contains("https://t.example"));
    }
}
