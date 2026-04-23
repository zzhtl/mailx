# 打包

```bash
cargo install cargo-packager --locked
```

## Linux (.deb / AppImage)

```bash
cd crates/mailx-app
cargo packager --release --formats deb appimage
```

运行时依赖：`libwebkit2gtk-4.1`（正文浏览器）、`libgtk-3`、`libreoffice`（Office 附件预览可选）。

## Windows (.msi)

在 Windows 环境里：

```powershell
cd crates\mailx-app
cargo packager --release --formats msi
```

## macOS (.app / .dmg)

```bash
cd crates/mailx-app
cargo packager --release --formats app dmg
```

## 手动 release 构建

```bash
cargo build --release -p mailx-app
./target/release/mailx
```

## 系统依赖（开发环境）

Debian/Ubuntu:
```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libssl-dev libreoffice
```

macOS:
```bash
brew install openssl libreoffice
```

Windows: MSVC 工具链 + WebView2 Runtime。
