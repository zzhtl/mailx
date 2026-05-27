//! HTML → 富文本块：把 sanitize 之后的邮件 HTML 解析成一组语义块（段落 / 标题 / 分割线 / 列表项 /
//! 引用 / 表格 / 图片），每个块里是一串带内联样式的 `Span`，供 UI 层用 egui 直接画出来。
//!
//! 这是一个"面向邮件"的极简解析器：
//! - 邮件 HTML 已经经过 ammonia 清洗，标签集合相对收敛；
//! - 我们关心的只是"让粗体/标题/颜色/超链接/基本字号/对齐/背景/表格"能在原生 UI 上显示出来；
//! - 表格按旧版线性文本路径降级，避免邮件里常见的布局 table 把原有正文排版改坏。

/// 一段文本的视觉样式。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    /// RGB 颜色（可选）。
    pub color: Option<[u8; 3]>,
    /// 相对默认正文的字号倍率，1.0 表示默认。
    pub size: f32,
    /// 超链接 URL（存在则该 span 为超链接）。
    pub href: Option<String>,
}

impl SpanStyle {
    pub fn base() -> Self {
        Self {
            size: 1.0,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    /// 该 span 之后是否有显式换行（`<br>`）。
    pub br_after: bool,
}

/// 块级对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Justify,
}

/// 内嵌图片（`<img>`）。`src` 可能是 `data:image/...;base64,...` 或远程 URL。
#[derive(Clone, Debug)]
pub struct ImageRef {
    pub src: String,
    pub alt: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// 表格单元格。`blocks` 是 cell 内部的递归解析结果。
#[derive(Clone, Debug)]
pub struct TableCell {
    pub blocks: Vec<Block>,
    pub colspan: u32,
    pub rowspan: u32,
    pub bg: Option<[u8; 3]>,
    pub align: Align,
    pub is_header: bool,
}

/// 块的语义类型。
#[derive(Clone, Debug)]
pub enum BlockKind {
    Paragraph(Vec<Span>),
    Heading(u8, Vec<Span>),
    Quote(Vec<Span>),
    ListItem(Vec<Span>),
    Rule,
    Image(ImageRef),
    Table(Vec<Vec<TableCell>>),
}

/// 语义块：内容 + 块级对齐 / 背景。
#[derive(Clone, Debug)]
pub struct Block {
    pub kind: BlockKind,
    pub align: Align,
    pub bg: Option<[u8; 3]>,
}

impl Block {
    pub fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            align: Align::Start,
            bg: None,
        }
    }

    pub fn spans(&self) -> Option<&[Span]> {
        match &self.kind {
            BlockKind::Paragraph(s)
            | BlockKind::Heading(_, s)
            | BlockKind::Quote(s)
            | BlockKind::ListItem(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.kind {
            BlockKind::Rule | BlockKind::Image(_) => false,
            BlockKind::Table(rows) => rows
                .iter()
                .all(|r| r.iter().all(|c| c.blocks.iter().all(|b| b.is_empty()))),
            _ => self
                .spans()
                .map(|s| s.iter().all(|sp| sp.text.trim().is_empty() && !sp.br_after))
                .unwrap_or(false),
        }
    }
}

/// 入口：把 HTML 解析成块列表。
pub fn parse(html: &str) -> Vec<Block> {
    let mut parser = Parser::new(html);
    parser.run();
    parser.finish()
}

// --- 解析内部 ---

#[derive(Clone, Debug)]
enum Current {
    Paragraph(Vec<Span>),
    Heading(u8, Vec<Span>),
    Quote(Vec<Span>),
    ListItem(Vec<Span>),
}

impl Current {
    fn spans_mut(&mut self) -> &mut Vec<Span> {
        match self {
            Current::Paragraph(s)
            | Current::Heading(_, s)
            | Current::Quote(s)
            | Current::ListItem(s) => s,
        }
    }

    fn into_kind(self) -> BlockKind {
        match self {
            Current::Paragraph(s) => BlockKind::Paragraph(s),
            Current::Heading(l, s) => BlockKind::Heading(l, s),
            Current::Quote(s) => BlockKind::Quote(s),
            Current::ListItem(s) => BlockKind::ListItem(s),
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    src: &'a str,
    pos: usize,
    style_stack: Vec<SpanStyle>,
    /// 块级属性栈：随 `<p>/<div>/<td>` 等 push，close 时 pop。栈顶决定下一段 flush 时块的 align/bg。
    block_attr_stack: Vec<BlockAttr>,
    blocks: Vec<Block>,
    current: Current,
    /// 是否处于引用块（blockquote）内，影响新段落的默认块类型。
    quote_depth: u32,
    /// 是否处于 pre / code 块，用于保留原始空白（简化处理，暂不展开）。
    pre_depth: u32,
}

#[derive(Clone, Debug, Default)]
struct BlockAttr {
    align: Option<Align>,
    bg: Option<[u8; 3]>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            bytes: src.as_bytes(),
            src,
            pos: 0,
            style_stack: vec![SpanStyle::base()],
            block_attr_stack: Vec::new(),
            blocks: Vec::new(),
            current: Current::Paragraph(Vec::new()),
            quote_depth: 0,
            pre_depth: 0,
        }
    }

    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'<' {
                self.consume_tag();
            } else {
                self.consume_text();
            }
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_current();
        // 去掉首尾空块，避免出现前后大段空白
        while self.blocks.first().map(|b| b.is_empty()).unwrap_or(false) {
            self.blocks.remove(0);
        }
        while self.blocks.last().map(|b| b.is_empty()).unwrap_or(false) {
            self.blocks.pop();
        }
        self.blocks
    }

    fn consume_tag(&mut self) {
        // 注释 / DOCTYPE / CDATA
        if self.starts_with_ci(b"<!--") {
            if let Some(end) = find_bytes(self.bytes, self.pos + 4, b"-->") {
                self.pos = end + 3;
            } else {
                self.pos = self.bytes.len();
            }
            return;
        }
        if self.starts_with_ci(b"<!") {
            if let Some(end) = find_byte(self.bytes, self.pos + 2, b'>') {
                self.pos = end + 1;
            } else {
                self.pos = self.bytes.len();
            }
            return;
        }

        // <script> / <style> / <title> / <head>：直接跳过整段内容
        for skip in [
            b"script".as_slice(),
            b"style".as_slice(),
            b"title".as_slice(),
            b"head".as_slice(),
        ] {
            if self.starts_with_ci_name(skip) {
                self.skip_block_tag(skip);
                return;
            }
        }

        // 普通标签
        let Some(end) = find_byte(self.bytes, self.pos + 1, b'>') else {
            self.pos = self.bytes.len();
            return;
        };
        let inner = &self.src[self.pos + 1..end];
        self.pos = end + 1;

        let (closing, body) = match inner.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, inner),
        };
        let self_closing = body.trim_end().ends_with('/');
        let body = body.trim_end().trim_end_matches('/');

        let (name, attr_str) = split_name_attrs(body);
        let name_lower = name.to_ascii_lowercase();

        if closing {
            self.on_close(&name_lower);
        } else {
            let attrs = parse_attrs(attr_str);
            self.on_open(&name_lower, &attrs, self_closing);
        }
    }

    fn skip_block_tag(&mut self, name: &[u8]) {
        let close_needle = {
            let mut v = Vec::with_capacity(name.len() + 3);
            v.push(b'<');
            v.push(b'/');
            v.extend_from_slice(name);
            v
        };
        let after_open = match find_byte(self.bytes, self.pos + 1, b'>') {
            Some(i) => i + 1,
            None => self.bytes.len(),
        };
        let mut i = after_open;
        let lower = self.src.to_ascii_lowercase();
        let lower_bytes = lower.as_bytes();
        while i + close_needle.len() <= lower_bytes.len() {
            if &lower_bytes[i..i + close_needle.len()] == close_needle.as_slice() {
                // 跳到 '>'
                if let Some(gt) = find_byte(self.bytes, i, b'>') {
                    self.pos = gt + 1;
                    return;
                }
            }
            i += 1;
        }
        self.pos = self.bytes.len();
    }

    fn consume_text(&mut self) {
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'<' {
            self.pos += 1;
        }
        let raw = &self.src[start..self.pos];
        if raw.is_empty() {
            return;
        }
        let decoded = html_escape::decode_html_entities(raw);
        if self.pre_depth > 0 {
            self.push_pre_text(&decoded);
            return;
        }
        let collapsed = collapse_ws(&decoded);
        if collapsed.is_empty() {
            return;
        }
        self.push_text(&collapsed);
    }

    fn push_pre_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for chunk in normalized.split_inclusive('\n') {
            let has_break = chunk.ends_with('\n');
            let text = chunk.trim_end_matches('\n').replace('\t', "    ");
            if !text.is_empty() {
                self.push_raw_text(&text);
            }
            if has_break {
                self.mark_break();
            }
        }
    }

    fn push_raw_text(&mut self, text: &str) {
        let style = self.current_style();
        let spans = self.current.spans_mut();
        if let Some(last) = spans.last_mut() {
            if !last.br_after && last.style == style {
                last.text.push_str(text);
                return;
            }
        }
        spans.push(Span {
            text: text.to_string(),
            style,
            br_after: false,
        });
    }

    fn push_text(&mut self, text: &str) {
        let style = self.current_style();
        let spans = self.current.spans_mut();

        // 避免行首出现孤立空格
        let mut s = text.to_string();
        if s.starts_with(' ') {
            let trim_leading = match spans.last() {
                None => true,
                Some(last) => last.br_after || last.text.ends_with(' ') || last.text.is_empty(),
            };
            if trim_leading {
                s = s.trim_start().to_string();
            }
        }
        if s.is_empty() {
            return;
        }

        if let Some(last) = spans.last_mut() {
            if !last.br_after && last.style == style {
                last.text.push_str(&s);
                return;
            }
        }
        spans.push(Span {
            text: s,
            style,
            br_after: false,
        });
    }

    fn current_style(&self) -> SpanStyle {
        self.style_stack
            .last()
            .cloned()
            .unwrap_or_else(SpanStyle::base)
    }

    fn push_style(&mut self, style: SpanStyle) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn modified(&self, f: impl FnOnce(&mut SpanStyle)) -> SpanStyle {
        let mut s = self.current_style();
        f(&mut s);
        s
    }

    fn current_block_attr(&self) -> BlockAttr {
        // 取栈中最近一个有效 align/bg。栈空时返回默认。
        let mut out = BlockAttr::default();
        for layer in self.block_attr_stack.iter().rev() {
            if out.align.is_none() && layer.align.is_some() {
                out.align = layer.align;
            }
            if out.bg.is_none() && layer.bg.is_some() {
                out.bg = layer.bg;
            }
            if out.align.is_some() && out.bg.is_some() {
                break;
            }
        }
        out
    }

    fn flush_current(&mut self) {
        let mut next_default = if self.quote_depth > 0 {
            Current::Quote(Vec::new())
        } else {
            Current::Paragraph(Vec::new())
        };
        std::mem::swap(&mut self.current, &mut next_default);
        let finished = next_default;
        let kind = finished.into_kind();
        let attr = self.current_block_attr();
        let block = Block {
            kind,
            align: attr.align.unwrap_or(Align::Start),
            bg: attr.bg,
        };
        if !block.is_empty() {
            self.blocks.push(block);
        }
    }

    fn mark_break(&mut self) {
        let style = self.current_style();
        let spans = self.current.spans_mut();
        if let Some(last) = spans.last_mut() {
            last.br_after = true;
        } else {
            // 空段落 + <br>：放一个空 span 承载 break，渲染时会插入一行空白
            spans.push(Span {
                text: String::new(),
                style,
                br_after: true,
            });
        }
    }

    fn start_block(&mut self, kind: Current) {
        self.flush_current();
        self.current = kind;
    }

    fn on_open(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        if (self_closing || is_void_tag(name)) && !matches!(name, "br" | "hr" | "img") {
            return;
        }
        match name {
            // 块级 — 起新段落
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav"
            | "aside" | "figure" | "figcaption" | "address" | "center" | "dl" | "dt" | "dd"
            | "table" | "tbody" | "thead" | "tfoot" => {
                self.start_block(Current::Paragraph(Vec::new()));
                let mut attr = block_attr_from_attrs(attrs);
                if name == "center" && attr.align.is_none() {
                    attr.align = Some(Align::Center);
                }
                self.block_attr_stack.push(attr);
                self.push_style(style_from_attrs(attrs, &self.current_style()));
            }
            "tr" => {
                self.start_block(Current::Paragraph(Vec::new()));
                self.block_attr_stack.push(block_attr_from_attrs(attrs));
                self.push_style(style_from_attrs(attrs, &self.current_style()));
            }
            "td" | "th" => {
                let mut style = style_from_attrs(attrs, &self.current_style());
                if name == "th" {
                    style.bold = true;
                }
                self.push_style(style);
                let cur_style = self.current_style();
                let spans = self.current.spans_mut();
                let needs_space = spans
                    .last()
                    .map(|s| !s.text.ends_with(' ') && !s.br_after && !s.text.is_empty())
                    .unwrap_or(false);
                if needs_space {
                    if let Some(last) = spans
                        .last_mut()
                        .filter(|s| s.style == cur_style && !s.br_after)
                    {
                        last.text.push(' ');
                    } else {
                        spans.push(Span {
                            text: " ".into(),
                            style: cur_style,
                            br_after: false,
                        });
                    }
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let lvl: u8 = name[1..].parse().unwrap_or(3);
                self.start_block(Current::Heading(lvl, Vec::new()));
                self.block_attr_stack.push(block_attr_from_attrs(attrs));
                let mut style = style_from_attrs(attrs, &self.current_style());
                style.bold = true;
                style.size = heading_size(lvl);
                self.push_style(style);
            }
            "blockquote" => {
                self.quote_depth += 1;
                self.start_block(Current::Quote(Vec::new()));
                self.block_attr_stack.push(block_attr_from_attrs(attrs));
                self.push_style(style_from_attrs(attrs, &self.current_style()));
            }
            "ul" | "ol" => {
                self.flush_current();
                self.block_attr_stack.push(block_attr_from_attrs(attrs));
                self.push_style(style_from_attrs(attrs, &self.current_style()));
            }
            "li" => {
                self.start_block(Current::ListItem(Vec::new()));
                self.block_attr_stack.push(block_attr_from_attrs(attrs));
                self.push_style(style_from_attrs(attrs, &self.current_style()));
            }
            "pre" => {
                self.pre_depth += 1;
                self.start_block(Current::Paragraph(Vec::new()));
                self.block_attr_stack.push(block_attr_from_attrs(attrs));
                let mut style = style_from_attrs(attrs, &self.current_style());
                style.code = true;
                self.push_style(style);
            }
            "hr" => {
                self.flush_current();
                self.blocks.push(Block::new(BlockKind::Rule));
            }
            "br" => {
                self.mark_break();
            }
            "img" => {
                if let Some(src) = attr(attrs, "src") {
                    let alt = attr(attrs, "alt").map(|s| s.to_string());
                    let w = attr(attrs, "width").and_then(parse_dim);
                    let h = attr(attrs, "height").and_then(parse_dim);
                    self.flush_current();
                    let attr_now = self.current_block_attr();
                    self.blocks.push(Block {
                        kind: BlockKind::Image(ImageRef {
                            src: src.to_string(),
                            alt,
                            width: w,
                            height: h,
                        }),
                        align: attr_now.align.unwrap_or(Align::Start),
                        bg: attr_now.bg,
                    });
                }
            }
            // 内联样式
            "b" | "strong" => {
                let s = self.modified(|s| s.bold = true);
                self.push_style(s);
            }
            "i" | "em" | "cite" | "var" => {
                let s = self.modified(|s| s.italic = true);
                self.push_style(s);
            }
            "u" | "ins" => {
                let s = self.modified(|s| s.underline = true);
                self.push_style(s);
            }
            "s" | "strike" | "del" => {
                let s = self.modified(|s| s.strike = true);
                self.push_style(s);
            }
            "code" | "tt" | "kbd" | "samp" => {
                let s = self.modified(|s| s.code = true);
                self.push_style(s);
            }
            "small" => {
                let s = self.modified(|s| s.size *= 0.85);
                self.push_style(s);
            }
            "big" => {
                let s = self.modified(|s| s.size *= 1.2);
                self.push_style(s);
            }
            "sup" | "sub" => {
                let s = self.modified(|s| s.size *= 0.75);
                self.push_style(s);
            }
            "mark" => {
                let mut s = self.current_style();
                s.color = Some([0x00, 0x00, 0x00]);
                self.push_style(s);
            }
            "a" => {
                let href = attr(attrs, "href").map(|v| v.to_string());
                let mut s = self.modified(|s| s.underline = true);
                s.href = href;
                // 邮件里链接常见是蓝色
                if s.color.is_none() {
                    s.color = Some([0x1a, 0x6b, 0xd6]);
                }
                self.push_style(s);
            }
            "font" => {
                let mut s = self.current_style();
                if let Some(c) = attr(attrs, "color").and_then(parse_color) {
                    s.color = Some(c);
                }
                if let Some(sz) = attr(attrs, "size").and_then(parse_font_size_legacy) {
                    s.size *= sz;
                }
                // 叠加 style 属性
                apply_style_attr(&mut s, attr(attrs, "style"));
                self.push_style(s);
            }
            "span" => {
                let s = style_from_attrs(attrs, &self.current_style());
                self.push_style(s);
            }
            _ => {
                // 未知或不关心的标签：仍然推入一个占位样式，保证 close 时栈平衡
                self.push_style(self.current_style());
            }
        }
    }

    fn on_close(&mut self, name: &str) {
        // 块级结束顺序：先 flush_current（用栈顶 attr 给"已结束的段"打上对齐/背景），
        // 再 pop attr / style，避免父级 attr 越级生效到当前段。
        match name {
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav"
            | "aside" | "figure" | "figcaption" | "address" | "center" | "dl" | "dt" | "dd"
            | "table" | "tbody" | "thead" | "tfoot" | "tr" => {
                self.flush_current();
                self.pop_style();
                self.block_attr_stack.pop();
            }
            "td" | "th" => {
                self.pop_style();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_current();
                self.pop_style();
                self.block_attr_stack.pop();
            }
            "blockquote" => {
                if self.quote_depth > 0 {
                    self.quote_depth -= 1;
                }
                self.flush_current();
                self.pop_style();
                self.block_attr_stack.pop();
            }
            "ul" | "ol" => {
                self.flush_current();
                self.pop_style();
                self.block_attr_stack.pop();
            }
            "li" => {
                self.flush_current();
                self.pop_style();
                self.block_attr_stack.pop();
            }
            "pre" => {
                if self.pre_depth > 0 {
                    self.pre_depth -= 1;
                }
                self.flush_current();
                self.pop_style();
                self.block_attr_stack.pop();
            }
            "b" | "strong" | "i" | "em" | "cite" | "var" | "u" | "ins" | "s" | "strike" | "del"
            | "code" | "tt" | "kbd" | "samp" | "small" | "big" | "sup" | "sub" | "mark" | "a"
            | "font" | "span" => {
                self.pop_style();
            }
            "br" | "hr" | "img" => {}
            _ => self.pop_style(),
        }
    }

    fn starts_with_ci(&self, needle: &[u8]) -> bool {
        if self.pos + needle.len() > self.bytes.len() {
            return false;
        }
        self.bytes[self.pos..self.pos + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// 判断当前位置是否是 `<name[空格/>/]` 形式的开标签（忽略大小写）。
    fn starts_with_ci_name(&self, name: &[u8]) -> bool {
        if self.pos + 1 + name.len() > self.bytes.len() {
            return false;
        }
        if self.bytes[self.pos] != b'<' {
            return false;
        }
        for (i, b) in name.iter().enumerate() {
            if !self.bytes[self.pos + 1 + i].eq_ignore_ascii_case(b) {
                return false;
            }
        }
        let after = self.bytes.get(self.pos + 1 + name.len()).copied();
        matches!(after, Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/'))
    }
}

// --- 小工具 ---

fn find_byte(hay: &[u8], from: usize, b: u8) -> Option<usize> {
    hay.iter()
        .enumerate()
        .skip(from)
        .find(|(_, x)| **x == b)
        .map(|(i, _)| i)
}

fn find_bytes(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn split_name_attrs(body: &str) -> (&str, &str) {
    let body = body.trim();
    match body.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&body[..i], body[i..].trim()),
        None => (body, ""),
    }
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// 把一段任意文本里的空白折叠成单个空格，换行视作空格。
/// 不同于 `collapse_blank_lines`，这里用于**内联**文本。
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\u{00A0}' || c == '\u{3000}' {
            if !prev_space {
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

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
        {
            i += 1;
        }
        let name = &s[name_start..i];
        if name.is_empty() {
            break;
        }
        let mut value = String::new();
        // 跳过空白找 '='
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'=' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() {
                let q = bytes[j];
                if q == b'"' || q == b'\'' {
                    j += 1;
                    let vstart = j;
                    while j < bytes.len() && bytes[j] != q {
                        j += 1;
                    }
                    value = s[vstart..j].to_string();
                    if j < bytes.len() {
                        j += 1;
                    }
                } else {
                    let vstart = j;
                    while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
                        j += 1;
                    }
                    value = s[vstart..j].to_string();
                }
            }
            i = j;
        } else {
            // 无值属性
            i = j;
        }
        out.push((
            name.to_ascii_lowercase(),
            html_escape::decode_html_entities(&value).into_owned(),
        ));
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn style_from_attrs(attrs: &[(String, String)], base: &SpanStyle) -> SpanStyle {
    let mut s = base.clone();
    apply_style_attr(&mut s, attr(attrs, "style"));
    s
}

/// 把元素属性（含 inline `style="..."` 与传统 `align=""` / `bgcolor=""`）解析成块级 align/bg。
fn block_attr_from_attrs(attrs: &[(String, String)]) -> BlockAttr {
    let mut out = BlockAttr::default();
    if let Some(a) = attr(attrs, "align") {
        if let Some(al) = parse_align(a) {
            out.align = Some(al);
        }
    }
    if let Some(c) = attr(attrs, "bgcolor").and_then(parse_color) {
        out.bg = Some(c);
    }
    if let Some(css) = attr(attrs, "style") {
        for decl in css.split(';') {
            let Some((k, v)) = decl.split_once(':') else {
                continue;
            };
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "text-align" => {
                    if let Some(al) = parse_align(v) {
                        out.align = Some(al);
                    }
                }
                "background-color" | "background" => {
                    // background 可能是 shorthand："#fff url(...) ..."；只取第一个能解析的颜色 token
                    if let Some(c) = v.split_ascii_whitespace().find_map(parse_color) {
                        out.bg = Some(c);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn parse_align(v: &str) -> Option<Align> {
    match v.trim().to_ascii_lowercase().as_str() {
        "left" | "start" => Some(Align::Start),
        "center" | "middle" => Some(Align::Center),
        "right" | "end" => Some(Align::End),
        "justify" => Some(Align::Justify),
        _ => None,
    }
}

fn parse_dim(v: &str) -> Option<u32> {
    let v = v.trim();
    let end = v
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(v.len());
    if end == 0 {
        return None;
    }
    v[..end].parse().ok()
}

fn apply_style_attr(style: &mut SpanStyle, css: Option<&str>) {
    let Some(css) = css else { return };
    for decl in css.split(';') {
        let Some((k, v)) = decl.split_once(':') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        match k.as_str() {
            "font-weight" => {
                let lower = v.to_ascii_lowercase();
                if lower == "bold" || lower == "bolder" {
                    style.bold = true;
                } else if let Ok(n) = lower.parse::<u32>() {
                    if n >= 600 {
                        style.bold = true;
                    }
                }
            }
            "font-style" => {
                if v.eq_ignore_ascii_case("italic") || v.eq_ignore_ascii_case("oblique") {
                    style.italic = true;
                }
            }
            "text-decoration" | "text-decoration-line" => {
                let lower = v.to_ascii_lowercase();
                if lower.contains("underline") {
                    style.underline = true;
                }
                if lower.contains("line-through") {
                    style.strike = true;
                }
            }
            "color" => {
                if let Some(c) = parse_color(v) {
                    style.color = Some(c);
                }
            }
            "font-size" => {
                if let Some(mul) = parse_font_size_css(v) {
                    style.size *= mul;
                }
            }
            _ => {}
        }
    }
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 2.0,
        2 => 1.5,
        3 => 1.25,
        4 => 1.1,
        5 => 0.95,
        _ => 0.88,
    }
}

fn parse_font_size_css(v: &str) -> Option<f32> {
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "xx-small" => return Some(0.6),
        "x-small" => return Some(0.75),
        "small" => return Some(0.88),
        "medium" => return Some(1.0),
        "large" => return Some(1.2),
        "x-large" => return Some(1.5),
        "xx-large" => return Some(2.0),
        "smaller" => return Some(0.85),
        "larger" => return Some(1.2),
        _ => {}
    }
    let (num_str, unit) = split_num_unit(&v)?;
    let n: f32 = num_str.parse().ok()?;
    // 以 14px 为默认参考值
    let px = match unit {
        "px" | "" => n,
        "pt" => n * 96.0 / 72.0,
        "em" | "rem" => n * 14.0,
        "%" => n / 100.0 * 14.0,
        _ => return None,
    };
    let mul = (px / 14.0).clamp(0.6, 3.0);
    Some(mul)
}

fn parse_font_size_legacy(v: &str) -> Option<f32> {
    // <font size="1".."7"> 或 "+n" / "-n"
    let v = v.trim();
    if let Some(stripped) = v.strip_prefix('+') {
        let n: i32 = stripped.parse().ok()?;
        return Some(match n {
            1 => 1.15,
            2 => 1.3,
            3 => 1.5,
            _ => 1.7,
        });
    }
    if let Some(stripped) = v.strip_prefix('-') {
        let n: i32 = stripped.parse().ok()?;
        return Some(match n {
            1 => 0.88,
            2 => 0.75,
            _ => 0.6,
        });
    }
    let n: i32 = v.parse().ok()?;
    Some(match n {
        1 => 0.7,
        2 => 0.85,
        3 => 1.0,
        4 => 1.15,
        5 => 1.35,
        6 => 1.6,
        _ => 2.0,
    })
}

fn split_num_unit(s: &str) -> Option<(&str, &str)> {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((&s[..end], s[end..].trim()))
}

fn parse_color(v: &str) -> Option<[u8; 3]> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    let lower = v.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() == 3 {
            let r = parts[0].trim().parse::<u32>().ok()?.min(255) as u8;
            let g = parts[1].trim().parse::<u32>().ok()?.min(255) as u8;
            let b = parts[2].trim().parse::<u32>().ok()?.min(255) as u8;
            return Some([r, g, b]);
        }
    }
    if let Some(rest) = lower
        .strip_prefix("rgba(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() == 4 {
            let r = parts[0].trim().parse::<u32>().ok()?.min(255) as u8;
            let g = parts[1].trim().parse::<u32>().ok()?.min(255) as u8;
            let b = parts[2].trim().parse::<u32>().ok()?.min(255) as u8;
            return Some([r, g, b]);
        }
    }
    lookup_named_color(&lower)
}

fn lookup_named_color(name: &str) -> Option<[u8; 3]> {
    NAMED_COLOR_TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_para(html: &str) -> Vec<Span> {
        let blocks = parse(html);
        for b in blocks {
            if let BlockKind::Paragraph(s) = b.kind {
                return s;
            }
        }
        Vec::new()
    }

    #[test]
    fn plain_text_becomes_single_paragraph() {
        let blocks = parse("Hello world");
        assert_eq!(blocks.len(), 1);
        let BlockKind::Paragraph(spans) = &blocks[0].kind else {
            panic!("not paragraph")
        };
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "Hello world");
        assert!(!spans[0].style.bold);
    }

    #[test]
    fn bold_and_italic_spans() {
        let spans = first_para("<b>bold</b><i>ital</i>");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "bold");
        assert!(spans[0].style.bold);
        assert_eq!(spans[1].text, "ital");
        assert!(spans[1].style.italic);
    }

    #[test]
    fn heading_block() {
        let blocks = parse("<h2>标题</h2>");
        assert_eq!(blocks.len(), 1);
        let BlockKind::Heading(lvl, spans) = &blocks[0].kind else {
            panic!("not heading")
        };
        assert_eq!(*lvl, 2);
        assert!(spans[0].style.bold);
        assert!(spans[0].style.size > 1.0);
    }

    #[test]
    fn anchor_carries_href_and_underline() {
        let spans = first_para(r#"<a href="https://example.com">link</a>"#);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "link");
        assert_eq!(spans[0].style.href.as_deref(), Some("https://example.com"));
        assert!(spans[0].style.underline);
    }

    #[test]
    fn css_font_weight_and_size_apply() {
        let spans = first_para(
            r#"<span style="font-weight:bold;font-size:24px;color:#ff0000">big red</span>"#,
        );
        assert_eq!(spans.len(), 1);
        assert!(spans[0].style.bold);
        assert!(spans[0].style.size > 1.5);
        assert_eq!(spans[0].style.color, Some([0xff, 0x00, 0x00]));
    }

    #[test]
    fn void_tag_does_not_leak_parent_style() {
        let spans = first_para(r##"<span style="color:#ff0000">red<wbr></span>plain"##);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "red");
        assert_eq!(spans[0].style.color, Some([0xff, 0x00, 0x00]));
        assert_eq!(spans[1].text, "plain");
        assert_eq!(spans[1].style.color, None);
    }

    #[test]
    fn br_breaks_line_within_paragraph() {
        let spans = first_para("<p>one<br>two</p>");
        assert_eq!(spans.len(), 2);
        assert!(spans[0].br_after);
        assert_eq!(spans[0].text, "one");
        assert_eq!(spans[1].text, "two");
    }

    #[test]
    fn pre_preserves_spacing_and_line_breaks() {
        let blocks = parse("<pre>  a\tb\n    c</pre>");
        let BlockKind::Paragraph(spans) = &blocks[0].kind else {
            panic!("not paragraph")
        };
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "  a    b");
        assert!(spans[0].br_after);
        assert_eq!(spans[1].text, "    c");
        assert!(spans.iter().all(|s| s.style.code));
    }

    #[test]
    fn script_and_style_are_skipped() {
        let blocks = parse("<style>body{color:red}</style><script>alert(1)</script><p>Body</p>");
        let text: String = blocks
            .iter()
            .flat_map(|b| b.spans().unwrap_or(&[]).to_vec())
            .map(|s| s.text)
            .collect();
        assert_eq!(text, "Body");
    }

    #[test]
    fn table_keeps_legacy_row_text_flow() {
        let blocks =
            parse("<table><tr><td>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr></table>");
        assert_eq!(blocks.len(), 2);
        let BlockKind::Paragraph(row1) = &blocks[0].kind else {
            panic!("row1 not paragraph")
        };
        let BlockKind::Paragraph(row2) = &blocks[1].kind else {
            panic!("row2 not paragraph")
        };
        assert_eq!(
            row1.iter().map(|s| s.text.as_str()).collect::<String>(),
            "A B"
        );
        assert_eq!(
            row2.iter().map(|s| s.text.as_str()).collect::<String>(),
            "C D"
        );
    }

    #[test]
    fn img_becomes_image_block() {
        let blocks = parse(r#"<p><img src="data:image/png;base64,iVBOR" alt="x" width="32"></p>"#);
        let img = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Image(r) => Some(r),
                _ => None,
            })
            .expect("expected image");
        assert!(img.src.starts_with("data:image/png"));
        assert_eq!(img.alt.as_deref(), Some("x"));
        assert_eq!(img.width, Some(32));
    }

    #[test]
    fn text_align_and_bgcolor_carry_to_block() {
        let blocks = parse(r#"<p style="text-align:center;background-color:#ff0">Hi</p>"#);
        let p = &blocks[0];
        assert_eq!(p.align, Align::Center);
        assert_eq!(p.bg, Some([0xff, 0xff, 0x00]));
    }

    #[test]
    fn legacy_align_attr_works() {
        let blocks = parse(r#"<div align="right">x</div>"#);
        assert_eq!(blocks[0].align, Align::End);
    }
}

static NAMED_COLOR_TABLE: &[(&str, [u8; 3])] = &[
    ("black", [0, 0, 0]),
    ("white", [255, 255, 255]),
    ("red", [255, 0, 0]),
    ("green", [0, 128, 0]),
    ("blue", [0, 0, 255]),
    ("yellow", [255, 255, 0]),
    ("cyan", [0, 255, 255]),
    ("magenta", [255, 0, 255]),
    ("gray", [128, 128, 128]),
    ("grey", [128, 128, 128]),
    ("orange", [255, 165, 0]),
    ("purple", [128, 0, 128]),
    ("pink", [255, 192, 203]),
    ("brown", [165, 42, 42]),
    ("silver", [192, 192, 192]),
    ("gold", [255, 215, 0]),
    ("navy", [0, 0, 128]),
    ("teal", [0, 128, 128]),
    ("olive", [128, 128, 0]),
    ("maroon", [128, 0, 0]),
    ("lime", [0, 255, 0]),
    ("aqua", [0, 255, 255]),
    ("fuchsia", [255, 0, 255]),
    ("darkgray", [169, 169, 169]),
    ("darkgrey", [169, 169, 169]),
    ("lightgray", [211, 211, 211]),
    ("lightgrey", [211, 211, 211]),
    ("darkred", [139, 0, 0]),
    ("darkgreen", [0, 100, 0]),
    ("darkblue", [0, 0, 139]),
    ("lightblue", [173, 216, 230]),
    ("skyblue", [135, 206, 235]),
];

fn parse_hex_color(hex: &str) -> Option<[u8; 3]> {
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some([r * 0x11, g * 0x11, b * 0x11])
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some([r, g, b])
        }
        _ => None,
    }
}
