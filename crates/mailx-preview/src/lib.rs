//! mailx-preview —— 按扩展名分派附件预览。
//!
//! 返回与 UI 无关的中间结构：像素图（RGBA8）或纯文本。UI 侧再把像素图包成 `egui::ColorImage`。
//!
//! 注意：
//! - 超过 50 MB 的文件直接返回 `TooLarge`，交给 UI 提示"在外部程序打开"。
//! - PDF / Office 预览只渲染第 1 页，更多页交由 UI 的翻页控件驱动（未来扩展）。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

pub const PREVIEW_SIZE_LIMIT: u64 = 50 * 1024 * 1024;

/// 预览结果。
pub enum Preview {
    /// RGBA8 像素图 + 宽高（UI 侧转 `egui::ColorImage`）。
    Image { width: u32, height: u32, rgba: Vec<u8> },
    /// 纯文本（UI 侧显示，已经按 encoding_rs 转成 UTF-8）。
    Text(String),
    /// 文件超过阈值，交给外部程序打开。
    TooLarge,
    /// 无法识别的类型。
    Unsupported,
}

pub fn preview_file(path: &Path) -> Result<Preview> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > PREVIEW_SIZE_LIMIT {
        return Ok(Preview::TooLarge);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => preview_image(path),
        "pdf" => preview_pdf(path, 0),
        "txt" | "md" | "log" | "csv" | "json" | "xml" | "yml" | "yaml"
        | "rs" | "py" | "go" | "js" | "ts" | "html" | "css" => preview_text(path),
        "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "doc" | "xls" | "ppt" => {
            preview_office(path)
        }
        _ => Ok(Preview::Unsupported),
    }
}

fn preview_image(path: &Path) -> Result<Preview> {
    let img = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
        .context("图片解码失败")?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Ok(Preview::Image {
        width: w,
        height: h,
        rgba: img.into_raw(),
    })
}

/// 从内存字节解码图片为 RGBA8（用于邮件正文中的内联图/附件图在 egui 侧直接渲染）。
pub fn decode_image_bytes(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let img = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
        .context("图片解码失败")?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Ok((w, h, img.into_raw()))
}

fn preview_text(path: &Path) -> Result<Preview> {
    let bytes = std::fs::read(path)?;
    // 先尝试 UTF-8，再按 GB18030 / Big5 回退
    let (cow, _, _had_errors) = encoding_rs::UTF_8.decode(&bytes);
    let text = if !cow.contains('\u{FFFD}') {
        cow.into_owned()
    } else {
        let (cow, _, _) = encoding_rs::GB18030.decode(&bytes);
        cow.into_owned()
    };
    Ok(Preview::Text(text))
}

/// 用 pdfium 渲染 PDF 指定页为 RGBA 图像。
pub fn preview_pdf(path: &Path, page_index: u16) -> Result<Preview> {
    use pdfium_render::prelude::*;
    let pdfium = Pdfium::new(
        Pdfium::bind_to_system_library()
            .or_else(|_| Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(".")))
            .map_err(|e| anyhow!("无法加载 pdfium 动态库: {e}"))?,
    );
    let doc = pdfium.load_pdf_from_file(path, None)?;
    let page = doc
        .pages()
        .get(page_index)
        .map_err(|e| anyhow!("翻到第 {} 页失败: {e}", page_index + 1))?;
    let cfg = PdfRenderConfig::new().set_target_width(1024);
    let img = page.render_with_config(&cfg)?.as_image().to_rgba8();
    let (w, h) = img.dimensions();
    Ok(Preview::Image {
        width: w,
        height: h,
        rgba: img.into_raw(),
    })
}

/// 调用 `libreoffice --headless --convert-to pdf` 转 PDF 后走 PDF 预览。
/// 依赖系统已安装 LibreOffice；未安装时返回错误。
fn preview_office(path: &Path) -> Result<Preview> {
    let pdf = convert_office_to_pdf(path)?;
    preview_pdf(&pdf, 0)
}

pub fn convert_office_to_pdf(src: &Path) -> Result<PathBuf> {
    let outdir = tempfile::Builder::new()
        .prefix("mailx-office-")
        .tempdir()?;
    let status = std::process::Command::new("libreoffice")
        .arg("--headless")
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(outdir.path())
        .arg(src)
        .status()
        .context("调用 libreoffice 失败（请确认已安装）")?;
    if !status.success() {
        return Err(anyhow!("libreoffice 转换 PDF 失败"));
    }
    let stem = src
        .file_stem()
        .ok_or_else(|| anyhow!("无效文件名"))?;
    let pdf = outdir.path().join(stem).with_extension("pdf");
    // 把临时文件移到稳定位置，随进程结束清理
    let stable = std::env::temp_dir().join(format!(
        "mailx-office-{}.pdf",
        std::process::id()
    ));
    std::fs::copy(&pdf, &stable)?;
    drop(outdir);
    Ok(stable)
}
