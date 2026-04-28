//! HTML 清洗：移除脚本/事件/远程资源；把 `cid:xxx` 替换成内嵌 data: URL。

use std::{borrow::Cow, collections::HashMap};

use ammonia::Builder;

use crate::InlinePart;

pub fn clean_email_html(raw_html: &str, inline: &HashMap<String, InlinePart>) -> String {
    // 先替换 cid，再交给 ammonia；否则 `cid:` 会被 URL scheme 过滤提前移除。
    let with_inline = replace_cids(raw_html, inline);

    // 先做 ammonia 清洗：允许常见排版标签与内联样式，禁止脚本、事件、远程跟踪图
    let mut builder = Builder::new();
    builder
        .add_url_schemes(&["data"])
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
            (_, "href" | "src") if value.trim_start().to_ascii_lowercase().starts_with("data:") => {
                None
            }
            _ => Some(Cow::Borrowed(value)),
        })
        .add_tag_attributes("a", &["href", "title", "target"])
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
    builder.clean(&with_inline).to_string()
}

fn replace_cids(html: &str, inline: &HashMap<String, InlinePart>) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("cid:") {
        result.push_str(&rest[..pos]);
        let tail = &rest[pos + 4..];
        let end = tail
            .find(|c: char| c == '"' || c == '\'' || c == ' ' || c == '>' || c == ')')
            .unwrap_or(tail.len());
        let cid = &tail[..end];
        if let Some(part) = inline.get(cid) {
            result.push_str(&format!(
                "data:{};base64,{}",
                part.content_type,
                encode_base64(&part.data)
            ));
        } else {
            result.push_str("cid:");
            result.push_str(cid);
        }
        rest = &tail[end..];
    }
    result.push_str(rest);
    result
}

// 避免多加一个 crate：手写一个简 base64（足够邮件内嵌图用）
fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
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
    fn data_href_is_removed_but_data_image_src_is_kept() {
        let cleaned = clean_email_html(
            r#"<a href="data:text/html;base64,PGgxPg==">x</a><img src="data:image/png;base64,AQID">"#,
            &HashMap::new(),
        );

        assert!(cleaned.contains("<a rel=\"noopener noreferrer\">x</a>"));
        assert!(cleaned.contains(r#"<img src="data:image/png;base64,AQID">"#));
    }
}
