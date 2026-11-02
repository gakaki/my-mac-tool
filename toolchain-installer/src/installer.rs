use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

pub const SETUP: usize = 0;
pub const RUST: usize = 1;
pub const GO: usize = 2;
pub const NODE: usize = 3;
pub const WRANGLER: usize = 4;
pub const CLOUDFLARED: usize = 5;
pub const FLUTTER: usize = 6;
const PROXY: &str = "http://127.0.0.1:7890";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Success,
    Warning,
    Failed,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "等待",
            Self::Running => "进行中",
            Self::Success => "已完成",
            Self::Warning => "有警告",
            Self::Failed => "失败",
        }
    }
}

pub enum Event {
    State {
        task: usize,
        state: TaskState,
        detail: String,
    },
    Progress {
        task: usize,
        progress: f32,
        detail: String,
    },
    Log {
        task: usize,
        line: String,
    },
    AllDone,
}

pub fn orchestrate(tx: Sender<Event>, helper: PathBuf) {
    let setup_ok = run_task(&tx, &helper, SETUP, "setup", "配置代理、PATH 和镜像");
    if setup_ok {
        send_state(&tx, SETUP, TaskState::Success, "代理、PATH 和镜像已配置");
    } else {
        send_state(
            &tx,
            SETUP,
            TaskState::Warning,
            "系统代理配置失败；继续使用终端代理安装",
        );
    }

    let mut workers = Vec::new();
    for (task, command, detail) in [
        (RUST, "install rust", "通过 rsproxy 安装 Rust"),
        (GO, "install go", "安装 Go 并编译 goproxycn"),
        (NODE, "install node", "通过 npmmirror 安装 Node.js"),
        (CLOUDFLARED, "install cloudflared", "下载 Cloudflare Tunnel"),
        (FLUTTER, "install flutter", "安装 Flutter 与 Dart"),
    ] {
        let tx = tx.clone();
        let helper = helper.clone();
        workers.push(thread::spawn(move || {
            install_task(&tx, &helper, task, command, detail)
        }));
    }

    let node_handle = workers.remove(2);
    let node_ok = node_handle.join().unwrap_or(false);
    if node_ok {
        install_task(
            &tx,
            &helper,
            WRANGLER,
            "install wrangler-only",
            "通过 npm 淘宝镜像安装 Wrangler",
        );
    } else {
        send_state(
            &tx,
            WRANGLER,
            TaskState::Failed,
            "Node 安装失败，无法安装 Wrangler",
        );
    }

    for worker in workers {
        let _ = worker.join();
    }
    let _ = tx.send(Event::AllDone);
}

pub fn install_single(tx: Sender<Event>, helper: PathBuf, task: usize) {
    let result = match task {
        RUST => install_task(&tx, &helper, RUST, "install rust", "通过 rsproxy 安装 Rust"),
        GO => install_task(&tx, &helper, GO, "install go", "安装 Go 并编译 goproxycn"),
        NODE => install_task(
            &tx,
            &helper,
            NODE,
            "install node",
            "通过 npmmirror 安装 Node.js",
        ),
        WRANGLER => install_task(
            &tx,
            &helper,
            WRANGLER,
            "install wrangler-only",
            "通过 npm 淘宝镜像安装 Wrangler",
        ),
        CLOUDFLARED => install_task(
            &tx,
            &helper,
            CLOUDFLARED,
            "install cloudflared",
            "下载 Cloudflare Tunnel",
        ),
        FLUTTER => install_task(
            &tx,
            &helper,
            FLUTTER,
            "install flutter",
            "安装 Flutter 与 Dart",
        ),
        _ => false,
    };
    let state = if result {
        TaskState::Success
    } else {
        TaskState::Failed
    };
    send_state(&tx, task, state, if result { "完成" } else { "安装失败" });
    let _ = tx.send(Event::AllDone);
}

fn install_task(
    tx: &Sender<Event>,
    helper: &PathBuf,
    task: usize,
    arguments: &str,
    detail: &str,
) -> bool {
    let component = match task {
        RUST => "rust",
        GO => "go",
        NODE => "node",
        WRANGLER => "wrangler",
        CLOUDFLARED => "cloudflared",
        FLUTTER => "flutter",
        _ => return false,
    };
    send_state(tx, task, TaskState::Running, "正在快速检测本地安装…");
    let check = Command::new(helper)
        .args(["check", component])
        .env("PATH", gui_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if check {
        let _ = tx.send(Event::Progress {
            task,
            progress: 100.0,
            detail: "已安装，跳过下载".to_string(),
        });
        send_state(tx, task, TaskState::Success, "已安装，跳过下载");
        return true;
    }
    run_task(tx, helper, task, arguments, detail)
}

fn run_task(
    tx: &Sender<Event>,
    helper: &PathBuf,
    task: usize,
    arguments: &str,
    detail: &str,
) -> bool {
    send_state(tx, task, TaskState::Running, detail);
    let mut command = Command::new(helper);
    command.args(arguments.split_whitespace());
    command
        .env("TOOLCHAIN_SKIP_CONFIGURE", "1")
        .env("NONINTERACTIVE", "1")
        .env("CI", "1")
        .env("HTTP_PROXY", PROXY)
        .env("HTTPS_PROXY", PROXY)
        .env("ALL_PROXY", PROXY)
        .env("http_proxy", PROXY)
        .env("https_proxy", PROXY)
        .env("all_proxy", PROXY)
        .env("NO_PROXY", "localhost,127.0.0.1,::1")
        .env("no_proxy", "localhost,127.0.0.1,::1")
        .env("PATH", gui_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            send_state(tx, task, TaskState::Failed, error.to_string());
            return false;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut readers = Vec::new();
    if let Some(stdout) = stdout {
        let tx = tx.clone();
        readers.push(thread::spawn(move || stream_output(stdout, tx, task)));
    }
    if let Some(stderr) = stderr {
        let tx = tx.clone();
        readers.push(thread::spawn(move || stream_output(stderr, tx, task)));
    }
    let status = child.wait();
    for reader in readers {
        let _ = reader.join();
    }
    match status {
        Ok(status) if status.success() => {
            send_state(tx, task, TaskState::Success, "安装完成");
            true
        }
        Ok(status) => {
            send_state(tx, task, TaskState::Failed, format!("进程退出：{status}"));
            false
        }
        Err(error) => {
            send_state(tx, task, TaskState::Failed, error.to_string());
            false
        }
    }
}

fn stream_output<R: Read>(mut reader: R, tx: Sender<Event>, task: usize) {
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                for &byte in &buffer[..count] {
                    if byte == b'\n' || byte == b'\r' {
                        emit_line(&tx, task, &mut pending);
                    } else if pending.len() < 32_000 {
                        pending.push(byte);
                    }
                }
            }
            Err(_) => break,
        }
    }
    emit_line(&tx, task, &mut pending);
}

fn emit_line(tx: &Sender<Event>, task: usize, pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    let line = String::from_utf8_lossy(pending).trim().to_string();
    pending.clear();
    if line.is_empty() {
        return;
    }
    let candidate = progress_for_line(task, &line);
    let _ = tx.send(Event::Log {
        task,
        line: line.clone(),
    });
    if let Some(progress) = candidate {
        let _ = tx.send(Event::Progress {
            task,
            progress,
            detail: line.chars().take(120).collect(),
        });
    }
}

fn progress_for_line(task: usize, line: &str) -> Option<f32> {
    let lower = line.to_ascii_lowercase();
    if let Some(percent) = extract_percent(line) {
        return Some(10.0 + percent * 0.62);
    }
    if lower.contains("downloading") {
        return Some(8.0);
    }
    if lower.contains("sha-256 verified") {
        return Some(76.0);
    }
    if lower.contains("unzip") || lower.contains("unpack") || lower.contains("extracting") {
        return Some(84.0);
    }
    if lower.contains("installed") || lower.contains("complete") {
        return Some(100.0);
    }
    match task {
        SETUP if lower.contains("configuring") => Some(20.0),
        SETUP if lower.contains("administrator") => Some(60.0),
        WRANGLER if lower.contains("added") => Some(95.0),
        _ => None,
    }
}

fn extract_percent(line: &str) -> Option<f32> {
    for token in line.split_whitespace() {
        let Some(position) = token.find('%') else {
            continue;
        };
        let number: String = token[..position]
            .chars()
            .rev()
            .take_while(|character| character.is_ascii_digit() || *character == '.')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if let Ok(value) = number.parse::<f32>() {
            return Some(value.clamp(0.0, 100.0));
        }
    }
    None
}

fn send_state(tx: &Sender<Event>, task: usize, state: TaskState, detail: impl Into<String>) {
    let _ = tx.send(Event::State {
        task,
        state,
        detail: detail.into(),
    });
}

pub fn find_helper() -> PathBuf {
    let sibling = env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .map(|parent| parent.join("toolchain-installer"))
        })
        .filter(|path| path.is_file());
    sibling.unwrap_or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/bin/toolchain-installer")
    })
}

pub(crate) fn gui_path() -> String {
    let home = env::var("HOME").unwrap_or_default();
    format!(
        "{home}/.local/bin:{home}/.local/share/toolchains/rustup/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    )
}
