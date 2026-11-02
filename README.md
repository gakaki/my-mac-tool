# 我的 Mac 设置和开发工具安装

单一 Native Shell GUI，使用 `nsh.dev` 纯 Rust 布局与 shadcn 风格组件。
项目不使用 TSX、React、GPUI、egui 或 Electron。

## 功能

- **安装编程工具**：自动准备显示器原生覆盖，配置 `127.0.0.1:7890`
  系统代理、终端代理、PATH 和国内镜像，然后检测并安装编程工具。
- **显示器**：CS40X 5K 原生覆盖、LoDPI/HiDPI 模式切换、备份与恢复。
- **系统设置**：代理、PATH、关闭桌面四角、Rosetta、弱密码策略和自动登录。
- **编程工具**：在同一页集中展示 Rust/rsproxy、Go/goproxy.cn、
  Node/npmmirror、Wrangler、cloudflared、Flutter/Dart。
- **自动启动进程**：移植 UU远程监视重启策略，进程缺失后自动启动。
- **自动清理**：搜索和多选进程，拖动进程行加入自动终结列表，
  并清理匹配的 LaunchAgent/LaunchDaemon。

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

默认监视 `/Applications/UURemote.app`。默认自动终结规则匹配
`Microsoft AutoUpdate` 与 `Microsoft Update Assistant`，并尝试移除：

```text
/Library/LaunchAgents/com.microsoft.update.agent.plist
/Library/LaunchDaemons/com.microsoft.autoupdate.helper.plist
```

删除系统启动项需要 macOS 管理员授权。自动终结规则每 5 秒检查一次，
先发送 SIGTERM，超时后发送 SIGKILL。

## 安全说明

- 弱密码和自动登录会显著降低登录安全性，必须由用户明确点击对应按钮。
- Android SDK 和 Apple 软件许可不会由安装器自动代用户确认。
- BetterDisplay 不是运行依赖。
