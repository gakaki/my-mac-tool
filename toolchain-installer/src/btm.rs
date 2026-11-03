use plist::{Dictionary, Value};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BTM_DIRECTORY: &str = "/var/db/com.apple.backgroundtaskmanagement";
const BTM_DAEMON_LABEL: &str = "com.apple.backgroundtaskmanagementd";
const BTM_DAEMON_PLIST: &str =
    "/System/Library/LaunchDaemons/com.apple.backgroundtaskmanagementd.plist";
const BTM_AGENT_LABEL: &str = "com.apple.backgroundtaskmanagement.agent";
const BTM_AGENT_PLIST: &str =
    "/System/Library/LaunchAgents/com.apple.backgroundtaskmanagement.agent.plist";
const PLIST_EPOCH_UNIX_SECONDS: u64 = 978_307_200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    App,
    Developer,
    LegacyAgent,
    LoginItem,
    BackgroundTask,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub identifier: String,
    pub name: String,
    pub developer: String,
    pub kind: RecordKind,
    pub enabled: bool,
    pub app_path: Option<String>,
    pub executable_path: Option<String>,
    pub display_path: Option<String>,
    pub last_use: Option<SystemTime>,
    pub container: Option<String>,
    pub plist_path: Option<String>,
}

impl Record {
    pub fn is_settings_row(&self) -> bool {
        matches!(self.kind, RecordKind::App | RecordKind::Developer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserStoreLocation {
    pub path: PathBuf,
    pub generated_uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalOutcome {
    pub removed_records: usize,
    pub backup_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedRemovalOutcome {
    pub removed_records: usize,
    pub removed_services: usize,
    pub backups: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RelatedService {
    identifier: String,
    plist_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoreRemovalPlan {
    path: PathBuf,
    generated_uid: String,
    top_level_identifiers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoreSnapshot {
    path: PathBuf,
    generated_uid: String,
    records: Vec<Record>,
}

pub fn current_user_records() -> Result<Vec<Record>, String> {
    let location = current_user_store_location()?;
    let value = Value::from_file(&location.path)
        .map_err(|error| format!("无法解析 {}：{error}", location.path.display()))?;
    parse_user_store(&value, &location.generated_uid)
        .map_err(|error| format!("无法解析 {}：{error}", location.path.display()))?
        .ok_or_else(|| "未找到当前用户的 macOS 后台项目记录".to_string())
}

pub fn current_user_store_location() -> Result<UserStoreLocation, String> {
    let generated_uid = current_user_generated_uid()?;
    let directory = Path::new(BTM_DIRECTORY);
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("无法读取 macOS 后台项目数据库：{error}"))?;
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("BackgroundItems-v") && name.ends_with(".btm"))
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut errors = Vec::new();
    for path in paths {
        match user_store_matches(&path, &generated_uid) {
            Ok(true) => {
                return Ok(UserStoreLocation {
                    path,
                    generated_uid,
                });
            }
            Ok(false) => {}
            Err(error) => errors.push(error),
        }
    }
    let detail = errors
        .first()
        .map(|error| format!("：{error}"))
        .unwrap_or_default();
    Err(format!("未找到当前用户的 macOS 后台项目记录{detail}"))
}

pub fn remove_records_as_root(
    path: &Path,
    uid: u32,
    generated_uid: &str,
    identifiers: &[String],
) -> Result<RemovalOutcome, String> {
    if unsafe { geteuid() } != 0 {
        return Err("修改 macOS 后台项目数据库需要管理员权限。".to_string());
    }
    validate_privileged_store_path(path)?;
    let _ = stop_btm_services(uid);
    let outcome = remove_records_from_store(path, generated_uid, identifiers);
    let refresh = restart_btm_services(uid);
    match (outcome, refresh) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(format!(
            "后台项目记录已删除，但 macOS 后台活动服务刷新失败：{error}"
        )),
        (Err(error), _) => Err(error),
    }
}

pub fn remove_related_records_as_root(
    uid: u32,
    home: &Path,
    identifiers: &[String],
) -> Result<RelatedRemovalOutcome, String> {
    if unsafe { geteuid() } != 0 {
        return Err("修改 macOS 后台项目数据库需要管理员权限。".to_string());
    }
    let requested = identifiers
        .iter()
        .map(|identifier| identifier.trim())
        .filter(|identifier| !identifier.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    if requested.is_empty() {
        return Err("没有指定要删除的后台项目记录".to_string());
    }

    let (store_plans, services, verification_identifiers) = related_removal_plan(&requested)?;
    if store_plans.is_empty() {
        return Err("没有找到要删除的后台项目记录，可能已被其他操作移除。".to_string());
    }
    validate_related_services(&services, home)?;

    let mut removed_services = 0;
    for service in &services {
        if remove_related_service(service, uid, home)? {
            removed_services += 1;
        }
    }

    let removal = (|| {
        let mut backups = Vec::new();
        let mut removed_records = 0;
        let mut removed_any = false;
        for _attempt in 0..2 {
            stop_btm_daemon()?;
            removed_records = 0;
            backups.clear();
            removed_any = false;
            for plan in &store_plans {
                let Some(outcome) = remove_records_from_store_if_present(
                    &plan.path,
                    &plan.generated_uid,
                    &plan.top_level_identifiers,
                )?
                else {
                    continue;
                };
                removed_any = true;
                removed_records += outcome.removed_records;
                backups.push(outcome.backup_path);
            }
            if btm_daemon_pid()?.is_none() {
                break;
            }
        }
        if !removed_any {
            return Err("没有找到要删除的后台项目记录，可能已被其他操作移除。".to_string());
        }
        Ok((removed_records, backups))
    })();
    if let Err(error) = removal {
        if btm_daemon_pid()?.is_none() {
            let _ = start_btm_daemon();
        }
        return Err(error);
    }
    let (removed_records, backups) = removal.unwrap_or_default();

    start_btm_daemon()?;
    let dump = dump_btm()?;
    if dump_contains_identifiers(&dump, &verification_identifiers) {
        return Err(
            "后台项目记录已尝试删除，但 macOS 又重新登记了它。请确认关联 App 或启动文件是否仍然存在。"
                .to_string(),
        );
    }
    thread::sleep(Duration::from_millis(600));
    let final_dump = dump_btm()?;
    if dump_contains_identifiers(&final_dump, &verification_identifiers) {
        return Err(
            "后台项目记录已尝试删除，但 macOS 后台服务又把它加回来了。请确认关联 App 或启动文件是否仍然存在。"
                .to_string(),
        );
    }

    Ok(RelatedRemovalOutcome {
        removed_records,
        removed_services,
        backups,
    })
}

pub fn check_store_access_as_root(path: &Path) -> Result<(), String> {
    if unsafe { geteuid() } != 0 {
        return Err("检查 macOS 后台项目数据库需要管理员权限。".to_string());
    }
    validate_privileged_store_path(path)?;
    let value = Value::from_file(path)
        .map_err(|error| plist_database_error("无法读取后台项目数据库", path, error))?;
    value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("$objects"))
        .and_then(Value::as_array)
        .ok_or_else(|| "后台项目数据库结构无效，已拒绝修改。".to_string())?;
    Ok(())
}

fn validate_privileged_store_path(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("无法读取后台项目数据库 {}：{error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("后台项目数据库必须是普通文件。".to_string());
    }
    if metadata.uid() != 0 {
        return Err("后台项目数据库不属于系统管理员，已拒绝修改。".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "后台项目数据库路径无效".to_string())?;
    if parent != Path::new(BTM_DIRECTORY) {
        return Err("后台项目数据库不在 macOS 系统目录中，已拒绝修改。".to_string());
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "后台项目数据库文件名无效".to_string())?;
    if !file_name.starts_with("BackgroundItems-v") || !file_name.ends_with(".btm") {
        return Err("后台项目数据库文件名无效，已拒绝修改。".to_string());
    }
    Ok(())
}

fn related_removal_plan(
    requested: &HashSet<String>,
) -> Result<(Vec<StoreRemovalPlan>, Vec<RelatedService>, HashSet<String>), String> {
    let snapshots = all_store_snapshots()?;
    let mut plans = Vec::new();
    let mut services = Vec::new();
    let mut seen_services = HashSet::new();
    let mut verification_identifiers = requested.clone();

    for snapshot in snapshots {
        let mut top_level_identifiers = snapshot
            .records
            .iter()
            .filter(|record| record.container.is_none() && requested.contains(&record.identifier))
            .map(|record| record.identifier.clone())
            .collect::<Vec<_>>();
        top_level_identifiers.sort();
        top_level_identifiers.dedup();
        if top_level_identifiers.is_empty() {
            continue;
        }

        let mut related_identifiers = top_level_identifiers
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        loop {
            let children = snapshot
                .records
                .iter()
                .filter(|record| {
                    record
                        .container
                        .as_deref()
                        .is_some_and(|container| related_identifiers.contains(container))
                })
                .map(|record| record.identifier.clone())
                .collect::<Vec<_>>();
            let before = related_identifiers.len();
            related_identifiers.extend(children);
            if related_identifiers.len() == before {
                break;
            }
        }
        verification_identifiers.extend(related_identifiers.iter().cloned());

        for record in &snapshot.records {
            if record.kind != RecordKind::LegacyAgent
                || !related_identifiers.contains(&record.identifier)
            {
                continue;
            }
            let Some(plist_path) = record.plist_path.as_deref() else {
                continue;
            };
            let key = (record.identifier.clone(), plist_path.to_string());
            if seen_services.insert(key) {
                services.push(RelatedService {
                    identifier: record.identifier.clone(),
                    plist_path: PathBuf::from(plist_path),
                });
            }
        }

        plans.push(StoreRemovalPlan {
            path: snapshot.path,
            generated_uid: snapshot.generated_uid,
            top_level_identifiers,
        });
    }

    services.sort_by(|left, right| {
        left.plist_path
            .cmp(&right.plist_path)
            .then_with(|| left.identifier.cmp(&right.identifier))
    });
    Ok((plans, services, verification_identifiers))
}

fn all_store_snapshots() -> Result<Vec<StoreSnapshot>, String> {
    if unsafe { geteuid() } != 0 {
        return Err("读取全部 macOS 后台项目数据库需要管理员权限。".to_string());
    }
    let mut paths = fs::read_dir(BTM_DIRECTORY)
        .map_err(|error| format!("无法读取 macOS 后台项目数据库目录：{error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("BackgroundItems-v") && name.ends_with(".btm"))
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut snapshots = Vec::new();
    for path in paths {
        validate_privileged_store_path(&path)?;
        let value = Value::from_file(&path)
            .map_err(|error| plist_database_error("无法读取后台项目数据库", &path, error))?;
        let Some(generated_uid) = user_store_identifier(&value)? else {
            continue;
        };
        let records = parse_user_store(&value, &generated_uid)
            .map_err(|error| format!("无法解析 {}：{error}", path.display()))?
            .ok_or_else(|| format!("无法解析 {}：缺少当前用户记录", path.display()))?;
        snapshots.push(StoreSnapshot {
            path,
            generated_uid,
            records,
        });
    }
    Ok(snapshots)
}

fn user_store_identifier(value: &Value) -> Result<Option<String>, String> {
    let archive = KeyedArchive::new(value)?;
    let Some(top) = value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("$top"))
        .and_then(Value::as_dictionary)
    else {
        return Ok(None);
    };
    let Some(user_store_value) = top.get("userStore") else {
        return Ok(None);
    };
    let Some(user_store) = archive.dictionary(user_store_value) else {
        return Ok(None);
    };
    Ok(archive
        .string_for_key(user_store, "userIdentifier")
        .map(str::to_string))
}

fn validate_related_services(services: &[RelatedService], home: &Path) -> Result<(), String> {
    for service in services {
        if !service.plist_path.is_file() {
            continue;
        }
        let metadata = fs::symlink_metadata(&service.plist_path).map_err(|error| {
            format!(
                "无法读取启动配置文件 {}：{error}",
                service.plist_path.display()
            )
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(format!(
                "启动配置文件不是普通文件，已拒绝删除：{}",
                service.plist_path.display()
            ));
        }
        let kind = related_service_kind(&service.plist_path, home).ok_or_else(|| {
            format!(
                "启动配置文件不在受管理的目录中，已拒绝删除：{}",
                service.plist_path.display()
            )
        })?;
        let value = Value::from_file(&service.plist_path).map_err(|error| {
            format!(
                "无法读取启动配置文件 {}：{error}",
                service.plist_path.display()
            )
        })?;
        let label = value
            .as_dictionary()
            .and_then(|dictionary| dictionary.get("Label"))
            .and_then(Value::as_string)
            .ok_or_else(|| {
                format!(
                    "启动配置文件缺少 Label，已拒绝删除：{}",
                    service.plist_path.display()
                )
            })?;
        let expected = launch_label_from_identifier(&service.identifier)
            .ok_or_else(|| format!("无法从后台项目标识解析启动服务名称：{}", service.identifier))?;
        if label != expected {
            return Err(format!(
                "启动配置文件 Label 不匹配（期望 {expected}，实际 {label}），已拒绝删除：{}",
                service.plist_path.display()
            ));
        }
        if matches!(kind, RelatedServiceKind::SystemDaemon)
            && !service.plist_path.starts_with("/Library/LaunchDaemons")
        {
            return Err("系统守护进程路径校验失败，已拒绝删除。".to_string());
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RelatedServiceKind {
    UserAgent,
    GlobalAgent,
    SystemDaemon,
}

fn related_service_kind(path: &Path, home: &Path) -> Option<RelatedServiceKind> {
    let parent = path.parent()?;
    if parent == home.join("Library/LaunchAgents") {
        Some(RelatedServiceKind::UserAgent)
    } else if parent == Path::new("/Library/LaunchAgents") {
        Some(RelatedServiceKind::GlobalAgent)
    } else if parent == Path::new("/Library/LaunchDaemons") {
        Some(RelatedServiceKind::SystemDaemon)
    } else {
        None
    }
}

fn remove_related_service(service: &RelatedService, uid: u32, home: &Path) -> Result<bool, String> {
    let Some(kind) = related_service_kind(&service.plist_path, home) else {
        return Err(format!(
            "启动配置文件不在受管理的目录中，已拒绝删除：{}",
            service.plist_path.display()
        ));
    };
    let Some(label) = launch_label_from_identifier(&service.identifier) else {
        return Err(format!(
            "无法从后台项目标识解析启动服务名称：{}",
            service.identifier
        ));
    };
    let target = match kind {
        RelatedServiceKind::UserAgent | RelatedServiceKind::GlobalAgent => {
            format!("gui/{uid}/{label}")
        }
        RelatedServiceKind::SystemDaemon => format!("system/{label}"),
    };
    let _ = run_launchctl(&["bootout", &target]);
    match fs::remove_file(&service.plist_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "无法删除启动配置文件 {}：{error}",
            service.plist_path.display()
        )),
    }
}

fn launch_label_from_identifier(identifier: &str) -> Option<String> {
    let (kind, label) = identifier.split_once('.')?;
    if kind.parse::<u32>().is_ok() && !label.is_empty() {
        Some(label.to_string())
    } else {
        None
    }
}

fn stop_btm_services(uid: u32) {
    let daemon = format!("system/{BTM_DAEMON_LABEL}");
    let agent = format!("gui/{uid}/{BTM_AGENT_LABEL}");
    stop_launch_service(&daemon);
    stop_launch_service(&agent);
}

fn stop_launch_service(target: &str) {
    if run_launchctl(&["bootout", target]).is_ok() && run_launchctl(&["print", target]).is_err() {
        return;
    }
    let _ = run_launchctl(&["kill", "SIGKILL", target]);
}

fn btm_daemon_pid() -> Result<Option<u32>, String> {
    let output = Command::new("/bin/launchctl")
        .args(["print", &format!("system/{BTM_DAEMON_LABEL}")])
        .output()
        .map_err(|error| format!("无法查询 macOS 后台活动服务：{error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("pid = ")
                .and_then(|value| value.parse::<u32>().ok())
        }))
}

fn stop_btm_daemon() -> Result<(), String> {
    for _attempt in 0..4 {
        let Some(pid) = btm_daemon_pid()? else {
            return Ok(());
        };
        if !btm_daemon_process_matches(pid)? {
            return Err(format!(
                "系统后台活动服务的进程校验失败（PID {pid}），已拒绝停止该进程。"
            ));
        }
        let status = Command::new("/bin/kill")
            .args(["-9", &pid.to_string()])
            .status()
            .map_err(|error| format!("无法停止 macOS 后台活动服务：{error}"))?;
        if !status.success() {
            return Err(format!(
                "无法停止 macOS 后台活动服务（PID {pid}），系统可能拒绝了这个操作。"
            ));
        }
        if wait_for_btm_daemon_exit()? {
            return Ok(());
        }
    }
    Err("macOS 后台活动服务被停止后又立即重新启动了，无法安全替换数据库。".to_string())
}

fn btm_daemon_process_matches(pid: u32) -> Result<bool, String> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map_err(|error| format!("无法校验 macOS 后台活动服务进程：{error}"))?;
    if !output.status.success() {
        return Ok(false);
    }
    let command = String::from_utf8_lossy(&output.stdout);
    Ok(command.trim()
        == format!(
            "/System/Library/PrivateFrameworks/BackgroundTaskManagement.framework/Resources/backgroundtaskmanagementd -daemon"
        ))
}

fn wait_for_btm_daemon_exit() -> Result<bool, String> {
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(50));
        if btm_daemon_pid()?.is_none() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn start_btm_daemon() -> Result<(), String> {
    if btm_daemon_pid()?.is_none() {
        let status = Command::new("/usr/bin/sfltool")
            .arg("dumpbtm")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| format!("无法启动 macOS 后台活动服务：{error}"))?;
        if !status.success() {
            return Err(
                "后台项目数据库已更新，但无法重新启动 macOS 后台活动服务；请重新打开“系统设置”或重启 Mac。"
                    .to_string(),
            );
        }
    }
    for _ in 0..50 {
        if btm_daemon_pid()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("macOS 后台活动服务未能重新启动；请重新打开“系统设置”或重启 Mac。".to_string())
}

fn dump_btm() -> Result<String, String> {
    let output = Command::new("/usr/bin/sfltool")
        .arg("dumpbtm")
        .output()
        .map_err(|error| format!("无法校验 macOS 后台项目：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "无法校验 macOS 后台项目：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn dump_contains_identifiers(dump: &str, identifiers: &HashSet<String>) -> bool {
    dump.lines().any(|line| {
        line.trim()
            .strip_prefix("Identifier: ")
            .is_some_and(|identifier| identifiers.contains(identifier))
    })
}

fn restart_btm_services(uid: u32) -> Result<(), String> {
    ensure_launch_service(
        "system",
        &format!("system/{BTM_DAEMON_LABEL}"),
        BTM_DAEMON_PLIST,
    )?;
    ensure_launch_service(
        &format!("gui/{uid}"),
        &format!("gui/{uid}/{BTM_AGENT_LABEL}"),
        BTM_AGENT_PLIST,
    )
}

fn ensure_launch_service(domain: &str, target: &str, plist: &str) -> Result<(), String> {
    if run_launchctl(&["print", target]).is_err() {
        run_launchctl(&["bootstrap", domain, plist]).map_err(|error| {
            format!(
                "无法重新载入 {}：{error}",
                plist.rsplit('/').next().unwrap_or(plist)
            )
        })?;
    }
    let _ = run_launchctl(&["kickstart", "-k", target]);
    Ok(())
}

fn run_launchctl(arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("/bin/launchctl")
        .args(arguments)
        .output()
        .map_err(|error| format!("无法执行 launchctl：{error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn enabled_for_identifier(identifier: &str) -> Result<Option<bool>, String> {
    Ok(current_user_records()?
        .into_iter()
        .find(|record| record.identifier == identifier)
        .map(|record| record.enabled))
}

pub fn remove_records_from_store(
    path: &Path,
    generated_uid: &str,
    identifiers: &[String],
) -> Result<RemovalOutcome, String> {
    remove_records_from_store_inner(path, generated_uid, identifiers, true)?
        .ok_or_else(|| "后台项目数据库中不存在记录，未修改系统数据库。".to_string())
}

fn remove_records_from_store_if_present(
    path: &Path,
    generated_uid: &str,
    identifiers: &[String],
) -> Result<Option<RemovalOutcome>, String> {
    remove_records_from_store_inner(path, generated_uid, identifiers, false)
}

fn remove_records_from_store_inner(
    path: &Path,
    generated_uid: &str,
    identifiers: &[String],
    require_all: bool,
) -> Result<Option<RemovalOutcome>, String> {
    if identifiers.is_empty() {
        return Err("没有指定要删除的后台项目记录".to_string());
    }
    let mut value = Value::from_file(path)
        .map_err(|error| plist_database_error("无法读取后台项目数据库", path, error))?;
    let (records_index, entries) = user_store_entries(&value, generated_uid)?;
    let requested = identifiers.iter().cloned().collect::<HashSet<_>>();
    for identifier in identifiers {
        let matching = entries
            .iter()
            .filter(|entry| entry.identifier == *identifier)
            .collect::<Vec<_>>();
        if matching.is_empty() {
            if require_all {
                return Err(format!("后台项目数据库中不存在记录：{identifier}"));
            }
            continue;
        }
        if matching.iter().any(|entry| entry.container.is_some()) {
            return Err(format!("记录不是顶层后台项目，已拒绝删除：{identifier}"));
        }
    }

    let mut remove_identifiers = requested.clone();
    remove_identifiers.retain(|identifier| {
        entries
            .iter()
            .any(|entry| entry.identifier == *identifier && entry.container.is_none())
    });
    if remove_identifiers.is_empty() {
        return Ok(None);
    }
    loop {
        let children = entries
            .iter()
            .filter(|entry| {
                entry
                    .container
                    .as_deref()
                    .is_some_and(|container| remove_identifiers.contains(container))
            })
            .map(|entry| entry.identifier.clone())
            .collect::<Vec<_>>();
        let mut added = 0;
        for identifier in children {
            if remove_identifiers.insert(identifier) {
                added += 1;
            }
        }
        if added == 0 {
            break;
        }
    }
    let remove_uids = entries
        .iter()
        .filter(|entry| remove_identifiers.contains(&entry.identifier))
        .map(|entry| entry.uid)
        .collect::<HashSet<_>>();
    let removed_records = remove_uids.len();
    let backup_path = backup_store(path)?;

    let objects = value
        .as_dictionary_mut()
        .and_then(|dictionary| dictionary.get_mut("$objects"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "后台项目数据库缺少 NSKeyedArchiver $objects".to_string())?;
    if let Some(records) = objects
        .get_mut(records_index)
        .and_then(Value::as_dictionary_mut)
        .and_then(|dictionary| dictionary.get_mut("NS.objects"))
        .and_then(Value::as_array_mut)
    {
        records.retain(|value| value_uid(value).is_none_or(|index| !remove_uids.contains(&index)));
    }
    for index in remove_uids {
        if let Some(object) = objects.get_mut(index) {
            *object = Value::String("$null".to_string());
        }
    }

    let temporary = path.with_extension(format!("btm-tmp-{}", std::process::id()));
    value
        .to_file_binary(&temporary)
        .map_err(|error| format!("无法写入临时后台项目数据库：{error}"))?;
    let verification = Value::from_file(&temporary)
        .map_err(|error| format!("无法重新读取临时后台项目数据库：{error}"))?;
    let remaining = parse_user_store(&verification, generated_uid)
        .map_err(|error| format!("无法验证临时后台项目数据库：{error}"))?
        .ok_or_else(|| "临时后台项目数据库缺少当前用户记录".to_string())?;
    if remaining
        .iter()
        .any(|record| remove_identifiers.contains(&record.identifier))
    {
        let _ = fs::remove_file(&temporary);
        return Err("后台项目删除校验失败，未写入系统数据库。".to_string());
    }
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("无法设置后台项目数据库权限：{error}"))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("无法替换后台项目数据库：{error}"));
    }
    Ok(Some(RemovalOutcome {
        removed_records,
        backup_path,
    }))
}

fn backup_store(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "后台项目数据库文件名无效".to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = path.with_file_name(format!("{file_name}.backup-{stamp}"));
    fs::copy(path, &backup)
        .map_err(|error| database_error("无法备份后台项目数据库", path, error))?;
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("无法设置后台项目数据库备份权限：{error}"))?;
    Ok(backup)
}

fn database_error(context: &str, path: &Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "macOS 拒绝访问后台项目数据库。请先在“系统设置 → 隐私与安全性 → 完全磁盘访问权限”中授权当前 App，然后重新打开 App 再试。"
        )
    } else {
        format!("{context} {}：{error}", path.display())
    }
}

fn plist_database_error(context: &str, path: &Path, error: plist::Error) -> String {
    let detail = error.to_string();
    if detail.contains("Operation not permitted") {
        "macOS 拒绝访问后台项目数据库。请先在“系统设置 → 隐私与安全性 → 完全磁盘访问权限”中授权当前 App，然后重新打开 App 再试。".to_string()
    } else {
        format!("{context} {}：{detail}", path.display())
    }
}

struct UserStoreEntry {
    uid: usize,
    identifier: String,
    container: Option<String>,
}

fn user_store_entries(
    value: &Value,
    generated_uid: &str,
) -> Result<(usize, Vec<UserStoreEntry>), String> {
    let archive = KeyedArchive::new(value)?;
    let top = value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("$top"))
        .and_then(Value::as_dictionary)
        .ok_or_else(|| "缺少 NSKeyedArchiver $top".to_string())?;
    let user_store_value = top
        .get("userStore")
        .ok_or_else(|| "后台项目数据库缺少当前用户存储".to_string())?;
    let user_store = archive
        .dictionary(user_store_value)
        .ok_or_else(|| "后台项目数据库的当前用户存储无效".to_string())?;
    let user_identifier = archive
        .string_for_key(user_store, "userIdentifier")
        .ok_or_else(|| "后台项目数据库缺少用户标识".to_string())?;
    if !user_identifier.eq_ignore_ascii_case(generated_uid) {
        return Err("后台项目数据库不属于当前用户".to_string());
    }
    let records_value = user_store
        .get("records")
        .ok_or_else(|| "后台项目数据库缺少记录列表".to_string())?;
    let records_index =
        value_uid(records_value).ok_or_else(|| "后台项目数据库记录列表引用无效".to_string())?;
    let records_object = archive
        .dictionary(records_value)
        .ok_or_else(|| "后台项目数据库记录列表无效".to_string())?;
    let record_values = archive
        .array_for_key(records_object, "NS.objects")
        .ok_or_else(|| "后台项目数据库记录列表为空或无效".to_string())?;
    let objects = value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("$objects"))
        .and_then(Value::as_array)
        .ok_or_else(|| "后台项目数据库缺少 NSKeyedArchiver $objects".to_string())?;
    let entries = record_values
        .iter()
        .filter_map(|value| {
            let uid = value_uid(value)?;
            let record = objects.get(uid)?.as_dictionary()?;
            let identifier = object_string_field(objects, record, "identifier")?.to_string();
            let container = object_string_field(objects, record, "container")
                .filter(|value| *value != "$null")
                .map(str::to_string);
            Some(UserStoreEntry {
                uid,
                identifier,
                container,
            })
        })
        .collect();
    Ok((records_index, entries))
}

fn object_string_field<'a>(
    objects: &'a [Value],
    dictionary: &'a Dictionary,
    key: &str,
) -> Option<&'a str> {
    let value = dictionary.get(key)?;
    let value = value_uid(value)
        .and_then(|index| objects.get(index))
        .unwrap_or(value);
    value.as_string()
}

fn value_uid(value: &Value) -> Option<usize> {
    match value {
        Value::Uid(uid) => usize::try_from(uid.get()).ok(),
        Value::Dictionary(dictionary) => dictionary
            .get("CF$UID")
            .and_then(Value::as_unsigned_integer)
            .and_then(|value| usize::try_from(value).ok()),
        _ => None,
    }
}

fn current_user_generated_uid() -> Result<String, String> {
    let user = env::var("USER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let output = Command::new("/usr/bin/id").arg("-un").output().ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| "无法确定当前用户".to_string())?;
    let account = format!("/Users/{user}");
    let output = Command::new("/usr/bin/dscl")
        .args([".", "-read", &account, "GeneratedUID"])
        .output()
        .map_err(|error| format!("无法读取当前用户标识：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "无法读取当前用户标识：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_once(':')
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "当前用户没有 GeneratedUID".to_string())
}

fn user_store_matches(path: &Path, generated_uid: &str) -> Result<bool, String> {
    let value =
        Value::from_file(path).map_err(|error| format!("无法解析 {}：{error}", path.display()))?;
    parse_user_store(&value, generated_uid)
        .map_err(|error| format!("无法解析 {}：{error}", path.display()))
        .map(|records| records.is_some())
}

fn parse_user_store(value: &Value, generated_uid: &str) -> Result<Option<Vec<Record>>, String> {
    let archive = KeyedArchive::new(value)?;
    let top = value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("$top"))
        .and_then(Value::as_dictionary)
        .ok_or_else(|| "缺少 NSKeyedArchiver $top".to_string())?;
    let Some(user_store_value) = top.get("userStore") else {
        return Ok(None);
    };
    let Some(user_store) = archive.dictionary(user_store_value) else {
        return Ok(None);
    };
    let Some(user_identifier) = archive.string_for_key(user_store, "userIdentifier") else {
        return Ok(None);
    };
    if !user_identifier.eq_ignore_ascii_case(generated_uid) {
        return Ok(None);
    }
    let Some(records_value) = archive.value_for_key(user_store, "records") else {
        return Ok(Some(Vec::new()));
    };
    let Some(records_object) = archive.dictionary(records_value) else {
        return Ok(Some(Vec::new()));
    };
    let Some(record_values) = archive.array_for_key(records_object, "NS.objects") else {
        return Ok(Some(Vec::new()));
    };
    Ok(Some(
        record_values
            .iter()
            .filter_map(|value| archive.record(value))
            .collect(),
    ))
}

struct KeyedArchive {
    objects: Vec<Value>,
}

impl KeyedArchive {
    fn new(root: &Value) -> Result<Self, String> {
        let objects = root
            .as_dictionary()
            .and_then(|dictionary| dictionary.get("$objects"))
            .and_then(Value::as_array)
            .ok_or_else(|| "缺少 NSKeyedArchiver $objects".to_string())?
            .clone();
        Ok(Self { objects })
    }

    fn resolve<'a>(&'a self, value: &'a Value) -> Option<&'a Value> {
        let index = match value {
            Value::Uid(uid) => Some(uid.get()),
            Value::Dictionary(dictionary) => dictionary
                .get("CF$UID")
                .and_then(Value::as_unsigned_integer),
            _ => None,
        };
        match index {
            Some(index) => self.objects.get(index as usize),
            None => Some(value),
        }
    }

    fn value_for_key<'a>(&'a self, dictionary: &'a Dictionary, key: &str) -> Option<&'a Value> {
        dictionary.get(key).and_then(|value| self.resolve(value))
    }

    fn dictionary<'a>(&'a self, value: &'a Value) -> Option<&'a Dictionary> {
        self.resolve(value).and_then(Value::as_dictionary)
    }

    fn string<'a>(&'a self, value: &'a Value) -> Option<&'a str> {
        self.resolve(value).and_then(Value::as_string)
    }

    fn string_for_key<'a>(&'a self, dictionary: &'a Dictionary, key: &str) -> Option<&'a str> {
        self.value_for_key(dictionary, key)
            .and_then(|value| self.string(value))
            .filter(|value| *value != "$null" && !value.is_empty())
    }

    fn integer_for_key(&self, dictionary: &Dictionary, key: &str) -> Option<u64> {
        self.value_for_key(dictionary, key)
            .and_then(Value::as_unsigned_integer)
    }

    fn array_for_key<'a>(
        &'a self,
        dictionary: &'a Dictionary,
        key: &str,
    ) -> Option<&'a Vec<Value>> {
        self.value_for_key(dictionary, key)
            .and_then(Value::as_array)
    }

    fn record(&self, value: &Value) -> Option<Record> {
        let dictionary = self.dictionary(value)?;
        let identifier = self.string_for_key(dictionary, "identifier")?.to_string();
        let name = self
            .string_for_key(dictionary, "name")
            .filter(|value| *value != "$null")
            .unwrap_or("未命名项目")
            .to_string();
        let developer = self
            .string_for_key(dictionary, "developerName")
            .filter(|value| *value != "$null")
            .unwrap_or("")
            .to_string();
        let type_bits = self.integer_for_key(dictionary, "type").unwrap_or_default();
        let disposition = self
            .integer_for_key(dictionary, "disposition")
            .unwrap_or_default();
        let url_path = self
            .value_for_key(dictionary, "url")
            .and_then(|value| self.url_path(value));
        let app_path = url_path
            .as_deref()
            .filter(|path| is_app_bundle(Path::new(path)))
            .map(str::to_string);
        let executable_path = self
            .string_for_key(dictionary, "executablePath")
            .filter(|value| *value != "$null")
            .map(str::to_string);
        let plist_path = url_path
            .as_deref()
            .filter(|path| path.ends_with(".plist"))
            .map(str::to_string);
        let display_path = app_path
            .clone()
            .or_else(|| executable_path.clone())
            .or_else(|| url_path.clone());
        Some(Record {
            identifier,
            name,
            developer,
            kind: RecordKind::from_type_bits(type_bits),
            enabled: disposition & 1 != 0,
            app_path,
            executable_path,
            display_path,
            last_use: self.last_use(dictionary),
            container: self
                .string_for_key(dictionary, "container")
                .filter(|value| *value != "$null")
                .map(str::to_string),
            plist_path,
        })
    }

    fn url_path(&self, value: &Value) -> Option<String> {
        if let Some(value) = self.string(value) {
            return file_url_to_path(value);
        }
        let dictionary = self.dictionary(value)?;
        let relative = self.string_for_key(dictionary, "NS.relative")?;
        if relative.starts_with("file://") {
            return file_url_to_path(relative);
        }
        let base = self
            .string_for_key(dictionary, "NS.base")
            .unwrap_or("file:///");
        if base.starts_with("file://") {
            let combined = if base.ends_with('/') {
                format!("{base}{relative}")
            } else {
                format!("{base}/{relative}")
            };
            return file_url_to_path(&combined);
        }
        None
    }

    fn last_use(&self, dictionary: &Dictionary) -> Option<SystemTime> {
        let value = self.value_for_key(dictionary, "lastUseDate")?;
        if let Value::Date(date) = self.resolve(value)? {
            return Some((*date).into());
        }
        let date = self.dictionary(value)?;
        let seconds = self
            .value_for_key(date, "NS.time")
            .and_then(|value| {
                self.resolve(value).and_then(|resolved| match resolved {
                    Value::Real(value) => Some(*value),
                    Value::Integer(value) => value.as_unsigned().map(|value| value as f64),
                    _ => None,
                })
            })
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)?;
        UNIX_EPOCH.checked_add(Duration::from_secs_f64(
            PLIST_EPOCH_UNIX_SECONDS as f64 + seconds,
        ))
    }
}

impl RecordKind {
    fn from_type_bits(value: u64) -> Self {
        if value & 2 != 0 {
            Self::App
        } else if value & 32 != 0 {
            Self::Developer
        } else if value & 0x1_0000 != 0 || value & 8 != 0 {
            Self::LegacyAgent
        } else if value & 4 != 0 {
            Self::LoginItem
        } else if value & 8192 != 0 {
            Self::BackgroundTask
        } else {
            Self::Other
        }
    }
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("app"))
}

fn file_url_to_path(value: &str) -> Option<String> {
    let path = value.strip_prefix("file://")?;
    let path = percent_decode(path);
    let path = path.trim_end_matches('/');
    (!path.is_empty()).then(|| PathBuf::from(path).to_string_lossy().into_owned())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            if let (Some(high), Some(low)) = (high, low) {
                output.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Uid;

    fn uid(value: u64) -> Value {
        Value::Uid(Uid::new(value))
    }

    fn dictionary(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Dictionary(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    fn sample_archive() -> Value {
        let objects = Value::Array(vec![
            Value::String("$null".to_string()),
            dictionary([
                ("userIdentifier", uid(2)),
                ("records", uid(3)),
            ]),
            Value::String("CURRENT-USER".to_string()),
            dictionary([(
                "NS.objects",
                Value::Array(vec![uid(4), uid(9), uid(10)]),
            )]),
            dictionary([
                ("identifier", Value::String("2.com.example.app".to_string())),
                ("name", Value::String("Example".to_string())),
                ("developerName", Value::String("Example Co".to_string())),
                ("type", Value::Integer(2.into())),
                ("disposition", Value::Integer(11.into())),
                ("container", Value::String("$null".to_string())),
                (
                    "url",
                    dictionary([(
                        "NS.relative",
                        Value::String("file:///Applications/Example.app/".to_string()),
                    )]),
                ),
                ("lastUseDate", dictionary([("NS.time", Value::Real(815_049_760.0))])),
            ]),
            Value::String("$null".to_string()),
            Value::String("$null".to_string()),
            Value::String("$null".to_string()),
            Value::String("$null".to_string()),
            dictionary([
                (
                    "identifier",
                    Value::String("8.com.example.app.agent".to_string()),
                ),
                ("name", Value::String("Example Agent".to_string())),
                ("type", Value::Integer(65_544.into())),
                ("disposition", Value::Integer(11.into())),
                ("container", Value::String("2.com.example.app".to_string())),
                (
                    "url",
                    dictionary([(
                        "NS.relative",
                        Value::String(
                            "file:///Users/example/Library/LaunchAgents/com.example.app.agent.plist"
                                .to_string(),
                        ),
                    )]),
                ),
                (
                    "executablePath",
                    Value::String("/Applications/Example.app/Contents/MacOS/Example".to_string()),
                ),
            ]),
            dictionary([
                (
                    "identifier",
                    Value::String("Microsoft OneDrive".to_string()),
                ),
                ("name", Value::String("Microsoft OneDrive".to_string())),
                ("developerName", Value::String("Microsoft OneDrive".to_string())),
                ("type", Value::Integer(32.into())),
                ("disposition", Value::Integer(2.into())),
                ("container", Value::String("$null".to_string())),
            ]),
        ]);
        dictionary([
            ("$top", dictionary([("userStore", uid(1))])),
            ("$objects", objects),
        ])
    }

    #[test]
    fn reads_only_the_current_user_store() {
        let records = parse_user_store(&sample_archive(), "CURRENT-USER")
            .unwrap()
            .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].name, "Example");
        assert_eq!(
            records[0].app_path.as_deref(),
            Some("/Applications/Example.app")
        );
        assert!(records[0].enabled);
        assert_eq!(records[1].kind, RecordKind::LegacyAgent);
        assert_eq!(records[1].container.as_deref(), Some("2.com.example.app"));
        assert_eq!(records[2].kind, RecordKind::Developer);
        assert!(!records[2].enabled);
        assert!(
            parse_user_store(&sample_archive(), "OTHER-USER")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn decodes_file_urls_and_percent_escapes() {
        assert_eq!(
            file_url_to_path("file:///Applications/My%20App.app/"),
            Some("/Applications/My App.app".to_string())
        );
        assert_eq!(
            file_url_to_path("file:///Users/example/%E4%B8%AD%E6%96%87.app"),
            Some("/Users/example/中文.app".to_string())
        );
    }

    #[test]
    fn removes_a_user_record_with_its_children() {
        let path = env::temp_dir().join(format!(
            "background-items-test-{}-{}.btm",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        sample_archive().to_file_binary(&path).unwrap();
        let outcome =
            remove_records_from_store(&path, "CURRENT-USER", &["2.com.example.app".to_string()])
                .unwrap();
        assert_eq!(outcome.removed_records, 2);
        assert!(outcome.backup_path.is_file());
        let records = parse_user_store(&Value::from_file(&path).unwrap(), "CURRENT-USER")
            .unwrap()
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].identifier, "Microsoft OneDrive");
        let _ = fs::remove_file(outcome.backup_path);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn skips_missing_records_without_touching_the_store() {
        let path = env::temp_dir().join(format!(
            "background-items-missing-test-{}-{}.btm",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        sample_archive().to_file_binary(&path).unwrap();
        let before = Value::from_file(&path).unwrap();
        assert!(
            remove_records_from_store_if_present(
                &path,
                "CURRENT-USER",
                &["2.com.example.missing".to_string()]
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(Value::from_file(&path).unwrap(), before);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn only_exact_launch_directories_are_related() {
        let home = Path::new("/Users/example");
        assert!(matches!(
            related_service_kind(
                Path::new("/Users/example/Library/LaunchAgents/com.example.agent.plist"),
                home
            ),
            Some(RelatedServiceKind::UserAgent)
        ));
        assert!(matches!(
            related_service_kind(
                Path::new("/Library/LaunchDaemons/com.example.daemon.plist"),
                home
            ),
            Some(RelatedServiceKind::SystemDaemon)
        ));
        assert!(
            related_service_kind(
                Path::new("/System/Library/LaunchDaemons/com.apple.example.plist"),
                home
            )
            .is_none()
        );
    }
}
