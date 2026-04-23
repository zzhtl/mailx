//! mailx —— eframe 入口。
//!
//! UI 只读视图状态，所有读写通过 CoreHandle 的命令/事件通道。

mod ui;

use eframe::egui;
use mailx_core::{spawn as spawn_core, CoreHandle};
use mailx_store::Store;
use tracing_subscriber::EnvFilter;

use ui::MailxApp;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,mailx=debug".into()),
        )
        .with_target(false)
        .init();

    // 专门起一个 tokio runtime 用来跑 "initialize store" 的一次性异步调用
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let store: Store = rt
        .block_on(Store::open_default())
        .expect("open store");
    drop(rt);

    let core: CoreHandle = spawn_core(store.clone());
    // 首次启动加载账户列表
    core.send(mailx_core::Command::LoadAccounts);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("mailx")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "mailx",
        options,
        Box::new(|cc| {
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(MailxApp::new(core)))
        }),
    )
}

fn install_cjk_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        tracing::warn!("未找到系统 CJK 字体，中文可能显示为豆腐块");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), egui::FontData::from_owned(bytes));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "cjk".into());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("cjk".into());
    ctx.set_fonts(fonts);
}
