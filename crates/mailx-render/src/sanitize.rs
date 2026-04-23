//! HTML 清洗：移除脚本/事件/远程资源；把 `cid:xxx` 替换成内嵌 data: URL。

use std::collections::HashMap;

use ammonia::Builder;

use crate::InlinePart;

pub fn clean_email_html(raw_html: &str, inline: &HashMap<String, InlinePart>) -> String {
    // 先做 ammonia 清洗：允许常见排版标签与内联样式，禁止脚本、事件、远程跟踪图
    let mut builder = Builder::new();
    builder
        .add_tag_attributes("a", &["href", "title", "target"])
        .add_tag_attributes("img", &["src", "alt", "width", "height", "style"])
        .add_tag_attributes("span", &["style"])
        .add_tag_attributes("p", &["style"])
        .add_tag_attributes("div", &["style"])
        .add_tag_attributes("td", &["style", "colspan", "rowspan"])
        .add_tag_attributes("table", &["style", "cellspacing", "cellpadding", "border"])
        .add_generic_attributes(&["style"]);
    let cleaned = builder.clean(raw_html).to_string();

    // 将 cid:xxx 换成 data: URL（小图内嵌最稳妥；大图后续可改为 wry 自定义 scheme）
    replace_cids(&cleaned, inline)
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
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

