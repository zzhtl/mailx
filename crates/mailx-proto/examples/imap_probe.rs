//! 从环境变量读取凭据，连接 IMAP，列出 INBOX 最近 20 封主题。
//!
//! 用法：
//!     MAILX_EMAIL=you@163.com MAILX_PASSWORD=<auth-code> \
//!         cargo run -p mailx-proto --example imap_probe

use anyhow::{Context, Result};
use mailx_proto::presets::{match_by_email, AuthKind, ProviderPreset};
use mailx_proto::{ImapClient, ImapCredentials};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,mailx=debug,async_imap=warn")
        .init();

    let email = std::env::var("MAILX_EMAIL").context("set MAILX_EMAIL")?;
    let secret = std::env::var("MAILX_PASSWORD").context("set MAILX_PASSWORD")?;
    let preset = match_by_email(&email).unwrap_or_else(|| {
        eprintln!("domain unknown, falling back to generic IMAPS on port 993");
        ProviderPreset {
            name: "Generic",
            imap_host: Box::leak(email.rsplit('@').next().unwrap().to_string().into_boxed_str()),
            imap_port: 993,
            smtp_host: "",
            smtp_port: 465,
            auth: AuthKind::AppPassword,
            requires_imap_id: false,
        }
    });
    println!("using preset: {}", preset.name);

    let creds = ImapCredentials { email, secret, auth: preset.auth };
    let mut client = ImapClient::connect(&preset, &creds).await?;
    println!("login OK");

    let folders = client.list_folders().await?;
    for f in &folders {
        println!("  folder: {} (delim={:?})", f.name, f.delimiter);
    }

    let (uidvalidity, exists) = client.select("INBOX").await?;
    println!("INBOX: uidvalidity={uidvalidity} exists={exists}");

    let start = exists.saturating_sub(19).max(1);
    let seq = format!("{start}:*");
    // 注意：上面是 seq 序号，而 fetch_envelopes 按 UID 查。对最近 N 封，这里先用一种常见做法：
    // 1) 用 SEARCH ALL 得到 UID 列表，取最后 20 个；
    // 2) 这里简化为 UID 区间 `1:*` + 客户端裁剪。
    let _ = seq;
    let mut envelopes = client.fetch_envelopes("1:*").await?;
    envelopes.sort_by_key(|e| e.uid);
    let tail = envelopes.iter().rev().take(20);
    println!("\n最近 20 封：");
    for e in tail {
        println!(
            "  UID {:>6} | {} | {} | {}",
            e.uid,
            e.internal_date.as_deref().unwrap_or("?"),
            e.from.as_deref().unwrap_or("?"),
            e.subject.as_deref().unwrap_or("?")
        );
    }

    client.logout().await?;
    Ok(())
}
