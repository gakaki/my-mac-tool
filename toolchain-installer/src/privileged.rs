use std::ffi::OsStr;
use std::process::Command;

pub fn run(shell_command: &str) -> Result<(), String> {
    let apple_script = format!(
        "do shell script \"{}\" with administrator privileges",
        shell_command.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", &apple_script])
        .output()
        .map_err(|error| format!("无法请求管理员权限：{error}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("-128") || stderr.contains("User canceled") {
        Err("已取消管理员授权".to_string())
    } else {
        Err(format!("系统命令执行失败：{}", stderr.trim()))
    }
}

pub fn shell_quote(value: impl AsRef<OsStr>) -> String {
    format!(
        "'{}'",
        value.as_ref().to_string_lossy().replace('\'', "'\\''")
    )
}
