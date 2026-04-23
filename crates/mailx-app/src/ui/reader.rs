//! 正文阅读面板：
//! - 面板内显示 sanitize 后的纯文本摘要（快速预览）
//! - 正文中的内联图 / 图片附件直接在 egui 里渲染（不再只能"在浏览器打开"）
//! - "在浏览器打开" 把清洗后的 HTML 写入临时目录，交给系统浏览器完整渲染
//! - 附件列表 + 预览（图片 / PDF / 文本 / Office）

use std::path::PathBuf;

use eframe::egui;
use mailx_preview::Preview;
use mailx_render::rich::{self, Block, Span};
use mailx_render::RenderedBody;
use mailx_store::MessageRow;

/// 预览弹窗状态，由上层持有。
pub struct PreviewState {
    pub title: String,
    pub content: PreviewContent,
}

/// 单封邮件正文的渲染缓存，归 `MailxApp` 持有。
///
/// `body_path` 变化时才会重建——避免每帧（≈5 FPS）都重新 fs::read + MIME 解析 +
/// sanitize + cid base64，对大附件 / 大 HTML 邮件特别明显。
pub struct CachedBody {
    pub path: PathBuf,
    pub rendered: Result<RenderedBody, String>,
    /// 纯文本兜底（仅在富文本块解析为空、或 HTML 解析失败时使用）。
    pub plain_preview: String,
    /// HTML → 富文本块（粗体/标题/链接/颜色/字号），供 UI 直接画出。
    pub blocks: Vec<Block>,
    /// 正文里可直接内嵌显示的图片（CID 内联 + image/* 附件），解码成 RGBA8。
    /// 纹理在首次渲染时懒加载，见下方同序 textures 数组。
    images: Vec<InlineImage>,
    textures: Vec<Option<egui::TextureHandle>>,
}

struct InlineImage {
    label: String,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl CachedBody {
    pub fn load(path: PathBuf) -> Self {
        let rendered = std::fs::read(&path)
            .map_err(|e| format!("读取正文失败: {e}"))
            .and_then(|raw| {
                mailx_render::render(&raw).map_err(|e| format!("MIME 解析失败: {e}"))
            });
        let blocks = match &rendered {
            Ok(body) => rich::parse(&body.html),
            Err(_) => Vec::new(),
        };
        let plain_preview = match &rendered {
            Ok(body) => {
                if body.plain_fallback.trim().is_empty() {
                    mailx_render::html_to_plaintext(&body.html)
                } else {
                    body.plain_fallback.clone()
                }
            }
            Err(_) => String::new(),
        };
        let images = match &rendered {
            Ok(body) => collect_images(body),
            Err(_) => Vec::new(),
        };
        let textures = (0..images.len()).map(|_| None).collect();
        Self { path, rendered, plain_preview, blocks, images, textures }
    }
}

fn collect_images(body: &RenderedBody) -> Vec<InlineImage> {
    let mut out = Vec::new();
    for (cid, part) in &body.inline_parts {
        if !part.content_type.to_ascii_lowercase().starts_with("image/") {
            continue;
        }
        if let Ok((w, h, rgba)) = mailx_preview::decode_image_bytes(&part.data) {
            out.push(InlineImage { label: format!("内嵌图 {cid}"), width: w, height: h, rgba });
        }
    }
    for att in &body.attachments {
        // 不仅看 content-type，也看扩展名——有的邮件把内联图打成 application/octet-stream。
        let ct = att.content_type.to_ascii_lowercase();
        let is_image = ct.starts_with("image/") || has_image_ext(&att.filename);
        if !is_image {
            continue;
        }
        if let Ok((w, h, rgba)) = mailx_preview::decode_image_bytes(&att.data) {
            out.push(InlineImage { label: att.filename.clone(), width: w, height: h, rgba });
        }
    }
    // HTML 里直接内嵌的 data:image/...;base64,... —— 很多营销/通知邮件不走 cid，
    // 而是把小图 base64 塞进 img src。sanitize 阶段会原样保留，这里扫出来上屏。
    extract_data_uri_images(&body.html, &mut out);
    out
}

fn has_image_ext(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        std::path::Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str()),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
}

fn extract_data_uri_images(html: &str, out: &mut Vec<InlineImage>) {
    const NEEDLE: &str = "data:image/";
    let mut rest = html;
    let mut idx = 0usize;
    while let Some(pos) = rest.find(NEEDLE) {
        let tail = &rest[pos + NEEDLE.len()..];
        // 格式：data:image/<subtype>[;charset=...];base64,<payload>  直到遇到 " ' 或 ) 或空白
        let Some(semi) = tail.find(";base64,") else { break };
        let payload_start = semi + ";base64,".len();
        let payload_end = tail[payload_start..]
            .find(|c: char| c == '"' || c == '\'' || c == ')' || c == ' ' || c == '\n')
            .map(|e| payload_start + e)
            .unwrap_or(tail.len());
        let payload = &tail[payload_start..payload_end];
        if let Some(bytes) = decode_base64(payload) {
            if let Ok((w, h, rgba)) = mailx_preview::decode_image_bytes(&bytes) {
                idx += 1;
                out.push(InlineImage {
                    label: format!("内嵌图 #{idx}"),
                    width: w,
                    height: h,
                    rgba,
                });
            }
        }
        rest = &tail[payload_end..];
    }
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    // 极简 base64 解码：忽略空白；不处理 URL-safe 变体（邮件用标准字母表）。
    let clean: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if clean.is_empty() {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(clean.len() / 4 * 3);
    let mut buf = [0i16; 4];
    let mut n = 0usize;
    for &b in &clean {
        let v: i16 = match b {
            b'A'..=b'Z' => (b - b'A') as i16,
            b'a'..=b'z' => (b - b'a' + 26) as i16,
            b'0'..=b'9' => (b - b'0' + 52) as i16,
            b'+' => 62,
            b'/' => 63,
            b'=' => -1,
            _ => return None,
        };
        buf[n] = v;
        n += 1;
        if n == 4 {
            let pads = buf.iter().filter(|&&x| x < 0).count();
            let v0 = buf[0].max(0) as u32;
            let v1 = buf[1].max(0) as u32;
            let v2 = buf[2].max(0) as u32;
            let v3 = buf[3].max(0) as u32;
            let combined = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
            out.push(((combined >> 16) & 0xFF) as u8);
            if pads < 2 {
                out.push(((combined >> 8) & 0xFF) as u8);
            }
            if pads < 1 {
                out.push((combined & 0xFF) as u8);
            }
            n = 0;
        }
    }
    Some(out)
}

pub enum PreviewContent {
    Text(String),
    Image {
        texture: Option<egui::TextureHandle>,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
    Error(String),
    TooLarge { path: PathBuf },
    Unsupported,
}

/// 渲染正文阅读器。返回"请求打开预览"的新状态（由 App 负责放进顶层状态渲染 modal）。
pub fn show(
    ui: &mut egui::Ui,
    row: Option<&MessageRow>,
    cached: Option<&mut CachedBody>,
) -> Option<PreviewState> {
    let mut preview_request: Option<PreviewState> = None;

    // 先画头部（主题 / 发件人 / 日期），不依赖正文
    if let Some(m) = row {
        render_header(ui, m);
        ui.separator();
    }

    let Some(cache) = cached else {
        ui.label("正在加载正文...");
        return preview_request;
    };
    let body = match &cache.rendered {
        Ok(b) => b,
        Err(e) => {
            ui.colored_label(egui::Color32::RED, e.clone());
            return preview_request;
        }
    };

    ui.horizontal(|ui| {
        if ui.button("🌐 在浏览器中打开完整视图").clicked() {
            if let Err(e) = open_in_browser(body) {
                tracing::warn!("open in browser failed: {e}");
            }
        }
        ui.weak(format!(
            "内嵌资源 {} 项，附件 {} 个",
            body.inline_parts.len(),
            body.attachments.len()
        ));
    });
    ui.separator();

    // 附件列表
    if !body.attachments.is_empty() {
        ui.collapsing("📎 附件", |ui| {
            for att in &body.attachments {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} · {} · {}",
                        att.filename,
                        human_size(att.data.len() as u64),
                        att.content_type
                    ));
                    if ui.small_button("预览").clicked() {
                        preview_request = Some(make_preview(&att.filename, &att.data));
                    }
                    if ui.small_button("保存").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_file_name(&att.filename)
                            .save_file()
                        {
                            if let Err(e) = std::fs::write(&path, &att.data) {
                                tracing::warn!("保存附件失败: {e}");
                            }
                        }
                    }
                });
            }
        });
        ui.separator();
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            let has_rich = !cache.blocks.is_empty();
            if has_rich {
                render_blocks(ui, &cache.blocks);
            } else if !cache.plain_preview.trim().is_empty() {
                ui.add(egui::Label::new(cache.plain_preview.as_str()).wrap());
            }
            // 正文内联图：首次绘制时上传到 GPU，缓存纹理句柄。
            if !cache.images.is_empty() {
                if has_rich || !cache.plain_preview.trim().is_empty() {
                    ui.add_space(8.0);
                    ui.separator();
                }
                let ctx = ui.ctx().clone();
                for (idx, img) in cache.images.iter().enumerate() {
                    if cache.textures[idx].is_none() {
                        let color_img = egui::ColorImage::from_rgba_unmultiplied(
                            [img.width as usize, img.height as usize],
                            &img.rgba,
                        );
                        cache.textures[idx] = Some(ctx.load_texture(
                            format!("inline-{}-{}", cache.path.display(), idx),
                            color_img,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    if let Some(tex) = &cache.textures[idx] {
                        ui.add_space(4.0);
                        ui.weak(&img.label);
                        let avail = ui.available_width();
                        let natural = tex.size_vec2();
                        let scale = if natural.x > avail && natural.x > 0.0 {
                            avail / natural.x
                        } else {
                            1.0
                        };
                        ui.image((tex.id(), natural * scale));
                    }
                }
            }
        });

    preview_request
}

fn make_preview(filename: &str, data: &[u8]) -> PreviewState {
    // 附件落盘到临时目录后调用 preview_file
    let tmp = match tempfile::Builder::new()
        .prefix("mailx-att-")
        .suffix(&format!("-{filename}"))
        .tempfile()
    {
        Ok(t) => t,
        Err(e) => {
            return PreviewState {
                title: filename.into(),
                content: PreviewContent::Error(format!("临时文件创建失败: {e}")),
            }
        }
    };
    if let Err(e) = std::fs::write(tmp.path(), data) {
        return PreviewState {
            title: filename.into(),
            content: PreviewContent::Error(format!("写入临时文件失败: {e}")),
        };
    }
    // tempfile 持有期跟作用域，这里 persist 之后手动管理
    let (_, path) = match tmp.keep() {
        Ok(kept) => kept,
        Err(e) => {
            return PreviewState {
                title: filename.into(),
                content: PreviewContent::Error(format!("保留临时文件失败: {e}")),
            }
        }
    };

    let content = match mailx_preview::preview_file(&path) {
        Ok(Preview::Text(t)) => PreviewContent::Text(t),
        Ok(Preview::Image { width, height, rgba }) => PreviewContent::Image {
            texture: None,
            rgba,
            width,
            height,
        },
        Ok(Preview::TooLarge) => PreviewContent::TooLarge { path: path.clone() },
        Ok(Preview::Unsupported) => PreviewContent::Unsupported,
        Err(e) => PreviewContent::Error(format!("{e:#}")),
    };
    PreviewState {
        title: filename.into(),
        content,
    }
}

/// 预览 modal 的渲染：返回 true 表示应关闭。
pub fn show_preview_modal(ctx: &egui::Context, state: &mut PreviewState) -> bool {
    let mut close = false;
    egui::Window::new(format!("预览 — {}", state.title))
        .collapsible(false)
        .resizable(true)
        .default_width(900.0)
        .default_height(640.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
            ui.separator();
            match &mut state.content {
                PreviewContent::Text(t) => {
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut t.as_str()).desired_rows(30));
                    });
                }
                PreviewContent::Image {
                    texture,
                    rgba,
                    width,
                    height,
                } => {
                    if texture.is_none() {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [*width as usize, *height as usize],
                            rgba,
                        );
                        *texture = Some(ctx.load_texture(
                            format!("preview-{}", state.title),
                            img,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    if let Some(tex) = texture {
                        egui::ScrollArea::both().show(ui, |ui| {
                            ui.image((tex.id(), tex.size_vec2()));
                        });
                    }
                }
                PreviewContent::Error(e) => {
                    ui.colored_label(egui::Color32::RED, e.clone());
                }
                PreviewContent::TooLarge { path } => {
                    ui.label("文件大于 50 MB，已跳过内嵌预览。");
                    if ui.button("在外部程序打开").clicked() {
                        let _ = open::that_detached(path);
                    }
                }
                PreviewContent::Unsupported => {
                    ui.label("暂不支持该类型的内嵌预览。");
                }
            }
        });
    close
}

fn open_in_browser(body: &RenderedBody) -> anyhow::Result<()> {
    let dir = tempfile::Builder::new().prefix("mailx-body-").tempdir()?;
    let path = dir.path().join("index.html");
    let shell = format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta name="referrer" content="no-referrer">
<style>body{{font-family:system-ui,sans-serif;max-width:900px;margin:24px auto;padding:0 16px}}</style>
</head><body>{}</body></html>"#,
        body.html
    );
    std::fs::write(&path, shell)?;
    let _ = dir.keep();
    open::that_detached(path)?;
    Ok(())
}

fn render_header(ui: &mut egui::Ui, m: &MessageRow) {
    let subject = mailx_proto::decode_rfc2047(m.subject.as_deref().unwrap_or("(无主题)"));
    ui.heading(subject);
    ui.horizontal_wrapped(|ui| {
        ui.weak("发件人：");
        let from = mailx_proto::decode_rfc2047(m.from_addr.as_deref().unwrap_or("?"));
        ui.label(from);
        if let Some(d) = m.internal_date.as_deref() {
            ui.weak("·");
            ui.weak(d.replace('T', " ").trim_end_matches('Z').to_string());
        }
    });
}

/// 把富文本块按视觉期望渲染。每个块之间留一点垂直间距。
///
/// `base_size` 给到 15px —— egui 默认 14 在中文字体上偏细，邮件正文读起来容易糊。
/// `bg` 是当前面板底色，用来判断邮件自带的 CSS 颜色能不能保留（对比度过低时回退到默认 text color）。
fn render_blocks(ui: &mut egui::Ui, blocks: &[Block]) {
    let base_size = 15.0_f32;
    let bg = ui.visuals().panel_fill;
    let default_text = ui.visuals().text_color();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            ui.add_space(4.0);
        }
        match block {
            Block::Rule => {
                ui.separator();
            }
            Block::Heading(_, spans) => {
                render_spans_wrapped(ui, spans, base_size, bg, default_text);
                ui.add_space(2.0);
            }
            Block::Paragraph(spans) => {
                render_spans_wrapped(ui, spans, base_size, bg, default_text);
            }
            Block::Quote(spans) => {
                // 引用块：稍微调整底色——深色主题下拉亮一点，浅色主题下拉暗一点。
                let quote_bg = shift_toward(bg, default_text, 0.08);
                egui::Frame::default()
                    .fill(quote_bg)
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                    .show(ui, |ui| {
                        render_spans_wrapped(ui, spans, base_size, quote_bg, default_text);
                    });
            }
            Block::ListItem(spans) => {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label("•");
                    let inner = ui.available_width();
                    ui.allocate_ui_with_layout(
                        egui::vec2(inner.max(64.0), 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| render_spans_wrapped(ui, spans, base_size, bg, default_text),
                    );
                });
            }
        }
    }
}

/// 渲染一段 spans —— 遇到 `br_after` 就切行。每行用 `horizontal_wrapped`，
/// 行内相邻 label / hyperlink 间 spacing 压到 0，避免出现"每个词之间都有大空格"。
fn render_spans_wrapped(
    ui: &mut egui::Ui,
    spans: &[Span],
    base_size: f32,
    bg: egui::Color32,
    default_text: egui::Color32,
) {
    if spans.is_empty() {
        return;
    }
    let mut lines: Vec<Vec<&Span>> = Vec::new();
    let mut current: Vec<&Span> = Vec::new();
    for s in spans {
        current.push(s);
        if s.br_after {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    for line in lines {
        let only_blank = line.iter().all(|s| s.text.is_empty());
        if only_blank {
            ui.add_space(base_size * 0.6);
            continue;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for s in &line {
                if s.text.is_empty() {
                    continue;
                }
                draw_span(ui, s, base_size, bg, default_text);
            }
        });
    }
}

fn draw_span(
    ui: &mut egui::Ui,
    span: &Span,
    base_size: f32,
    bg: egui::Color32,
    default_text: egui::Color32,
) {
    let text = &span.text;
    let st = &span.style;
    let size = (base_size * st.size.max(0.5)).clamp(10.0, base_size * 3.0);
    let mut rt = egui::RichText::new(text).size(size);
    if st.bold {
        rt = rt.strong();
    }
    if st.italic {
        rt = rt.italics();
    }
    if st.underline {
        rt = rt.underline();
    }
    if st.strike {
        rt = rt.strikethrough();
    }
    if st.code {
        rt = rt.monospace();
    }

    // 颜色处理：
    // - 如果 span 是超链接：保留 egui 的链接默认色（不强塞 CSS 色）——很多邮件把
    //   链接写成 `color: #333`（深灰），在暗色主题下直接消失。
    // - 普通文字：只有对比度达标的颜色才保留；否则用主题默认 text color。
    let final_color = if st.href.is_some() {
        None
    } else {
        st.color.and_then(|rgb| {
            let c = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            if contrast_ratio(c, bg) >= 3.5 {
                Some(c)
            } else {
                None
            }
        })
    };
    if let Some(c) = final_color {
        rt = rt.color(c);
    } else if st.href.is_none() {
        // 显式设一次默认色，避免被上层 weak/muted 派生的样式影响。
        rt = rt.color(default_text);
    }

    if let Some(href) = &st.href {
        ui.add(egui::Hyperlink::from_label_and_url(rt, href).open_in_new_tab(true));
    } else {
        ui.add(egui::Label::new(rt).wrap());
    }
}

/// WCAG 相对亮度。
fn rel_luminance(c: egui::Color32) -> f32 {
    fn channel(x: u8) -> f32 {
        let v = x as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f32 {
    let la = rel_luminance(a);
    let lb = rel_luminance(b);
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 在 `from` 颜色上混入 `toward` 的一小部分；用于在面板底色上生成"稍有区分"的引用块底色，
/// 不会因为硬编码 RGB 在深/浅主题下看起来很突兀。
fn shift_toward(from: egui::Color32, toward: egui::Color32, amount: f32) -> egui::Color32 {
    let a = amount.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| ((x as f32) * (1.0 - a) + (y as f32) * a).round().clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgb(
        mix(from.r(), toward.r()),
        mix(from.g(), toward.g()),
        mix(from.b(), toward.b()),
    )
}

fn human_size(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut f = b as f64;
    let mut i = 0;
    while f >= 1024.0 && i + 1 < UNITS.len() {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1} {}", UNITS[i])
}
