use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SYNC_CHUNKS_PER_WORKER: usize = 8;

const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".DS_Store",
    "._*",
    "__MACOSX",
    "__MAXOSX",
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    "Thumbs.db",
    "desktop.ini",
    ".mymac-partial",
    "*.tmp",
];

const INVENTORY_SCRIPT: &str = r#"
root="$1"
case "$root" in
    "~") root="$HOME" ;;
    "~/"*) root="$HOME/${root#~/}" ;;
esac
if [ ! -d "$root" ]; then
    printf 'MISSING\t%s\n' "$root"
    exit 4
fi
printf 'ROOT|%s\n' "$root"
/usr/bin/find "$root" ! -path "$root" -exec /usr/bin/stat -f '%HT|%z|%N' {} +
printf 'DONE\n'
"#;

const CONNECTION_SCRIPT: &str = r#"
root="$1"
case "$root" in
    "~") root="$HOME" ;;
    "~/"*) root="$HOME/${root#~/}" ;;
esac
printf 'OK|%s|%s\n' "$USER" "$HOME"
if [ ! -d "$root" ]; then
    printf 'MISSING|%s\n' "$root"
    exit 4
fi
"#;

const TAR_ARCHIVE_SCRIPT: &str = r#"
root="$1"
shift
case "$root" in
    "~") root="$HOME" ;;
    "~/"*) root="$HOME/${root#~/}" ;;
esac
if [ ! -d "$root" ]; then
    printf 'MISSING\t%s\n' "$root" >&2
    exit 4
fi
cd "$root" || exit 5
exclude_args=()
for pattern in "$@"; do
    exclude_args+=("--exclude=$pattern")
done
exec /usr/bin/caffeinate -i /usr/bin/tar -cpf - --acls --xattrs "${exclude_args[@]}" .
"#;

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub remote_path: String,
    pub local_path: String,
    pub jobs: usize,
    pub delete_missing: bool,
    pub ignore_patterns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncConfig {
    #[serde(default = "default_ignore_patterns")]
    pub ignore_patterns: Vec<String>,
    #[serde(default)]
    pub devices: Vec<SyncDeviceConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SyncDeviceConfig {
    pub enabled: bool,
    pub host: String,
    pub user: String,
    pub port: String,
    pub remote_path: String,
    pub local_path: String,
    pub jobs: String,
    pub delete_missing: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            ignore_patterns: default_ignore_patterns(),
            devices: Vec::new(),
        }
    }
}

pub fn load_config() -> Result<SyncConfig, String> {
    let path = sync_config_path()?;
    if !path.exists() {
        let config = SyncConfig::default();
        save_config(&config)?;
        return Ok(config);
    }
    let bytes = fs::read(&path).map_err(|error| format!("无法读取同步忽略列表：{error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("无法解析同步忽略列表：{error}"))
}

pub fn save_config(config: &SyncConfig) -> Result<(), String> {
    let path = sync_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建同步配置目录：{error}"))?;
    }
    let bytes = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("无法编码同步忽略列表：{error}"))?;
    fs::write(path, bytes).map_err(|error| format!("无法保存同步忽略列表：{error}"))
}

pub fn parse_ignore_patterns(text: &str) -> Vec<String> {
    let mut patterns = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.trim_matches('/').to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    patterns.sort();
    patterns.dedup();
    patterns
}

pub fn default_ignore_patterns() -> Vec<String> {
    DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|pattern| (*pattern).to_string())
        .collect()
}

fn sync_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "无法确定用户目录。".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/我的 Mac 设置和开发工具安装/sync-config.json"))
}

#[derive(Clone, Debug)]
pub struct SyncProgress {
    pub processed_files: u64,
    pub total_files: u64,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    pub active_batches: usize,
    pub batch_count: usize,
    pub elapsed: Duration,
    pub percent: f32,
    pub rate_bytes_per_second: u64,
    pub small_file_mode: bool,
    pub stream_mode: bool,
}

#[derive(Clone, Debug)]
pub enum SyncEvent {
    Status(String),
    Progress(SyncProgress),
    Finished { success: bool, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Link,
}

#[derive(Clone, Debug)]
struct RemoteEntry {
    relative_path: String,
    kind: EntryKind,
    size: u64,
}

#[derive(Debug)]
struct Inventory {
    root: String,
    entries: Vec<RemoteEntry>,
    file_count: u64,
    total_bytes: u64,
    ignored_entries: u64,
}

#[derive(Clone, Debug)]
struct Batch {
    entries: Vec<RemoteEntry>,
    file_count: u64,
    total_bytes: u64,
}

enum WorkerMessage {
    Output {
        batch: usize,
        line: String,
        counted: bool,
    },
    Error {
        batch: usize,
        line: String,
        skippable: bool,
    },
    Finished {
        batch: usize,
        success: bool,
        cancelled: bool,
        items: u64,
        bytes: u64,
    },
    WorkerFinished,
}

pub fn test_connection(options: &SyncOptions, tx: Sender<SyncEvent>) {
    let result = test_connection_inner(options);
    let _ = tx.send(match result {
        Ok(message) => SyncEvent::Finished {
            success: true,
            message,
        },
        Err(message) => SyncEvent::Finished {
            success: false,
            message,
        },
    });
}

pub fn run_sync(options: SyncOptions, cancel: Arc<AtomicBool>, tx: Sender<SyncEvent>) {
    let mut reconnect_attempt = 0_u32;
    loop {
        let result = run_sync_inner(&options, &cancel, &tx);
        match result {
            Ok(message) => {
                let _ = tx.send(SyncEvent::Finished {
                    success: true,
                    message,
                });
                return;
            }
            Err(message) if cancel.load(Ordering::Relaxed) => {
                let _ = tx.send(SyncEvent::Finished {
                    success: false,
                    message,
                });
                return;
            }
            Err(message) if is_retryable_connection_error(&message) => {
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                let delay =
                    Duration::from_secs(2_u64.saturating_pow(reconnect_attempt.min(4)).min(30));
                let status = format!(
                    "连接中断：{message}；{delay_seconds} 秒后自动重连并继续（第 {reconnect_attempt} 次）",
                    delay_seconds = delay.as_secs()
                );
                if tx.send(SyncEvent::Status(status)).is_err() {
                    return;
                }
                if !wait_for_retry(&cancel, delay) {
                    let _ = tx.send(SyncEvent::Finished {
                        success: false,
                        message: "同步已停止；已完成的文件会保留。".to_string(),
                    });
                    return;
                }
            }
            Err(message) => {
                let _ = tx.send(SyncEvent::Finished {
                    success: false,
                    message,
                });
                return;
            }
        }
    }
}

fn is_retryable_connection_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "connection timed out",
        "operation timed out",
        "connection reset",
        "connection closed",
        "connection refused",
        "broken pipe",
        "host is down",
        "network is unreachable",
        "no route to host",
        "unexpectedly closed",
        "ssh_exchange_identification",
        "remote tar",
        "远端 tar 流失败",
        "扫描远端目录失败",
        "code 255",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_skippable_rsync_error(line: &str) -> bool {
    let lower = line.to_lowercase();
    [
        "permission denied",
        "operation not permitted",
        "file name too long",
        "no such file or directory",
        "file has vanished",
        "files vanished",
        "failed to stat",
        "cannot stat",
        "readlink",
        "too many levels of symbolic links",
        "socket ignored",
        "some files/attrs were not transferred",
        "code 23",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn wait_for_retry(cancel: &AtomicBool, delay: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < delay {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !cancel.load(Ordering::Relaxed)
}

pub fn prepare_ssh_access(options: &SyncOptions) -> Result<String, String> {
    validate_options(options)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "无法读取当前用户目录。".to_string())?;
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).map_err(|error| format!("创建 ~/.ssh 失败：{error}"))?;
    fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("设置 ~/.ssh 权限失败：{error}"))?;

    let key = ssh_dir.join("id_ed25519");
    if !key.exists() {
        let output = Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(format!(
                "{}@{}",
                std::env::var("USER").unwrap_or_default(),
                options.host
            ))
            .arg("-f")
            .arg(&key)
            .output()
            .map_err(|error| format!("无法启动 ssh-keygen：{error}"))?;
        if !output.status.success() {
            return Err(command_error("生成 SSH 密钥失败", &output.stderr));
        }
    }

    let shell_command = format!(
        "/usr/bin/ssh-copy-id -i {} -p {} {}",
        shell_quote(&key.to_string_lossy()),
        options.port,
        shell_quote(&ssh_target(options)?)
    );
    let script = format!(
        "tell application \"Terminal\" to do script {}",
        apple_script_quote(&shell_command)
    );
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .map_err(|error| format!("无法打开终端：{error}"))?;
    if !output.status.success() {
        return Err(command_error("无法打开终端配置免密登录", &output.stderr));
    }
    Ok("已在终端打开一次性授权。输入远端 Mac 登录密码后返回本页点击“检测连接”。".to_string())
}

fn has_completed_copy(options: &SyncOptions, local_root: &Path) -> bool {
    let has_files = fs::read_dir(local_root)
        .ok()
        .and_then(|mut entries| entries.next())
        .is_some();
    has_files && state_marker(options, local_root, "complete").is_some_and(|path| path.exists())
}

fn mark_completed_copy(options: &SyncOptions, local_root: &Path) -> Result<(), String> {
    let Some(marker) = state_marker(options, local_root, "complete") else {
        return Ok(());
    };
    write_state_marker(&marker, options, local_root)
}

fn mark_partial_copy(options: &SyncOptions, local_root: &Path) -> Result<(), String> {
    let Some(marker) = state_marker(options, local_root, "partial") else {
        return Ok(());
    };
    write_state_marker(&marker, options, local_root)
}

fn has_partial_copy(options: &SyncOptions, local_root: &Path) -> bool {
    state_marker(options, local_root, "partial").is_some_and(|path| path.exists())
}

fn clear_partial_copy(options: &SyncOptions, local_root: &Path) -> Result<(), String> {
    let Some(marker) = state_marker(options, local_root, "partial") else {
        return Ok(());
    };
    match fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法清理未完成同步状态：{error}")),
    }
}

fn write_state_marker(
    marker: &Path,
    options: &SyncOptions,
    local_root: &Path,
) -> Result<(), String> {
    let Some(parent) = marker.parent() else {
        return Err("同步状态文件缺少父目录。".to_string());
    };
    fs::create_dir_all(parent).map_err(|error| format!("无法创建同步状态目录：{error}"))?;
    let temporary = marker.with_file_name(format!(
        ".{}.{}.tmp",
        marker
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sync-state"),
        std::process::id()
    ));
    fs::write(
        &temporary,
        format!(
            "{}@{}:{}|{}|{}",
            options.user,
            options.host,
            options.port,
            options.remote_path,
            local_root.display()
        ),
    )
    .map_err(|error| format!("无法写入同步状态：{error}"))?;
    fs::rename(&temporary, marker).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("无法保存同步状态：{error}")
    })
}

fn state_marker(options: &SyncOptions, local_root: &Path, suffix: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let identity = format!(
        "{}@{}:{}|{}|{}",
        options.user,
        options.host,
        options.port,
        options.remote_path,
        local_root.display()
    );
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in identity.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(
        home.join("Library/Application Support/我的 Mac 设置和开发工具安装/sync-state")
            .join(format!("{hash:016x}.{suffix}")),
    )
}

fn test_connection_inner(options: &SyncOptions) -> Result<String, String> {
    validate_options(options)?;
    let command = remote_command(CONNECTION_SCRIPT, &options.remote_path);
    let mut process = ssh_command(options);
    let output = process
        .arg(command)
        .output()
        .map_err(|error| format!("无法启动 SSH：{error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Permission denied") || stderr.contains("publickey") {
            return Err(format!(
                "SSH 免密登录尚未配置。点击“配置免密登录”，在终端输入一次 {} 的密码。",
                ssh_target(options)?
            ));
        }
        if output.status.code() == Some(4) {
            return Err(format!("远端目录不存在：{}", options.remote_path));
        }
        return Err(format!("连接失败：{}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or_default();
    let fields = line.split('|').collect::<Vec<_>>();
    if fields.len() >= 3 && fields[0] == "OK" {
        Ok(format!(
            "连接成功：{}@{} · 远端目录 {} 可用。",
            fields[1], options.host, options.remote_path
        ))
    } else {
        Err(format!("连接返回了无法识别的内容：{}", line))
    }
}

fn run_sync_inner(
    options: &SyncOptions,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<SyncEvent>,
) -> Result<String, String> {
    validate_options(options)?;
    let started = Instant::now();
    send_status(tx, "准备连接远端 Mac…");
    test_connection_inner(options)?;

    send_status(tx, "正在扫描远端文件清单；大量小文件需要先完成索引…");
    let inventory = scan_remote(options, cancel, tx)?;
    let local_root = expand_local_path(&options.local_path)?;
    validate_local_destination(&local_root, options.delete_missing)?;
    fs::create_dir_all(&local_root)
        .map_err(|error| format!("无法创建本地目录 {}：{error}", local_root.display()))?;
    let small_file_mode = inventory.total_bytes < inventory.file_count.saturating_mul(1024 * 1024);
    let completed_copy = has_completed_copy(options, &local_root);
    let interrupted_copy = has_partial_copy(options, &local_root);
    let (results, engine, lanes) = if completed_copy {
        let batches = plan_work_batches(&inventory.entries, options.jobs, small_file_mode);
        let chunk_count = batches.len();
        let lanes = options.jobs.clamp(1, 8).min(chunk_count);
        send_status(
            tx,
            &format!(
                "索引完成：{} 个文件，{}，{} 路并行、{} 个动态工作单元。",
                inventory.file_count,
                format_bytes(inventory.total_bytes),
                lanes,
                chunk_count
            ),
        );
        let results = run_batches(
            options,
            &local_root,
            &inventory,
            batches,
            cancel,
            tx,
            started,
            small_file_mode,
        )?;
        (results, "多路 rsync 增量", lanes)
    } else if interrupted_copy {
        send_status(
            tx,
            "检测到上次首次传输尚未完成。正在继续 tar 流镜像；已有文件会保留，进度会立即更新…",
        );
        let results = run_stream_archive(options, &local_root, &inventory, cancel, tx, started)?;
        mark_completed_copy(options, &local_root)?;
        clear_partial_copy(options, &local_root)?;
        (results, "tar 流断点续传", 1)
    } else {
        send_status(
            tx,
            "索引完成。首次同步启用单路 tar 流镜像；实测比 4 路系统 rsync 快，并会立即显示字节进度。",
        );
        mark_partial_copy(options, &local_root)?;
        let results = run_stream_archive(options, &local_root, &inventory, cancel, tx, started)?;
        mark_completed_copy(options, &local_root)?;
        clear_partial_copy(options, &local_root)?;
        (results, "tar 流", 1)
    };

    if cancel.load(Ordering::Relaxed) {
        return Err("同步已停止；已完成的文件会保留。".to_string());
    }

    if options.delete_missing {
        send_status(tx, "正在删除本地已不存在于远端的文件…");
        run_delete_pass(options, &inventory.root, &local_root)?;
    }

    let elapsed = started.elapsed();
    let warning_note = if results.warnings > 0 {
        format!("；跳过 {} 条无权限/失效路径警告", results.warnings)
    } else {
        String::new()
    };
    let ignored_note = if inventory.ignored_entries > 0 {
        format!("；忽略列表排除 {} 项", inventory.ignored_entries)
    } else {
        String::new()
    };
    Ok(format!(
        "同步完成：{} 个文件，{}；耗时 {}；{}，{} 路；平均 {}/s{warning_note}{ignored_note}。",
        results.files,
        format_bytes(results.bytes),
        format_duration(elapsed),
        engine,
        lanes,
        format_bytes(results.rate)
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_stream_archive(
    options: &SyncOptions,
    local_root: &Path,
    inventory: &Inventory,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<SyncEvent>,
    started: Instant,
) -> Result<BatchResults, String> {
    let mut tar_arguments = Vec::with_capacity(options.ignore_patterns.len() + 1);
    tar_arguments.push(options.remote_path.clone());
    tar_arguments.extend(options.ignore_patterns.iter().cloned());
    let tar_argument_refs = tar_arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let mut archive = ssh_command(options)
        .arg(remote_command_with_args(
            TAR_ARCHIVE_SCRIPT,
            &tar_argument_refs,
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动远端 tar 流：{error}"))?;
    let mut extractor = Command::new("/usr/bin/tar")
        .args(["-xpf", "-", "--acls", "--xattrs", "-C"])
        .arg(local_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动本地 tar 解包：{error}"))?;

    let archive_stderr_reader = archive.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut text = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_string(&mut text);
            text
        })
    });
    let extract_stderr_reader = extractor.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut text = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_string(&mut text);
            text
        })
    });

    let mut source = archive
        .stdout
        .take()
        .ok_or_else(|| "无法读取远端 tar 流。".to_string())?;
    let mut destination = extractor
        .stdin
        .take()
        .ok_or_else(|| "无法写入本地 tar 解包进程。".to_string())?;
    let estimated_archive_bytes = inventory
        .total_bytes
        .saturating_add((inventory.entries.len() as u64).saturating_mul(1536))
        .saturating_add(1024 * 1024);
    let mut archive_bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut last_emit = Instant::now();
    let mut cancelled = false;
    let mut stream_error = None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            let _ = archive.kill();
            let _ = extractor.kill();
            break;
        }
        let read = match source.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                stream_error = Some(format!("读取远端 tar 流失败：{error}"));
                let _ = archive.kill();
                let _ = extractor.kill();
                break;
            }
        };
        if read == 0 {
            break;
        }
        if let Err(error) = destination.write_all(&buffer[..read]) {
            stream_error = Some(format!("写入本地 tar 解包进程失败：{error}"));
            let _ = archive.kill();
            let _ = extractor.kill();
            break;
        }
        archive_bytes = archive_bytes.saturating_add(read as u64);
        if last_emit.elapsed() >= Duration::from_millis(150) {
            send_stream_progress(
                tx,
                inventory,
                archive_bytes,
                estimated_archive_bytes,
                started,
            );
            last_emit = Instant::now();
        }
    }
    drop(destination);

    let archive_status = archive
        .wait()
        .map_err(|error| format!("等待远端 tar 流失败：{error}"))?;
    let extract_status = extractor
        .wait()
        .map_err(|error| format!("等待本地 tar 解包失败：{error}"))?;
    let archive_stderr = archive_stderr_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let extract_stderr = extract_stderr_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();

    if cancelled {
        return Err("同步已停止；已解包的文件会保留。".to_string());
    }
    if let Some(error) = stream_error {
        return Err(error);
    }
    if !archive_status.success() {
        return Err(command_error("远端 tar 流失败", archive_stderr.as_bytes()));
    }
    if !extract_status.success() {
        return Err(command_error(
            "本地 tar 解包失败",
            extract_stderr.as_bytes(),
        ));
    }

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let _ = tx.send(SyncEvent::Progress(SyncProgress {
        processed_files: inventory.file_count,
        total_files: inventory.file_count,
        completed_bytes: inventory.total_bytes,
        total_bytes: inventory.total_bytes,
        active_batches: 0,
        batch_count: 1,
        elapsed: started.elapsed(),
        percent: 100.0,
        rate_bytes_per_second: (inventory.total_bytes as f64 / elapsed) as u64,
        small_file_mode: inventory.total_bytes < inventory.file_count.saturating_mul(1024 * 1024),
        stream_mode: true,
    }));
    Ok(BatchResults {
        files: inventory.file_count,
        bytes: inventory.total_bytes,
        rate: (inventory.total_bytes as f64 / elapsed) as u64,
        warnings: 0,
    })
}

struct BatchResults {
    files: u64,
    bytes: u64,
    rate: u64,
    warnings: u64,
}

#[allow(clippy::too_many_arguments)]
fn run_batches(
    options: &SyncOptions,
    local_root: &Path,
    inventory: &Inventory,
    batches: Vec<Batch>,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<SyncEvent>,
    started: Instant,
    small_file_mode: bool,
) -> Result<BatchResults, String> {
    let total_batches = batches.len();
    if total_batches == 0 {
        send_completed_progress(tx, inventory, started, small_file_mode);
        return Ok(BatchResults {
            files: 0,
            bytes: 0,
            rate: 0,
            warnings: 0,
        });
    }
    let worker_count = options.jobs.clamp(1, 8).min(total_batches);
    let abort = Arc::new(AtomicBool::new(false));
    let (task_tx, task_rx) = mpsc::channel::<(usize, Batch)>();
    for (batch_index, batch) in batches.into_iter().enumerate() {
        let _ = task_tx.send((batch_index, batch));
    }
    drop(task_tx);
    let task_rx = Arc::new(Mutex::new(task_rx));
    let (worker_tx, worker_rx): (Sender<WorkerMessage>, Receiver<WorkerMessage>) = mpsc::channel();
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let sender = worker_tx.clone();
        let options = options.clone();
        let local_root = local_root.to_path_buf();
        let cancel = cancel.clone();
        let abort = abort.clone();
        let source_root = inventory.root.clone();
        let task_rx = task_rx.clone();
        handles.push(thread::spawn(move || {
            loop {
                if cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                    break;
                }
                let next = task_rx
                    .lock()
                    .expect("sync task queue poisoned")
                    .recv()
                    .ok();
                let Some((batch_index, batch)) = next else {
                    break;
                };
                run_batch(
                    batch_index,
                    batch,
                    &options,
                    &source_root,
                    &local_root,
                    cancel.clone(),
                    abort.clone(),
                    sender.clone(),
                );
            }
            let _ = sender.send(WorkerMessage::WorkerFinished);
        }));
    }
    drop(task_rx);
    drop(worker_tx);

    let mut active = worker_count;
    let mut completed_items = vec![0_u64; total_batches];
    let mut completed_bytes = vec![0_u64; total_batches];
    let mut batch_errors = vec![Vec::<String>::new(); total_batches];
    let mut worker_failures = Vec::new();
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut cancelled = false;
    let mut warnings = 0_u64;
    let mut fatal_error = None;
    let mut last_warning_status = Instant::now() - Duration::from_secs(2);

    while active > 0 {
        match worker_rx.recv_timeout(Duration::from_millis(150)) {
            Ok(WorkerMessage::Output {
                batch,
                line,
                counted,
            }) => {
                if let Some(size) = parse_xfer_size(&line)
                    && counted
                {
                    completed_items[batch] += 1;
                    completed_bytes[batch] += size;
                }
                if last_emit.elapsed() >= Duration::from_millis(120) {
                    send_progress(
                        tx,
                        inventory,
                        &completed_items,
                        &completed_bytes,
                        worker_count,
                        active,
                        started,
                        small_file_mode,
                        false,
                    );
                    last_emit = Instant::now();
                }
            }
            Ok(WorkerMessage::Error {
                batch,
                line,
                skippable,
            }) => {
                if skippable {
                    warnings = warnings.saturating_add(1);
                    if last_warning_status.elapsed() >= Duration::from_secs(2) {
                        send_status(
                            tx,
                            &format!(
                                "已跳过 {warnings} 条无法访问或路径过长项目，其余批次继续运行…"
                            ),
                        );
                        last_warning_status = Instant::now();
                    }
                } else {
                    if fatal_error.is_none() {
                        fatal_error = Some(format!("批次 {}：{line}", batch + 1));
                        abort.store(true, Ordering::Relaxed);
                    }
                    batch_errors[batch].push(line);
                }
            }
            Ok(WorkerMessage::Finished {
                batch,
                success,
                cancelled: was_cancelled,
                items,
                bytes,
            }) => {
                cancelled |= was_cancelled;
                if success {
                    completed_items[batch] = items;
                    completed_bytes[batch] = bytes;
                }
                send_progress(
                    tx,
                    inventory,
                    &completed_items,
                    &completed_bytes,
                    worker_count,
                    active,
                    started,
                    small_file_mode,
                    false,
                );
            }
            Ok(WorkerMessage::WorkerFinished) => {
                active = active.saturating_sub(1);
            }
            Err(RecvTimeoutError::Timeout) => {
                if last_emit.elapsed() >= Duration::from_millis(250) {
                    send_progress(
                        tx,
                        inventory,
                        &completed_items,
                        &completed_bytes,
                        worker_count,
                        active,
                        started,
                        small_file_mode,
                        false,
                    );
                    last_emit = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                worker_failures.push("同步工作线程通信意外结束。".to_string());
                break;
            }
        }
    }

    for handle in handles {
        if handle.join().is_err() {
            worker_failures.push("同步工作线程异常退出。".to_string());
        }
    }

    if let Some(error) = fatal_error {
        return Err(format!(
            "同步因严重错误停止；已完成内容会保留，下次可继续：\n{error}"
        ));
    }
    if cancelled {
        return Err("同步已停止；已完成的文件会保留。".to_string());
    }
    let mut failed = batch_errors
        .into_iter()
        .enumerate()
        .filter(|(_, errors)| !errors.is_empty())
        .map(|(batch, errors)| {
            format!(
                "批次 {}：{}",
                batch + 1,
                errors
                    .into_iter()
                    .rev()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("；")
            )
        })
        .collect::<Vec<_>>();
    failed.extend(worker_failures);
    if !failed.is_empty() {
        let detail = failed
            .into_iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("部分批次失败：\n{detail}"));
    }
    let bytes = completed_bytes.iter().sum::<u64>();
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    Ok(BatchResults {
        files: completed_items.iter().sum(),
        bytes,
        rate: (bytes as f64 / elapsed) as u64,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_batch(
    batch_index: usize,
    batch: Batch,
    options: &SyncOptions,
    source_root: &str,
    local_root: &Path,
    cancel: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    sender: Sender<WorkerMessage>,
) {
    let expected_directories = batch
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .map(|entry| entry.relative_path.trim_end_matches('/').to_string())
        .collect::<HashSet<_>>();
    let mut command = rsync_command(options, source_root, local_root);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = sender.send(WorkerMessage::Error {
                batch: batch_index,
                line: format!("无法启动 rsync：{error}"),
                skippable: false,
            });
            let _ = sender.send(WorkerMessage::Finished {
                batch: batch_index,
                success: false,
                cancelled: false,
                items: 0,
                bytes: 0,
            });
            return;
        }
    };

    let stdin_writer = child.stdin.take().map(|mut stdin| {
        let entries = batch.entries.clone();
        thread::spawn(move || {
            for entry in entries {
                if stdin.write_all(entry.relative_path.as_bytes()).is_err()
                    || stdin.write_all(&[0]).is_err()
                {
                    break;
                }
            }
        })
    });

    let stdout_reader = child.stdout.take().map(|stdout| {
        let sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let counted = parse_xfer_path(&line)
                    .map(|path| is_file_transfer_path(path, &expected_directories))
                    .unwrap_or(false);
                let _ = sender.send(WorkerMessage::Output {
                    batch: batch_index,
                    line,
                    counted,
                });
            }
        })
    });
    let stderr_reader = child.stderr.take().map(|stderr| {
        let sender = sender.clone();
        thread::spawn(move || {
            let lines = BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>();
            for line in &lines {
                if !line.trim().is_empty() {
                    let _ = sender.send(WorkerMessage::Error {
                        batch: batch_index,
                        line: line.clone(),
                        skippable: is_skippable_rsync_error(line),
                    });
                }
            }
            lines
        })
    });

    let mut was_cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) => {
                was_cancelled = true;
                let _ = child.kill();
            }
            Ok(None) => thread::sleep(Duration::from_millis(80)),
            Err(error) => {
                let _ = sender.send(WorkerMessage::Error {
                    batch: batch_index,
                    line: format!("等待 rsync 失败：{error}"),
                    skippable: false,
                });
                break None;
            }
        }
    };
    if let Some(reader) = stdout_reader {
        let _ = reader.join();
    }
    let stderr_lines = stderr_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if let Some(reader) = stdin_writer {
        let _ = reader.join();
    }
    let status_code = status.and_then(|status| status.code());
    let status_success = status.is_some_and(|status| status.success());
    let only_skippable_errors = !stderr_lines.is_empty()
        && stderr_lines
            .iter()
            .all(|line| is_skippable_rsync_error(line));
    let success = status_success
        || status_code.is_some_and(|code| (code == 23 || code == 24) && only_skippable_errors);
    let _ = sender.send(WorkerMessage::Finished {
        batch: batch_index,
        success,
        cancelled: was_cancelled,
        items: batch.file_count,
        bytes: batch.total_bytes,
    });
}

fn rsync_command(options: &SyncOptions, source_root: &str, local_root: &Path) -> Command {
    let mut command = Command::new("/usr/bin/rsync");
    command
        .arg("-aE")
        .arg("--no-r")
        .arg("--partial")
        .arg("--partial-dir=.mymac-partial")
        .arg("--rsync-path=/usr/bin/caffeinate -i /usr/bin/rsync")
        .arg("--8-bit-output")
        .arg("--from0")
        .arg("--files-from=-")
        .arg("--out-format=XFER|%l|%n")
        .arg("-e")
        .arg(ssh_transport(options))
        .arg(remote_source(options, source_root))
        .arg(ensure_trailing_slash(local_root))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub fn rsync_command_preview(options: &SyncOptions) -> Result<String, String> {
    validate_options(options)?;
    let local_root = expand_local_path(&options.local_path)?;
    let source = remote_source(options, &options.remote_path);
    let destination = ensure_trailing_slash(&local_root);
    let mut parts = vec![
        "/usr/bin/rsync".to_string(),
        "-aE".to_string(),
        "--partial".to_string(),
        "--partial-dir=.mymac-partial".to_string(),
        shell_quote("--rsync-path=/usr/bin/caffeinate -i /usr/bin/rsync"),
        "--8-bit-output".to_string(),
        "-e".to_string(),
        shell_quote(&ssh_transport(options)),
    ];
    for pattern in &options.ignore_patterns {
        parts.push("--exclude".to_string());
        parts.push(shell_quote(pattern));
    }
    if options.delete_missing {
        parts.push("--delete".to_string());
    }
    parts.push(shell_quote(&source));
    parts.push(shell_quote(&destination.to_string_lossy()));
    Ok(format!(
        "# 增量同步并发：{} 个独立 rsync 进程（rsync 本身没有 --parallel 参数）。\n\
         # 首次同步走单路 tar 流；下面是可单独运行的单路 rsync 命令，会递归同步整个目录。\n{}",
        options.jobs,
        parts.join(" ")
    ))
}

/// Reject overlapping targets before starting two device jobs at the same time.
/// The expanded paths are compared after resolving their existing ancestors so
/// `~`, symlinked Desktop folders, and equivalent absolute paths are caught too.
pub fn validate_distinct_destinations(options: &[SyncOptions]) -> Result<(), String> {
    let mut destinations = Vec::with_capacity(options.len());
    for options in options {
        validate_options(options)?;
        let expanded = expand_local_path(&options.local_path)?;
        let normalized = normalize_existing_ancestor(&expanded);
        if destinations.iter().any(|existing| existing == &normalized) {
            return Err(format!(
                "两台设备不能写入同一个本地文件夹：{}",
                expanded.display()
            ));
        }
        destinations.push(normalized);
    }
    Ok(())
}

fn run_delete_pass(
    options: &SyncOptions,
    source_root: &str,
    local_root: &Path,
) -> Result<(), String> {
    let output = Command::new("/usr/bin/rsync")
        .arg("-aE")
        .arg("--delete")
        .args(
            options
                .ignore_patterns
                .iter()
                .flat_map(|pattern| ["--exclude".to_string(), pattern.clone()]),
        )
        .arg("-e")
        .arg(ssh_transport(options))
        .arg(remote_source(options, source_root))
        .arg(ensure_trailing_slash(local_root))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("无法启动清理同步：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("删除本地多余文件失败", &output.stderr))
    }
}

fn scan_remote(
    options: &SyncOptions,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<SyncEvent>,
) -> Result<Inventory, String> {
    let command = remote_command(INVENTORY_SCRIPT, &options.remote_path);
    let mut child = ssh_command(options)
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法扫描远端目录：{error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取远端文件清单。".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取远端错误输出。".to_string())?;
    let (line_tx, line_rx) = mpsc::channel();
    let stdout_reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let stderr_reader = thread::spawn(move || {
        BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
            .join("\n")
    });

    let mut root = None;
    let mut entries = Vec::new();
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut ignored_entries = 0_u64;
    let mut done = false;
    let mut cancelled = false;
    let mut last_status = Instant::now();
    loop {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    let _ = child.kill();
                    break;
                }
                if let Some(value) = line.strip_prefix("ROOT|") {
                    root = Some(value.to_string());
                    continue;
                }
                if line == "DONE" {
                    done = true;
                    continue;
                }
                let Some(root_value) = root.as_deref() else {
                    continue;
                };
                let Some(mut entry) = parse_inventory_entry(&line) else {
                    continue;
                };
                let Some(relative_path) = relative_to_root(&entry.relative_path, root_value) else {
                    continue;
                };
                entry.relative_path = relative_path;
                if should_ignore_path(&entry.relative_path, &options.ignore_patterns) {
                    ignored_entries = ignored_entries.saturating_add(1);
                    continue;
                }
                if entry.kind == EntryKind::File {
                    file_count += 1;
                    total_bytes = total_bytes.saturating_add(entry.size);
                }
                entries.push(entry);
                if last_status.elapsed() >= Duration::from_millis(600) {
                    send_status(
                        tx,
                        &format!(
                            "正在索引远端文件：已发现 {file_count} 个文件 · 已忽略 {ignored_entries} 项…"
                        ),
                    );
                    last_status = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) if cancel.load(Ordering::Relaxed) => {
                cancelled = true;
                let _ = child.kill();
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("等待远端扫描失败：{error}"))?;
    let _ = stdout_reader.join();
    let stderr = stderr_reader.join().unwrap_or_default();
    if cancelled {
        return Err("同步已停止。".to_string());
    }
    if !status.success() {
        if status.code() == Some(4) {
            return Err(format!("远端目录不存在：{}", options.remote_path));
        }
        return Err(command_error("扫描远端目录失败", stderr.as_bytes()));
    }
    let root = root.ok_or_else(|| format!("远端没有返回目录信息：{}", stderr.trim()))?;
    if !done {
        return Err("远端文件清单不完整；请检查目录权限后重试。".to_string());
    }
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(Inventory {
        root,
        entries,
        file_count,
        total_bytes,
        ignored_entries,
    })
}

fn parse_inventory_entry(line: &str) -> Option<RemoteEntry> {
    let mut fields = line.splitn(3, '|');
    let kind = match fields.next()? {
        "Regular File" => EntryKind::File,
        "Directory" => EntryKind::Directory,
        "Symbolic Link" => EntryKind::Link,
        _ => return None,
    };
    let size = fields.next()?.parse::<u64>().ok()?;
    let relative_path = fields.next()?.to_string();
    if relative_path.is_empty() {
        return None;
    }
    Some(RemoteEntry {
        relative_path,
        kind,
        size,
    })
}

fn relative_to_root(path: &str, root: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    let relative = path.strip_prefix(root)?.strip_prefix('/')?.to_string();
    if relative.is_empty() || relative.split('/').any(|part| part == "..") {
        None
    } else {
        Some(relative)
    }
}

fn should_ignore_path(path: &str, patterns: &[String]) -> bool {
    let normalized = path.trim_start_matches("./").trim_end_matches('/');
    let components = normalized.split('/').collect::<Vec<_>>();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_matches('/');
        if pattern.is_empty() {
            return false;
        }
        if pattern.contains('/') {
            let pattern_components = pattern.split('/').collect::<Vec<_>>();
            components.len() >= pattern_components.len()
                && components.windows(pattern_components.len()).any(|window| {
                    window
                        .iter()
                        .zip(pattern_components.iter())
                        .all(|(component, pattern)| wildcard_match(pattern, component))
                })
        } else {
            components
                .iter()
                .any(|component| wildcard_match(pattern, component))
        }
    })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_lowercase().bytes().collect::<Vec<_>>();
    let value = value.to_lowercase().bytes().collect::<Vec<_>>();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if token == b'*' {
            current[0] = previous[0];
            let mut matched_prefix = current[0];
            for index in 1..=value.len() {
                matched_prefix |= previous[index];
                current[index] = matched_prefix;
            }
        } else {
            for index in 1..=value.len() {
                current[index] =
                    previous[index - 1] && (token == b'?' || token == value[index - 1]);
            }
        }
        previous = current;
    }
    previous[value.len()]
}

#[cfg(test)]
fn plan_batches(entries: &[RemoteEntry], jobs: usize) -> Vec<Batch> {
    plan_batches_with_mode(entries, jobs, false)
}

#[cfg(test)]
fn plan_batches_with_mode(
    entries: &[RemoteEntry],
    jobs: usize,
    small_file_mode: bool,
) -> Vec<Batch> {
    plan_batches_with_count(entries, jobs, small_file_mode)
}

fn plan_work_batches(entries: &[RemoteEntry], jobs: usize, small_file_mode: bool) -> Vec<Batch> {
    let count = jobs
        .clamp(1, 8)
        .saturating_mul(SYNC_CHUNKS_PER_WORKER)
        .min(entries.len().max(1));
    plan_batches_with_count(entries, count, small_file_mode)
}

fn plan_batches_with_count(
    entries: &[RemoteEntry],
    requested_batches: usize,
    small_file_mode: bool,
) -> Vec<Batch> {
    if entries.is_empty() {
        return Vec::new();
    }
    let count = requested_batches.clamp(1, entries.len());
    if small_file_mode {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let chunk_size = entries.len().div_ceil(count);
        return entries
            .chunks(chunk_size)
            .map(|chunk| make_batch(chunk.to_vec()))
            .collect();
    }

    let mut files = entries
        .iter()
        .filter(|entry| entry.kind != EntryKind::Directory)
        .cloned()
        .collect::<Vec<_>>();
    let directories = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .cloned()
        .collect::<Vec<_>>();
    if files.is_empty() {
        let chunk_size = directories.len().div_ceil(count);
        return directories
            .chunks(chunk_size)
            .map(|chunk| make_batch(chunk.to_vec()))
            .collect();
    }

    files.sort_by_key(|entry| std::cmp::Reverse(entry.size));
    let mut batches = (0..count)
        .map(|_| make_batch(Vec::new()))
        .collect::<Vec<_>>();
    for entry in files {
        let index = batches
            .iter()
            .enumerate()
            .min_by_key(|(_, batch)| batch.total_bytes)
            .map(|(index, _)| index)
            .unwrap_or(0);
        batches[index].file_count += 1;
        batches[index].total_bytes = batches[index].total_bytes.saturating_add(entry.size);
        batches[index].entries.push(entry);
    }
    for entry in directories {
        let index = batches
            .iter()
            .enumerate()
            .min_by_key(|(_, batch)| batch.entries.len())
            .map(|(index, _)| index)
            .unwrap_or(0);
        batches[index].entries.push(entry);
    }
    batches.retain(|batch| !batch.entries.is_empty());
    batches
}

fn make_batch(entries: Vec<RemoteEntry>) -> Batch {
    let file_count = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .count() as u64;
    let total_bytes = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .map(|entry| entry.size)
        .sum();
    Batch {
        entries,
        file_count,
        total_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn send_progress(
    tx: &Sender<SyncEvent>,
    inventory: &Inventory,
    completed_items: &[u64],
    completed_bytes: &[u64],
    batch_count: usize,
    active_batches: usize,
    started: Instant,
    small_file_mode: bool,
    stream_mode: bool,
) {
    let processed_files = completed_items
        .iter()
        .sum::<u64>()
        .min(inventory.file_count);
    let bytes = completed_bytes
        .iter()
        .sum::<u64>()
        .min(inventory.total_bytes);
    let elapsed = started.elapsed();
    let fraction = if small_file_mode {
        processed_files as f32 / inventory.file_count.max(1) as f32
    } else {
        bytes as f32 / inventory.total_bytes.max(1) as f32
    };
    let rate = (bytes as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
    let _ = tx.send(SyncEvent::Progress(SyncProgress {
        processed_files,
        total_files: inventory.file_count,
        completed_bytes: bytes,
        total_bytes: inventory.total_bytes,
        active_batches,
        batch_count,
        elapsed,
        percent: (fraction * 100.0).clamp(0.0, 100.0),
        rate_bytes_per_second: rate,
        small_file_mode,
        stream_mode,
    }));
}

fn send_stream_progress(
    tx: &Sender<SyncEvent>,
    inventory: &Inventory,
    archive_bytes: u64,
    estimated_archive_bytes: u64,
    started: Instant,
) {
    let fraction = (archive_bytes as f32 / estimated_archive_bytes.max(1) as f32).min(1.0);
    let data_bytes = (inventory.total_bytes as f64 * fraction as f64) as u64;
    let elapsed = started.elapsed();
    let rate = (data_bytes as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
    let _ = tx.send(SyncEvent::Progress(SyncProgress {
        processed_files: 0,
        total_files: inventory.file_count,
        completed_bytes: data_bytes,
        total_bytes: inventory.total_bytes,
        active_batches: 1,
        batch_count: 1,
        elapsed,
        percent: (fraction * 100.0).clamp(0.0, 99.9),
        rate_bytes_per_second: rate,
        small_file_mode: inventory.total_bytes < inventory.file_count.saturating_mul(1024 * 1024),
        stream_mode: true,
    }));
}

fn send_completed_progress(
    tx: &Sender<SyncEvent>,
    inventory: &Inventory,
    started: Instant,
    small_file_mode: bool,
) {
    let elapsed = started.elapsed();
    let rate = (inventory.total_bytes as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
    let _ = tx.send(SyncEvent::Progress(SyncProgress {
        processed_files: inventory.file_count,
        total_files: inventory.file_count,
        completed_bytes: inventory.total_bytes,
        total_bytes: inventory.total_bytes,
        active_batches: 0,
        batch_count: 0,
        elapsed,
        percent: 100.0,
        rate_bytes_per_second: rate,
        small_file_mode,
        stream_mode: false,
    }));
}

fn is_file_transfer_path(path: &str, directories: &HashSet<String>) -> bool {
    let normalized = path.trim_start_matches("./").trim_end_matches('/');
    !path.ends_with('/') && !normalized.is_empty() && !directories.contains(normalized)
}

fn send_status(tx: &Sender<SyncEvent>, message: &str) {
    let _ = tx.send(SyncEvent::Status(message.to_string()));
}

fn validate_options(options: &SyncOptions) -> Result<(), String> {
    let host = options.host.trim();
    let user = options.user.trim();
    if host.is_empty() || host.starts_with('-') {
        return Err("主机名或 IP 无效。".to_string());
    }
    if user.is_empty()
        || user.starts_with('-')
        || !user
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err("SSH 用户名无效。".to_string());
    }
    if options.port == 0 {
        return Err("SSH 端口无效。".to_string());
    }
    if options.remote_path.trim().is_empty() {
        return Err("远端目录不能为空。".to_string());
    }
    if options.local_path.trim().is_empty() {
        return Err("本地目录不能为空。".to_string());
    }
    Ok(())
}

fn validate_local_destination(path: &Path, _delete_missing: bool) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "无法读取当前用户目录。".to_string())?;
    let normalized_path = normalize_existing_ancestor(path);
    let normalized_home = normalize_existing_ancestor(&home);
    if destination_is_unsafe(&normalized_path, &normalized_home) {
        return Err(format!(
            "本地目标是系统或用户目录的上级目录，已拒绝以免覆盖数据：{}",
            normalized_path.display()
        ));
    }
    Ok(())
}

fn normalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        ancestor = parent;
    }
    let Ok(canonical) = ancestor.canonicalize() else {
        return path.to_path_buf();
    };
    let Ok(suffix) = path.strip_prefix(ancestor) else {
        return path.to_path_buf();
    };
    canonical.join(suffix)
}

fn destination_is_unsafe(path: &Path, home: &Path) -> bool {
    path == Path::new("/") || path == home || home.starts_with(path)
}

fn ssh_command(options: &SyncOptions) -> Command {
    let mut command = Command::new("/usr/bin/ssh");
    command
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=6",
            "-o",
            "ServerAliveInterval=15",
            "-p",
        ])
        .arg(options.port.to_string())
        .arg(ssh_target(options).unwrap_or_default());
    command
}

fn ssh_transport(options: &SyncOptions) -> String {
    format!(
        "/usr/bin/ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=15 -o Compression=no -p {}",
        options.port
    )
}

fn ssh_target(options: &SyncOptions) -> Result<String, String> {
    validate_options(options)?;
    Ok(format!("{}@{}", options.user.trim(), options.host.trim()))
}

fn remote_source(options: &SyncOptions, root: &str) -> String {
    let escaped = escape_remote_path(root);
    format!("{}:{}/", ssh_target(options).unwrap_or_default(), escaped)
}

fn remote_command(script: &str, argument: &str) -> String {
    remote_command_with_args(script, &[argument])
}

fn remote_command_with_args(script: &str, arguments: &[&str]) -> String {
    let arguments = arguments
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    format!("/bin/bash -lc {} -- {arguments}", shell_quote(script),)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn apple_script_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn escape_remote_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if character.is_ascii_whitespace() || "\"'\\$`;&|<>(){}[]*?!#".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn ensure_trailing_slash(path: &Path) -> PathBuf {
    let mut value = path.to_string_lossy().into_owned();
    if !value.ends_with('/') {
        value.push('/');
    }
    PathBuf::from(value)
}

fn expand_local_path(path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    if trimmed == "~" || trimmed.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "无法读取当前用户目录。".to_string())?;
        if trimmed == "~" {
            return Ok(home);
        }
        return Ok(home.join(&trimmed[2..]));
    }
    if !Path::new(trimmed).is_absolute() {
        return Err("本地目录请使用绝对路径或以 ~/ 开头。".to_string());
    }
    Ok(PathBuf::from(trimmed))
}

fn parse_xfer_size(line: &str) -> Option<u64> {
    let (_, size) = parse_xfer_line(line)?;
    Some(size)
}

fn parse_xfer_path(line: &str) -> Option<&str> {
    parse_xfer_line(line).map(|(path, _)| path)
}

fn parse_xfer_line(line: &str) -> Option<(&str, u64)> {
    let mut fields = line.splitn(3, '|');
    if fields.next()? != "XFER" {
        return None;
    }
    let size = fields.next()?.parse().ok()?;
    let path = fields.next()?;
    Some((path, size))
}

fn command_error(context: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        format!("{context}。")
    } else {
        format!("{context}：{detail}")
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 小时 {} 分", seconds / 3600, seconds % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sync_options(local_path: &str) -> SyncOptions {
        SyncOptions {
            host: "192.168.32.8".to_string(),
            user: "tester".to_string(),
            port: 2202,
            remote_path: "/Users/tester/Desktop".to_string(),
            local_path: local_path.to_string(),
            jobs: 4,
            delete_missing: true,
            ignore_patterns: vec![".DS_Store".to_string(), "Project Files".to_string()],
        }
    }

    #[test]
    fn legacy_sync_config_without_devices_still_loads() {
        let config: SyncConfig = serde_json::from_str(r#"{"ignore_patterns":[".DS_Store"]}"#)
            .expect("legacy sync config should deserialize");
        assert_eq!(config.ignore_patterns, vec![".DS_Store"]);
        assert!(config.devices.is_empty());
    }

    #[test]
    fn rsync_preview_reflects_options_and_shell_quotes_paths() {
        let command = rsync_command_preview(&test_sync_options("/tmp/Mac Sync/设备 2")).unwrap();
        assert!(command.starts_with("# 增量同步并发：4 个独立 rsync 进程"));
        assert!(command.contains("rsync 本身没有 --parallel 参数"));
        assert!(!command.contains("--no-r"));
        assert!(command.contains("可单独运行的单路 rsync 命令"));
        assert!(command.contains("\n/usr/bin/rsync "));
        assert!(command.contains("-p 2202"));
        assert!(command.contains("--exclude '.DS_Store'"));
        assert!(command.contains("--exclude 'Project Files'"));
        assert!(command.contains("--delete"));
        assert!(command.contains("'tester@192.168.32.8:/Users/tester/Desktop/'"));
        assert!(command.contains("'/tmp/Mac Sync/设备 2/'"));
    }

    #[test]
    fn simultaneous_sync_rejects_the_same_local_destination() {
        let first = test_sync_options("/tmp/my-mac-device-one");
        let mut second = test_sync_options("/tmp/my-mac-device-two");
        assert!(validate_distinct_destinations(&[first.clone(), second.clone()]).is_ok());

        second.local_path = first.local_path.clone();
        let error = validate_distinct_destinations(&[first, second]).unwrap_err();
        assert!(error.contains("同一个本地文件夹"));
    }

    #[test]
    fn inventory_parser_keeps_spaces_and_chinese() {
        let entry = parse_inventory_entry("Regular File|123|/资料/项目 文件.txt").unwrap();
        let relative = relative_to_root(&entry.relative_path, "/资料").unwrap();
        assert_eq!(relative, "项目 文件.txt");
        assert_eq!(entry.size, 123);
        assert_eq!(entry.kind, EntryKind::File);
    }

    #[test]
    fn many_small_files_are_balanced_across_all_batches() {
        let entries = (0..1000)
            .map(|index| RemoteEntry {
                relative_path: format!("small/{index:04}.txt"),
                kind: EntryKind::File,
                size: 100,
            })
            .collect::<Vec<_>>();
        let batches = plan_batches(&entries, 4);
        assert_eq!(batches.len(), 4);
        assert_eq!(
            batches.iter().map(|batch| batch.file_count).sum::<u64>(),
            1000
        );
        assert!(
            batches
                .iter()
                .all(|batch| (250..=250).contains(&batch.file_count))
        );
    }

    #[test]
    fn work_queue_has_multiple_chunks_per_lane_for_tail_balancing() {
        let entries = (0..1000)
            .map(|index| RemoteEntry {
                relative_path: format!("small/{index:04}.txt"),
                kind: EntryKind::File,
                size: 100,
            })
            .collect::<Vec<_>>();
        let batches = plan_work_batches(&entries, 4, true);
        assert_eq!(batches.len(), 32);
        assert_eq!(
            batches.iter().map(|batch| batch.file_count).sum::<u64>(),
            1000
        );
    }

    #[test]
    fn ignore_patterns_match_builtins_and_custom_paths() {
        let patterns = parse_ignore_patterns(
            ".DS_Store\n._*\n__MACOSX\nnode_modules/.cache\n*.tmp\n# comment\n",
        );
        assert!(should_ignore_path(".DS_Store", &patterns));
        assert!(should_ignore_path("photos/._cover.jpg", &patterns));
        assert!(should_ignore_path("archive/__MACOSX/._file", &patterns));
        assert!(should_ignore_path(
            "project/node_modules/.cache/data",
            &patterns
        ));
        assert!(should_ignore_path("project/cache.tmp", &patterns));
        assert!(!should_ignore_path("project/source.rs", &patterns));
    }

    #[test]
    fn connection_and_partial_transfer_errors_are_classified() {
        assert!(is_retryable_connection_error(
            "ssh: connect to host example port 22: Operation timed out"
        ));
        assert!(is_retryable_connection_error(
            "rsync error: unexplained error (code 255)"
        ));
        assert!(!is_retryable_connection_error("No space left on device"));
        assert!(is_skippable_rsync_error(
            "rsync: open failed: Permission denied (13)"
        ));
        assert!(is_skippable_rsync_error(
            "rsync: open failed: File name too long (63)"
        ));
        assert!(!is_skippable_rsync_error("No space left on device"));
    }

    #[test]
    fn small_file_batches_keep_nearby_paths_together() {
        let entries = (0..1000)
            .rev()
            .map(|index| RemoteEntry {
                relative_path: format!("projects/{index:04}/file.txt"),
                kind: EntryKind::File,
                size: 100,
            })
            .collect::<Vec<_>>();
        let batches = plan_batches_with_mode(&entries, 4, true);
        assert_eq!(batches.len(), 4);
        assert!(batches.iter().all(|batch| batch.entries.len() == 250));
        assert!(
            batches[0].entries.last().unwrap().relative_path
                < batches[1].entries.first().unwrap().relative_path
        );
    }

    #[test]
    fn directory_xfer_events_do_not_advance_file_progress() {
        let directories = HashSet::from(["projects".to_string()]);
        assert!(!is_file_transfer_path("projects/", &directories));
        assert!(!is_file_transfer_path("projects", &directories));
        assert!(is_file_transfer_path("projects/file.txt", &directories));
        assert!(is_file_transfer_path("file.txt", &directories));
    }

    #[test]
    fn large_files_are_balanced_by_bytes() {
        let entries = [900, 800, 700, 100]
            .into_iter()
            .enumerate()
            .map(|(index, size)| RemoteEntry {
                relative_path: format!("large-{index}"),
                kind: EntryKind::File,
                size,
            })
            .collect::<Vec<_>>();
        let batches = plan_batches(&entries, 2);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].total_bytes + batches[1].total_bytes, 2500);
        assert!(batches.iter().all(|batch| batch.total_bytes <= 1500));
    }

    #[test]
    fn directory_entries_are_included_without_counting_them_as_files() {
        let entries = vec![
            RemoteEntry {
                relative_path: "empty".to_string(),
                kind: EntryKind::Directory,
                size: 0,
            },
            RemoteEntry {
                relative_path: "empty/file.txt".to_string(),
                kind: EntryKind::File,
                size: 12,
            },
        ];
        let batches = plan_batches_with_mode(&entries, 1, false);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 2);
        assert_eq!(batches[0].file_count, 1);
        assert_eq!(batches[0].total_bytes, 12);
    }

    #[test]
    fn directory_only_inventory_still_creates_a_batch() {
        let entries = vec![
            RemoteEntry {
                relative_path: "empty".to_string(),
                kind: EntryKind::Directory,
                size: 0,
            },
            RemoteEntry {
                relative_path: "empty/nested".to_string(),
                kind: EntryKind::Directory,
                size: 0,
            },
        ];
        let batches = plan_batches_with_mode(&entries, 2, true);
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.entries.len())
                .sum::<usize>(),
            2
        );
        assert!(batches.iter().all(|batch| batch.file_count == 0));
    }

    #[test]
    fn unsafe_destination_rejects_home_and_ancestors() {
        let home = Path::new("/Users/example");
        assert!(destination_is_unsafe(Path::new("/"), home));
        assert!(destination_is_unsafe(Path::new("/Users"), home));
        assert!(destination_is_unsafe(home, home));
        assert!(!destination_is_unsafe(
            Path::new("/Users/example/Desktop/backup"),
            home
        ));
    }

    #[test]
    fn parses_rsync_output() {
        assert_eq!(parse_xfer_size("XFER|4096|a.txt"), Some(4096));
        assert_eq!(
            parse_xfer_path("XFER|4096|目录/文件.txt"),
            Some("目录/文件.txt")
        );
        assert_eq!(parse_xfer_size("normal output"), None);
    }

    #[test]
    fn parallel_files_from_list_copies_many_small_files() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture =
            std::env::temp_dir().join(format!("my-mac-sync-{}-{stamp}", std::process::id()));
        let source = fixture.join("source");
        let destination = fixture.join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();

        fs::create_dir_all(source.join("empty/nested")).unwrap();
        let mut entries = vec![
            RemoteEntry {
                relative_path: "empty".to_string(),
                kind: EntryKind::Directory,
                size: 0,
            },
            RemoteEntry {
                relative_path: "empty/nested".to_string(),
                kind: EntryKind::Directory,
                size: 0,
            },
        ];
        for directory in 0..8 {
            let directory_path = source.join(format!("folder-{directory}"));
            fs::create_dir_all(&directory_path).unwrap();
            for file in 0..50 {
                let relative_path = format!("folder-{directory}/small-{file:03}.txt");
                let contents = format!("{directory}:{file}");
                fs::write(source.join(&relative_path), contents.as_bytes()).unwrap();
                entries.push(RemoteEntry {
                    relative_path,
                    kind: EntryKind::File,
                    size: contents.len() as u64,
                });
            }
        }

        let batches = plan_batches(&entries, 4);
        let handles = batches
            .into_iter()
            .map(|batch| {
                let source = source.clone();
                let destination = destination.clone();
                thread::spawn(move || {
                    let mut child = Command::new("/usr/bin/rsync")
                        .args([
                            "-aE",
                            "--no-r",
                            "--partial-dir=.mymac-partial",
                            "--from0",
                            "--files-from=-",
                        ])
                        .arg(ensure_trailing_slash(&source))
                        .arg(ensure_trailing_slash(&destination))
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    let mut stdin = child.stdin.take().unwrap();
                    for entry in batch.entries {
                        stdin.write_all(entry.relative_path.as_bytes()).unwrap();
                        stdin.write_all(&[0]).unwrap();
                    }
                    drop(stdin);
                    assert!(child.wait().unwrap().success());
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(destination.join("empty/nested").is_dir());
        for entry in entries.iter().filter(|entry| entry.kind == EntryKind::File) {
            assert_eq!(
                fs::read(destination.join(&entry.relative_path))
                    .unwrap()
                    .len() as u64,
                entry.size,
                "missing {}",
                entry.relative_path
            );
        }
        fs::remove_dir_all(fixture).unwrap();
    }
}
