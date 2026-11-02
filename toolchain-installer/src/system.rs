use crate::privileged::{run as run_privileged, shell_quote};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CORNERS: [(&str, &str); 4] = [
    ("左上角", "wvous-tl-corner"),
    ("右上角", "wvous-tr-corner"),
    ("左下角", "wvous-bl-corner"),
    ("右下角", "wvous-br-corner"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct SystemStatus {
    pub active_hot_corners: Vec<String>,
    pub rosetta_installed: bool,
    pub auto_login_enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeakPasswordOutcome {
    pub account: String,
    pub policy_path: PathBuf,
    pub previous_policy_path: Option<PathBuf>,
    pub previous_global_policy_path: Option<PathBuf>,
}

pub fn detect() -> Result<SystemStatus, String> {
    Ok(SystemStatus {
        active_hot_corners: active_hot_corners()?,
        rosetta_installed: rosetta_installed(),
        auto_login_enabled: auto_login_enabled()?,
    })
}

pub fn active_hot_corners() -> Result<Vec<String>, String> {
    let mut active = Vec::new();
    for (label, key) in CORNERS {
        if read_dock_integer(key)? > 1 {
            active.push(format!("{label}（{key}）"));
        }
    }
    Ok(active)
}

pub fn disable_hot_corners() -> Result<(), String> {
    for (_, key) in CORNERS {
        let output = Command::new("/usr/bin/defaults")
            .args(["write", "com.apple.dock", key, "-int", "1"])
            .output()
            .map_err(|error| format!("无法修改四角设置：{error}"))?;
        if !output.status.success() {
            return Err(format!(
                "无法关闭 {}：{}",
                key,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    for key in [
        "wvous-tl-modifier",
        "wvous-tr-modifier",
        "wvous-bl-modifier",
        "wvous-br-modifier",
    ] {
        let output = Command::new("/usr/bin/defaults")
            .args(["write", "com.apple.dock", key, "-int", "0"])
            .output()
            .map_err(|error| format!("无法修改四角修饰键设置：{error}"))?;
        if !output.status.success() {
            return Err(format!(
                "无法清除 {}：{}",
                key,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    let output = Command::new("/usr/bin/killall")
        .arg("Dock")
        .output()
        .map_err(|error| format!("无法重新载入 Dock：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "设置已写入，但 Dock 重新载入失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub fn rosetta_installed() -> bool {
    Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists()
}

pub fn install_rosetta() -> Result<(), String> {
    if rosetta_installed() {
        Ok(())
    } else {
        run_privileged("/usr/sbin/softwareupdate --install-rosetta --agree-to-license")
    }
}

pub fn apply_weak_password() -> Result<WeakPasswordOutcome, String> {
    let account = current_account()?;
    let state = state_dir(&account)?;
    fs::create_dir_all(&state).map_err(|error| format!("无法创建状态目录：{error}"))?;

    let policy_path = state.join("weak-password-policy.plist");

    let previous_policy_path = backup_policy(
        &state,
        Some(&account),
        "password-policy-before-weak-password",
        "No account-specific password policy existed before Display Doctor.\n",
    )?;
    let previous_global_policy_path = backup_policy(
        &state,
        None,
        "global-password-policy-before-weak-password",
        "No global password policy existed before Display Doctor.\n",
    )?;

    let account_arg = shell_quote(&account);
    let command = format!(
        "/usr/bin/pwpolicy -clearaccountpolicies && /usr/bin/pwpolicy -u {account_arg} -clearaccountpolicies"
    );
    run_privileged(&command)?;

    // dscl's password prompt trims spaces, but explicit old/new arguments preserve
    // them. Using the same value twice verifies that the current password is exactly
    // one space without asking for another credential in the app.
    verify_space_password(&account)?;

    Ok(WeakPasswordOutcome {
        account,
        policy_path,
        previous_policy_path,
        previous_global_policy_path,
    })
}

pub fn restore_password_policy() -> Result<(), String> {
    let account = current_account()?;
    let state = state_dir(&account)?;
    let global_restore =
        policy_restore_command(&state, None, "global-password-policy-before-weak-password")?;
    let account_restore = policy_restore_command(
        &state,
        Some(&account),
        "password-policy-before-weak-password",
    )?;
    let command = format!("{global_restore} && {account_restore}");
    run_privileged(&command)
}

pub fn auto_login_enabled() -> Result<bool, String> {
    let account = current_account()?;
    Ok(
        read_loginwindow_string("autoLoginUser")?.as_deref() == Some(account.as_str())
            && Path::new("/etc/kcpassword").exists(),
    )
}

pub fn enable_space_auto_login() -> Result<(), String> {
    let account = current_account()?;
    let filevault = Command::new("/usr/bin/fdesetup")
        .arg("status")
        .output()
        .map_err(|error| format!("无法读取 FileVault 状态：{error}"))?;
    let filevault_status = String::from_utf8_lossy(&filevault.stdout);
    if !filevault_status.contains("FileVault is Off") {
        return Err(
            "FileVault 已开启时 macOS 不允许自动登录。请先关闭 FileVault，再启用自动登录。"
                .to_string(),
        );
    }
    verify_space_password(&account)?;

    let state = state_dir(&account)?;
    fs::create_dir_all(&state).map_err(|error| format!("无法创建状态目录：{error}"))?;
    let encoded = state.join("autologin-kcpassword-space");
    fs::write(&encoded, encode_kcpassword(b" "))
        .map_err(|error| format!("无法生成自动登录凭据：{error}"))?;

    let account_arg = shell_quote(&account);
    let encoded_arg = shell_quote(&encoded);
    let command = format!(
        "/bin/cp {encoded_arg} /etc/kcpassword && /usr/sbin/chown root:wheel /etc/kcpassword && /bin/chmod 600 /etc/kcpassword && /usr/bin/defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser -string {account_arg} && (/usr/bin/defaults delete /Library/Preferences/com.apple.loginwindow autoLoginUserScreenLocked >/dev/null 2>&1 || true)"
    );
    run_privileged(&command)
}

pub fn disable_auto_login() -> Result<(), String> {
    run_privileged(
        "/bin/rm -f /etc/kcpassword && (/usr/bin/defaults delete /Library/Preferences/com.apple.loginwindow autoLoginUser >/dev/null 2>&1 || true) && (/usr/bin/defaults delete /Library/Preferences/com.apple.loginwindow autoLoginUserScreenLocked >/dev/null 2>&1 || true)",
    )
}

fn read_dock_integer(key: &str) -> Result<i64, String> {
    let output = Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.dock", key])
        .output()
        .map_err(|error| format!("无法读取四角设置：{error}"))?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout);
        return value
            .trim()
            .parse::<i64>()
            .map_err(|error| format!("无法解析 {key}：{error}"));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if missing_default_key(&stderr) {
        Ok(0)
    } else {
        Err(format!("无法读取 {key}：{}", stderr.trim()))
    }
}

fn read_loginwindow_string(key: &str) -> Result<Option<String>, String> {
    let output = Command::new("/usr/bin/defaults")
        .args(["read", "/Library/Preferences/com.apple.loginwindow", key])
        .output()
        .map_err(|error| format!("无法读取自动登录设置：{error}"))?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((!value.is_empty()).then_some(value))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if missing_default_key(&stderr) {
            Ok(None)
        } else {
            Err(format!("无法读取自动登录设置：{}", stderr.trim()))
        }
    }
}

fn missing_default_key(stderr: &str) -> bool {
    stderr.contains("Could not find key")
        || stderr.contains("does not exist")
        || stderr.contains("not found")
}

fn backup_policy(
    state: &Path,
    account: Option<&str>,
    stem: &str,
    missing_message: &str,
) -> Result<Option<PathBuf>, String> {
    let backup_path = state.join(format!("{stem}.plist"));
    let missing_path = state.join(format!("{stem}.missing"));
    if backup_path.exists() {
        return Ok(Some(backup_path));
    }
    if missing_path.exists() {
        return Ok(None);
    }

    let mut command = Command::new("/usr/bin/pwpolicy");
    if let Some(account) = account {
        command.args(["-u", account]);
    }
    let output = command
        .arg("getaccountpolicies")
        .output()
        .map_err(|error| format!("无法读取现有密码策略：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "无法读取现有密码策略：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    if let Some(plist_start) = text.find("<?xml") {
        fs::write(&backup_path, &text.as_bytes()[plist_start..])
            .map_err(|error| format!("无法备份现有密码策略：{error}"))?;
        Ok(Some(backup_path))
    } else {
        fs::write(&missing_path, missing_message)
            .map_err(|error| format!("无法记录密码策略状态：{error}"))?;
        Ok(None)
    }
}

fn policy_restore_command(
    state: &Path,
    account: Option<&str>,
    stem: &str,
) -> Result<String, String> {
    let backup = state.join(format!("{stem}.plist"));
    let missing = state.join(format!("{stem}.missing"));
    let account_arg = account
        .map(|account| format!(" -u {}", shell_quote(account)))
        .unwrap_or_default();

    if backup.exists() {
        Ok(format!(
            "/usr/bin/pwpolicy{account_arg} -setaccountpolicies {}",
            shell_quote(&backup)
        ))
    } else if missing.exists() {
        Ok(format!(
            "/usr/bin/pwpolicy{account_arg} -clearaccountpolicies"
        ))
    } else {
        Err(format!("没有找到密码策略备份：{stem}"))
    }
}

fn current_account() -> Result<String, String> {
    let output = Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .map_err(|error| format!("无法读取当前账户：{error}"))?;
    if !output.status.success() {
        return Err("无法读取当前账户".to_string());
    }
    let account = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if account.is_empty() {
        Err("当前账户名为空".to_string())
    } else {
        Ok(account)
    }
}

fn state_dir(account: &str) -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or_else(|| "无法确定当前用户目录".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/Display Doctor")
        .join(format!("system-{account}")))
}

fn verify_space_password(account: &str) -> Result<(), String> {
    let user_path = format!("/Users/{account}");
    let verification = Command::new("/usr/bin/dscl")
        .args([".", "-passwd", &user_path, " ", " "])
        .output()
        .map_err(|error| format!("无法验证一个空格密码：{error}"))?;
    if verification.status.success() {
        Ok(())
    } else {
        Err(
            "弱密码策略已开启，但当前登录密码不是恰好一个空格。请在“系统设置 → 登录密码”中将其改为一个空格。"
                .to_string(),
        )
    }
}

fn encode_kcpassword(password: &[u8]) -> Vec<u8> {
    const KEY: [u8; 11] = [
        0x7d, 0x89, 0x52, 0x23, 0xd2, 0xbc, 0xdd, 0xea, 0xa3, 0xb9, 0x1f,
    ];
    password
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ KEY[index % KEY.len()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_account_names_without_shell_interpolation() {
        assert_eq!(shell_quote("normal-user"), "'normal-user'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn recognizes_missing_defaults_keys_on_macos_27() {
        assert!(missing_default_key(
            "Error: Could not find key 'wvous-tl-corner' in domain 'com.apple.dock'."
        ));
        assert!(!missing_default_key("Permission denied"));
    }

    #[test]
    fn encodes_a_one_space_auto_login_password() {
        assert_eq!(encode_kcpassword(b" "), vec![0x5d]);
    }
}
