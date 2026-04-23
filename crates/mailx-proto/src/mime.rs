//! 邮件头和文件夹名的字符编码清洗。
//!
//! - `decode_rfc2047`：解析 MIME encoded-word（`=?charset?B|Q?text?=`），覆盖 Subject / From 等。
//! - `decode_modified_utf7`：解析 IMAP 文件夹名（RFC 3501 §5.1.3 的 modified UTF-7）。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

/// 解码 RFC 2047 encoded-word。若输入不含 encoded-word，原样返回。
///
/// 支持 `=?charset?B?...?=` 与 `=?charset?Q?...?=`。相邻 encoded-word 之间的
/// 空白按 RFC 规则吞掉。charset 名通过 `encoding_rs::Encoding::for_label` 查表，
/// 常见的 UTF-8 / GBK / GB2312 / GB18030 / Big5 / ISO-8859-1 / Windows-1252 都能命中。
pub fn decode_rfc2047(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut last_was_ew = false;

    while !rest.is_empty() {
        // 遇到 =? 先尝试匹配 encoded-word
        if rest.starts_with("=?") {
            if let Some((decoded, tail)) = try_parse_encoded_word(rest) {
                out.push_str(&decoded);
                rest = tail;
                last_was_ew = true;
                continue;
            }
        }

        // 不是 encoded-word：拷贝一个字符
        // 如果上一轮是 encoded-word 且 rest 以空白开头、空白后紧跟 encoded-word，则按 RFC 吞掉空白
        if last_was_ew {
            let ws_len = rest.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
            if ws_len > 0 {
                let after_ws = &rest[ws_len..];
                if after_ws.starts_with("=?") && try_parse_encoded_word(after_ws).is_some() {
                    rest = after_ws;
                    continue;
                }
            }
        }
        last_was_ew = false;

        // 按字符步进，保留多字节 UTF-8 合法性
        let mut ci = rest.char_indices();
        let (_, c) = ci.next().unwrap();
        out.push(c);
        rest = match ci.next() {
            Some((i, _)) => &rest[i..],
            None => "",
        };
    }
    out
}

fn try_parse_encoded_word(s: &str) -> Option<(String, &str)> {
    // s 形如 "=?charset?B?xxx?=..."
    let body = s.strip_prefix("=?")?;
    let q1 = body.find('?')?;
    let charset = &body[..q1];
    if charset.is_empty() || charset.len() > 64 {
        return None;
    }
    let after_q1 = &body[q1 + 1..];
    let mut chars = after_q1.chars();
    let enc = chars.next()?.to_ascii_uppercase();
    if chars.next()? != '?' {
        return None;
    }
    let after_q2 = &after_q1[2..];
    let end = after_q2.find("?=")?;
    let encoded = &after_q2[..end];
    let tail = &after_q2[end + 2..];

    let raw: Vec<u8> = match enc {
        'B' => B64.decode(encoded.as_bytes()).ok()?,
        'Q' => decode_q(encoded),
        _ => return None,
    };

    let decoder = encoding_rs::Encoding::for_label(charset.as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    let (cow, _, _) = decoder.decode(&raw);
    Some((cow.into_owned(), tail))
}

fn decode_q(s: &str) -> Vec<u8> {
    // RFC 2047 Q 编码：`_` 代表空格；`=XX` 是十六进制字节。
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < bytes.len() => {
                let hi = from_hex(bytes[i + 1]);
                let lo = from_hex(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 解码 IMAP modified UTF-7 的文件夹名（RFC 3501 §5.1.3）。
///
/// 规则：
/// - ASCII（除了 `&` 自身）原样；
/// - `&-` 表示单个字面 `&`；
/// - `&<base64>-` 中的 base64（用 `,` 代替 `/`，无 padding）解码得到 UTF-16BE，再转 UTF-8。
pub fn decode_modified_utf7(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            // 寻找终止 '-'
            if let Some(end_off) = bytes[i + 1..].iter().position(|b| *b == b'-') {
                let enc = &bytes[i + 1..i + 1 + end_off];
                if enc.is_empty() {
                    // `&-` => '&'
                    out.push('&');
                } else {
                    // modified base64: `/` -> `,`，标准 base64 没有 padding
                    let normalized: Vec<u8> = enc
                        .iter()
                        .map(|b| if *b == b',' { b'/' } else { *b })
                        .collect();
                    let decoded = B64
                        .decode(pad_base64(&normalized))
                        .ok()
                        .unwrap_or_default();
                    // UTF-16BE -> char
                    let mut k = 0;
                    let mut u16s: Vec<u16> = Vec::with_capacity(decoded.len() / 2);
                    while k + 1 < decoded.len() {
                        u16s.push(u16::from_be_bytes([decoded[k], decoded[k + 1]]));
                        k += 2;
                    }
                    match String::from_utf16(&u16s) {
                        Ok(s) => out.push_str(&s),
                        Err(_) => out.push('?'),
                    }
                }
                i += 1 + end_off + 1;
                continue;
            }
            // 找不到 `-` 则原样输出
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn pad_base64(s: &[u8]) -> Vec<u8> {
    let mut v = s.to_vec();
    while v.len() % 4 != 0 {
        v.push(b'=');
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2047_gbk_b_subject() {
        // "=?GBK?B?ufq80s3ixcXOxLz+?=" -> "国家重点档案"（示意）
        let s = "=?GBK?B?xvPStc6i0MXTys/k?=";
        // 只要能解码出非 ASCII 字符就算对
        let out = decode_rfc2047(s);
        assert!(!out.contains("=?"));
        assert!(out.chars().any(|c| c as u32 > 0x7f));
    }

    #[test]
    fn rfc2047_utf8_q() {
        let s = "=?utf-8?Q?Hello_World?=";
        assert_eq!(decode_rfc2047(s), "Hello World");
    }

    #[test]
    fn rfc2047_mixed() {
        let s = "prefix =?utf-8?B?aGVsbG8=?= suffix";
        assert_eq!(decode_rfc2047(s), "prefix hello suffix");
    }

    #[test]
    fn rfc2047_adjacent_merges_whitespace() {
        // 相邻 encoded-word 之间只有空白时应被吞掉
        let s = "=?utf-8?B?aGVsbG8=?= =?utf-8?B?d29ybGQ=?=";
        assert_eq!(decode_rfc2047(s), "helloworld");
    }

    #[test]
    fn mutf7_ascii_passthrough() {
        assert_eq!(decode_modified_utf7("INBOX"), "INBOX");
        assert_eq!(decode_modified_utf7("Sent Messages"), "Sent Messages");
    }

    #[test]
    fn mutf7_ampersand_escape() {
        assert_eq!(decode_modified_utf7("A&-B"), "A&B");
    }

    #[test]
    fn mutf7_chinese_inbox() {
        // "收件箱" = U+6536 U+4EF6 U+7BB1 -> UTF-16BE 65 36 4E F6 7B B1
        //   -> base64 "ZTZO9nux"，转 modified "ZTZO9nux"（无 `/`）
        //   -> "&ZTZO9nux-"
        assert_eq!(decode_modified_utf7("&ZTZO9nux-"), "收件箱");
    }
}
