# 我的 Mac 设置和开发工具安装

单一 Native Shell GUI，使用 `nsh.dev` 纯 Rust 布局与 shadcn 风格组件。
项目不使用 TSX、React、GPUI、egui 或 Electron。

## 功能

- **安装编程工具**：自动准备显示器原生覆盖，配置 `127.0.0.1:7890`
  系统代理、终端代理、PATH 和国内镜像，然后检测并安装编程工具。
- **显示器**：CS40X 5K 原生覆盖、LoDPI/HiDPI 模式切换、备份与恢复。
- **系统设置**：一键配置代理、PATH、镜像和 Git 代理，并关闭桌面四角、
  开启双击桌面显示桌面、调整 Dock/Finder、安装 Rosetta、设置弱密码和自动登录。
  键盘最快重复开关默认开启，把重复速率调到最快、重复前延迟调到最短。
- **编程工具**：在同一页集中展示 Rust/rsproxy、Go/goproxy.cn、
  Node/npmmirror、Wrangler、cloudflared、Flutter/Dart。
- **自动启动进程**：移植 UU远程监视重启策略，进程缺失后自动启动。
- **自动清理**：搜索和多选进程，拖动进程行加入自动终结列表，
  并清理匹配的 LaunchAgent/LaunchDaemon。
- **后台 App 活动**：读取系统“登录项与扩展”中的后台活动，支持搜索、
  启停系统项目；可新增、编辑本工具创建的用户级 LaunchAgent，并单条或
  批量删除任意后台登记及其精确关联的 LaunchAgents/LaunchDaemons。

每个开发工具都有独立进度、当前阶段和日志。Rust、Go、Node、
cloudflared、Flutter 并行安装；Wrangler 在 Node 完成后自动开始。
所有工具在下载前都会先做本地文件检查；已安装项会立即标记为
“已安装，跳过下载”，只并行安装缺失项。

## 构建

```sh
cd toolchain-installer
cargo test --lib
./packaging/package_app.sh
open "dist/我的 Mac 设置和开发工具安装.app"
```

后台 `toolchain-installer` 二进制仅用于安装引擎，不提供第二个 GUI。

## 进程规则

进程规则保存在：

```text
~/Library/Application Support/MacSettingsDevTools/processes.json
```

默认监视 `/Applications/UURemote.app`。主应用只按
`Contents/Info.plist` 中的 `CFBundleExecutable` 对应的完整可执行文件路径
精确匹配；`UURemoteService` 等辅助进程不会被当成主应用。监视器每 5 秒
复检一次，主进程连续缺失后才自动启动，并在启动后保留启动宽限和冷却时间。
默认自动终结规则匹配
`Microsoft AutoUpdate` 与 `Microsoft Update Assistant`，并尝试移除：

```text
/Library/LaunchAgents/com.microsoft.update.agent.plist
/Library/LaunchDaemons/com.microsoft.autoupdate.helper.plist
```

删除系统启动项需要 macOS 管理员授权。自动终结规则每 5 秒检查一次，
先发送 SIGTERM，超时后发送 SIGKILL。

## 后台 App 活动

“后台 App 活动”页读取 macOS 当前用户的后台项目数据库，
并使用 `launchctl` 切换有安全对应用户级 LaunchAgent 的系统项目。
页面可直接打开 `~/Library/LaunchAgents` 目录。新增和编辑会写入：

```text
~/Library/Application Support/MacSettingsDevTools/background_activities.json
~/Library/LaunchAgents/cn.gakaki.mac-settings-devtools.background.*.plist
```

删除会按同一 identifier 精确清理当前用户、Root 与系统级的 BTM 顶层记录，
连同其嵌入式子记录一起移除；只处理列表项明确关联、且位于用户/全局
LaunchAgents 或 `/Library/LaunchDaemons` 的 plist。绝不使用
`sfltool resetbtm`，不会重置其他登录项。变更前会在原目录生成
`.backup-时间戳` 备份，系统文件和 BTM 修改合并为一次管理员授权。删除时会
停止并重新启动 macOS 后台活动服务，再用 `sfltool dumpbtm` 校验目标记录未
被重新登记。macOS 要求执行修改的 App 拥有“完全磁盘访问权限”；页面顶部
提供直接打开该设置页的按钮。更新 App 后若签名变化，需要重新开关一次该授权。

## 安全说明

- 弱密码和自动登录会显著降低登录安全性，必须由用户明确点击对应按钮。
- Android SDK 和 Apple 软件许可不会由安装器自动代用户确认。
- BetterDisplay 不是运行依赖。
