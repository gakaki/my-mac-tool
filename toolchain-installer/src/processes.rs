use crate::privileged::{run as run_privileged, shell_quote};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MonitorApp {
    pub display_name: String,
    pub app_path: String,
    pub executable_name: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct KillRule {
    pub id: String,
    pub name: String,
    pub match_terms: Vec<String>,
    #[serde(default)]
    pub startup_terms: Vec<String>,
    pub remove_startup: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProcessConfig {
    pub monitoring_enabled: bool,
    pub check_interval_seconds: u64,
    pub launch_grace_seconds: u64,
    pub missing_confirmations: u32,
    #[serde(default = "default_hang_confirmations")]
    pub hang_confirmations: u32,
    pub restart_cooldown_seconds: u64,
    pub terminate_timeout_seconds: u64,
    pub monitors: Vec<MonitorApp>,
    pub kill_rules: Vec<KillRule>,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            monitoring_enabled: true,
            check_interval_seconds: 5,
            launch_grace_seconds: 25,
            missing_confirmations: 2,
            hang_confirmations: 3,
            restart_cooldown_seconds: 60,
            terminate_timeout_seconds: 5,
            monitors: vec![MonitorApp {
                display_name: "UU远程".to_string(),
                app_path: "/Applications/UURemote.app".to_string(),
                executable_name: "UURemote".to_string(),
                enabled: true,
            }],
            kill_rules: vec![KillRule {
                id: "microsoft-auto-update".to_string(),
                name: "Microsoft AutoUpdate".to_string(),
                match_terms: vec![
                    "Microsoft AutoUpdate".to_string(),
                    "Microsoft Update Assistant".to_string(),
                ],
                startup_terms: vec![
                    "com.microsoft.update.agent".to_string(),
                    "com.microsoft.autoupdate.helper".to_string(),
                    "Microsoft AutoUpdate.app".to_string(),
                    "Microsoft Update Assistant.app".to_string(),
                ],
                remove_startup: true,
                enabled: true,
            }],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub stat: String,
    pub cpu: String,
    pub executable_path: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorSnapshot {
    pub app_path: String,
    pub display_name: String,
    pub status: String,
    pub pid: Option<u32>,
    pub message: String,
    pub restarts: u32,
}

#[derive(Default)]
struct AppRuntime {
    missing_checks: u32,
    stopped_checks: u32,
    last_restart: Option<Instant>,
    restarts: u32,
}

#[derive(Default)]
pub struct MonitorRuntime {
    apps: HashMap<String, AppRuntime>,
}

pub fn config_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/MacSettingsDevTools")
        .join("processes.json"))
}

pub fn load_config() -> Result<ProcessConfig, String> {
    let path = config_path()?;
    if !path.exists() {
        let config = ProcessConfig::default();
        save_config(&config)?;
        return Ok(config);
    }
    let bytes = fs::read(&path).map_err(|error| format!("无法读取进程规则：{error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("无法解析进程规则：{error}"))
}

pub fn save_config(config: &ProcessConfig) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建配置目录：{error}"))?;
    }
    let bytes =
        serde_json::to_vec_pretty(config).map_err(|error| format!("无法编码进程规则：{error}"))?;
    fs::write(path, bytes).map_err(|error| format!("无法保存进程规则：{error}"))
}

pub fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,stat=,pcpu=,comm="])
        .output()
        .map_err(|error| format!("无法读取进程列表：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ps 执行失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let current = std::process::id();
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| parse_process_line(line, current))
        .collect())
}

fn parse_process_line(line: &str, current_pid: u32) -> Option<ProcessInfo> {
    let mut remaining = line.trim_start();
    let mut next_field = || {
        let split = remaining.find(char::is_whitespace)?;
        let value = remaining[..split].to_string();
        remaining = remaining[split..].trim_start();
        Some(value)
    };
    let pid = next_field()?.parse().ok()?;
    let ppid = next_field()?.parse().ok()?;
    let stat = next_field()?;
    let cpu = next_field()?;
    let executable_path = remaining.trim().to_string();
    if pid == current_pid || executable_path.is_empty() {
        return None;
    }
    let name = Path::new(&executable_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&executable_path)
        .to_string();
    Some(ProcessInfo {
        pid,
        ppid,
        stat,
        cpu,
        executable_path,
        name,
    })
}

pub fn filter_processes(processes: &[ProcessInfo], query: &str) -> Vec<ProcessInfo> {
    let query = query.trim().to_ascii_lowercase();
    let mut result = if query.is_empty() {
        processes.to_vec()
    } else {
        processes
            .iter()
            .filter(|process| {
                process.name.to_ascii_lowercase().contains(&query)
                    || process
                        .executable_path
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect()
    };
    result.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.pid.cmp(&b.pid))
    });
    result
}

pub fn make_rule_from_processes(processes: &[ProcessInfo]) -> Option<KillRule> {
    let first = processes.first()?;
    let name = if processes.len() == 1 {
        first.name.clone()
    } else {
        format!("{} 等 {} 个进程", first.name, processes.len())
    };
    let match_terms = processes
        .iter()
        .flat_map(|process| [process.name.clone(), process.executable_path.clone()])
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    Some(KillRule {
        id: format!("rule-{}-{}", first.pid, processes.len()),
        name,
        startup_terms: match_terms.clone(),
        match_terms,
        remove_startup: true,
        enabled: true,
    })
}

pub fn apply_kill_rules(config: &mut ProcessConfig, cleanup_startup: bool) -> Vec<String> {
    let processes = match list_processes() {
        Ok(processes) => processes,
        Err(error) => return vec![error],
    };
    let mut messages = Vec::new();
    for rule in config.kill_rules.iter().filter(|rule| rule.enabled) {
        let matching = processes
            .iter()
            .filter(|process| rule_matches(rule, process))
            .cloned()
            .collect::<Vec<_>>();
        for process in matching {
            match terminate_pid(process.pid, config.terminate_timeout_seconds) {
                Ok(()) => messages.push(format!(
                    "已终结 {}（PID {}，规则“{}”）",
                    process.name, process.pid, rule.name
                )),
                Err(error) => messages.push(format!(
                    "终结 {}（PID {}）失败：{error}",
                    process.name, process.pid
                )),
            }
        }
        if cleanup_startup && rule.remove_startup {
            match remove_startup_entries(rule) {
                Ok(paths) if !paths.is_empty() => {
                    messages.push(format!(
                        "已移除 {} 个启动项：{}",
                        paths.len(),
                        paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("、")
                    ));
                }
                Ok(_) => {}
                Err(error) => messages.push(format!("清理启动项失败：{error}")),
            }
        }
    }
    messages
}

pub fn add_monitor(config: &mut ProcessConfig, app_path: &str) -> Result<(), String> {
    let path = Path::new(app_path.trim());
    if !path.is_dir() || path.extension().and_then(|value| value.to_str()) != Some("app") {
        return Err("请输入有效的 .app 路径".to_string());
    }
    let display_name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("未命名应用")
        .to_string();
    let app_path = path
        .canonicalize()
        .map_err(|error| format!("无法解析应用路径：{error}"))?
        .to_string_lossy()
        .into_owned();
    if config
        .monitors
        .iter()
        .all(|monitor| monitor.app_path != app_path)
    {
        config.monitors.push(MonitorApp {
            display_name: display_name.clone(),
            executable_name: display_name,
            app_path,
            enabled: true,
        });
    }
    save_config(config)
}

pub fn monitor_tick(config: &ProcessConfig, runtime: &mut MonitorRuntime) -> Vec<MonitorSnapshot> {
    if !config.monitoring_enabled {
        return Vec::new();
    }
    let processes = list_processes().unwrap_or_default();
    let now = Instant::now();
    let mut snapshots = Vec::new();
    for app in config.monitors.iter().filter(|app| app.enabled) {
        let found = find_app_process(app, &processes);
        let state = runtime.apps.entry(app.app_path.clone()).or_default();
        if let Some(process) = found {
            state.missing_checks = 0;
            let stopped = process.stat.contains('T');
            if stopped {
                state.stopped_checks += 1;
            } else {
                state.stopped_checks = 0;
            }
            let cooldown_ready = state.last_restart.is_none_or(|last| {
                now.duration_since(last).as_secs() >= config.restart_cooldown_seconds
            });
            if stopped && state.stopped_checks >= config.hang_confirmations.max(1) && cooldown_ready
            {
                let restart_result = terminate_pid(process.pid, config.terminate_timeout_seconds)
                    .and_then(|_| launch_app(&app.app_path));
                state.stopped_checks = 0;
                state.last_restart = Some(now);
                match restart_result {
                    Ok(()) => {
                        state.restarts += 1;
                        snapshots.push(MonitorSnapshot {
                            app_path: app.app_path.clone(),
                            display_name: app.display_name.clone(),
                            status: "正在自动重启".to_string(),
                            pid: None,
                            message: format!("进程 {} 已停止，已自动重启", process.pid),
                            restarts: state.restarts,
                        });
                    }
                    Err(error) => snapshots.push(MonitorSnapshot {
                        app_path: app.app_path.clone(),
                        display_name: app.display_name.clone(),
                        status: "重启失败".to_string(),
                        pid: Some(process.pid),
                        message: error,
                        restarts: state.restarts,
                    }),
                }
                continue;
            }
            snapshots.push(MonitorSnapshot {
                app_path: app.app_path.clone(),
                display_name: app.display_name.clone(),
                status: if stopped {
                    "进程已停止"
                } else {
                    "运行中"
                }
                .to_string(),
                pid: Some(process.pid),
                message: format!("PID {} · {}% CPU", process.pid, process.cpu),
                restarts: state.restarts,
            });
            continue;
        }
        state.missing_checks += 1;
        state.stopped_checks = 0;
        let cooldown_ready = state.last_restart.is_none_or(|last| {
            now.duration_since(last).as_secs() >= config.restart_cooldown_seconds
        });
        if state.missing_checks >= config.missing_confirmations && cooldown_ready {
            match launch_app(&app.app_path) {
                Ok(()) => {
                    state.missing_checks = 0;
                    state.last_restart = Some(now);
                    state.restarts += 1;
                    snapshots.push(MonitorSnapshot {
                        app_path: app.app_path.clone(),
                        display_name: app.display_name.clone(),
                        status: "正在自动启动".to_string(),
                        pid: None,
                        message: "已发送启动命令，等待进程出现".to_string(),
                        restarts: state.restarts,
                    });
                }
                Err(error) => snapshots.push(MonitorSnapshot {
                    app_path: app.app_path.clone(),
                    display_name: app.display_name.clone(),
                    status: "启动失败".to_string(),
                    pid: None,
                    message: error,
                    restarts: state.restarts,
                }),
            }
        } else {
            snapshots.push(MonitorSnapshot {
                app_path: app.app_path.clone(),
                display_name: app.display_name.clone(),
                status: "进程缺失".to_string(),
                pid: None,
                message: format!(
                    "确认中 {}/{}；{}",
                    state.missing_checks,
                    config.missing_confirmations,
                    if cooldown_ready {
                        "等待自动启动"
                    } else {
                        "冷却中"
                    }
                ),
                restarts: state.restarts,
            });
        }
    }
    snapshots
}

pub fn terminate_pid(pid: u32, timeout_seconds: u64) -> Result<(), String> {
    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map_err(|error| format!("无法发送 SIGTERM：{error}"))?;
    if !status.success() {
        return Err("进程可能已经退出".to_string());
    }
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    while Instant::now() < deadline {
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !alive {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    let killed = Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .map_err(|error| format!("无法发送 SIGKILL：{error}"))?;
    if killed.success() {
        Ok(())
    } else {
        Err(format!("进程 {pid} 在 SIGKILL 后仍未退出"))
    }
}

fn find_app_process<'a>(app: &MonitorApp, processes: &'a [ProcessInfo]) -> Option<&'a ProcessInfo> {
    processes.iter().find(|process| {
        process.executable_path == app.executable_name
            || process.name == app.executable_name
            || process
                .executable_path
                .starts_with(&format!("{}/Contents/MacOS/", app.app_path))
    })
}

fn default_hang_confirmations() -> u32 {
    3
}

fn launch_app(path: &str) -> Result<(), String> {
    let output = Command::new("/usr/bin/open")
        .args(["-g", path])
        .output()
        .map_err(|error| format!("无法启动 {path}：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "启动 {path} 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn rule_matches(rule: &KillRule, process: &ProcessInfo) -> bool {
    let haystack = format!("{} {}", process.name, process.executable_path).to_ascii_lowercase();
    rule.match_terms
        .iter()
        .any(|term| haystack.contains(&term.to_ascii_lowercase()))
}

fn remove_startup_entries(rule: &KillRule) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for directory in startup_directories() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("plist") {
                continue;
            }
            let content = Command::new("/usr/bin/plutil")
                .args(["-convert", "xml1", "-o", "-"])
                .arg(&path)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .or_else(|| fs::read_to_string(&path).ok())
                .unwrap_or_default()
                .to_ascii_lowercase()
                + &path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase();
            if rule
                .startup_terms
                .iter()
                .any(|term| content.contains(&term.to_ascii_lowercase()))
            {
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        return Ok(paths);
    }
    let uid = unsafe { getuid() };
    let commands = paths
        .iter()
        .map(|path| {
            format!(
                "(/bin/launchctl bootout system {} >/dev/null 2>&1 || true); (/bin/launchctl bootout gui/{} {} >/dev/null 2>&1 || true); /bin/rm -f {}",
                shell_quote(path),
                uid,
                shell_quote(path),
                shell_quote(path)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    run_privileged(&commands)?;
    Ok(paths)
}

fn startup_directories() -> Vec<PathBuf> {
    let mut directories = vec![
        PathBuf::from("/Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchDaemons"),
    ];
    if let Some(home) = env::var_os("HOME") {
        directories.push(PathBuf::from(home).join("Library/LaunchAgents"));
    }
    directories
}

unsafe extern "C" {
    fn getuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_by_name_or_path_case_insensitively() {
        let processes = vec![ProcessInfo {
            pid: 42,
            ppid: 1,
            stat: "S".to_string(),
            cpu: "0.1".to_string(),
            executable_path: "/Library/Application Support/Microsoft/MAU2.0/Microsoft AutoUpdate.app/Contents/MacOS/Microsoft AutoUpdate".to_string(),
            name: "Microsoft AutoUpdate".to_string(),
        }];
        assert_eq!(filter_processes(&processes, "microsoft").len(), 1);
        assert_eq!(filter_processes(&processes, "not-found").len(), 0);
    }

    #[test]
    fn default_rule_covers_both_microsoft_processes() {
        let config = ProcessConfig::default();
        let rule = &config.kill_rules[0];
        assert!(
            rule.match_terms
                .iter()
                .any(|term| term == "Microsoft AutoUpdate")
        );
        assert!(
            rule.match_terms
                .iter()
                .any(|term| term == "Microsoft Update Assistant")
        );
        assert!(
            rule.startup_terms
                .iter()
                .any(|term| term == "com.microsoft.update.agent")
        );
    }

    #[test]
    fn lists_real_processes_without_empty_names() {
        let processes = list_processes().unwrap();
        assert!(!processes.is_empty());
        assert!(processes.iter().all(|process| !process.name.is_empty()));
        assert!(
            processes
                .iter()
                .all(|process| process.pid != std::process::id())
        );
    }

    #[test]
    fn parses_executable_paths_that_contain_spaces() {
        let process = parse_process_line(
            " 11744 1 S 0.0 /Library/Application Support/Microsoft/MAU2.0/Microsoft AutoUpdate.app/Contents/MacOS/Microsoft AutoUpdate",
            1,
        )
        .unwrap();
        assert_eq!(process.pid, 11744);
        assert_eq!(process.name, "Microsoft AutoUpdate");
        assert!(process.executable_path.contains("Application Support"));
    }
}
