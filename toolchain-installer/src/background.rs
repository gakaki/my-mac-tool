use crate::btm;
use crate::installer;
use crate::privileged::{run_capture as run_privileged_capture, shell_quote};
use crate::processes::{self, ProcessInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANAGED_LABEL_PREFIX: &str = "cn.gakaki.mac-settings-devtools.background";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    App,
    Developer,
    LegacyAgent,
    LoginItem,
    BackgroundTask,
    Other,
}

impl ActivityKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::App => "应用",
            Self::Developer => "开发者项目",
            Self::LegacyAgent => "后台代理",
            Self::LoginItem => "登录项",
            Self::BackgroundTask => "后台任务",
            Self::Other => "系统项目",
        }
    }

    fn from_record_kind(kind: btm::RecordKind) -> Self {
        match kind {
            btm::RecordKind::App => Self::App,
            btm::RecordKind::Developer => Self::Developer,
            btm::RecordKind::LegacyAgent => Self::LegacyAgent,
            btm::RecordKind::LoginItem => Self::LoginItem,
            btm::RecordKind::BackgroundTask => Self::BackgroundTask,
            btm::RecordKind::Other => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchService {
    pub label: String,
    pub plist_path: String,
    kind: LaunchServiceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchServiceKind {
    UserAgent,
    GlobalAgent,
    SystemDaemon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundActivity {
    pub id: String,
    pub name: String,
    pub developer: String,
    pub kind: ActivityKind,
    pub enabled: bool,
    pub app_path: Option<String>,
    pub executable_path: Option<String>,
    pub display_path: Option<String>,
    pub last_use: Option<String>,
    pub running: bool,
    pub managed: bool,
    pub btm_record: bool,
    pub launch_label: Option<String>,
    pub plist_path: Option<String>,
    pub toggle_services: Vec<LaunchService>,
    pub removable_services: Vec<LaunchService>,
}

impl BackgroundActivity {
    pub fn is_toggleable(&self) -> bool {
        self.managed || !self.toggle_services.is_empty()
    }

    pub fn is_deletable(&self) -> bool {
        self.managed || self.btm_record
    }

    pub fn status_text(&self) -> String {
        let state = if self.running {
            "正在后台运行"
        } else if self.enabled {
            "已启用，当前未运行"
        } else {
            "已关闭"
        };
        match self
            .last_use
            .as_deref()
            .and_then(format_last_use)
            .filter(|_| !self.running)
        {
            Some(last_use) => format!("{state} · 上次后台运行：{last_use}"),
            None => state.to_string(),
        }
    }

    pub fn subtitle(&self) -> String {
        let developer = self.developer.trim();
        let path = self
            .display_path
            .as_deref()
            .or(self.app_path.as_deref())
            .or(self.executable_path.as_deref())
            .unwrap_or("");
        match (developer.is_empty(), path.is_empty()) {
            (true, true) => self.kind.label().to_string(),
            (false, true) => format!("{} · {}", self.kind.label(), developer),
            (true, false) => format!("{} · {path}", self.kind.label()),
            (false, false) => format!("{} · {developer} · {path}", self.kind.label()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteSummary {
    pub deleted_activities: usize,
    pub removed_services: usize,
    pub removed_btm_records: usize,
    pub skipped: Vec<String>,
    pub failures: Vec<String>,
    pub deleted_managed_labels: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedActivity {
    pub label: String,
    pub name: String,
    pub target: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub enabled: bool,
    pub run_at_load: bool,
    pub keep_alive: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ManagedConfig {
    #[serde(default)]
    activities: Vec<ManagedActivity>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivityScan {
    pub activities: Vec<BackgroundActivity>,
    pub warning: Option<String>,
}

pub fn config_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/MacSettingsDevTools")
        .join("background_activities.json"))
}

pub fn load_managed_activities() -> Result<Vec<ManagedActivity>, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path).map_err(|error| format!("无法读取后台活动配置：{error}"))?;
    let config: ManagedConfig =
        serde_json::from_slice(&bytes).map_err(|error| format!("无法解析后台活动配置：{error}"))?;
    Ok(config.activities)
}

fn save_managed_activities(activities: &[ManagedActivity]) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建后台活动配置目录：{error}"))?;
    }
    let config = ManagedConfig {
        activities: activities.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&config)
        .map_err(|error| format!("无法编码后台活动配置：{error}"))?;
    fs::write(path, bytes).map_err(|error| format!("无法保存后台活动配置：{error}"))
}

pub fn scan_activities() -> ActivityScan {
    let processes = processes::list_processes().unwrap_or_default();
    let (managed, managed_warning) = match load_managed_activities() {
        Ok(managed) => (managed, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let mut activities = match read_btm_records() {
        Ok(activities) => activities,
        Err(error) => {
            return ActivityScan {
                activities: managed
                    .iter()
                    .map(|item| activity_from_managed(item, &processes))
                    .collect(),
                warning: Some(format!("{error}；下方仅显示本工具创建的后台活动。")),
            };
        }
    };
    activities.extend(
        managed
            .iter()
            .map(|item| activity_from_managed(item, &processes)),
    );
    activities.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    ActivityScan {
        activities,
        warning: managed_warning,
    }
}

pub fn create_activity(
    name: &str,
    target: &str,
    arguments: Vec<String>,
    enabled: bool,
    run_at_load: bool,
    keep_alive: bool,
) -> Result<ManagedActivity, String> {
    let (target, is_app) = normalize_target(target)?;
    let name = normalized_name(name, &target);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let label = format!("{MANAGED_LABEL_PREFIX}.{stamp:x}");
    let mut activity = ManagedActivity {
        label,
        name,
        target: target.to_string_lossy().into_owned(),
        arguments,
        enabled,
        run_at_load,
        keep_alive: keep_alive && !is_app,
    };
    if let Err(error) = apply_managed(&activity) {
        if let Ok(path) = managed_plist_path(&activity.label) {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    activity.enabled = inspect_launch_service(&activity.label)
        .map(|status| status.enabled)
        .unwrap_or(enabled);
    let mut activities = load_managed_activities()?;
    activities.push(activity.clone());
    save_managed_activities(&activities)?;
    Ok(activity)
}

pub fn update_activity(
    label: &str,
    name: &str,
    target: &str,
    arguments: Vec<String>,
    enabled: bool,
    run_at_load: bool,
    keep_alive: bool,
) -> Result<ManagedActivity, String> {
    let (target, is_app) = normalize_target(target)?;
    let name = normalized_name(name, &target);
    let mut activities = load_managed_activities()?;
    let activity = activities
        .iter_mut()
        .find(|item| item.label == label)
        .ok_or_else(|| "未找到要编辑的后台活动。".to_string())?;
    activity.name = name;
    activity.target = target.to_string_lossy().into_owned();
    activity.arguments = arguments;
    activity.enabled = enabled;
    activity.run_at_load = run_at_load;
    activity.keep_alive = keep_alive && !is_app;
    let updated = activity.clone();
    apply_managed(&updated)?;
    save_managed_activities(&activities)?;
    Ok(updated)
}

pub fn delete_activity(label: &str) -> Result<(), String> {
    let mut activities = load_managed_activities()?;
    if !activities.iter().any(|item| item.label == label) {
        return Err("只能删除由本工具创建的后台活动。".to_string());
    }
    remove_managed_service(label)?;
    activities.retain(|item| item.label != label);
    save_managed_activities(&activities)
}

pub fn delete_activities(activities: &[BackgroundActivity]) -> Result<DeleteSummary, String> {
    let managed_labels = load_managed_activities()?
        .into_iter()
        .map(|activity| activity.label)
        .collect::<HashSet<_>>();
    let mut summary = DeleteSummary::default();
    let mut btm_identifiers = Vec::new();
    let mut managed_to_delete = Vec::new();

    for activity in activities {
        if activity.managed {
            let Some(label) = activity
                .launch_label
                .as_deref()
                .filter(|label| managed_labels.contains(*label))
            else {
                summary
                    .failures
                    .push(format!("{}：未找到自定义活动配置。", activity.name));
                continue;
            };
            managed_to_delete.push((activity.name.clone(), label.to_string()));
            continue;
        }

        if !activity.btm_record || activity.id.trim().is_empty() {
            summary
                .failures
                .push(format!("{}：没有可删除的后台登记。", activity.name));
            continue;
        }
        if !btm_identifiers.contains(&activity.id) {
            btm_identifiers.push(activity.id.clone());
        }
    }

    if !btm_identifiers.is_empty() {
        match remove_background_items_as_admin(unsafe { getuid() }, &btm_identifiers) {
            Ok(removed_services) => {
                summary.removed_services = removed_services;
                summary.removed_btm_records = btm_identifiers.len();
                summary.deleted_activities += btm_identifiers.len();
            }
            Err(error) => {
                summary
                    .failures
                    .push(format!("后台 App 活动未删除：{error}"));
                return Ok(summary);
            }
        }
    }

    for (name, label) in managed_to_delete {
        match delete_activity(&label) {
            Ok(()) => {
                summary.deleted_activities += 1;
                summary.deleted_managed_labels.push(label);
            }
            Err(error) => summary.failures.push(format!("{name}：{error}")),
        }
    }
    Ok(summary)
}

fn remove_background_items_as_admin(uid: u32, identifiers: &[String]) -> Result<usize, String> {
    let helper = btm_runner_path()?;
    if !helper.is_file() {
        return Err(format!(
            "找不到安装引擎 {}，无法修改系统后台登记。",
            helper.display()
        ));
    }
    let home = env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
    let mut command = format!(
        "{} __btm-remove-related {} {}",
        shell_quote(&helper),
        uid,
        shell_quote(&home)
    );
    for identifier in identifiers {
        let _ = write!(command, " {}", shell_quote(identifier));
    }
    let output = run_privileged_capture(&command)?;
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("removed_services="))
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| "系统没有返回后台项目删除结果。".to_string())
}

fn btm_runner_path() -> Result<PathBuf, String> {
    let current = env::current_exe().map_err(|error| format!("无法确定当前 App 路径：{error}"))?;
    if current
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == "my-mac-installer")
    {
        return Ok(current);
    }
    Ok(installer::find_helper())
}

pub fn set_enabled(activity: &BackgroundActivity, enabled: bool) -> Result<(), String> {
    if let Some(label) = activity
        .launch_label
        .as_deref()
        .filter(|_| activity.managed)
    {
        let mut activities = load_managed_activities()?;
        let managed = activities
            .iter_mut()
            .find(|item| item.label == label)
            .ok_or_else(|| "未找到该自定义后台活动。".to_string())?;
        managed.enabled = enabled;
        apply_managed(managed)?;
        save_managed_activities(&activities)?;
        return Ok(());
    }
    if activity.toggle_services.is_empty() {
        return Err("这个系统项目不能在工具内直接切换，请到系统设置中管理。".to_string());
    }
    for service in &activity.toggle_services {
        set_launch_service_enabled(service, enabled)?;
    }
    Ok(())
}

pub fn open_login_items_settings() -> Result<(), String> {
    processes::open_login_items_settings()
}

pub fn open_launch_agents_directory() -> Result<(), String> {
    let home = env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
    let directory = PathBuf::from(home).join("Library/LaunchAgents");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("无法创建 LaunchAgents 目录：{error}"))?;
    let status = Command::new("/usr/bin/open")
        .arg(&directory)
        .status()
        .map_err(|error| format!("无法打开 LaunchAgents 目录：{error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("Finder 未能打开 LaunchAgents 目录。".to_string())
    }
}

pub fn open_full_disk_access_settings() -> Result<(), String> {
    let urls = [
        "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
    ];
    let mut last_error = String::new();
    for url in urls {
        let output = Command::new("/usr/bin/open")
            .arg(url)
            .output()
            .map_err(|error| format!("无法打开完全磁盘访问权限设置：{error}"))?;
        if output.status.success() {
            return Ok(());
        }
        last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    Err(format!("无法打开完全磁盘访问权限设置：{last_error}"))
}

pub fn full_disk_access_app_path() -> String {
    let Ok(executable) = env::current_exe() else {
        return "当前 App".to_string();
    };
    let Some(contents) = executable.parent().and_then(Path::parent) else {
        return executable.display().to_string();
    };
    if contents.file_name().and_then(|value| value.to_str()) == Some("Contents") {
        if let Some(app) = contents.parent() {
            return app.display().to_string();
        }
    }
    executable.display().to_string()
}

fn activity_from_managed(
    activity: &ManagedActivity,
    processes: &[ProcessInfo],
) -> BackgroundActivity {
    let path = Path::new(&activity.target);
    let is_app = is_app_bundle(path);
    let launch_service = managed_launch_service(&activity.label).ok();
    BackgroundActivity {
        id: activity.label.clone(),
        name: activity.name.clone(),
        developer: "由本工具创建的 LaunchAgent".to_string(),
        kind: ActivityKind::LegacyAgent,
        enabled: activity.enabled,
        app_path: is_app.then(|| activity.target.clone()),
        executable_path: (!is_app).then(|| activity.target.clone()),
        display_path: Some(activity.target.clone()),
        last_use: None,
        running: target_running(path, processes),
        managed: true,
        btm_record: false,
        launch_label: Some(activity.label.clone()),
        plist_path: managed_plist_path(&activity.label)
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        toggle_services: launch_service.iter().cloned().collect(),
        removable_services: launch_service.into_iter().collect(),
    }
}

fn read_btm_records() -> Result<Vec<BackgroundActivity>, String> {
    let records = btm::current_user_records()?;
    let processes = processes::list_processes().unwrap_or_default();
    Ok(records
        .iter()
        .filter(|record| record.is_settings_row() && record.container.is_none())
        .map(|record| activity_from_btm(record, &records, &processes))
        .collect())
}

fn activity_from_btm(
    record: &btm::Record,
    records: &[btm::Record],
    processes: &[ProcessInfo],
) -> BackgroundActivity {
    let mut toggle_services = Vec::new();
    let mut removable_services = Vec::new();
    let mut services_enabled = true;
    let mut services_running = false;
    for child in descendant_legacy_agents(record, records) {
        let Some(plist_path) = child.plist_path.as_deref() else {
            continue;
        };
        let Some(kind) = launch_service_kind(Path::new(plist_path)) else {
            continue;
        };
        let Some(label) = launch_label_from_identifier(&child.identifier) else {
            continue;
        };
        let status = inspect_launch_service(&label);
        services_enabled &= status.map(|status| status.enabled).unwrap_or(child.enabled);
        services_running |= status.is_some_and(|status| status.running);
        let service = LaunchService {
            label,
            plist_path: plist_path.to_string(),
            kind,
        };
        if kind == LaunchServiceKind::UserAgent {
            toggle_services.push(service.clone());
        }
        removable_services.push(service);
    }
    let app_running = record
        .app_path
        .as_deref()
        .is_some_and(|path| target_running(Path::new(path), processes));
    BackgroundActivity {
        id: record.identifier.clone(),
        name: record.name.clone(),
        developer: record.developer.clone(),
        kind: ActivityKind::from_record_kind(record.kind),
        enabled: if toggle_services.is_empty() {
            record.enabled
        } else {
            services_enabled
        },
        app_path: record.app_path.clone(),
        executable_path: record.executable_path.clone(),
        display_path: record.display_path.clone(),
        last_use: record.last_use.and_then(relative_last_use),
        running: app_running || services_running,
        managed: false,
        btm_record: true,
        launch_label: None,
        plist_path: None,
        toggle_services,
        removable_services,
    }
}

fn descendant_legacy_agents<'a>(
    record: &btm::Record,
    records: &'a [btm::Record],
) -> Vec<&'a btm::Record> {
    let mut pending = vec![record.identifier.clone()];
    let mut seen = HashSet::new();
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        for child in records.iter().filter(|child| {
            child.container.as_deref() == Some(parent.as_str())
                && child.kind == btm::RecordKind::LegacyAgent
        }) {
            descendants.push(child);
            pending.push(child.identifier.clone());
        }
    }
    descendants
}

fn launch_label_from_identifier(identifier: &str) -> Option<String> {
    let (kind, label) = identifier.split_once('.')?;
    if kind.parse::<u32>().is_ok() {
        Some(label.to_string())
    } else {
        None
    }
}

fn launch_service_kind(path: &Path) -> Option<LaunchServiceKind> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    launch_service_kind_for_home(path, &home)
}

fn launch_service_kind_for_home(path: &Path, home: &Path) -> Option<LaunchServiceKind> {
    let parent = path.parent()?;
    if parent == home.join("Library/LaunchAgents") {
        Some(LaunchServiceKind::UserAgent)
    } else if parent == Path::new("/Library/LaunchAgents") {
        Some(LaunchServiceKind::GlobalAgent)
    } else if parent == Path::new("/Library/LaunchDaemons") {
        Some(LaunchServiceKind::SystemDaemon)
    } else {
        None
    }
}

fn format_last_use(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value == "(null)" {
        return None;
    }
    Some(value.to_string())
}

fn relative_last_use(value: SystemTime) -> Option<String> {
    let elapsed = SystemTime::now().duration_since(value).ok()?;
    let days = elapsed.as_secs() / 86_400;
    Some(if days == 0 {
        "今天".to_string()
    } else {
        format!("{days} 天前")
    })
}

fn normalize_target(value: &str) -> Result<(PathBuf, bool), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("请输入 App 或可执行文件路径。".to_string());
    }
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        let home = env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(value)
    };
    let path = expanded
        .canonicalize()
        .map_err(|error| format!("路径不存在或无法访问：{error}"))?;
    if path.is_dir() {
        if is_app_bundle(&path) {
            return Ok((path, true));
        }
        return Err("目录必须是有效的 .app 应用包。".to_string());
    }
    if !path.is_file() {
        return Err("路径必须是 .app 应用包或可执行文件。".to_string());
    }
    let mode = fs::metadata(&path)
        .map_err(|error| format!("无法读取文件权限：{error}"))?
        .permissions()
        .mode();
    if mode & 0o111 == 0 {
        return Err("目标文件没有可执行权限。".to_string());
    }
    Ok((path, false))
}

fn normalized_name(name: &str, target: &Path) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    target
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("未命名后台活动")
        .to_string()
}

fn is_app_bundle(path: &Path) -> bool {
    path.is_dir()
        && path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("app"))
}

fn managed_plist_path(label: &str) -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn managed_launch_service(label: &str) -> Result<LaunchService, String> {
    Ok(LaunchService {
        label: label.to_string(),
        plist_path: managed_plist_path(label)?.to_string_lossy().into_owned(),
        kind: LaunchServiceKind::UserAgent,
    })
}

fn apply_managed(activity: &ManagedActivity) -> Result<(), String> {
    let plist_path = managed_plist_path(&activity.label)?;
    write_launch_agent(activity, &plist_path)?;
    let service = managed_launch_service(&activity.label)?;
    set_launch_service_enabled(&service, activity.enabled)
}

fn set_launch_service_enabled(service: &LaunchService, enabled: bool) -> Result<(), String> {
    let uid = unsafe { getuid() };
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{}", service.label);
    if enabled {
        run_launchctl(&["enable", &target])?;
        if run_launchctl(&["print", &target]).is_ok() {
            let _ = run_launchctl(&["bootout", &target]);
        }
        let plist = Path::new(&service.plist_path);
        if !plist.is_file() {
            return Err(format!("启动项文件不存在：{}", plist.display()));
        }
        run_launchctl(&["bootstrap", &domain, &service.plist_path])?;
        let _ = run_launchctl(&["kickstart", "-k", &target]);
    } else {
        run_launchctl(&["disable", &target])?;
        if run_launchctl(&["print", &target]).is_ok() {
            let _ = run_launchctl(&["bootout", &target]);
        }
    }
    Ok(())
}

fn remove_managed_service(label: &str) -> Result<(), String> {
    let uid = unsafe { getuid() };
    let target = format!("gui/{uid}/{label}");
    if run_launchctl(&["print", &target]).is_ok() {
        let _ = run_launchctl(&["bootout", &target]);
    }
    let _ = run_launchctl(&["enable", &target]);
    let path = managed_plist_path(label)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法删除启动项文件：{error}")),
    }
}

fn run_launchctl(arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("/bin/launchctl")
        .args(arguments)
        .output()
        .map_err(|error| format!("无法执行 launchctl：{error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "launchctl {} 失败：{}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn inspect_launch_service(label: &str) -> Option<ManagedServiceStatus> {
    let uid = unsafe { getuid() };
    let target = format!("gui/{uid}/{label}");
    let loaded = run_launchctl(&["print", &target]).ok();
    let disabled_output = run_launchctl(&["print-disabled", &format!("gui/{uid}")]).ok();
    if loaded.is_none() && disabled_output.is_none() {
        return None;
    }
    let disabled = disabled_output
        .as_deref()
        .unwrap_or_default()
        .lines()
        .any(|line| line.contains(label) && line.contains("=> disabled"));
    Some(ManagedServiceStatus {
        enabled: !disabled,
        running: loaded
            .as_deref()
            .is_some_and(|output| output.contains("state = running") && output.contains("pid =")),
    })
}

#[derive(Clone, Copy)]
struct ManagedServiceStatus {
    enabled: bool,
    running: bool,
}

fn write_launch_agent(activity: &ManagedActivity, path: &Path) -> Result<(), String> {
    let (program_arguments, bundle_identifier) = launch_arguments(activity)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建登录项目录：{error}"))?;
    }
    let xml = render_launch_agent(activity, &program_arguments, bundle_identifier.as_deref());
    let temporary = path.with_extension("plist.tmp");
    fs::write(&temporary, xml).map_err(|error| format!("无法写入后台活动配置：{error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("无法安装后台活动配置：{error}"))
}

fn launch_arguments(activity: &ManagedActivity) -> Result<(Vec<String>, Option<String>), String> {
    let target = Path::new(&activity.target);
    if is_app_bundle(target) {
        let mut arguments = vec![
            "/usr/bin/open".to_string(),
            "-g".to_string(),
            activity.target.clone(),
        ];
        if !activity.arguments.is_empty() {
            arguments.push("--args".to_string());
            arguments.extend(activity.arguments.clone());
        }
        Ok((arguments, app_bundle_identifier(target)))
    } else {
        let mut arguments = vec![activity.target.clone()];
        arguments.extend(activity.arguments.clone());
        Ok((arguments, None))
    }
}

fn app_bundle_identifier(path: &Path) -> Option<String> {
    let info = path.join("Contents/Info.plist");
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(info)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn render_launch_agent(
    activity: &ManagedActivity,
    program_arguments: &[String],
    bundle_identifier: Option<&str>,
) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let mut value = |key: &str, value: &str| {
        let _ = writeln!(
            xml,
            "<key>{}</key><string>{}</string>",
            xml_escape(key),
            xml_escape(value)
        );
    };
    value("Label", &activity.label);
    let _ = writeln!(xml, "<key>ProgramArguments</key><array>");
    for argument in program_arguments {
        let _ = writeln!(xml, "<string>{}</string>", xml_escape(argument));
    }
    let _ = writeln!(xml, "</array>");
    if let Some(bundle_identifier) = bundle_identifier {
        let _ = writeln!(
            xml,
            "<key>AssociatedBundleIdentifiers</key><array><string>{}</string></array>",
            xml_escape(bundle_identifier)
        );
    }
    let _ = writeln!(
        xml,
        "<key>RunAtLoad</key>{}",
        if activity.run_at_load {
            "<true/>"
        } else {
            "<false/>"
        }
    );
    let _ = writeln!(
        xml,
        "<key>KeepAlive</key>{}",
        if activity.keep_alive {
            "<true/>"
        } else {
            "<false/>"
        }
    );
    let _ = writeln!(
        xml,
        "<key>LimitLoadToSessionType</key><string>Aqua</string>"
    );
    xml.push_str("</dict>\n</plist>\n");
    xml
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn target_running(target: &Path, processes: &[ProcessInfo]) -> bool {
    if is_app_bundle(target) {
        let prefix = format!("{}/Contents/MacOS/", target.display());
        processes
            .iter()
            .any(|process| process.executable_path.starts_with(&prefix))
    } else {
        let target = target.to_string_lossy();
        processes
            .iter()
            .any(|process| process.executable_path == target)
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_last_use_as_relative_days() {
        let three_days_ago = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400 + 60);
        assert_eq!(relative_last_use(three_days_ago).as_deref(), Some("3 天前"));
    }

    #[test]
    fn renders_launch_agent_with_escaped_arguments() {
        let activity = ManagedActivity {
            label: "cn.example.background.1".to_string(),
            name: "Example".to_string(),
            target: "/usr/bin/true".to_string(),
            arguments: vec!["a&b".to_string(), "<tag>".to_string()],
            enabled: true,
            run_at_load: true,
            keep_alive: false,
        };
        let (arguments, bundle) = launch_arguments(&activity).unwrap();
        assert_eq!(arguments[0], "/usr/bin/true");
        assert!(bundle.is_none());
        let plist = render_launch_agent(&activity, &arguments, None);
        assert!(plist.contains("<string>a&amp;b</string>"));
        assert!(plist.contains("<string>&lt;tag&gt;</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn app_activities_launch_through_open_without_keep_alive() {
        let path = env::temp_dir().join("Example.app");
        let _ = fs::create_dir_all(path.join("Contents/MacOS"));
        let activity = ManagedActivity {
            label: "cn.example.background.app".to_string(),
            name: "Example".to_string(),
            target: path.to_string_lossy().into_owned(),
            arguments: vec!["--background".to_string()],
            enabled: true,
            run_at_load: true,
            keep_alive: true,
        };
        let (arguments, _) = launch_arguments(&activity).unwrap();
        assert_eq!(arguments[0], "/usr/bin/open");
        assert!(arguments.contains(&"--args".to_string()));
        assert!(!activity.keep_alive || is_app_bundle(Path::new(&activity.target)));
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn extracts_launch_label_from_btm_identifier() {
        assert_eq!(
            launch_label_from_identifier("8.com.example.agent").as_deref(),
            Some("com.example.agent")
        );
        assert_eq!(launch_label_from_identifier("L2D LIMITED"), None);
    }

    #[test]
    fn only_exact_launch_directories_are_removable() {
        let home = Path::new("/Users/example");
        assert_eq!(
            launch_service_kind_for_home(
                Path::new("/Users/example/Library/LaunchAgents/com.example.agent.plist"),
                home
            ),
            Some(LaunchServiceKind::UserAgent)
        );
        assert_eq!(
            launch_service_kind_for_home(
                Path::new("/Library/LaunchAgents/com.example.agent.plist"),
                home
            ),
            Some(LaunchServiceKind::GlobalAgent)
        );
        assert_eq!(
            launch_service_kind_for_home(
                Path::new("/Library/LaunchDaemons/com.example.daemon.plist"),
                home
            ),
            Some(LaunchServiceKind::SystemDaemon)
        );
        assert_eq!(
            launch_service_kind_for_home(
                Path::new("/Users/example/Library/LaunchAgents/nested/com.example.plist"),
                home
            ),
            None
        );
        assert_eq!(
            launch_service_kind_for_home(
                Path::new("/System/Library/LaunchDaemons/com.apple.example.plist"),
                home
            ),
            None
        );
    }
}
