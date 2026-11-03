#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;

#[cfg(target_os = "macos")]
mod macos_shell;

#[cfg(target_os = "macos")]
fn main() -> impl std::process::Termination {
    env_logger::init();
    if let Err(error) = macos_shell::install_status_item() {
        eprintln!("无法创建菜单栏图标：{error}");
    }
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<mac_settings_ui::App>()),
        platform::AppOptions {
            title: "我的 Mac 设置和开发工具安装".to_owned(),
            size: (866, 1666),
            keep_alive_on_close: true,
            ..Default::default()
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("我的 Mac 设置和开发工具安装目前只支持 macOS");
}
