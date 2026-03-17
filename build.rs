/// 构建脚本：将 tray_icon.ico 嵌入 exe（设置应用程序图标）。
///
/// winres crate 会生成 .rc 文件并调用 rc.exe / windres 进行编译链接。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        let icon_path = format!("{manifest_dir}/assets/tray_icon.ico");

        let mut res = winres::WindowsResource::new();
        res.set_icon(&icon_path);

        if let Err(e) = res.compile() {
            // 仅警告，不中断构建（环境未安装 rc.exe 时降级处理）
            eprintln!("cargo:warning=winres 编译图标资源失败: {e}");
        }
    }
}
