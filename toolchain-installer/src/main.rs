#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;

#[cfg(target_os = "macos")]
mod macos_shell;

#[cfg(target_os = "macos")]
fn main() -> impl std::process::Termination {
    if let Some(exit_code) = run_internal_command() {
        std::process::exit(exit_code);
    }
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

#[cfg(target_os = "macos")]
fn run_internal_command() -> Option<i32> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return None;
    };
    match command {
        "__btm-check" => {
            let [path] = &args[1..] else {
                eprintln!("error: invalid internal BTM check arguments");
                return Some(1);
            };
            return Some(
                match mac_settings_ui::check_background_store_for_helper(path) {
                    Ok(()) => 0,
                    Err(error) => {
                        eprintln!("error: {error}");
                        1
                    }
                },
            );
        }
        "__btm-remove-related" => {
            let [uid, home, identifiers @ ..] = &args[1..] else {
                eprintln!("error: invalid internal related BTM arguments");
                return Some(1);
            };
            if identifiers.is_empty() {
                eprintln!("error: no BTM identifiers supplied");
                return Some(1);
            }
            let uid = match uid.parse::<u32>() {
                Ok(uid) => uid,
                Err(_) => {
                    eprintln!("error: invalid internal BTM uid");
                    return Some(1);
                }
            };
            return Some(
                match mac_settings_ui::remove_related_background_records_for_helper(
                    uid,
                    home,
                    identifiers,
                ) {
                    Ok(output) => {
                        println!("{output}");
                        0
                    }
                    Err(error) => {
                        eprintln!("error: {error}");
                        1
                    }
                },
            );
        }
        "__btm-remove" => {}
        _ => return None,
    }
    let [_, path, uid, generated_uid, identifiers @ ..] = args.as_slice() else {
        eprintln!("error: invalid internal BTM arguments");
        return Some(1);
    };
    if identifiers.is_empty() {
        eprintln!("error: no BTM identifiers supplied");
        return Some(1);
    }
    let uid = match uid.parse::<u32>() {
        Ok(uid) => uid,
        Err(_) => {
            eprintln!("error: invalid internal BTM uid");
            return Some(1);
        }
    };
    match mac_settings_ui::remove_background_records_for_helper(
        path,
        uid,
        generated_uid,
        identifiers,
    ) {
        Ok(output) => {
            println!("{output}");
            Some(0)
        }
        Err(error) => {
            eprintln!("error: {error}");
            Some(1)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("我的 Mac 设置和开发工具安装目前只支持 macOS");
}
