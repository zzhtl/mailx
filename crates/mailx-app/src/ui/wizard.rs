//! 添加账户向导。

use eframe::egui;
use mailx_core::{
    command::{AddAccountReq, ServerConfig},
    Command, CoreHandle,
};
use mailx_proto::{AuthKind, ProviderPreset, ALL_PRESETS};

/// 预设选择：None = 自动识别（按邮箱域名），Some(-1) = 自定义，Some(idx) = ALL_PRESETS 内的某项。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Auto,
    Preset(usize),
    Custom,
}

pub struct AddAccountState {
    pub email: String,
    pub secret: String,
    pub display_name: String,
    choice: Choice,
    imap_host: String,
    imap_port: String,
    smtp_host: String,
    smtp_port: String,
    requires_imap_id: bool,
    auth_oauth2: bool,
    error: Option<String>,
}

impl Default for AddAccountState {
    fn default() -> Self {
        Self {
            email: String::new(),
            secret: String::new(),
            display_name: String::new(),
            choice: Choice::Auto,
            imap_host: String::new(),
            imap_port: "993".into(),
            smtp_host: String::new(),
            smtp_port: "465".into(),
            requires_imap_id: false,
            auth_oauth2: false,
            error: None,
        }
    }
}

impl AddAccountState {
    fn apply_preset(&mut self, p: &ProviderPreset) {
        self.imap_host = p.imap_host.to_string();
        self.imap_port = p.imap_port.to_string();
        self.smtp_host = p.smtp_host.to_string();
        self.smtp_port = p.smtp_port.to_string();
        self.requires_imap_id = p.requires_imap_id;
        self.auth_oauth2 = matches!(p.auth, AuthKind::OAuth2);
    }
}

fn choice_label(c: Choice) -> String {
    match c {
        Choice::Auto => "自动识别（按邮箱域名）".into(),
        Choice::Custom => "自定义".into(),
        Choice::Preset(i) => ALL_PRESETS[i].name.to_string(),
    }
}

/// 返回 true 表示应关闭此向导。
pub fn show(ctx: &egui::Context, state: &mut AddAccountState, core: &CoreHandle) -> bool {
    let mut should_close = false;
    egui::Window::new("添加邮箱账户")
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            ui.label("邮箱地址");
            ui.text_edit_singleline(&mut state.email);
            ui.add_space(4.0);
            ui.label("授权码 / 应用专用密码 / OAuth access token");
            ui.add(egui::TextEdit::singleline(&mut state.secret).password(true));
            ui.add_space(4.0);
            ui.label("显示名（可选）");
            ui.text_edit_singleline(&mut state.display_name);

            ui.add_space(8.0);
            ui.separator();
            ui.label("邮箱类型");
            let prev = state.choice;
            egui::ComboBox::from_id_salt("provider_choice")
                .width(260.0)
                .selected_text(choice_label(state.choice))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.choice, Choice::Auto, choice_label(Choice::Auto));
                    for (i, p) in ALL_PRESETS.iter().enumerate() {
                        ui.selectable_value(
                            &mut state.choice,
                            Choice::Preset(i),
                            p.name,
                        );
                    }
                    ui.selectable_value(&mut state.choice, Choice::Custom, choice_label(Choice::Custom));
                });
            if state.choice != prev {
                // 切换时自动填充默认值
                match state.choice {
                    Choice::Preset(i) => {
                        let p = ALL_PRESETS[i];
                        state.apply_preset(&p);
                    }
                    Choice::Auto => {
                        state.imap_host.clear();
                        state.smtp_host.clear();
                        state.imap_port = "993".into();
                        state.smtp_port = "465".into();
                        state.requires_imap_id = false;
                        state.auth_oauth2 = false;
                    }
                    Choice::Custom => {}
                }
            }

            // 非自动模式下显示服务器参数编辑
            if !matches!(state.choice, Choice::Auto) {
                ui.add_space(6.0);
                egui::Grid::new("server_cfg")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("IMAP 服务器");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut state.imap_host).desired_width(200.0));
                            ui.label(":");
                            ui.add(egui::TextEdit::singleline(&mut state.imap_port).desired_width(60.0));
                        });
                        ui.end_row();
                        ui.label("SMTP 服务器");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut state.smtp_host).desired_width(200.0));
                            ui.label(":");
                            ui.add(egui::TextEdit::singleline(&mut state.smtp_port).desired_width(60.0));
                        });
                        ui.end_row();
                    });
                ui.checkbox(&mut state.requires_imap_id, "IMAP 登录后需发送 ID 命令（163/126）");
                ui.checkbox(&mut state.auth_oauth2, "使用 OAuth2（XOAUTH2）");
                ui.small("端口 993/465 默认走 SSL；Outlook SMTP 587 使用 STARTTLS。");
            } else {
                ui.add_space(6.0);
                ui.small(
                    "自动识别：Gmail / 163 / 126 / 腾讯企业邮箱 / QQ / Outlook 会按内置预设连接；\
                     其他域名按 imap.<域名>:993 / smtp.<域名>:465 尝试。",
                );
            }

            if let Some(err) = &state.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(0xc0, 0x39, 0x2b), err);
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    should_close = true;
                }
                if ui.button("测试并保存").clicked() {
                    match build_req(state) {
                        Ok(req) => {
                            core.send(Command::AddAccount(req));
                            should_close = true;
                        }
                        Err(msg) => state.error = Some(msg),
                    }
                }
            });
        });
    should_close
}

fn build_req(s: &AddAccountState) -> Result<AddAccountReq, String> {
    if !s.email.contains('@') {
        return Err("邮箱格式不正确".into());
    }
    if s.secret.is_empty() {
        return Err("请填写授权码 / 密码".into());
    }

    let server = match s.choice {
        Choice::Auto => None,
        Choice::Preset(_) | Choice::Custom => {
            let imap_port: u16 = s
                .imap_port
                .trim()
                .parse()
                .map_err(|_| "IMAP 端口必须是数字".to_string())?;
            let smtp_port: u16 = s
                .smtp_port
                .trim()
                .parse()
                .map_err(|_| "SMTP 端口必须是数字".to_string())?;
            if s.imap_host.trim().is_empty() || s.smtp_host.trim().is_empty() {
                return Err("IMAP / SMTP 服务器地址不能为空".into());
            }
            Some(ServerConfig {
                imap_host: s.imap_host.trim().to_string(),
                imap_port,
                smtp_host: s.smtp_host.trim().to_string(),
                smtp_port,
                auth: if s.auth_oauth2 { AuthKind::OAuth2 } else { AuthKind::AppPassword },
                requires_imap_id: s.requires_imap_id,
            })
        }
    };

    Ok(AddAccountReq {
        email: s.email.trim().to_string(),
        display_name: Some(s.display_name.clone()).filter(|v| !v.trim().is_empty()),
        secret: s.secret.clone(),
        server,
    })
}
