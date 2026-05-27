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

    if let Err(e) = ensure_desktop_icon() {
        tracing::warn!("生成桌面图标失败: {e}");
    }

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

#[cfg(target_os = "linux")]
fn ensure_desktop_icon() -> std::io::Result<()> {
    let exe_path = std::env::current_exe()?;
    let mut targets = Vec::new();
    if let Some(dir) = local_applications_dir() {
        targets.push(dir.join("dev.zzhtl.mailx.desktop"));
    }
    if let Some(dir) = desktop_dir() {
        targets.push(dir.join("mailx.desktop"));
    }
    for target in targets {
        let _ = ensure_desktop_shortcut(&target, &exe_path)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_desktop_icon() -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_desktop_shortcut(path: &std::path::Path, exe_path: &std::path::Path) -> std::io::Result<bool> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if desktop_exec_path(&existing)
            .map(|exec| std::path::PathBuf::from(exec) == exe_path)
            .unwrap_or(false)
        {
            return Ok(false);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, desktop_entry_for(exe_path))?;
    set_executable(path)?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn desktop_entry_for(exe_path: &std::path::Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=mailx\nComment=mailx 邮件客户端\nExec={}\nIcon=mail-message-new\nTerminal=false\nCategories=Network;Email;\nStartupWMClass=mailx\n",
        quote_desktop_exec(exe_path)
    )
}

#[cfg(target_os = "linux")]
fn quote_desktop_exec(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "linux")]
fn desktop_exec_path(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let value = line.trim().strip_prefix("Exec=")?.trim();
        if let Some(rest) = value.strip_prefix('"') {
            let mut out = String::new();
            let mut escaped = false;
            for c in rest.chars() {
                if escaped {
                    out.push(c);
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == '"' {
                    return Some(out);
                }
                out.push(c);
            }
            None
        } else {
            value.split_whitespace().next().map(|s| s.to_string())
        }
    })
}

#[cfg(target_os = "linux")]
fn set_executable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
}

#[cfg(target_os = "linux")]
fn local_applications_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local/share")))
        .map(|dir| dir.join("applications"))
}

#[cfg(target_os = "linux")]
fn desktop_dir() -> Option<std::path::PathBuf> {
    xdg_user_dir("XDG_DESKTOP_DIR")
        .or_else(|| {
            let home = home_dir()?;
            let zh = home.join("桌面");
            if zh.is_dir() {
                return Some(zh);
            }
            Some(home.join("Desktop"))
        })
}

#[cfg(target_os = "linux")]
fn xdg_user_dir(key: &str) -> Option<std::path::PathBuf> {
    let path = home_dir()?.join(".config/user-dirs.dirs");
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')) else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            return None;
        }
        if let Some(suffix) = value.strip_prefix("$HOME/") {
            return home_dir().map(|home| home.join(suffix));
        }
        if value == "$HOME" {
            return home_dir();
        }
        return Some(std::path::PathBuf::from(value));
    }
    None
}

#[cfg(target_os = "linux")]
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn desktop_exec_path_reads_quoted_exec() {
        let content = "[Desktop Entry]\nExec=\"/tmp/mail x\" --ignored\n";

        assert_eq!(desktop_exec_path(content).as_deref(), Some("/tmp/mail x"));
    }

    #[test]
    fn desktop_shortcut_keeps_existing_file_when_exec_is_same() {
        let tmp = tempfile::tempdir().unwrap();
        let shortcut = tmp.path().join("mailx.desktop");
        let exe = tmp.path().join("mailx");
        std::fs::write(&shortcut, desktop_entry_for(&exe)).unwrap();
        let before = std::fs::read_to_string(&shortcut).unwrap();

        let changed = ensure_desktop_shortcut(&shortcut, &exe).unwrap();

        assert!(!changed);
        assert_eq!(std::fs::read_to_string(&shortcut).unwrap(), before);
    }

    #[test]
    fn desktop_shortcut_overwrites_when_exec_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let shortcut = tmp.path().join("mailx.desktop");
        let old_exe = tmp.path().join("old-mailx");
        let new_exe = tmp.path().join("new-mailx");
        std::fs::write(&shortcut, desktop_entry_for(&old_exe)).unwrap();

        let changed = ensure_desktop_shortcut(&shortcut, &new_exe).unwrap();

        assert!(changed);
        assert_eq!(
            desktop_exec_path(&std::fs::read_to_string(&shortcut).unwrap()).unwrap(),
            new_exe.to_string_lossy().as_ref()
        );
    }
}
