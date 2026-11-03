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
    let specs = [
        (RUST, "install rust", "通过 rsproxy 安装 Rust"),
        (GO, "install go", "安装 Go 并编译 goproxycn"),
        (NODE, "install node", "通过 npmmirror 安装 Node.js"),
        (
            WRANGLER,
            "install wrangler-only",
            "通过 npm 淘宝镜像安装 Wrangler",
        ),
        (CLOUDFLARED, "install cloudflared", "下载 Cloudflare Tunnel"),
        (FLUTTER, "install flutter", "安装 Flutter 与 Dart"),
    ];

    for (task, _, _) in specs {
        let _ = tx.send(Event::Progress {
            task,
            progress: 1.0,
            detail: "正在并行检测本地安装…".to_string(),
        });
        send_state(&tx, task, TaskState::Running, "正在并行检测本地安装…");
    }

    let check_workers = specs
        .iter()
        .map(|(task, _, _)| {
            let tx = tx.clone();
            let helper = helper.clone();
            let task = *task;
            thread::spawn(move || {
                let installed = component_installed(&helper, task);
                if installed {
                    let _ = tx.send(Event::Progress {
                        task,
                        progress: 100.0,
                        detail: "检测完成：已安装，跳过下载".to_string(),
                    });
                    send_state(&tx, task, TaskState::Success, "检测完成：已安装，跳过下载");
                } else {
                    let _ = tx.send(Event::Progress {
                        task,
                        progress: 3.0,
                        detail: "检测完成：未安装，等待并行安装".to_string(),
                    });
                }
                installed
            })
        })
        .collect::<Vec<_>>();
    let installed = check_workers
        .into_iter()
        .map(|worker| worker.join().unwrap_or(false))
        .collect::<Vec<_>>();
    let node_index = specs
        .iter()
        .position(|(task, _, _)| *task == NODE)
        .unwrap_or(2);
    let wrangler_index = specs
        .iter()
        .position(|(task, _, _)| *task == WRANGLER)
        .unwrap_or(3);
    for (index, (task, _, _)) in specs.iter().enumerate() {
        if installed[index] {
            continue;
        }
        let detail = if *task == WRANGLER && !installed[node_index] {
            "未安装：等待 Node.js 完成后安装"
        } else {
            "未安装：即将开始并行安装"
        };
        let _ = tx.send(Event::Progress {
            task: *task,
            progress: 3.0,
            detail: detail.to_string(),
        });
        send_state(&tx, *task, TaskState::Running, detail);
    }

    let setup_tx = tx.clone();
    let setup_helper = helper.clone();
    let setup_worker = thread::spawn(move || {
        run_task(
            &setup_tx,
            &setup_helper,
            SETUP,
            "setup",
            "配置代理、PATH 和镜像",
        )
    });

    let mut workers = Vec::new();
    let mut node_worker = None;
    for (index, (task, command, detail)) in specs.into_iter().enumerate() {
        if installed[index] {
            continue;
        }
        let tx = tx.clone();
        let helper = helper.clone();
        let worker = thread::spawn(move || install_task(&tx, &helper, task, command, detail));
        if task == NODE {
            node_worker = Some(worker);
        } else if task != WRANGLER {
            workers.push((task, worker));
        }
    }

    let node_ok = node_worker
        .map(|worker| worker.join().unwrap_or(false))
        .unwrap_or(true);
    if !installed[wrangler_index] {
        if node_ok {
            let _ = install_task(
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
    }

    for (_, worker) in workers {
        let _ = worker.join();
    }
    if setup_worker.join().unwrap_or(false) {
        send_state(&tx, SETUP, TaskState::Success, "代理、PATH 和镜像已配置");
    } else {
        send_state(
            &tx,
            SETUP,
            TaskState::Warning,
            "系统代理配置失败；安装仍使用终端代理",
        );
    }
    let _ = tx.send(Event::AllDone);
}

fn component_installed(helper: &PathBuf, task: usize) -> bool {
    let Some(component) = component_name(task) else {
        return false;
    };
    Command::new(helper)
        .args(["check", component])
        .env("PATH", gui_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn install_task(
    tx: &Sender<Event>,
    helper: &PathBuf,
    task: usize,
    arguments: &str,
    detail: &str,
) -> bool {
    let Some(component) = component_name(task) else {
        return false;
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

fn component_name(task: usize) -> Option<&'static str> {
    match task {
        RUST => "rust",
        GO => "go",
        NODE => "node",
        WRANGLER => "wrangler",
        CLOUDFLARED => "cloudflared",
        FLUTTER => "flutter",
        _ => return None,
    }
    .into()
}

fn run_task(
    tx: &Sender<Event>,
    helper: &PathBuf,
    task: usize,
    arguments: &str,
    detail: &str,
) -> bool {
    let _ = tx.send(Event::Progress {
        task,
        progress: 2.0,
        detail: format!("{detail}：正在启动"),
    });
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
        SETUP if lower.contains("preparing local") => Some(6.0),
        SETUP if lower.contains("writing npm") => Some(18.0),
        SETUP if lower.contains("writing cargo") => Some(30.0),
        SETUP if lower.contains("configuring git") => Some(42.0),
        SETUP if lower.contains("checking macos proxy") => Some(55.0),
        SETUP if lower.contains("administrator") => Some(72.0),
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

pub fn configure_environment() -> Result<(), String> {
    let helper = find_helper();
    if !helper.is_file() {
        return Err(format!(
            "找不到安装引擎 {}；请重新构建或安装 App。",
            helper.display()
        ));
    }

    let output = Command::new(&helper)
        .arg("setup")
        .env("PATH", gui_path())
        .output()
        .map_err(|error| format!("无法启动开发环境配置：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.is_empty() {
            stdout
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("开发环境配置失败")
                .trim()
                .to_owned()
        } else {
            stderr
        };
        Err(format!("开发环境配置失败：{detail}"))
    }
}

pub(crate) fn gui_path() -> String {
    let home = env::var("HOME").unwrap_or_default();
    format!(
        "{home}/.local/bin:{home}/.local/share/toolchains/rustup/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_stages_advance_the_visible_progress() {
        assert_eq!(
            progress_for_line(SETUP, "==> Preparing local directories and PATH"),
            Some(6.0)
        );
        assert_eq!(
            progress_for_line(SETUP, "==> Configuring Git proxy"),
            Some(42.0)
        );
        assert_eq!(
            progress_for_line(SETUP, "==> Checking macOS proxy for network service: Wi-Fi"),
            Some(55.0)
        );
        assert_eq!(
            progress_for_line(SETUP, "==> Environment setup complete"),
            Some(100.0)
        );
    }
}
