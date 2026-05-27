//! 正文阅读面板：
//! - 把 sanitize 后的 HTML 解析成富文本块（rich.rs），用 egui 直接渲染——含表格 / 图片 /
//!   对齐 / 背景色，以接近"主流浏览器"的视觉效果但不内嵌 webview
//! - "在浏览器打开" 把清洗后的 HTML 写入临时目录，交给系统浏览器完整渲染（兜底，对极端排版邮件）
//! - 附件列表 + 预览（图片 / PDF / 文本 / Office）

use std::path::PathBuf;

use eframe::egui;
use mailx_preview::Preview;
use mailx_render::rich::{self, Align, Block, BlockKind, ImageRef, Span, TableCell};
use mailx_render::RenderedBody;
use mailx_store::MessageRow;

const BASE_FONT_SIZE: f32 = 15.0;
const MIN_ZOOM: f32 = 0.6;
const MAX_ZOOM: f32 = 2.0;
const ZOOM_STEP: f32 = 0.1;

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
    /// HTML → 富文本块（粗体/标题/链接/颜色/字号/对齐/背景/表格/图片），供 UI 直接画出。
    pub blocks: Vec<Block>,
    /// 与 `blocks` 中 `BlockKind::Image` 出现顺序一一对应的解码图。
    /// 渲染时按出现顺序消费——遇到 Image 块时取 `images[image_cursor]`。
    images: Vec<DecodedImage>,
    textures: Vec<Option<egui::TextureHandle>>,
    zoom: f32,
}

/// 一张已经从 `<img src>` 解码出来的图。`pixels` 为 None 时表示远程 URL 或解码失败，
/// UI 退化为"无法显示远程图"占位。
struct DecodedImage {
    /// 显示用的标题（alt 优先；否则 "图片 #n"）。
    label: String,
    pixels: Option<DecodedPixels>,
    /// 原始 src（远程图占位时显示）。
    src_hint: String,
}

struct DecodedPixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl CachedBody {
    pub fn load(path: PathBuf) -> Self {
        let rendered = std::fs::read(&path)
            .map_err(|e| format!("读取正文失败: {e}"))
            .and_then(|raw| mailx_render::render(&raw).map_err(|e| format!("MIME 解析失败: {e}")));
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
            Ok(body) => collect_images(body, &blocks),
            Err(_) => Vec::new(),
        };
        let textures = (0..images.len()).map(|_| None).collect();
        Self {
            path,
            rendered,
            plain_preview,
            blocks,
            images,
            textures,
            zoom: 1.0,
        }
    }
}

fn collect_images(body: &RenderedBody, blocks: &[Block]) -> Vec<DecodedImage> {
    let mut out = Vec::new();
    visit_images(blocks, &mut |img| {
        let label = img
            .alt
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("图片 #{}", out.len() + 1));
        let pixels = decode_image_src(&img.src);
        out.push(DecodedImage {
            label,
            pixels,
            src_hint: short_src(&img.src),
        });
    });

    for (cid, part) in &body.inline_parts {
        if !part.content_type.to_ascii_lowercase().starts_with("image/") {
            continue;
        }
        push_decoded_bytes_unique(&mut out, format!("内嵌图 {cid}"), &part.data);
    }
    for att in &body.attachments {
        let ct = att.content_type.to_ascii_lowercase();
        let is_image = ct.starts_with("image/") || has_image_ext(&att.filename);
        if is_image {
            push_decoded_bytes_unique(&mut out, att.filename.clone(), &att.data);
        }
    }
    extract_data_uri_images(&body.html, &mut out);

    out
}

fn visit_images(blocks: &[Block], f: &mut impl FnMut(&ImageRef)) {
    for b in blocks {
        match &b.kind {
            BlockKind::Image(img) => f(img),
            BlockKind::Table(rows) => {
                for row in rows {
                    for cell in row {
                        visit_images(&cell.blocks, f);
                    }
                }
            }
            _ => {}
        }
    }
}

fn push_decoded_bytes_unique(out: &mut Vec<DecodedImage>, label: String, data: &[u8]) {
    let Ok((w, h, rgba)) = mailx_preview::decode_image_bytes(data) else {
        return;
    };
    if out.iter().any(|img| match &img.pixels {
        Some(p) => p.width == w && p.height == h && p.rgba == rgba,
        None => false,
    }) {
        return;
    }
    out.push(DecodedImage {
        label,
        pixels: Some(DecodedPixels {
            width: w,
            height: h,
            rgba,
        }),
        src_hint: String::new(),
    });
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

/// 试着把 `<img src>` 解成像素：`data:image/...;base64,...` 走 base64 + image crate；
/// 远程 URL（http/https）一律不抓——避免泄露阅读行为给跟踪像素。
fn decode_image_src(src: &str) -> Option<DecodedPixels> {
    let lower = src.trim_start().to_ascii_lowercase();
    if !lower.starts_with("data:image/") {
        return None;
    }
    // data:image/<sub>[;...];base64,<payload>
    let comma = src.find(',')?;
    let header = &src[..comma];
    let payload = &src[comma + 1..];
    if !header.to_ascii_lowercase().contains(";base64") {
        return None;
    }
    let bytes = decode_base64(payload)?;
    let (w, h, rgba) = mailx_preview::decode_image_bytes(&bytes).ok()?;
    Some(DecodedPixels {
        width: w,
        height: h,
        rgba,
    })
}

fn short_src(src: &str) -> String {
    let trimmed = src.trim();
    if trimmed.len() > 80 {
        let head: String = trimmed.chars().take(60).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}

fn extract_data_uri_images(html: &str, out: &mut Vec<DecodedImage>) {
    const NEEDLE: &str = "data:image/";
    let mut rest = html;
    let mut idx = 0usize;
    while let Some(pos) = rest.find(NEEDLE) {
        let tail = &rest[pos + NEEDLE.len()..];
        let Some(semi) = tail.find(";base64,") else {
            break;
        };
        let payload_start = semi + ";base64,".len();
        let payload_end = tail[payload_start..]
            .find(['"', '\'', ')', ' ', '\n'])
            .map(|e| payload_start + e)
            .unwrap_or(tail.len());
        let payload = &tail[payload_start..payload_end];
        if let Some(bytes) = decode_base64(payload) {
            idx += 1;
            push_decoded_bytes_unique(out, format!("内嵌图 #{idx}"), &bytes);
        }
        rest = &tail[payload_end..];
    }
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    // 极简 base64 解码：忽略空白；不处理 URL-safe 变体（邮件用标准字母表）。
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
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
    TooLarge {
        path: PathBuf,
    },
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
    if let Err(e) = &cache.rendered {
        ui.colored_label(egui::Color32::RED, e.clone());
        return preview_request;
    }

    apply_wheel_zoom(ui, &mut cache.zoom);

    ui.horizontal(|ui| {
        render_zoom_controls(ui, &mut cache.zoom);
    });
    ui.separator();

    let body = cache.rendered.as_ref().expect("rendered checked above");
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
                            .set_file_name(safe_attachment_name(&att.filename))
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

    let scroll_size = ui.available_size_before_wrap();
    ui.allocate_ui_with_layout(
        scroll_size,
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::ScrollArea::both()
                .id_salt(("mail-body-scroll", cache.path.display().to_string()))
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    let has_rich = !cache.blocks.is_empty();
                    if has_rich {
                        let path_id = cache.path.display().to_string();
                        let mut ctx = RenderCtx {
                            base_size: BASE_FONT_SIZE * cache.zoom,
                            panel_bg: ui.visuals().panel_fill,
                            default_text: ui.visuals().text_color(),
                            zoom: cache.zoom,
                            image_cursor: 0,
                            images: &cache.images,
                            textures: &mut cache.textures,
                            path_id: &path_id,
                        };
                        render_blocks(ui, &cache.blocks, &mut ctx);
                        if ctx.image_cursor < ctx.images.len() {
                            ui.add_space(8.0);
                            ui.separator();
                            render_standalone_images(ui, ctx.image_cursor, &mut ctx);
                        }
                    } else if !cache.plain_preview.trim().is_empty() {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(cache.plain_preview.as_str())
                                    .size(BASE_FONT_SIZE * cache.zoom),
                            )
                            .wrap(),
                        );
                        if !cache.images.is_empty() {
                            ui.add_space(8.0);
                            ui.separator();
                            let path_id = cache.path.display().to_string();
                            let mut ctx = RenderCtx {
                                base_size: BASE_FONT_SIZE * cache.zoom,
                                panel_bg: ui.visuals().panel_fill,
                                default_text: ui.visuals().text_color(),
                                zoom: cache.zoom,
                                image_cursor: 0,
                                images: &cache.images,
                                textures: &mut cache.textures,
                                path_id: &path_id,
                            };
                            render_standalone_images(ui, 0, &mut ctx);
                        }
                    } else if !cache.images.is_empty() {
                        let path_id = cache.path.display().to_string();
                        let mut ctx = RenderCtx {
                            base_size: BASE_FONT_SIZE * cache.zoom,
                            panel_bg: ui.visuals().panel_fill,
                            default_text: ui.visuals().text_color(),
                            zoom: cache.zoom,
                            image_cursor: 0,
                            images: &cache.images,
                            textures: &mut cache.textures,
                            path_id: &path_id,
                        };
                        render_standalone_images(ui, 0, &mut ctx);
                    }
                });
        },
    );

    preview_request
}

fn apply_wheel_zoom(ui: &mut egui::Ui, zoom: &mut f32) {
    let delta = ui.input(|i| {
        if i.modifiers.ctrl || i.modifiers.command || i.modifiers.mac_cmd {
            i.zoom_delta()
        } else {
            1.0
        }
    });
    if (delta - 1.0).abs() > 0.001 {
        *zoom = (*zoom * delta).clamp(MIN_ZOOM, MAX_ZOOM);
    }
}

fn render_zoom_controls(ui: &mut egui::Ui, zoom: &mut f32) {
    if ui.small_button("−").on_hover_text("缩小正文").clicked() {
        *zoom = (*zoom - ZOOM_STEP).max(MIN_ZOOM);
    }
    if ui
        .small_button(format!("{:.0}%", *zoom * 100.0))
        .on_hover_text("恢复 100%")
        .clicked()
    {
        *zoom = 1.0;
    }
    if ui.small_button("+").on_hover_text("放大正文").clicked() {
        *zoom = (*zoom + ZOOM_STEP).min(MAX_ZOOM);
    }
}

fn make_preview(filename: &str, data: &[u8]) -> PreviewState {
    // 附件落盘到临时目录后调用 preview_file
    let safe_name = safe_attachment_name(filename);
    let tmp = match tempfile::Builder::new()
        .prefix("mailx-att-")
        .suffix(&format!("-{safe_name}"))
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
        Ok(Preview::Image {
            width,
            height,
            rgba,
        }) => PreviewContent::Image {
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

fn safe_attachment_name(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment.bin");
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if cleaned.trim_matches('_').trim().is_empty() {
        "attachment.bin".into()
    } else {
        cleaned
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
                        ui.add(egui::TextEdit::multiline(t).desired_rows(30));
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

/// 渲染上下文：跨递归调用透传基础信息 + 推进图片游标 + 上传纹理。
struct RenderCtx<'a> {
    base_size: f32,
    panel_bg: egui::Color32,
    default_text: egui::Color32,
    zoom: f32,
    image_cursor: usize,
    images: &'a [DecodedImage],
    textures: &'a mut Vec<Option<egui::TextureHandle>>,
    path_id: &'a str,
}

/// 把富文本块按视觉期望渲染。每个块之间留一点垂直间距。
///
/// `base_size` 给到 15px —— egui 默认 14 在中文字体上偏细，邮件正文读起来容易糊。
fn render_blocks(ui: &mut egui::Ui, blocks: &[Block], ctx: &mut RenderCtx<'_>) {
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            ui.add_space(4.0);
        }
        render_one_block(ui, block, ctx);
    }
}

/// 渲染一个块：先按 `block.bg` 包一层 Frame，再按 `block.kind` 分发。
fn render_one_block(ui: &mut egui::Ui, block: &Block, ctx: &mut RenderCtx<'_>) {
    let bg_for_inner = block
        .bg
        .map(|rgb| egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
        .unwrap_or(ctx.panel_bg);
    let frame = egui::Frame::default()
        .fill(bg_for_inner)
        .inner_margin(if block.bg.is_some() {
            egui::Margin::symmetric(8.0, 4.0)
        } else {
            egui::Margin::ZERO
        });
    frame.show(ui, |ui| {
        // 临时切换上下文背景色，影响子节点对比度判断
        let prev_bg = ctx.panel_bg;
        ctx.panel_bg = bg_for_inner;
        render_block_inner(ui, block, ctx);
        ctx.panel_bg = prev_bg;
    });
}

fn render_block_inner(ui: &mut egui::Ui, block: &Block, ctx: &mut RenderCtx<'_>) {
    match &block.kind {
        BlockKind::Rule => {
            ui.separator();
        }
        BlockKind::Heading(_, spans) => {
            render_aligned_spans(ui, spans, block.align, ctx);
            ui.add_space(2.0);
        }
        BlockKind::Paragraph(spans) => {
            render_aligned_spans(ui, spans, block.align, ctx);
        }
        BlockKind::Quote(spans) => {
            // 引用块：稍微调整底色——深色主题下拉亮一点，浅色主题下拉暗一点。
            let quote_bg = shift_toward(ctx.panel_bg, ctx.default_text, 0.08);
            egui::Frame::default()
                .fill(quote_bg)
                .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                .show(ui, |ui| {
                    let prev = ctx.panel_bg;
                    ctx.panel_bg = quote_bg;
                    render_aligned_spans(ui, spans, block.align, ctx);
                    ctx.panel_bg = prev;
                });
        }
        BlockKind::ListItem(spans) => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label("•");
                render_aligned_spans(ui, spans, block.align, ctx);
            });
        }
        BlockKind::Image(_) => {
            render_inline_image(ui, block.align, ctx);
        }
        BlockKind::Table(rows) => {
            render_table(ui, rows, ctx);
        }
    }
}

/// 用 `egui::Grid` 渲染一个表格：每个 cell 内容递归走 `render_blocks`。
/// 邮件 table 普遍只用作"两列对齐 / 标签—值"排版，简单 grid 足够；
/// colspan/rowspan 用占位空 cell 模拟不做精细处理。
fn render_table(ui: &mut egui::Ui, rows: &[Vec<TableCell>], ctx: &mut RenderCtx<'_>) {
    let id = egui::Id::new(("mailx-table", ctx.path_id, ui.next_auto_id()));
    let max_cols = rows
        .iter()
        .map(|r| r.iter().map(|c| c.colspan as usize).sum::<usize>())
        .max()
        .unwrap_or(0);
    if max_cols == 0 {
        return;
    }
    egui::Grid::new(id)
        .num_columns(max_cols)
        .spacing(egui::vec2(8.0, 4.0))
        .striped(false)
        .show(ui, |ui| {
            for row in rows {
                let mut col_used = 0usize;
                for cell in row {
                    let bg_color = cell
                        .bg
                        .map(|rgb| egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
                        .unwrap_or(ctx.panel_bg);
                    let frame = egui::Frame::default()
                        .fill(bg_color)
                        .inner_margin(egui::Margin::symmetric(6.0, 3.0));
                    frame.show(ui, |ui| {
                        let prev_bg = ctx.panel_bg;
                        ctx.panel_bg = bg_color;
                        let cell_align = cell.align;
                        if cell.blocks.is_empty() {
                            ui.label(if cell.is_header { " " } else { "" });
                        } else {
                            for (i, b) in cell.blocks.iter().enumerate() {
                                if i > 0 {
                                    ui.add_space(2.0);
                                }
                                let mut shadow = b.clone();
                                if shadow.align == Align::Start {
                                    shadow.align = cell_align;
                                }
                                if cell.is_header {
                                    if let BlockKind::Paragraph(ref mut spans) = shadow.kind {
                                        for s in spans.iter_mut() {
                                            s.style.bold = true;
                                        }
                                    }
                                }
                                render_one_block(ui, &shadow, ctx);
                            }
                        }
                        ctx.panel_bg = prev_bg;
                    });
                    col_used += cell.colspan.max(1) as usize;
                }
                // 用空 label 占位补齐到 max_cols，避免 Grid 列数不一致时 layout 漂移
                while col_used < max_cols {
                    ui.label("");
                    col_used += 1;
                }
                ui.end_row();
            }
        });
}

fn render_inline_image(ui: &mut egui::Ui, align: Align, ctx: &mut RenderCtx<'_>) {
    let idx = ctx.image_cursor;
    ctx.image_cursor += 1;
    render_image_at(ui, idx, align, false, ctx);
}

fn render_standalone_images(ui: &mut egui::Ui, start: usize, ctx: &mut RenderCtx<'_>) {
    for idx in start..ctx.images.len() {
        if idx > start {
            ui.add_space(4.0);
        }
        render_image_at(ui, idx, Align::Start, true, ctx);
    }
}

fn render_image_at(
    ui: &mut egui::Ui,
    idx: usize,
    align: Align,
    show_label: bool,
    ctx: &mut RenderCtx<'_>,
) {
    if idx >= ctx.images.len() {
        return;
    }
    let img = &ctx.images[idx];
    let layout_align = match align {
        Align::Center => Some(egui::Align::Center),
        Align::End => Some(egui::Align::Max),
        _ => None,
    };
    let mut render = |ui: &mut egui::Ui| {
        if show_label {
            ui.weak(&img.label);
        }
        if let Some(pixels) = &img.pixels {
            if ctx.textures[idx].is_none() {
                let color_img = egui::ColorImage::from_rgba_unmultiplied(
                    [pixels.width as usize, pixels.height as usize],
                    &pixels.rgba,
                );
                ctx.textures[idx] = Some(ui.ctx().load_texture(
                    format!("inline-{}-{}", ctx.path_id, idx),
                    color_img,
                    egui::TextureOptions::LINEAR,
                ));
            }
            if let Some(tex) = &ctx.textures[idx] {
                let avail = ui.available_width();
                let natural = tex.size_vec2();
                let scale = if natural.x > 0.0 {
                    let fit = if natural.x > avail {
                        avail / natural.x
                    } else {
                        1.0
                    };
                    (fit * ctx.zoom).max(0.05)
                } else {
                    ctx.zoom
                };
                ui.image((tex.id(), natural * scale));
            }
        } else {
            let _ = (&img.label, &img.src_hint);
        }
    };
    if let Some(a) = layout_align {
        ui.with_layout(egui::Layout::top_down(a), |ui| render(ui));
    } else {
        render(ui);
    }
}

/// 按 align 决定行内 layout：center / right 用 `with_layout` 整体居中或右对齐；
/// 其它情况维持现有 `horizontal_wrapped` 左起布局。
fn render_aligned_spans(ui: &mut egui::Ui, spans: &[Span], align: Align, ctx: &mut RenderCtx<'_>) {
    if spans.is_empty() {
        return;
    }
    match align {
        Align::Center => {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                render_spans_wrapped(ui, spans, ctx);
            });
        }
        Align::End => {
            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                render_spans_wrapped(ui, spans, ctx);
            });
        }
        _ => render_spans_wrapped(ui, spans, ctx),
    }
}

/// 渲染一段 spans —— 遇到 `br_after` 就切行。每行用 `horizontal_wrapped`，
/// 行内相邻 label / hyperlink 间 spacing 压到 0，避免出现"每个词之间都有大空格"。
fn render_spans_wrapped(ui: &mut egui::Ui, spans: &[Span], ctx: &RenderCtx<'_>) {
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
            ui.add_space(ctx.base_size * 0.6);
            continue;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for s in &line {
                if s.text.is_empty() {
                    continue;
                }
                draw_span(ui, s, ctx);
            }
        });
    }
}

fn draw_span(ui: &mut egui::Ui, span: &Span, ctx: &RenderCtx<'_>) {
    let text = &span.text;
    let st = &span.style;
    let size = (ctx.base_size * st.size.max(0.5)).clamp(10.0, ctx.base_size * 3.0);
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
            if contrast_ratio(c, ctx.panel_bg) >= 3.5 {
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
        rt = rt.color(ctx.default_text);
    }

    if let Some(href) = &st.href {
        ui.add(egui::Hyperlink::from_label_and_url(rt, href).open_in_new_tab(true))
            .on_hover_text(href);
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
    let mix = |x: u8, y: u8| {
        ((x as f32) * (1.0 - a) + (y as f32) * a)
            .round()
            .clamp(0.0, 255.0) as u8
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_attachment_name_removes_path_and_invalid_chars() {
        assert_eq!(safe_attachment_name("../dir/report?.txt"), "report_.txt");
        assert_eq!(safe_attachment_name(r#"bad\name:1.txt"#), "bad_name_1.txt");
        assert_eq!(safe_attachment_name("\n"), "attachment.bin");
    }
}
