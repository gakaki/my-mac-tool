use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

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

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub remote_path: String,
    pub local_path: String,
    pub jobs: usize,
    pub delete_missing: bool,
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
    },
    Finished {
        batch: usize,
        success: bool,
        cancelled: bool,
        items: u64,
        bytes: u64,
    },
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
    let result = run_sync_inner(&options, &cancel, &tx);
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
    if inventory.file_count == 0 {
        return Ok(format!(
            "远端目录没有普通文件。已确认空目录 {} 可用。",
            inventory.root
        ));
    }

    let local_root = expand_local_path(&options.local_path)?;
    fs::create_dir_all(&local_root)
        .map_err(|error| format!("无法创建本地目录 {}：{error}", local_root.display()))?;
    for entry in inventory
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
    {
        fs::create_dir_all(local_root.join(&entry.relative_path))
            .map_err(|error| format!("无法创建本地子目录 {}：{error}", entry.relative_path))?;
    }

    let batches = plan_batches(&inventory.entries, options.jobs);
    let batch_count = batches.len();
    let small_file_mode = inventory.total_bytes < inventory.file_count.saturating_mul(1024 * 1024);
    send_status(
        tx,
        &format!(
            "索引完成：{} 个文件，{}，拆分为 {} 路并行同步。",
            inventory.file_count,
            format_bytes(inventory.total_bytes),
            batch_count
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

    if cancel.load(Ordering::Relaxed) {
        return Err("同步已停止；已完成的文件会保留。".to_string());
    }

    send_status(tx, "恢复目录权限、时间与扩展属性…");
    run_directory_metadata(options, &inventory, &local_root)?;

    if options.delete_missing {
        send_status(tx, "正在删除本地已不存在于远端的文件…");
        run_delete_pass(options, &inventory.root, &local_root)?;
    }

    let elapsed = started.elapsed();
    Ok(format!(
        "同步完成：{} 个文件，{}；耗时 {}；并行 {} 路，平均 {}/s。",
        results.files,
        format_bytes(results.bytes),
        format_duration(elapsed),
        batch_count,
        format_bytes(results.rate)
    ))
}

struct BatchResults {
    files: u64,
    bytes: u64,
    rate: u64,
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
    let batch_count = batches.len();
    let (worker_tx, worker_rx): (Sender<WorkerMessage>, Receiver<WorkerMessage>) = mpsc::channel();
    let mut handles = Vec::with_capacity(batch_count);
    for (batch_index, batch) in batches.iter().cloned().enumerate() {
        let sender = worker_tx.clone();
        let options = options.clone();
        let local_root = local_root.to_path_buf();
        let cancel = cancel.clone();
        let source_root = inventory.root.clone();
        handles.push(thread::spawn(move || {
            run_batch(
                batch_index,
                batch,
                &options,
                &source_root,
                &local_root,
                cancel,
                sender,
            );
        }));
    }
    drop(worker_tx);

    let mut active = batch_count;
    let mut completed_items = vec![0_u64; batch_count];
    let mut completed_bytes = vec![0_u64; batch_count];
    let mut failed = Vec::new();
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut cancelled = false;

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
                        batch_count,
                        active,
                        started,
                        small_file_mode,
                    );
                    last_emit = Instant::now();
                }
            }
            Ok(WorkerMessage::Error { batch, line }) => {
                failed.push(format!("批次 {}：{}", batch + 1, line));
            }
            Ok(WorkerMessage::Finished {
                batch,
                success,
                cancelled: was_cancelled,
                items,
                bytes,
            }) => {
                active = active.saturating_sub(1);
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
                    batch_count,
                    active,
                    started,
                    small_file_mode,
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                if last_emit.elapsed() >= Duration::from_millis(250) {
                    send_progress(
                        tx,
                        inventory,
                        &completed_items,
                        &completed_bytes,
                        batch_count,
                        active,
                        started,
                        small_file_mode,
                    );
                    last_emit = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    for handle in handles {
        if handle.join().is_err() {
            failed.push("同步工作线程异常退出。".to_string());
        }
    }

    if cancelled {
        return Err("同步已停止；已完成的文件会保留。".to_string());
    }
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
    })
}

fn run_batch(
    batch_index: usize,
    batch: Batch,
    options: &SyncOptions,
    source_root: &str,
    local_root: &Path,
    cancel: Arc<AtomicBool>,
    sender: Sender<WorkerMessage>,
) {
    let expected_paths = batch
        .entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect::<HashSet<_>>();
    let mut command = rsync_command(options, source_root, local_root);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = sender.send(WorkerMessage::Error {
                batch: batch_index,
                line: format!("无法启动 rsync：{error}"),
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

    if let Some(mut stdin) = child.stdin.take() {
        for entry in &batch.entries {
            if stdin.write_all(entry.relative_path.as_bytes()).is_err()
                || stdin.write_all(&[0]).is_err()
            {
                break;
            }
        }
    }

    let stdout_reader = child.stdout.take().map(|stdout| {
        let sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let counted = parse_xfer_path(&line)
                    .map(|path| expected_paths.contains(path.trim_end_matches('/')))
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
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    let _ = sender.send(WorkerMessage::Error {
                        batch: batch_index,
                        line,
                    });
                }
            }
        })
    });

    let mut was_cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if cancel.load(Ordering::Relaxed) => {
                was_cancelled = true;
                let _ = child.kill();
            }
            Ok(None) => thread::sleep(Duration::from_millis(80)),
            Err(error) => {
                let _ = sender.send(WorkerMessage::Error {
                    batch: batch_index,
                    line: format!("等待 rsync 失败：{error}"),
                });
                break None;
            }
        }
    };
    if let Some(reader) = stdout_reader {
        let _ = reader.join();
    }
    if let Some(reader) = stderr_reader {
        let _ = reader.join();
    }
    let success = status.map(|status| status.success()).unwrap_or(false);
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
        .arg("--partial")
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

fn run_directory_metadata(
    options: &SyncOptions,
    inventory: &Inventory,
    local_root: &Path,
) -> Result<(), String> {
    let mut directories = inventory
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .peekable();
    if directories.peek().is_none() {
        return Ok(());
    }
    let mut command = Command::new("/usr/bin/rsync");
    command
        .arg("-aE")
        .arg("--from0")
        .arg("--files-from=-")
        .arg("-e")
        .arg(ssh_transport(options))
        .arg(remote_source(options, &inventory.root))
        .arg(ensure_trailing_slash(local_root))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法校正目录元数据：{error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        for entry in directories {
            let _ = stdin.write_all(entry.relative_path.as_bytes());
            let _ = stdin.write_all(&[0]);
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("等待目录元数据同步失败：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("恢复目录元数据失败", &output.stderr))
    }
}

fn run_delete_pass(
    options: &SyncOptions,
    source_root: &str,
    local_root: &Path,
) -> Result<(), String> {
    let output = Command::new("/usr/bin/rsync")
        .arg("-aE")
        .arg("--delete")
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
                if entry.kind == EntryKind::File {
                    file_count += 1;
                    total_bytes = total_bytes.saturating_add(entry.size);
                }
                entries.push(entry);
                if last_status.elapsed() >= Duration::from_millis(600) {
                    send_status(
                        tx,
                        &format!("正在索引远端文件：已发现 {file_count} 个文件…"),
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

fn plan_batches(entries: &[RemoteEntry], jobs: usize) -> Vec<Batch> {
    let mut files = entries
        .iter()
        .filter(|entry| entry.kind != EntryKind::Directory)
        .cloned()
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| std::cmp::Reverse(entry.size));
    let count = jobs.clamp(1, 8).min(files.len().max(1));
    let mut batches = (0..count)
        .map(|_| Batch {
            entries: Vec::new(),
            file_count: 0,
            total_bytes: 0,
        })
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
    batches.retain(|batch| !batch.entries.is_empty());
    batches
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
    }));
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
    format!(
        "/bin/bash -lc {} -- {}",
        shell_quote(script),
        shell_quote(argument)
    )
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
    fn parses_openrsync_output() {
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

        let mut entries = Vec::new();
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
                        .args(["-aE", "--from0", "--files-from=-"])
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
        for entry in &entries {
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
