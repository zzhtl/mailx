//! 按邮箱域名命中的主流供应商预设（M1 占位，M2 接入真实 IMAP/SMTP）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// 授权码 / 应用专用密码（通过 IMAP/SMTP LOGIN / AUTH PLAIN）。
    AppPassword,
    /// OAuth2 XOAUTH2。
    OAuth2,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub name: &'static str,
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub auth: AuthKind,
    /// 163/126 要求 IMAP 登录后立即发 `ID` 命令。
    pub requires_imap_id: bool,
}

pub const GMAIL: ProviderPreset = ProviderPreset {
    name: "Gmail",
    imap_host: "imap.gmail.com",
    imap_port: 993,
    smtp_host: "smtp.gmail.com",
    smtp_port: 465,
    auth: AuthKind::OAuth2,
    requires_imap_id: false,
};

pub const NETEASE_163: ProviderPreset = ProviderPreset {
    name: "163",
    imap_host: "imap.163.com",
    imap_port: 993,
    smtp_host: "smtp.163.com",
    smtp_port: 465,
    auth: AuthKind::AppPassword,
    requires_imap_id: true,
};

pub const NETEASE_126: ProviderPreset = ProviderPreset {
    name: "126",
    imap_host: "imap.126.com",
    imap_port: 993,
    smtp_host: "smtp.126.com",
    smtp_port: 465,
    auth: AuthKind::AppPassword,
    requires_imap_id: true,
};

pub const TENCENT_EXMAIL: ProviderPreset = ProviderPreset {
    name: "腾讯企业邮箱",
    imap_host: "imap.exmail.qq.com",
    imap_port: 993,
    smtp_host: "smtp.exmail.qq.com",
    smtp_port: 465,
    auth: AuthKind::AppPassword,
    requires_imap_id: false,
};

pub const QQ_MAIL: ProviderPreset = ProviderPreset {
    name: "QQ 邮箱",
    imap_host: "imap.qq.com",
    imap_port: 993,
    smtp_host: "smtp.qq.com",
    smtp_port: 465,
    auth: AuthKind::AppPassword,
    requires_imap_id: false,
};

pub const OUTLOOK: ProviderPreset = ProviderPreset {
    name: "Outlook / Hotmail",
    imap_host: "outlook.office365.com",
    imap_port: 993,
    smtp_host: "smtp.office365.com",
    smtp_port: 587,
    auth: AuthKind::AppPassword,
    requires_imap_id: false,
};

/// UI 下拉可选的全部预设。顺序即展示顺序。
pub const ALL_PRESETS: &[ProviderPreset] = &[
    NETEASE_163,
    NETEASE_126,
    TENCENT_EXMAIL,
    QQ_MAIL,
    GMAIL,
    OUTLOOK,
];

/// 根据邮箱地址的域名部分匹配预设。未命中时返回 None，由调用方走通用模式。
pub fn match_by_email(email: &str) -> Option<ProviderPreset> {
    let domain = email.rsplit_once('@')?.1.to_ascii_lowercase();
    Some(match domain.as_str() {
        "gmail.com" | "googlemail.com" => GMAIL,
        "163.com" => NETEASE_163,
        "126.com" => NETEASE_126,
        "exmail.qq.com" => TENCENT_EXMAIL,
        "qq.com" | "foxmail.com" => QQ_MAIL,
        "outlook.com" | "hotmail.com" | "live.com" => OUTLOOK,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_gmail() {
        assert_eq!(match_by_email("a@gmail.com").unwrap().name, "Gmail");
    }

    #[test]
    fn matches_163_case_insensitive() {
        assert!(match_by_email("A@163.COM").unwrap().requires_imap_id);
    }

    #[test]
    fn unknown_returns_none() {
        assert!(match_by_email("a@example.org").is_none());
    }
}
