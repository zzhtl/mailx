//! 凭据存储：使用操作系统 Keychain（keyring crate）保存 IMAP/SMTP 密码或 OAuth token。

use anyhow::Result;
use keyring::Entry;

const SERVICE: &str = "dev.zzhtl.mailx";

pub fn save_secret(email: &str, secret: &str) -> Result<()> {
    Entry::new(SERVICE, email)?.set_password(secret)?;
    Ok(())
}

pub fn load_secret(email: &str) -> Result<String> {
    Ok(Entry::new(SERVICE, email)?.get_password()?)
}

pub fn delete_secret(email: &str) -> Result<()> {
    let entry = Entry::new(SERVICE, email)?;
    let _ = entry.delete_credential();
    Ok(())
}
