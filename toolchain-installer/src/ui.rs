use crate::display::{self, Detection, DisplayInfo, TargetMode};
use crate::installer::{self, Event, TaskState};
use crate::processes::{self, KillRule, MonitorSnapshot, ProcessConfig, ProcessInfo};
use crate::sync::{self, SyncEvent, SyncOptions, SyncProgress};
use crate::system;
use nsh_base::theme;
use nsh_core::element::input;
use nsh_core::prelude::*;
use std::env;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TAB_ALL: usize = 0;
const TAB_DISPLAY: usize = 1;
const TAB_SYSTEM: usize = 2;
const TAB_AUTO_START: usize = 3;
const TAB_AUTO_CLEAN: usize = 4;
const TAB_SYNC: usize = 5;

#[derive(Clone, PartialEq)]
struct InstallTask {
    title: &'static str,
    state: TaskState,
    progress: f32,
    detail: String,
    logs: Vec<String>,
}

impl InstallTask {
    fn new(title: &'static str) -> Self {
        Self {
            title,
            state: TaskState::Pending,
            progress: 0.0,
            detail: "等待一键安装".to_string(),
            logs: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct UiState {
    tab: Signal<usize>,
    tasks: Signal<Vec<InstallTask>>,
    install_running: Signal<bool>,
    install_finished: Signal<bool>,
    display_info: Signal<Option<DisplayInfo>>,
    macos_version: Signal<String>,
    display_summary: Signal<String>,
    display_status: Signal<String>,
    display_error: Signal<bool>,
    display_working: Signal<bool>,
    target_width: Signal<String>,
    target_height: Signal<String>,
    target_refresh: Signal<String>,
    desktop_width: Signal<String>,
    desktop_height: Signal<String>,
    system_status: Signal<Option<system::SystemStatus>>,
    hot_corners: Signal<String>,
    dock_position: Signal<String>,
    finder_view: Signal<String>,
    rosetta: Signal<String>,
    auto_login: Signal<String>,
    helper_status: Signal<String>,
    helper_error: Signal<bool>,
    helper_working: Signal<bool>,
}

#[derive(Clone)]
struct Actions {
    start_all: Rc<dyn Fn()>,
    install_display: Rc<dyn Fn()>,
    apply_display: Rc<dyn Fn()>,
    restore_display: Rc<dyn Fn()>,
    open_display_settings: Rc<dyn Fn()>,
    disable_hot_corners: Rc<dyn Fn()>,
    move_dock_left: Rc<dyn Fn()>,
    configure_finder_list_view: Rc<dyn Fn()>,
    run_system_setup: Rc<dyn Fn()>,
    install_rosetta: Rc<dyn Fn()>,
    apply_weak_password: Rc<dyn Fn()>,
    restore_password_policy: Rc<dyn Fn()>,
    enable_auto_login: Rc<dyn Fn()>,
    disable_auto_login: Rc<dyn Fn()>,
}

#[derive(Clone)]
struct ProcessUi {
    config: Arc<Mutex<ProcessConfig>>,
    runtime: Arc<Mutex<processes::MonitorRuntime>>,
    snapshots: Signal<Vec<MonitorSnapshot>>,
    processes: Signal<Vec<ProcessInfo>>,
    kill_rules: Signal<Vec<KillRule>>,
    monitors: Signal<Vec<processes::MonitorApp>>,
    query: Signal<String>,
    selected_pids: Signal<Vec<u32>>,
    status: Signal<String>,
    monitor_enabled: Signal<bool>,
    monitor_path: Signal<String>,
    rule_name: Signal<String>,
    rule_terms: Signal<String>,
    rule_startup: Signal<bool>,
    editing_rule: Signal<Option<usize>>,
}

#[derive(Clone)]
struct SyncUi {
    host: Signal<String>,
    user: Signal<String>,
    port: Signal<String>,
    remote_path: Signal<String>,
    local_path: Signal<String>,
    jobs: Signal<String>,
    delete_missing: Signal<bool>,
    status: Signal<String>,
    detail: Signal<String>,
    progress: Signal<f32>,
    processed: Signal<u64>,
    total_files: Signal<u64>,
    completed_bytes: Signal<u64>,
    total_bytes: Signal<u64>,
    rate: Signal<String>,
    elapsed: Signal<String>,
    heartbeat: Signal<String>,
    batch_summary: Signal<String>,
    running: Signal<bool>,
    success: Signal<bool>,
    cancel: Arc<AtomicBool>,
    runtime: Arc<Mutex<SyncUiRuntime>>,
}

#[derive(Default)]
struct SyncUiRuntime {
    started_at: Option<Instant>,
    last_movement_at: Option<Instant>,
    last_heartbeat_at: Option<Instant>,
    latest_progress: Option<SyncProgress>,
}

fn initial_tasks() -> Vec<InstallTask> {
    vec![
        InstallTask::new("系统代理与 PATH"),
        InstallTask::new("Rust / Cargo"),
        InstallTask::new("Go / goproxycn"),
        InstallTask::new("Node.js / npm"),
        InstallTask::new("Wrangler"),
        InstallTask::new("cloudflared"),
        InstallTask::new("Flutter Android"),
    ]
}

impl ProcessUi {
    fn new() -> Self {
        let config = processes::load_config().unwrap_or_else(|_| ProcessConfig::default());
        let monitors = config.monitors.clone();
        let rules = config.kill_rules.clone();
        let enabled = config.monitoring_enabled;
        Self {
            config: Arc::new(Mutex::new(config)),
            runtime: Arc::new(Mutex::new(processes::MonitorRuntime::default())),
            snapshots: signal(Vec::new()),
            processes: signal(processes::list_processes().unwrap_or_default()),
            kill_rules: signal(rules),
            monitors: signal(monitors),
            query: signal(String::new()),
            selected_pids: signal(Vec::new()),
            status: signal("监视器使用 5 秒检查、连续确认和 60 秒重启冷却。".to_string()),
            monitor_enabled: signal(enabled),
            monitor_path: signal("/Applications/UURemote.app".to_string()),
            rule_name: signal(String::new()),
            rule_terms: signal(String::new()),
            rule_startup: signal(true),
            editing_rule: signal(None),
        }
    }

    fn refresh_processes(&self) {
        match processes::list_processes() {
            Ok(processes) => {
                self.processes.set(processes);
                self.status.set("进程列表已刷新。".to_string());
            }
            Err(error) => self.status.set(error),
        }
    }

    fn sync_config_signals(&self) {
        if let Ok(config) = self.config.lock() {
            self.kill_rules.set(config.kill_rules.clone());
            self.monitors.set(config.monitors.clone());
            self.monitor_enabled.set(config.monitoring_enabled);
        }
    }

    fn persist_and_enforce(&self, cleanup_startup: bool) {
        let mut config = match self.config.lock() {
            Ok(config) => config.clone(),
            Err(_) => return,
        };
        if let Err(error) = processes::save_config(&config) {
            self.status.set(error);
            return;
        }
        let messages = processes::apply_kill_rules(&mut config, cleanup_startup);
        if let Ok(mut shared) = self.config.lock() {
            *shared = config;
        }
        self.sync_config_signals();
        self.refresh_processes();
        self.status.set(if messages.is_empty() {
            "规则已保存；当前没有匹配的进程或启动项。".to_string()
        } else {
            messages.join("\n")
        });
    }

    fn start_monitor(&self) {
        let ui = self.clone();
        spawn(async move {
            let cleanup_config = ui.config.clone();
            let cleanup_ui = ui.clone();
            spawn(async move {
                let first_cleanup = blocking(move || {
                    let mut config = match cleanup_config.lock() {
                        Ok(config) => config.clone(),
                        Err(_) => return vec!["进程配置锁失败".to_string()],
                    };
                    let mut messages = processes::apply_kill_rules(&mut config, true);
                    if messages.is_empty() {
                        messages.push("启动项清理已完成。".to_string());
                    }
                    messages
                })
                .await;
                if let Ok(messages) = first_cleanup {
                    cleanup_ui.status.set(messages.join("\n"));
                }
            });
            loop {
                let ui_for_tick = ui.clone();
                let result = blocking(move || {
                    let mut config = match ui_for_tick.config.lock() {
                        Ok(config) => config.clone(),
                        Err(_) => return (Vec::new(), Vec::new(), Vec::new(), 5),
                    };
                    let interval = config.check_interval_seconds.max(1);
                    let snapshots = {
                        let mut runtime = ui_for_tick
                            .runtime
                            .lock()
                            .expect("monitor runtime poisoned");
                        processes::monitor_tick(&config, &mut runtime)
                    };
                    let kill_messages = processes::apply_kill_rules(&mut config, false);
                    let all_processes = processes::list_processes().unwrap_or_default();
                    if let Ok(mut shared) = ui_for_tick.config.lock() {
                        *shared = config;
                    }
                    (snapshots, kill_messages, all_processes, interval)
                })
                .await;
                let Ok((snapshots, messages, all_processes, interval)) = result else {
                    return;
                };
                ui.snapshots.set(snapshots);
                ui.processes.set(all_processes);
                if !messages.is_empty() {
                    ui.status.set(messages.join("\n"));
                }
                let _ = blocking(move || {
                    std::thread::sleep(Duration::from_secs(interval));
                })
                .await;
            }
        });
    }
}

impl SyncUi {
    fn new() -> Self {
        let current_user = env::var("USER").unwrap_or_else(|_| "g".to_string());
        Self {
            host: signal("192.168.32.6".to_string()),
            user: signal(current_user),
            port: signal("22".to_string()),
            remote_path: signal("/Users/g/Desktop".to_string()),
            local_path: signal("~/Desktop/来自-192.168.32.6".to_string()),
            jobs: signal("4".to_string()),
            delete_missing: signal(false),
            status: signal("准备连接另一台 Mac。首次使用请先配置 SSH 免密登录。".to_string()),
            detail: signal(
                "首次同步走单路 tar 流；中断后自动补齐，完成后切换为增量 openrsync。".to_string(),
            ),
            progress: signal(0.0),
            processed: signal(0),
            total_files: signal(0),
            completed_bytes: signal(0),
            total_bytes: signal(0),
            rate: signal("—".to_string()),
            elapsed: signal("00:00".to_string()),
            heartbeat: signal("等待开始".to_string()),
            batch_summary: signal("尚未开始".to_string()),
            running: signal(false),
            success: signal(false),
            cancel: Arc::new(AtomicBool::new(false)),
            runtime: Arc::new(Mutex::new(SyncUiRuntime::default())),
        }
    }

    fn options(&self) -> Result<SyncOptions, String> {
        let port = self
            .port
            .get()
            .trim()
            .parse::<u16>()
            .map_err(|_| "SSH 端口必须是 1–65535。".to_string())?;
        let jobs = self
            .jobs
            .get()
            .trim()
            .parse::<usize>()
            .map_err(|_| "并行任务数必须是 1–8。".to_string())?
            .clamp(1, 8);
        Ok(SyncOptions {
            host: self.host.get(),
            user: self.user.get(),
            port,
            remote_path: self.remote_path.get(),
            local_path: self.local_path.get(),
            jobs,
            delete_missing: self.delete_missing.get(),
        })
    }

    fn reset_progress(&self) {
        self.cancel.store(false, Ordering::Relaxed);
        if let Ok(mut runtime) = self.runtime.lock() {
            let now = Instant::now();
            *runtime = SyncUiRuntime {
                started_at: Some(now),
                last_heartbeat_at: Some(now),
                ..SyncUiRuntime::default()
            };
        }
        self.progress.set(0.0);
        self.processed.set(0);
        self.total_files.set(0);
        self.completed_bytes.set(0);
        self.total_bytes.set(0);
        self.rate.set("—".to_string());
        self.elapsed.set("00:00".to_string());
        self.heartbeat.set("刚启动 · 正在建立 SSH 会话".to_string());
        self.batch_summary.set("正在准备".to_string());
        self.success.set(false);
    }

    fn test_connection(&self) {
        if self.running.get() {
            return;
        }
        let options = match self.options() {
            Ok(options) => options,
            Err(error) => {
                self.status.set(error);
                self.success.set(false);
                return;
            }
        };
        self.reset_progress();
        self.running.set(true);
        self.status.set("正在快速检测 SSH 和远端目录…".to_string());
        self.detail
            .set("使用 BatchMode，不会在后台卡住等待密码。".to_string());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || sync::test_connection(&options, tx));
        self.pump_events(rx);
    }

    fn start(&self) {
        if self.running.get() {
            return;
        }
        let options = match self.options() {
            Ok(options) => options,
            Err(error) => {
                self.status.set(error);
                self.success.set(false);
                return;
            }
        };
        self.reset_progress();
        self.running.set(true);
        self.status.set("正在连接并建立远端文件索引…".to_string());
        self.detail.set(
            "首次使用高速 tar 流；若中断，再次开始会校验并补齐已有内容，不从头重传。".to_string(),
        );
        let (tx, rx) = mpsc::channel();
        let cancel = self.cancel.clone();
        std::thread::spawn(move || sync::run_sync(options, cancel, tx));
        self.pump_events(rx);
    }

    fn stop(&self) {
        if self.running.get() {
            self.cancel.store(true, Ordering::Relaxed);
            self.status.set("正在停止同步…".to_string());
        }
    }

    fn prepare_access(&self) {
        let options = match self.options() {
            Ok(options) => options,
            Err(error) => {
                self.status.set(error);
                self.success.set(false);
                return;
            }
        };
        match sync::prepare_ssh_access(&options) {
            Ok(message) => {
                self.status.set(message);
                self.success.set(true);
            }
            Err(error) => {
                self.status.set(error);
                self.success.set(false);
            }
        }
    }

    fn pump_events(&self, rx: Receiver<SyncEvent>) {
        let ui = self.clone();
        let rx = Arc::new(Mutex::new(rx));
        spawn(async move {
            loop {
                let receiver = rx.clone();
                let batch = blocking(move || drain_sync_events(&receiver)).await;
                let Ok((events, disconnected)) = batch else {
                    ui.running.set(false);
                    return;
                };
                for event in events {
                    match event {
                        SyncEvent::Status(message) => ui.status.set(message),
                        SyncEvent::Progress(progress) => apply_sync_progress(&ui, progress),
                        SyncEvent::Finished { success, message } => {
                            ui.running.set(false);
                            ui.success.set(success);
                            ui.status.set(message.clone());
                            ui.detail.set(if success {
                                "同步会话已结束。再次运行只会传输发生变化的内容。".to_string()
                            } else {
                                message
                            });
                            ui.heartbeat.set(if success {
                                "同步完成".to_string()
                            } else {
                                "同步已停止".to_string()
                            });
                            if success {
                                ui.progress.set(100.0);
                            }
                            return;
                        }
                    }
                }
                if disconnected {
                    ui.running.set(false);
                    return;
                }
                ui.tick_heartbeat();
            }
        });
    }

    fn tick_heartbeat(&self) {
        if !self.running.get() {
            return;
        }
        let now = Instant::now();
        let (started_at, last_movement_at, latest_progress) = {
            let Ok(mut runtime) = self.runtime.lock() else {
                return;
            };
            if runtime
                .last_heartbeat_at
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
            {
                return;
            }
            runtime.last_heartbeat_at = Some(now);
            (
                runtime.started_at,
                runtime.last_movement_at,
                runtime.latest_progress.clone(),
            )
        };

        if let Some(started_at) = started_at {
            let seconds = started_at.elapsed().as_secs();
            self.elapsed
                .set(format!("{:02}:{:02}", seconds / 60, seconds % 60));
        }

        match (latest_progress, last_movement_at) {
            (Some(progress), Some(last_movement)) => {
                let idle = now.duration_since(last_movement);
                self.detail.set(sync_progress_detail(&progress, Some(idle)));
                self.heartbeat.set(if idle < Duration::from_secs(5) {
                    format!("界面刷新正常 · {} 秒前收到新数据", idle.as_secs().max(1))
                } else if idle < Duration::from_secs(15) {
                    format!("等待远端数据 · 已 {} 秒没有新字节", idle.as_secs())
                } else {
                    format!(
                        "任务仍在等待 · 已 {} 秒没有新字节；SSH 会话未退出",
                        idle.as_secs()
                    )
                });
            }
            _ => {
                let seconds = started_at.map_or(0, |started| started.elapsed().as_secs());
                self.heartbeat.set(format!(
                    "界面刷新正常 · 正在执行当前阶段 · 已运行 {seconds} 秒"
                ));
            }
        }
    }
}

fn apply_sync_progress(ui: &SyncUi, progress: SyncProgress) {
    if let Ok(mut runtime) = ui.runtime.lock() {
        let now = Instant::now();
        runtime.started_at.get_or_insert(now);
        runtime.last_movement_at = Some(now);
        runtime.latest_progress = Some(progress.clone());
    }
    ui.progress.set(progress.percent);
    ui.processed.set(progress.processed_files);
    ui.total_files.set(progress.total_files);
    ui.completed_bytes.set(progress.completed_bytes);
    ui.total_bytes.set(progress.total_bytes);
    ui.rate.set(format!(
        "{}/s",
        format_bytes(progress.rate_bytes_per_second)
    ));
    let seconds = progress.elapsed.as_secs();
    ui.elapsed
        .set(format!("{:02}:{:02}", seconds / 60, seconds % 60));
    ui.batch_summary.set(if progress.percent >= 100.0 {
        "已完成".to_string()
    } else if progress.stream_mode {
        "单路 tar 流镜像".to_string()
    } else {
        format!(
            "{} / {} 路运行中{}",
            progress.active_batches,
            progress.batch_count,
            if progress.small_file_mode {
                " · 小文件模式"
            } else {
                ""
            }
        )
    });
    ui.detail.set(sync_progress_detail(&progress, None));
    ui.heartbeat
        .set("界面刷新正常 · 刚刚收到新数据".to_string());
}

fn drain_sync_events(rx: &Arc<Mutex<Receiver<SyncEvent>>>) -> (Vec<SyncEvent>, bool) {
    let receiver = rx.lock().expect("sync event receiver poisoned");
    match receiver.recv_timeout(Duration::from_millis(150)) {
        Ok(event) => {
            let mut events = vec![event];
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
            (events, false)
        }
        Err(RecvTimeoutError::Timeout) => (Vec::new(), false),
        Err(RecvTimeoutError::Disconnected) => (Vec::new(), true),
    }
}

fn sync_progress_detail(progress: &SyncProgress, idle: Option<Duration>) -> String {
    let eta = estimated_remaining(progress)
        .map(|value| format!(" · 预计剩余 {value}"))
        .unwrap_or_default();
    let idle = idle
        .filter(|idle| *idle >= Duration::from_secs(5))
        .map(|idle| format!(" · 已 {} 秒没有新字节", idle.as_secs()))
        .unwrap_or_default();
    if progress.stream_mode && progress.percent < 100.0 {
        format!(
            "高速流式镜像 · {} / {}；文件计数将在解包完成后确认{eta}{idle}",
            format_bytes(progress.completed_bytes),
            format_bytes(progress.total_bytes)
        )
    } else {
        format!(
            "{} / {} 个文件 · {} / {}{eta}{idle}",
            progress.processed_files,
            progress.total_files,
            format_bytes(progress.completed_bytes),
            format_bytes(progress.total_bytes)
        )
    }
}

fn estimated_remaining(progress: &SyncProgress) -> Option<String> {
    if progress.percent < 1.0 || progress.percent >= 99.9 || progress.elapsed.as_secs() < 2 {
        return None;
    }
    let fraction = f64::from(progress.percent / 100.0);
    let seconds = progress.elapsed.as_secs_f64() * (1.0 - fraction) / fraction;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let seconds = seconds.ceil() as u64;
    Some(if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 小时 {} 分", seconds / 3600, seconds / 60 % 60)
    })
}

fn action_button(label: &'static str, primary: bool, on_press: impl Fn() + 'static) -> View {
    let background = move || {
        if primary {
            theme().primary
        } else {
            theme().secondary
        }
    };
    let foreground = move || {
        if primary {
            theme().primary_foreground
        } else {
            theme().secondary_foreground
        }
    };
    view()
        .px_4()
        .py_3()
        .rounded_md()
        .bg(background)
        .active(|style| style.opacity(0.72))
        .on_press(on_press)
        .child(
            text(label)
                .text_size(px(14.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(foreground),
        )
}

fn tab_button(label: &'static str, index: usize, state: UiState) -> View {
    view()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(move || {
            if state.tab.get() == index {
                theme().primary
            } else {
                theme().muted
            }
        })
        .on_press(move || state.tab.set(index))
        .child(
            text(label)
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || {
                    if state.tab.get() == index {
                        theme().primary_foreground
                    } else {
                        theme().muted_foreground
                    }
                }),
        )
}

fn card() -> View {
    view()
        .w_full()
        .p_5()
        .gap_4()
        .rounded_lg()
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
}

fn muted_card() -> View {
    view()
        .w_full()
        .p_4()
        .gap_2()
        .rounded_lg()
        .bg(move || theme().muted)
}

fn section_title(title: &'static str, subtitle: &'static str) -> View {
    view()
        .gap_1()
        .child(
            text(title)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(
            text(subtitle)
                .text_size(px(13.0))
                .text_color(move || theme().muted_foreground),
        )
}

fn field(label: &'static str, value: Signal<String>) -> View {
    view()
        .flex_1()
        .gap_1()
        .child(
            text(label)
                .text_size(px(12.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(
            input()
                .value(move || value.get())
                .on_change(move |next: &str| value.set(next.to_owned()))
                .h(px(40.0))
                .px_3()
                .rounded_md()
                .border_1()
                .border_color(move || theme().border)
                .bg(move || theme().background),
        )
}

fn progress_row(task: InstallTask) -> View {
    let color = match task.state {
        TaskState::Success => theme().chart_1,
        TaskState::Failed => theme().destructive,
        TaskState::Warning => theme().chart_2,
        TaskState::Running => theme().primary,
        TaskState::Pending => theme().muted_foreground,
    };
    let filled = (task.progress / 100.0 * 620.0).clamp(0.0, 620.0);
    view()
        .w_full()
        .gap_2()
        .child(
            view()
                .flex_row()
                .justify_between()
                .items_center()
                .child(
                    text(task.title)
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    text(format!("{} · {:.0}%", task.state.label(), task.progress))
                        .text_size(px(11.0))
                        .text_color(move || color),
                ),
        )
        .child(
            view()
                .w(px(620.0))
                .h(px(7.0))
                .rounded_full()
                .overflow_hidden()
                .bg(move || theme().muted)
                .child(
                    view()
                        .h_full()
                        .w(px(filled))
                        .rounded_full()
                        .bg(move || color),
                ),
        )
        .child(
            text(task.detail)
                .text_size(px(11.0))
                .text_color(move || theme().muted_foreground),
        )
}

fn page_shell(content: View) -> AnyElement {
    scroll()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(
            view()
                .w_full()
                .items_center()
                .p_safe_area()
                .px_6()
                .py_6()
                .child(content.w_full().max_w(px(900.0)).gap_5()),
        )
        .into_element()
}

fn apply_install_event(tasks: &mut [InstallTask], event: Event) -> bool {
    match event {
        Event::State {
            task,
            state,
            detail,
        } => {
            if let Some(view) = tasks.get_mut(task) {
                view.state = state;
                view.detail = detail;
                if state == TaskState::Success {
                    view.progress = 100.0;
                }
            }
        }
        Event::Progress {
            task,
            progress,
            detail,
        } => {
            if let Some(view) = tasks.get_mut(task) {
                view.progress = view.progress.max(progress.clamp(0.0, 100.0));
                if !detail.is_empty() {
                    view.detail = detail;
                }
            }
        }
        Event::Log { task, line } => {
            if let Some(view) = tasks.get_mut(task) {
                view.detail = line.chars().take(120).collect();
                view.logs.push(line);
                if view.logs.len() > 300 {
                    view.logs.drain(0..80);
                }
            }
        }
        Event::AllDone => return true,
    }
    false
}

fn drain_events(rx: &Arc<Mutex<Receiver<Event>>>) -> (Vec<Event>, bool) {
    let receiver = match rx.lock() {
        Ok(receiver) => receiver,
        Err(_) => return (Vec::new(), true),
    };
    let mut events = Vec::new();
    let mut done = false;
    match receiver.recv_timeout(Duration::from_millis(120)) {
        Ok(event) => events.push(event),
        Err(RecvTimeoutError::Timeout) => return (events, false),
        Err(RecvTimeoutError::Disconnected) => return (events, true),
    }
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    if matches!(events.last(), Some(Event::AllDone)) {
        done = true;
    }
    (events, done)
}

fn pump_events(state: UiState, rx: Arc<Mutex<Receiver<Event>>>) {
    spawn(async move {
        loop {
            let rx_for_poll = rx.clone();
            let batch = blocking(move || drain_events(&rx_for_poll)).await;
            let Ok((events, disconnected)) = batch else {
                state.install_running.set(false);
                return;
            };
            let mut finished = disconnected;
            state.tasks.update(|tasks| {
                for event in events {
                    finished |= apply_install_event(tasks, event);
                }
            });
            if finished {
                state.install_running.set(false);
                state.install_finished.set(true);
                return;
            }
        }
    });
}

fn start_developer_installers(state: UiState) {
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));
    let helper = installer::find_helper();
    std::thread::spawn(move || installer::orchestrate(tx, helper));
    pump_events(state, rx);
}

fn preflight_installed_tasks(state: UiState) {
    spawn(async move {
        let helper = installer::find_helper();
        let output = blocking(move || {
            Command::new(helper)
                .arg("status")
                .env("PATH", installer::gui_path())
                .output()
        })
        .await;
        let Ok(Ok(output)) = output else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return;
        };
        let mapping = [
            (installer::RUST, "rust"),
            (installer::GO, "go"),
            (installer::NODE, "node"),
            (installer::WRANGLER, "wrangler"),
            (installer::CLOUDFLARED, "cloudflared"),
            (installer::FLUTTER, "flutter"),
        ];
        state.tasks.update(|tasks| {
            for (task, key) in mapping {
                if status
                    .get(key)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    && let Some(view) = tasks.get_mut(task)
                {
                    view.state = TaskState::Success;
                    view.progress = 100.0;
                    view.detail = "检测到已安装，跳过下载".to_string();
                }
            }
        });
    });
}

fn target_from_state(state: UiState) -> Result<TargetMode, String> {
    Ok(TargetMode {
        width: parse_u32(&state.target_width.get(), "物理宽度")?,
        height: parse_u32(&state.target_height.get(), "物理高度")?,
        refresh: parse_u32(&state.target_refresh.get(), "刷新率")?,
    })
}

async fn install_display_for_state(state: UiState) {
    let detection = match blocking(display::detect).await {
        Ok(Ok(detection)) => detection,
        Ok(Err(error)) => {
            state.display_status.set(format!("检测失败：{error}"));
            state.display_error.set(true);
            state.display_working.set(false);
            return;
        }
        Err(error) => {
            state.display_status.set(format!("检测任务失败：{error}"));
            state.display_error.set(true);
            state.display_working.set(false);
            return;
        }
    };
    let target = match target_from_state(state) {
        Ok(target) => target,
        Err(error) => {
            state.display_status.set(error);
            state.display_error.set(true);
            state.display_working.set(false);
            return;
        }
    };
    let display = detection.display.clone();
    match blocking(move || display::install_override(&display, target)).await {
        Ok(Ok(outcome)) => {
            let backup = outcome
                .backup_path
                .map(|path| format!("\n原配置已备份到 {}", path.display()))
                .unwrap_or_default();
            state.display_status.set(format!(
                "安装成功。重启 Mac 后，系统会载入新增模式。\n覆盖文件：{}{backup}",
                outcome.override_path.display()
            ));
            state.display_error.set(false);
        }
        Ok(Err(error)) => {
            state.display_status.set(error);
            state.display_error.set(true);
        }
        Err(error) => {
            state.display_status.set(format!("安装任务失败：{error}"));
            state.display_error.set(true);
        }
    }
    state.display_working.set(false);
}

fn parse_u32(value: &str, label: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("{label}必须是整数"))
}

fn make_actions(state: UiState) -> Actions {
    let start_state = state;
    let start_all = Rc::new(move || {
        if start_state.install_running.get() {
            return;
        }
        start_state.tasks.set(initial_tasks());
        start_state.install_running.set(true);
        start_state.install_finished.set(false);
        start_state.display_working.set(true);
        start_state
            .display_status
            .set("正在准备显示器原生覆盖…".to_string());
        start_developer_installers(start_state);
        spawn(async move {
            install_display_for_state(start_state).await;
        });
    });

    let display_state = state;
    let install_display = Rc::new(move || {
        if display_state.display_working.get() {
            return;
        }
        display_state.display_working.set(true);
        display_state.display_error.set(false);
        display_state
            .display_status
            .set("正在生成并安装系统原生显示覆盖…".to_string());
        spawn(async move {
            install_display_for_state(display_state).await;
        });
    });

    let apply_state = state;
    let apply_display = Rc::new(move || {
        if apply_state.display_working.get() {
            return;
        }
        let target = match target_from_state(apply_state) {
            Ok(target) => target,
            Err(error) => {
                apply_state.display_status.set(error);
                apply_state.display_error.set(true);
                return;
            }
        };
        let logical_width = match parse_u32(&apply_state.desktop_width.get(), "桌面宽度") {
            Ok(value) => value,
            Err(error) => {
                apply_state.display_status.set(error);
                apply_state.display_error.set(true);
                return;
            }
        };
        let logical_height = match parse_u32(&apply_state.desktop_height.get(), "桌面高度") {
            Ok(value) => value,
            Err(error) => {
                apply_state.display_status.set(error);
                apply_state.display_error.set(true);
                return;
            }
        };
        apply_state.display_working.set(true);
        apply_state.display_error.set(false);
        apply_state
            .display_status
            .set("正在切换显示模式…".to_string());
        spawn(async move {
            match blocking(move || display::apply_mode(target, logical_width, logical_height)).await
            {
                Ok(Ok(())) => apply_state.display_status.set(format!(
                    "已应用：{}×{} 物理输出，{}×{} 桌面。",
                    target.width, target.height, logical_width, logical_height
                )),
                Ok(Err(error)) => {
                    apply_state.display_status.set(error);
                    apply_state.display_error.set(true);
                }
                Err(error) => {
                    apply_state
                        .display_status
                        .set(format!("切换任务失败：{error}"));
                    apply_state.display_error.set(true);
                }
            }
            apply_state.display_working.set(false);
        });
    });

    let restore_state = state;
    let restore_display = Rc::new(move || {
        let Some(display) = restore_state.display_info.get() else {
            restore_state
                .display_status
                .set("还没有检测到显示器".to_string());
            restore_state.display_error.set(true);
            return;
        };
        if restore_state.display_working.get() {
            return;
        }
        restore_state.display_working.set(true);
        restore_state.display_error.set(false);
        restore_state
            .display_status
            .set("正在恢复安装前的显示覆盖…".to_string());
        spawn(async move {
            match blocking(move || display::restore_override(&display)).await {
                Ok(Ok(path)) => restore_state.display_status.set(format!(
                    "已恢复原配置：{}\n重启 Mac 后完全生效。",
                    path.display()
                )),
                Ok(Err(error)) => {
                    restore_state.display_status.set(error);
                    restore_state.display_error.set(true);
                }
                Err(error) => {
                    restore_state
                        .display_status
                        .set(format!("恢复任务失败：{error}"));
                    restore_state.display_error.set(true);
                }
            }
            restore_state.display_working.set(false);
        });
    });

    let open_display_settings = Rc::new(|| {
        let _ = display::open_display_settings();
    });

    let corner_state = state;
    let disable_hot_corners = Rc::new(move || {
        if corner_state.helper_working.get() {
            return;
        }
        corner_state.helper_working.set(true);
        corner_state.helper_error.set(false);
        corner_state
            .helper_status
            .set("正在关闭桌面四角的所有触发动作…".to_string());
        spawn(async move {
            match blocking(system::disable_hot_corners).await {
                Ok(Ok(())) => {
                    corner_state
                        .hot_corners
                        .set("桌面四角触发已全部关闭".to_string());
                    corner_state
                        .helper_status
                        .set("四角触发已关闭，Dock 已重新载入。".to_string());
                }
                Ok(Err(error)) => {
                    corner_state.helper_status.set(error);
                    corner_state.helper_error.set(true);
                }
                Err(error) => {
                    corner_state
                        .helper_status
                        .set(format!("修改任务失败：{error}"));
                    corner_state.helper_error.set(true);
                }
            }
            corner_state.helper_working.set(false);
        });
    });

    let dock_state = state;
    let move_dock_left = Rc::new(move || {
        if dock_state.helper_working.get() {
            return;
        }
        dock_state.helper_working.set(true);
        dock_state.helper_error.set(false);
        dock_state
            .helper_status
            .set("正在把 Dock 移到屏幕左侧…".to_string());
        spawn(async move {
            match blocking(system::move_dock_left).await {
                Ok(Ok(())) => {
                    dock_state
                        .dock_position
                        .set("Dock 位于屏幕左侧".to_string());
                    dock_state
                        .helper_status
                        .set("Dock 已移动到屏幕左侧。".to_string());
                }
                Ok(Err(error)) => {
                    dock_state.helper_status.set(error);
                    dock_state.helper_error.set(true);
                }
                Err(error) => {
                    dock_state
                        .helper_status
                        .set(format!("Dock 位置设置任务失败：{error}"));
                    dock_state.helper_error.set(true);
                }
            }
            dock_state.helper_working.set(false);
        });
    });

    let finder_state = state;
    let configure_finder_list_view = Rc::new(move || {
        if finder_state.helper_working.get() {
            return;
        }
        finder_state.helper_working.set(true);
        finder_state.helper_error.set(false);
        finder_state
            .helper_status
            .set("正在把 Finder 默认视图设为列表视图…".to_string());
        spawn(async move {
            match blocking(system::configure_finder_list_view).await {
                Ok(Ok(())) => {
                    finder_state
                        .finder_view
                        .set("Finder 默认使用列表视图，浏览时保持列表".to_string());
                    finder_state
                        .helper_status
                        .set("Finder 已设为默认列表视图，并重新载入。".to_string());
                }
                Ok(Err(error)) => {
                    finder_state.helper_status.set(error);
                    finder_state.helper_error.set(true);
                }
                Err(error) => {
                    finder_state
                        .helper_status
                        .set(format!("Finder 视图设置任务失败：{error}"));
                    finder_state.helper_error.set(true);
                }
            }
            finder_state.helper_working.set(false);
        });
    });

    let setup_state = state;
    let run_system_setup = Rc::new(move || {
        if setup_state.helper_working.get() {
            return;
        }
        setup_state.helper_working.set(true);
        setup_state.helper_error.set(false);
        setup_state
            .helper_status
            .set("一键设置 1/6：正在关闭桌面四角…".to_string());
        spawn(async move {
            let mut summary = Vec::new();
            let mut failed = false;

            match blocking(system::disable_hot_corners).await {
                Ok(Ok(())) => {
                    setup_state
                        .hot_corners
                        .set("桌面四角触发已全部关闭".to_string());
                    summary.push("1/6  桌面四角已关闭".to_string());
                }
                Ok(Err(error)) => {
                    failed = true;
                    summary.push(format!("1/6  桌面四角失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    summary.push(format!("1/6  桌面四角任务失败：{error}"));
                }
            }

            setup_state.helper_status.set(format!(
                "{}\n2/6  正在把 Dock 移到左侧…",
                summary.join("\n")
            ));
            match blocking(system::move_dock_left).await {
                Ok(Ok(())) => {
                    setup_state
                        .dock_position
                        .set("Dock 位于屏幕左侧".to_string());
                    summary.push("2/6  Dock 已移到屏幕左侧".to_string());
                }
                Ok(Err(error)) => {
                    failed = true;
                    summary.push(format!("2/6  Dock 位置失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    summary.push(format!("2/6  Dock 位置任务失败：{error}"));
                }
            }

            setup_state.helper_status.set(format!(
                "{}\n3/6  正在设置 Finder 列表视图…",
                summary.join("\n")
            ));
            match blocking(system::configure_finder_list_view).await {
                Ok(Ok(())) => {
                    setup_state
                        .finder_view
                        .set("Finder 默认使用列表视图，浏览时保持列表".to_string());
                    summary.push("3/6  Finder 已设为列表视图".to_string());
                }
                Ok(Err(error)) => {
                    failed = true;
                    summary.push(format!("3/6  Finder 视图失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    summary.push(format!("3/6  Finder 视图任务失败：{error}"));
                }
            }

            setup_state
                .helper_status
                .set(format!("{}\n4/6  正在安装 Rosetta 2…", summary.join("\n")));
            match blocking(system::install_rosetta).await {
                Ok(Ok(())) => {
                    setup_state.rosetta.set("Rosetta 2 已安装".to_string());
                    summary.push("4/6  Rosetta 2 已安装".to_string());
                }
                Ok(Err(error)) => {
                    failed = true;
                    summary.push(format!("4/6  Rosetta 2 失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    summary.push(format!("4/6  Rosetta 2 任务失败：{error}"));
                }
            }

            setup_state.helper_status.set(format!(
                "{}\n5/6  正在设置一个空格密码…",
                summary.join("\n")
            ));
            match blocking(system::apply_weak_password).await {
                Ok(Ok(outcome)) => summary.push(format!(
                    "5/6  弱密码策略已开启并验证账户“{}”",
                    outcome.account
                )),
                Ok(Err(error)) => {
                    failed = true;
                    summary.push(format!("5/6  弱密码设置失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    summary.push(format!("5/6  弱密码任务失败：{error}"));
                }
            }

            setup_state
                .helper_status
                .set(format!("{}\n6/6  正在启用自动登录…", summary.join("\n")));
            match blocking(system::enable_space_auto_login).await {
                Ok(Ok(())) => {
                    setup_state
                        .auto_login
                        .set("自动登录已开启，下次启动将跳过登录密码输入".to_string());
                    summary.push("6/6  自动登录已开启".to_string());
                }
                Ok(Err(error)) => {
                    failed = true;
                    summary.push(format!("6/6  自动登录失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    summary.push(format!("6/6  自动登录任务失败：{error}"));
                }
            }

            setup_state.helper_status.set(format!(
                "{}：\n{}",
                if failed {
                    "一键设置流程已结束，部分步骤失败"
                } else {
                    "一键设置全部完成"
                },
                summary.join("\n")
            ));
            setup_state.helper_error.set(failed);
            setup_state.helper_working.set(false);
        });
    });

    let rosetta_state = state;
    let install_rosetta = Rc::new(move || {
        if rosetta_state.helper_working.get() {
            return;
        }
        rosetta_state.helper_working.set(true);
        rosetta_state.helper_error.set(false);
        rosetta_state
            .helper_status
            .set("正在自动安装 Rosetta 并接受软件许可，可能需要管理员密码。".to_string());
        spawn(async move {
            match blocking(system::install_rosetta).await {
                Ok(Ok(())) => {
                    rosetta_state.rosetta.set("Rosetta 2 已安装".to_string());
                    rosetta_state
                        .helper_status
                        .set("Rosetta 2 安装完成。".to_string());
                }
                Ok(Err(error)) => {
                    rosetta_state.helper_status.set(error);
                    rosetta_state.helper_error.set(true);
                }
                Err(error) => {
                    rosetta_state
                        .helper_status
                        .set(format!("Rosetta 安装任务失败：{error}"));
                    rosetta_state.helper_error.set(true);
                }
            }
            rosetta_state.helper_working.set(false);
        });
    });

    let password_state = state;
    let apply_weak_password = Rc::new(move || {
        if password_state.helper_working.get() {
            return;
        }
        password_state.helper_working.set(true);
        password_state.helper_error.set(false);
        password_state.helper_status.set(
            "正在清除弱密码限制，并把当前登录密码设为一个空格。请完成管理员授权。".to_string(),
        );
        spawn(async move {
            match blocking(system::apply_weak_password).await {
                Ok(Ok(outcome)) => password_state.helper_status.set(format!(
                    "已清除密码复杂度限制并验证账户“{}”的当前登录密码恰好是一个空格。\n密码策略：{}",
                    outcome.account,
                    outcome.policy_path.display()
                )),
                Ok(Err(error)) => {
                    password_state.helper_status.set(error);
                    password_state.helper_error.set(true);
                }
                Err(error) => {
                    password_state
                        .helper_status
                        .set(format!("密码设置任务失败：{error}"));
                    password_state.helper_error.set(true);
                }
            }
            password_state.helper_working.set(false);
        });
    });

    let policy_state = state;
    let restore_password_policy = Rc::new(move || {
        if policy_state.helper_working.get() {
            return;
        }
        policy_state.helper_working.set(true);
        policy_state.helper_error.set(false);
        policy_state
            .helper_status
            .set("正在恢复修改前的密码策略…".to_string());
        spawn(async move {
            match blocking(system::restore_password_policy).await {
                Ok(Ok(())) => policy_state
                    .helper_status
                    .set("已恢复修改前的密码策略；当前密码本身不会自动改回。".to_string()),
                Ok(Err(error)) => {
                    policy_state.helper_status.set(error);
                    policy_state.helper_error.set(true);
                }
                Err(error) => {
                    policy_state
                        .helper_status
                        .set(format!("恢复密码策略任务失败：{error}"));
                    policy_state.helper_error.set(true);
                }
            }
            policy_state.helper_working.set(false);
        });
    });

    let enable_auto_state = state;
    let enable_auto_login = Rc::new(move || {
        if enable_auto_state.helper_working.get() {
            return;
        }
        enable_auto_state.helper_working.set(true);
        enable_auto_state.helper_error.set(false);
        enable_auto_state
            .helper_status
            .set("正在启用单空格密码自动登录…".to_string());
        spawn(async move {
            match blocking(system::enable_space_auto_login).await {
                Ok(Ok(())) => {
                    enable_auto_state
                        .auto_login
                        .set("自动登录已开启，下次启动将跳过登录密码输入".to_string());
                    enable_auto_state
                        .helper_status
                        .set("自动登录已开启。FileVault 必须关闭。".to_string());
                }
                Ok(Err(error)) => {
                    enable_auto_state.helper_status.set(error);
                    enable_auto_state.helper_error.set(true);
                }
                Err(error) => {
                    enable_auto_state
                        .helper_status
                        .set(format!("自动登录设置任务失败：{error}"));
                    enable_auto_state.helper_error.set(true);
                }
            }
            enable_auto_state.helper_working.set(false);
        });
    });

    let disable_auto_state = state;
    let disable_auto_login = Rc::new(move || {
        if disable_auto_state.helper_working.get() {
            return;
        }
        disable_auto_state.helper_working.set(true);
        disable_auto_state.helper_error.set(false);
        disable_auto_state
            .helper_status
            .set("正在关闭自动登录…".to_string());
        spawn(async move {
            match blocking(system::disable_auto_login).await {
                Ok(Ok(())) => {
                    disable_auto_state
                        .auto_login
                        .set("自动登录未开启".to_string());
                    disable_auto_state
                        .helper_status
                        .set("自动登录已关闭。".to_string());
                }
                Ok(Err(error)) => {
                    disable_auto_state.helper_status.set(error);
                    disable_auto_state.helper_error.set(true);
                }
                Err(error) => {
                    disable_auto_state
                        .helper_status
                        .set(format!("关闭自动登录任务失败：{error}"));
                    disable_auto_state.helper_error.set(true);
                }
            }
            disable_auto_state.helper_working.set(false);
        });
    });

    Actions {
        start_all,
        install_display,
        apply_display,
        restore_display,
        open_display_settings,
        disable_hot_corners,
        move_dock_left,
        configure_finder_list_view,
        run_system_setup,
        install_rosetta,
        apply_weak_password,
        restore_password_policy,
        enable_auto_login,
        disable_auto_login,
    }
}

#[component]
pub fn App() -> impl IntoElement {
    let initial_tab = env::var("MAC_SETTINGS_START_TAB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(TAB_ALL)
        .min(TAB_SYNC);
    let tab = signal(initial_tab);
    let tasks = signal(initial_tasks());
    let install_running = signal(false);
    let install_finished = signal(false);
    let display_info = signal::<Option<DisplayInfo>>(None);
    let macos_version = signal("正在读取".to_string());
    let display_summary = signal("正在读取当前显示器…".to_string());
    let display_status = signal("检测显示器后安装原生覆盖；首次安装后需要重启 Mac。".to_string());
    let display_error = signal(false);
    let display_working = signal(false);
    let target_width = signal("5120".to_string());
    let target_height = signal("2160".to_string());
    let target_refresh = signal("120".to_string());
    let desktop_width = signal("5120".to_string());
    let desktop_height = signal("2160".to_string());
    let system_status = signal::<Option<system::SystemStatus>>(None);
    let hot_corners = signal("正在读取四角设置…".to_string());
    let dock_position = signal("正在读取 Dock 位置…".to_string());
    let finder_view = signal("正在读取 Finder 视图…".to_string());
    let rosetta = signal("正在检测 Rosetta…".to_string());
    let auto_login = signal("正在检测自动登录…".to_string());
    let helper_status = signal("系统设置操作完成后会在这里显示结果。".to_string());
    let helper_error = signal(false);
    let helper_working = signal(false);

    let state = UiState {
        tab,
        tasks,
        install_running,
        install_finished,
        display_info,
        macos_version,
        display_summary,
        display_status,
        display_error,
        display_working,
        target_width,
        target_height,
        target_refresh,
        desktop_width,
        desktop_height,
        system_status,
        hot_corners,
        dock_position,
        finder_view,
        rosetta,
        auto_login,
        helper_status,
        helper_error,
        helper_working,
    };
    let actions = make_actions(state);
    preflight_installed_tasks(state);
    let process_ui = ProcessUi::new();
    process_ui.start_monitor();
    let sync_ui = SyncUi::new();

    spawn(async move {
        match blocking(display::detect).await {
            Ok(Ok(Detection {
                macos_version: version,
                display,
                target,
            })) => {
                state.display_summary.set(format!(
                    "{} · {}×{} @ {} Hz",
                    display.name,
                    display.current_width,
                    display.current_height,
                    display.current_refresh
                ));
                state.target_width.set(target.width.to_string());
                state.target_height.set(target.height.to_string());
                state.target_refresh.set(target.refresh.to_string());
                state.desktop_width.set(target.width.to_string());
                state.desktop_height.set(target.height.to_string());
                state.macos_version.set(version);
                state
                    .display_status
                    .set("检测完成。可安装原生覆盖或应用已载入的模式。".to_string());
                state.display_info.set(Some(display));
            }
            Ok(Err(error)) => {
                state.display_status.set(format!("检测失败：{error}"));
                state.display_error.set(true);
            }
            Err(error) => {
                state.display_status.set(format!("检测任务失败：{error}"));
                state.display_error.set(true);
            }
        }
    });

    spawn(async move {
        match blocking(system::detect).await {
            Ok(Ok(detected)) => {
                state
                    .hot_corners
                    .set(if detected.active_hot_corners.is_empty() {
                        "桌面四角触发已全部关闭".to_string()
                    } else {
                        format!("当前仍启用：{}", detected.active_hot_corners.join("、"))
                    });
                state.rosetta.set(if detected.rosetta_installed {
                    "Rosetta 2 已安装".to_string()
                } else {
                    "尚未检测到 Rosetta 2".to_string()
                });
                state.dock_position.set(detected.dock_position.clone());
                state.finder_view.set(if detected.finder_list_view {
                    "Finder 默认使用列表视图，浏览时保持列表".to_string()
                } else {
                    "Finder 尚未默认使用列表视图".to_string()
                });
                state.auto_login.set(if detected.auto_login_enabled {
                    "自动登录已开启".to_string()
                } else {
                    "自动登录未开启".to_string()
                });
                state.system_status.set(Some(detected));
            }
            Ok(Err(error)) => {
                state.hot_corners.set(format!("读取系统设置失败：{error}"));
            }
            Err(error) => {
                state
                    .hot_corners
                    .set(format!("系统设置读取任务失败：{error}"));
            }
        }
    });

    let state_for_pages = state;
    let actions_for_pages = actions.clone();
    let process_pages = process_ui.clone();
    let sync_pages = sync_ui.clone();
    let pages = dynamic(move || match state_for_pages.tab.get() {
        TAB_DISPLAY => display_page(state_for_pages, &actions_for_pages),
        TAB_SYSTEM => system_page(state_for_pages, &actions_for_pages),
        TAB_AUTO_START => auto_start_page(state_for_pages, process_pages.clone()),
        TAB_AUTO_CLEAN => auto_clean_page(state_for_pages, process_pages.clone()),
        TAB_SYNC => sync_page(sync_pages.clone()),
        _ => all_page(state_for_pages, &actions_for_pages),
    });

    if env::var_os("MAC_SETTINGS_AUTOSTART").is_some() {
        (actions.start_all)();
    }

    view()
        .flex_col()
        .size_full()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(
            view()
                .w_full()
                .p_safe_area()
                .px_6()
                .pt_5()
                .pb_3()
                .gap_1()
                .border_b_1()
                .border_color(move || theme().border)
                .child(
                    text("我的 Mac 设置和开发工具安装")
                        .text_size(px(24.0))
                        .font_weight(FontWeight::BOLD),
                )
                .child(
                    text("系统代理 · PATH · 显示器修复 · 跨 Mac 文件同步 · 实时进度")
                        .text_size(px(13.0))
                        .text_color(move || theme().muted_foreground),
                ),
        )
        .child(
            view()
                .w_full()
                .px_6()
                .py_3()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .border_b_1()
                .border_color(move || theme().border)
                .child(tab_button("安装编程工具", TAB_ALL, state))
                .child(tab_button("显示器", TAB_DISPLAY, state))
                .child(tab_button("系统设置", TAB_SYSTEM, state))
                .child(tab_button("自动启动进程", TAB_AUTO_START, state))
                .child(tab_button("自动清理", TAB_AUTO_CLEAN, state))
                .child(tab_button("文件同步", TAB_SYNC, state)),
        )
        .child(pages)
}

fn sync_page(ui: SyncUi) -> AnyElement {
    let running = ui.running.get();
    let delete_missing = ui.delete_missing.get();
    let check_ui = ui.clone();
    let start_ui = ui.clone();
    let stop_ui = ui.clone();
    let access_ui = ui.clone();
    let toggle_ui = ui.clone();
    let status_ui = ui.clone();
    let progress_color_ui = ui.clone();
    let progress_text_ui = ui.clone();
    let progress_bar_ui = ui.clone();
    let status_color = move || {
        if status_ui.success.get() {
            theme().chart_1
        } else {
            theme().foreground
        }
    };

    page_shell(
        view()
            .gap_2()
            .child(
                text("文件同步")
                    .text_size(px(28.0))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                text("首次高速 tar 流 · 后续多路增量同步 · 实时字节进度 · SSH 传输")
                    .text_size(px(13.0))
                    .text_color(move || theme().muted_foreground),
            )
            .child(
                muted_card()
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .items_center()
                            .child(
                                text("智能后端")
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                text("tar 流 + openrsync")
                                    .text_size(px(12.0))
                                    .font_family("Menlo"),
                            )
                            .child(
                                text("首次 1 路 · 增量默认 4 路")
                                    .text_size(px(12.0))
                                    .text_color(move || theme().muted_foreground),
                            ),
                    )
                    .child(
                        text(
                            "2 万个远端空文件实测：首次 tar 流 1.10 秒，4 路 openrsync 2.53 秒，parsync 8 路 127 秒仍只有 8,071 个。首次镜像用 tar 流，完成后建立状态标记并自动切换增量 openrsync；目录不再逐个串行预建。",
                        )
                        .text_size(px(12.0))
                        .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        text(
                            "进一步实测 SFTP/SSH：2 万个 4 KiB 文件用 tar 4.45 秒、msy 99.35 秒；rclone 在 1,000 文件用 17.79 秒，rsy 97 秒仍写入 0 个。它们不进入默认流程，避免额外依赖和不稳定协议。",
                        )
                        .text_size(px(12.0))
                        .text_color(move || theme().muted_foreground),
                    ),
            )
            .child(
                card()
                    .child(section_title(
                        "远端 Mac",
                        "默认连接 192.168.32.6 的 /Users/g/Desktop。支持 SSH config 别名和自定义端口。",
                    ))
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(field("主机名或 IP", ui.host))
                            .child(field("SSH 用户名", ui.user))
                            .child(field("SSH 端口", ui.port)),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(field("远端文件夹", ui.remote_path))
                            .child(field("本地目标文件夹", ui.local_path)),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(field("增量 openrsync 数量（1–8）", ui.jobs))
                            .child(action_button(
                                if delete_missing {
                                    "删除本地多余文件：已开启"
                                } else {
                                    "删除本地多余文件：已关闭"
                                },
                                false,
                                move || {
                                    toggle_ui
                                        .delete_missing
                                        .update(|enabled| *enabled = !*enabled);
                                },
                            )),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(action_button("配置免密登录", false, move || {
                                access_ui.prepare_access();
                            }))
                            .child(action_button("检测连接", false, move || {
                                check_ui.test_connection();
                            }))
                            .child(action_button(
                                if running {
                                    "同步进行中…"
                                } else {
                                    "开始智能同步"
                                },
                                true,
                                move || start_ui.start(),
                            ))
                            .child(action_button("停止", false, move || stop_ui.stop())),
                    ),
            )
            .child(
                card()
                    .child(
                        view()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(
                                text(move || ui.status.get())
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(status_color),
                            )
                            .child(
                                text(move || {
                                    format_percent(progress_text_ui.progress.get())
                                })
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::BOLD),
                            ),
                    )
                    .child(
                        view()
                            .w(px(780.0))
                            .h(px(10.0))
                            .rounded_full()
                            .overflow_hidden()
                            .bg(move || theme().muted)
                            .child(
                                view()
                                    .h_full()
                                    .w(move || {
                                        px(progress_bar_ui.progress.get() / 100.0 * 780.0)
                                    })
                                    .rounded_full()
                                    .bg(move || {
                                        if progress_color_ui.success.get() {
                                            theme().chart_1
                                        } else {
                                            theme().primary
                                        }
                                    }),
                            ),
                    )
                    .child(
                        text(move || ui.detail.get())
                            .text_size(px(12.0))
                            .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        text(move || ui.heartbeat.get())
                            .text_size(px(11.0))
                            .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_4()
                            .child(sync_stat("文件", move || {
                                format!("{} / {}", ui.processed.get(), ui.total_files.get())
                            }))
                            .child(sync_stat("数据", move || {
                                format!(
                                    "{} / {}",
                                    format_bytes(ui.completed_bytes.get()),
                                    format_bytes(ui.total_bytes.get())
                                )
                            }))
                            .child(sync_stat("速度", move || ui.rate.get()))
                            .child(sync_stat("耗时", move || ui.elapsed.get()))
                            .child(sync_stat("并行状态", move || ui.batch_summary.get())),
                    ),
            )
            .child(
                muted_card()
                    .child(
                        text("为什么这样选")
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        text(
                            "首次 tar 传输前会保存未完成状态；若中途断开，再次开始会用 openrsync 校验并补齐已完成内容，全部成功后才写入完成标记。后续按路径连续切分文件与空目录，减少远端目录反复遍历，并用非目录 XFER 事件修正进度计数。",
                        )
                        .text_size(px(12.0))
                        .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        text(
                            "fast_rsync 0.2.0 实测算术很快：4 MiB 签名约 1.9 ms、64 KiB 修改差分约 3.1 ms；但它只是差分算法库，没有 SSH、遍历、断点或接收协议，不能直接替代 openrsync。它适合未来给超大单文件做按块增量，不适合当前 122 万小文件的完整同步。",
                        )
                        .text_size(px(12.0))
                        .text_color(move || theme().muted_foreground),
                    ),
            ),
    )
}

fn sync_stat(label: &'static str, value: impl Fn() -> String + 'static) -> View {
    view()
        .gap_1()
        .child(
            text(label)
                .text_size(px(11.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(
            text(value)
                .text_size(px(12.0))
                .font_family("Menlo")
                .font_weight(FontWeight::SEMIBOLD),
        )
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

fn format_percent(percent: f32) -> String {
    let percent = percent.clamp(0.0, 100.0);
    if percent < 1.0 {
        format!("{percent:.2}%")
    } else {
        format!("{percent:.1}%")
    }
}

fn all_page(state: UiState, actions: &Actions) -> AnyElement {
    let tasks = state.tasks.get();
    let completed = tasks
        .iter()
        .filter(|task| task.state == TaskState::Success)
        .count();
    let overall = tasks.iter().map(|task| task.progress).sum::<f32>() / tasks.len().max(1) as f32;
    let start = actions.start_all.clone();
    page_shell(
        view()
            .gap_2()
            .child(
                text("安装编程工具")
                    .text_size(px(28.0))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                text("统一检测所有开发工具；已安装的立即跳过，只并行安装缺失项。")
                    .text_size(px(13.0))
                    .text_color(move || theme().muted_foreground),
            )
            .child(
                muted_card()
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(text("系统代理").text_size(px(12.0)))
                            .child(
                                text("http://127.0.0.1:7890")
                                    .text_size(px(12.0))
                                    .font_family("Menlo"),
                            )
                            .child(text("安装位置").text_size(px(12.0)))
                            .child(text("~/.local").text_size(px(12.0)).font_family("Menlo")),
                    )
                    .child(
                        text(
                            "Rust、Go、Node、cloudflared、Flutter 并行；Wrangler 跟随 Node 完成。",
                        )
                        .text_size(px(12.0))
                        .text_color(move || theme().muted_foreground),
                    ),
            )
            .child(
                card()
                    .child(
                        view()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(
                                text(move || {
                                    if state.install_running.get() {
                                        "正在自动安装…".to_string()
                                    } else if state.install_finished.get() {
                                        "安装流程已结束".to_string()
                                    } else {
                                        "准备开始".to_string()
                                    }
                                })
                                .text_size(px(15.0))
                                .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                text(format!("{completed} / {} 个组件", tasks.len()))
                                    .text_size(px(12.0))
                                    .text_color(move || theme().muted_foreground),
                            ),
                    )
                    .child(
                        view()
                            .w(px(780.0))
                            .h(px(8.0))
                            .rounded_full()
                            .overflow_hidden()
                            .bg(move || theme().muted)
                            .child(
                                view()
                                    .h_full()
                                    .w(px(overall / 100.0 * 780.0))
                                    .rounded_full()
                                    .bg(move || theme().primary),
                            ),
                    )
                    .child(action_button("检测并安装", true, move || start()))
                    .children(tasks.into_iter().map(progress_row)),
            ),
    )
}

fn display_page(state: UiState, actions: &Actions) -> AnyElement {
    let install = actions.install_display.clone();
    let apply = actions.apply_display.clone();
    let restore = actions.restore_display.clone();
    let open_settings = actions.open_display_settings.clone();
    page_shell(
        view()
            .gap_2()
            .child(
                text("显示器修复")
                    .text_size(px(28.0))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                text("原生覆盖 · 自动识别 · 可恢复 · 无常驻服务")
                    .text_size(px(13.0))
                    .text_color(move || theme().muted_foreground),
            )
            .child(
                muted_card()
                    .child(
                        view()
                            .flex_row()
                            .justify_between()
                            .child(
                                text("当前显示器")
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                text(move || format!("macOS {}", state.macos_version.get()))
                                    .text_size(px(12.0))
                                    .text_color(move || theme().muted_foreground),
                            ),
                    )
                    .child(
                        text(move || state.display_summary.get())
                            .text_size(px(20.0))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        text(move || {
                            state
                                .display_info
                                .get()
                                .map(|display| {
                                    format!(
                                        "厂商 0x{} · 产品 0x{} · {}",
                                        display.vendor_hex,
                                        display.product_hex,
                                        if display.better_display_running {
                                            "BetterDisplay 正在运行"
                                        } else {
                                            "未检测到 BetterDisplay"
                                        }
                                    )
                                })
                                .unwrap_or_else(|| "等待检测…".to_string())
                        })
                        .text_size(px(12.0))
                        .text_color(move || theme().muted_foreground),
                    ),
            )
            .child(
                card()
                    .child(section_title(
                        "1 · 生成原生显示覆盖",
                        "安装前自动备份；同一款显示器只需安装一次。",
                    ))
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(field("物理宽度", state.target_width))
                            .child(field("物理高度", state.target_height))
                            .child(field("刷新率 Hz", state.target_refresh)),
                    )
                    .child(
                        text("同时加入 LoDPI 和最高 3840×1620 HiDPI 桌面档位。")
                            .text_size(px(12.0))
                            .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(action_button("安装原生覆盖", true, move || install()))
                            .child(action_button("打开显示器设置", false, move || {
                                open_settings()
                            })),
                    ),
            )
            .child(
                card()
                    .child(section_title(
                        "2 · 选择桌面分辨率",
                        "物理输出保持 5120×2160；3840×1620 是更清晰的 HiDPI 桌面。",
                    ))
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(field("桌面宽度", state.desktop_width))
                            .child(field("桌面高度", state.desktop_height)),
                    )
                    .child(action_button("立即应用模式", true, move || apply())),
            )
            .child(
                muted_card()
                    .border_1()
                    .border_color(move || {
                        if state.display_error.get() {
                            theme().destructive
                        } else {
                            theme().border
                        }
                    })
                    .child(
                        text(move || state.display_status.get())
                            .text_size(px(13.0))
                            .text_color(move || {
                                if state.display_error.get() {
                                    theme().destructive
                                } else {
                                    theme().foreground
                                }
                            }),
                    )
                    .child(action_button("恢复原配置", false, move || restore())),
            ),
    )
}

fn system_page(state: UiState, actions: &Actions) -> AnyElement {
    let corners = actions.disable_hot_corners.clone();
    let dock_left = actions.move_dock_left.clone();
    let finder_list = actions.configure_finder_list_view.clone();
    let run_all = actions.run_system_setup.clone();
    let rosetta = actions.install_rosetta.clone();
    let weak = actions.apply_weak_password.clone();
    let restore_policy = actions.restore_password_policy.clone();
    let auto_on = actions.enable_auto_login.clone();
    let auto_off = actions.disable_auto_login.clone();
    page_shell(
        view()
            .gap_2()
            .child(
                text("系统设置")
                    .text_size(px(28.0))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                text("macOS 辅助设置")
                    .text_size(px(13.0))
                    .text_color(move || theme().muted_foreground),
            )
            .child(
                card()
                    .child(section_title(
                        "macOS 辅助设置",
                        "需要管理员权限时会使用 macOS 原生授权窗口。",
                    ))
                    .child(
                        muted_card()
                            .child(
                                text("一键设置全部")
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                text("关闭四角 · Dock 左侧 · Finder 列表 · Rosetta · 一个空格密码 · 自动登录")
                                    .text_size(px(11.0))
                                    .text_color(move || theme().muted_foreground),
                            )
                            .child(action_button("一键设置全部", true, move || run_all())),
                    )
                    .child(separator())
                    .child(system_action_row(
                        "桌面四角",
                        move || state.hot_corners.get(),
                        "一次关闭四个触发点并重新载入 Dock。",
                        "关闭四角桌面触发",
                        false,
                        move || corners(),
                    ))
                    .child(separator())
                    .child(system_action_row(
                        "Dock 位置",
                        move || state.dock_position.get(),
                        "把 Dock 固定到屏幕左侧，并立即重新载入。",
                        "移到屏幕左侧",
                        false,
                        move || dock_left(),
                    ))
                    .child(separator())
                    .child(system_action_row(
                        "Finder 列表视图",
                        move || state.finder_view.get(),
                        "默认以列表视图打开文件夹，并在浏览时保持列表视图、不分组。",
                        "设为默认列表视图",
                        false,
                        move || finder_list(),
                    ))
                    .child(separator())
                    .child(system_action_row(
                        "Rosetta 2",
                        move || state.rosetta.get(),
                        "自动安装并接受 Rosetta 软件许可。",
                        "全自动安装 Rosetta",
                        true,
                        move || rosetta(),
                    ))
                    .child(separator())
                    .child(system_action_row(
                        "弱密码登录",
                        move || "清除复杂度限制，并验证登录密码为一个空格".to_string(),
                        "这会显著降低本机安全性。",
                        "允许弱密码并设为一个空格",
                        true,
                        move || weak(),
                    ))
                    .child(action_button("恢复原密码策略", false, move || {
                        restore_policy()
                    }))
                    .child(separator())
                    .child(system_action_row(
                        "启动时自动登录",
                        move || state.auto_login.get(),
                        "FileVault 开启时 macOS 不允许自动登录。",
                        "启用自动登录",
                        true,
                        move || auto_on(),
                    ))
                    .child(action_button("关闭自动登录", false, move || {
                        auto_off()
                    }))
                    .child(
                        muted_card().child(
                            text(move || state.helper_status.get())
                                .text_size(px(12.0))
                                .text_color(move || {
                                    if state.helper_error.get() {
                                        theme().destructive
                                    } else {
                                        theme().foreground
                                    }
                                }),
                        ),
                    ),
            ),
    )
}

fn system_action_row(
    title: &'static str,
    detail: impl Fn() -> String + 'static,
    hint: &'static str,
    button: &'static str,
    primary: bool,
    action: impl Fn() + 'static,
) -> View {
    view()
        .gap_2()
        .child(
            text(title)
                .text_size(px(14.0))
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(
            text(detail)
                .text_size(px(12.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(
            text(hint)
                .text_size(px(11.0))
                .text_color(move || theme().muted_foreground),
        )
        .child(action_button(button, primary, action))
}

fn separator() -> View {
    view().w_full().h(px(1.0)).bg(move || theme().border)
}

fn auto_start_page(_state: UiState, ui: ProcessUi) -> AnyElement {
    let snapshots = ui.snapshots.get();
    let monitors = ui.monitors.get();
    let enabled = ui.monitor_enabled.get();
    let toggle_ui = ui.clone();
    let add_ui = ui.clone();

    let monitor_rows = monitors.into_iter().map(|monitor| {
        let remove_ui = ui.clone();
        let path = monitor.app_path.clone();
        muted_card().child(
            view()
                .flex_row()
                .justify_between()
                .items_center()
                .child(
                    view()
                        .gap_1()
                        .child(
                            text(monitor.display_name)
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            text(monitor.app_path.clone())
                                .font_family("Menlo")
                                .text_size(px(10.0))
                                .text_color(move || theme().muted_foreground),
                        ),
                )
                .child(action_button("移除", false, move || {
                    if let Ok(mut config) = remove_ui.config.lock() {
                        config.monitors.retain(|item| item.app_path != path);
                        let _ = processes::save_config(&config);
                    }
                    remove_ui.sync_config_signals();
                    remove_ui.status.set("监视应用已移除。".to_string());
                })),
        )
    });

    let snapshot_rows = snapshots.into_iter().map(|snapshot| {
        muted_card()
            .child(
                view()
                    .flex_row()
                    .justify_between()
                    .child(
                        text(snapshot.display_name)
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        text(snapshot.status)
                            .text_size(px(12.0))
                            .text_color(move || theme().muted_foreground),
                    ),
            )
            .child(
                text(format!(
                    "{} · 自动启动/重启 {} 次",
                    snapshot.message, snapshot.restarts
                ))
                .text_size(px(11.0))
                .text_color(move || theme().muted_foreground),
            )
    });

    page_shell(
        view()
            .gap_2()
            .child(
                text("自动启动进程")
                    .text_size(px(28.0))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                text("移植 UU远程监视策略：缺失确认、启动宽限、冷却重启和停止状态恢复")
                    .text_size(px(13.0))
                    .text_color(move || theme().muted_foreground),
            )
            .child(
                card()
                    .child(
                        view()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(section_title(
                                "监视应用",
                                "默认监视 /Applications/UURemote.app；进程缺失后自动启动。",
                            ))
                            .child(action_button(
                                if enabled {
                                    "暂停监视"
                                } else {
                                    "开启监视"
                                },
                                true,
                                move || {
                                    if let Ok(mut config) = toggle_ui.config.lock() {
                                        config.monitoring_enabled = !config.monitoring_enabled;
                                        let _ = processes::save_config(&config);
                                    }
                                    toggle_ui.sync_config_signals();
                                },
                            )),
                    )
                    .children(monitor_rows)
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(field("添加监视应用路径", ui.monitor_path))
                            .child(action_button("添加监视", false, move || {
                                let path = add_ui.monitor_path.get();
                                add_ui.status.set(match add_ui.config.lock() {
                                    Ok(mut config) => {
                                        match processes::add_monitor(&mut config, &path) {
                                            Ok(()) => "监视应用已添加。".to_string(),
                                            Err(error) => error,
                                        }
                                    }
                                    Err(_) => "进程配置锁失败".to_string(),
                                });
                                add_ui.sync_config_signals();
                            })),
                    ),
            )
            .children(snapshot_rows)
            .child(
                muted_card().child(
                    text(move || ui.status.get())
                        .text_size(px(12.0))
                        .text_color(move || theme().foreground),
                ),
            ),
    )
}

fn auto_clean_page(_state: UiState, ui: ProcessUi) -> AnyElement {
    let processes = ui.processes.get();
    let rules = ui.kill_rules.get();
    let filtered = processes::filter_processes(&processes, &ui.query.get());
    let selected = ui.selected_pids.get();
    let remove_startup = ui.rule_startup.get();
    let edit_index = ui.editing_rule.get();

    let refresh_ui = ui.clone();
    let add_selected_ui = ui.clone();
    let save_rule_ui = ui.clone();
    let new_rule_ui = ui.clone();
    let enforce_ui = ui.clone();

    let process_rows = filtered.into_iter().take(80).map(|process| {
        let pid = process.pid;
        let name = process.name.clone();
        let path = process.executable_path.clone();
        let selected_row = selected.contains(&pid);
        let process_for_click = process.clone();
        let select_ui = ui.clone();
        let drag_ui = ui.clone();
        let drag_process = process.clone();
        view()
            .w_full()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(move || {
                if selected_row {
                    theme().primary
                } else {
                    theme().border
                }
            })
            .bg(move || {
                if selected_row {
                    theme().accent
                } else {
                    theme().background
                }
            })
            .on_press(move |_: &()| {
                let mut pids = select_ui.selected_pids.get();
                if let Some(index) = pids.iter().position(|value| *value == pid) {
                    pids.remove(index);
                } else {
                    pids.push(pid);
                }
                select_ui.selected_pids.set(pids);
                let _ = &process_for_click;
            })
            .on_pan_end(move |event: &PanEvent| {
                if !event.cancelled && event.translation.y.0 > 45.0 {
                    add_process_rule_async(drag_ui.clone(), vec![drag_process.clone()]);
                }
            })
            .child(
                view()
                    .flex_row()
                    .gap_3()
                    .items_center()
                    .child(
                        text(if selected_row { "☑" } else { "☐" })
                            .text_size(px(15.0))
                            .text_color(move || {
                                if selected_row {
                                    theme().primary
                                } else {
                                    theme().muted_foreground
                                }
                            }),
                    )
                    .child(
                        view()
                            .flex_1()
                            .gap_1()
                            .child(
                                text(name)
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                text(path)
                                    .font_family("Menlo")
                                    .text_size(px(9.0))
                                    .text_color(move || theme().muted_foreground),
                            ),
                    )
                    .child(
                        text(format!("PID {} · {}%", process.pid, process.cpu))
                            .text_size(px(10.0))
                            .text_color(move || theme().muted_foreground),
                    ),
            )
    });

    let rule_rows = rules.iter().enumerate().map(|(index, rule)| {
        let rule = rule.clone();
        let edit_ui = ui.clone();
        let delete_ui = ui.clone();
        let toggle_ui = ui.clone();
        let rule_name = rule.name.clone();
        let rule_id = rule.id.clone();
        let toggle_rule_id = rule_id.clone();
        let delete_rule_id = rule_id.clone();
        let terms = rule.match_terms.join("、");
        let enabled_rule = rule.enabled;
        let startup = rule.remove_startup;
        muted_card().child(
            view()
                .flex_row()
                .justify_between()
                .items_center()
                .child(
                    view()
                        .gap_1()
                        .child(
                            text(rule_name)
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            text(terms)
                                .font_family("Menlo")
                                .text_size(px(10.0))
                                .text_color(move || theme().muted_foreground),
                        )
                        .child(
                            text(format!(
                                "{} · {}",
                                if enabled_rule {
                                    "自动终结开启"
                                } else {
                                    "已暂停"
                                },
                                if startup {
                                    "同时清理启动项"
                                } else {
                                    "保留启动项"
                                }
                            ))
                            .text_size(px(10.0))
                            .text_color(move || theme().muted_foreground),
                        ),
                )
                .child(
                    view()
                        .flex_row()
                        .gap_2()
                        .child(action_button(
                            if enabled_rule { "暂停" } else { "启用" },
                            false,
                            move || {
                                if let Ok(mut config) = toggle_ui.config.lock() {
                                    if let Some(item) = config
                                        .kill_rules
                                        .iter_mut()
                                        .find(|item| item.id == toggle_rule_id)
                                    {
                                        item.enabled = !item.enabled;
                                    }
                                    let _ = processes::save_config(&config);
                                }
                                toggle_ui.sync_config_signals();
                            },
                        ))
                        .child(action_button("编辑", false, move || {
                            edit_ui.editing_rule.set(Some(index));
                            edit_ui.rule_name.set(rule.name.clone());
                            edit_ui.rule_terms.set(rule.match_terms.join(", "));
                            edit_ui.rule_startup.set(rule.remove_startup);
                        }))
                        .child(action_button("删除", false, move || {
                            if let Ok(mut config) = delete_ui.config.lock() {
                                config.kill_rules.retain(|item| item.id != delete_rule_id);
                                let _ = processes::save_config(&config);
                            }
                            delete_ui.sync_config_signals();
                            delete_ui.status.set("规则已删除。".to_string());
                        })),
                ),
        )
    });

    page_shell(
        view()
            .gap_2()
            .child(
                text("自动清理")
                    .text_size(px(28.0))
                    .font_weight(FontWeight::BOLD),
            )
            .child(
                text("搜索并多选进程，拖动加入自动终结规则，并清理对应启动项")
                    .text_size(px(13.0))
                    .text_color(move || theme().muted_foreground),
            )
            .child(
                card()
                    .child(
                        view()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(section_title(
                                "搜索并多选当前进程",
                                "勾选后加入规则；也可以把进程行向下拖过 45pt 直接加入。",
                            ))
                            .child(action_button("刷新进程", false, move || {
                                refresh_ui.refresh_processes();
                            })),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(field("搜索进程名或路径", ui.query))
                            .child(action_button("加入选中进程", true, move || {
                                let selected = add_selected_ui.selected_pids.get();
                                let selected_processes = add_selected_ui
                                    .processes
                                    .get()
                                    .into_iter()
                                    .filter(|process| selected.contains(&process.pid))
                                    .collect::<Vec<_>>();
                                if selected_processes.is_empty() {
                                    add_selected_ui
                                        .status
                                        .set("请先勾选至少一个进程。".to_string());
                                } else {
                                    add_process_rule_async(
                                        add_selected_ui.clone(),
                                        selected_processes,
                                    );
                                }
                            })),
                    )
                    .child(
                        view()
                            .w_full()
                            .h(px(320.0))
                            .overflow_hidden()
                            .rounded_md()
                            .child(
                                scroll()
                                    .w_full()
                                    .h_full()
                                    .scrollbar(true)
                                    .bounces(false)
                                    .child(view().w_full().p_1().gap_1().children(process_rows)),
                            ),
                    ),
            )
            .child(
                card()
                    .child(section_title(
                        "自动杀进程列表",
                        "规则会持久保存；启用后每 5 秒检查并终止匹配进程。",
                    ))
                    .children(rule_rows)
                    .child(separator())
                    .child(
                        text(if edit_index.is_some() {
                            "编辑规则"
                        } else {
                            "新建规则"
                        })
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_3()
                            .child(field("规则名称", ui.rule_name))
                            .child(field("匹配词（逗号分隔）", ui.rule_terms)),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .child(action_button(
                                if remove_startup {
                                    "启动项清理：已开启"
                                } else {
                                    "启动项清理：已关闭"
                                },
                                false,
                                move || ui.rule_startup.update(|enabled| *enabled = !*enabled),
                            ))
                            .child(action_button("新建规则", false, move || {
                                new_rule_ui.editing_rule.set(None);
                                new_rule_ui.rule_name.set(String::new());
                                new_rule_ui.rule_terms.set(String::new());
                                new_rule_ui.rule_startup.set(true);
                            }))
                            .child(action_button("保存规则", true, move || {
                                let name = save_rule_ui.rule_name.get().trim().to_string();
                                let terms = save_rule_ui
                                    .rule_terms
                                    .get()
                                    .split([',', '，', '\n'])
                                    .map(str::trim)
                                    .filter(|term| !term.is_empty())
                                    .map(str::to_string)
                                    .collect::<Vec<_>>();
                                if name.is_empty() || terms.is_empty() {
                                    save_rule_ui
                                        .status
                                        .set("规则名称和匹配词不能为空。".to_string());
                                } else {
                                    if let Ok(mut config) = save_rule_ui.config.lock() {
                                        let index = save_rule_ui.editing_rule.get();
                                        let rule = KillRule {
                                            id: index
                                                .and_then(|index| {
                                                    config
                                                        .kill_rules
                                                        .get(index)
                                                        .map(|rule| rule.id.clone())
                                                })
                                                .unwrap_or_else(|| {
                                                    format!(
                                                        "rule-{}",
                                                        std::time::SystemTime::now()
                                                            .duration_since(std::time::UNIX_EPOCH)
                                                            .unwrap_or_default()
                                                            .as_millis()
                                                    )
                                                }),
                                            name,
                                            startup_terms: terms.clone(),
                                            match_terms: terms,
                                            remove_startup: save_rule_ui.rule_startup.get(),
                                            enabled: true,
                                        };
                                        if let Some(index) = index {
                                            config.kill_rules[index] = rule;
                                        } else {
                                            config.kill_rules.push(rule);
                                        }
                                    }
                                    save_rule_ui.persist_and_enforce(true);
                                    save_rule_ui.editing_rule.set(None);
                                }
                            }))
                            .child(action_button("保存并应用全部", true, move || {
                                enforce_ui.persist_and_enforce(true);
                            })),
                    ),
            )
            .child(
                muted_card().child(
                    text(move || ui.status.get())
                        .text_size(px(12.0))
                        .text_color(move || theme().foreground),
                ),
            ),
    )
}

fn add_process_rule_async(ui: ProcessUi, processes: Vec<ProcessInfo>) {
    spawn(async move {
        let ui_for_work = ui.clone();
        let result = blocking(move || {
            let Some(rule) = processes::make_rule_from_processes(&processes) else {
                return "没有可添加的进程。".to_string();
            };
            if let Ok(mut config) = ui_for_work.config.lock() {
                config.kill_rules.push(rule);
            }
            ui_for_work.persist_and_enforce(true);
            ui_for_work.status.get()
        })
        .await;
        if let Ok(message) = result {
            ui.status.set(message);
            ui.sync_config_signals();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::format_percent;

    #[test]
    fn fractional_progress_is_not_rounded_down_to_zero() {
        assert_eq!(format_percent(0.0), "0.00%");
        assert_eq!(format_percent(0.76), "0.76%");
        assert_eq!(format_percent(1.04), "1.0%");
    }
}
