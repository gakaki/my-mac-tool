# 国内镜像工具链安装器

这是一个用 Rust 编写的 macOS 用户级全局安装器。它不依赖 Homebrew，也不会改动系统目录。

## GUI

构建并打开“我的 Mac 设置和开发工具安装”：

```sh
./packaging/package_app.sh
open "dist/我的 Mac 设置和开发工具安装.app"
```

生成 DMG 和 SHA-256：

```sh
./packaging/package_dmg.sh 1.0.0
```

推送 `v*` 标签会触发 GitHub Actions，自动测试、构建 DMG 并发布到
对应的 GitHub Release。

GUI 首页是“安装编程工具”。按下“检测并安装”后先配置
`127.0.0.1:7890` 系统代理、终端代理、PATH 和镜像，再并行安装各组件。
Rust、Go、Node、cloudflared 和 Flutter 会同时下载；Wrangler 在 Node
完成后自动开始。所有工具在同一页显示单独进度、当前阶段和日志；
已安装的工具会快速跳过，只安装缺失项。

“后台 App 活动”页读取 macOS 的“登录项与扩展”后台项目。系统项目
可在安全时直接启停；新增和编辑作用于本工具创建的用户级 LaunchAgent。
列表支持单条和批量删除任意后台登记，包括没有 plist 的纯 App 项目；
有关联启动配置时会精确清理用户/全局 LaunchAgents 或 LaunchDaemons。
删除会按 identifier 精确清理当前用户、Root 与系统级 BTM 登记，并在修改
前后自动生成 `.backup-时间戳` 备份；系统文件和登记变更合并为一次管理员
授权，且绝不使用 `sfltool resetbtm`。删除完成后会重启 macOS 后台活动
服务，并用 `sfltool dumpbtm` 校验目标没有被重新登记。macOS 要求执行修改
的 App 拥有“完全磁盘访问权限”，页面提供直接打开该设置页的按钮。
页面还提供直接打开 `~/Library/LaunchAgents` 目录的按钮。

“系统设置”页提供“双击桌面显示桌面”开关，直接对应 macOS 的
“点按墙纸显示桌面”偏好；切换后会立即重新载入窗口管理器。
“键盘最快重复”开关默认开启，把系统键盘重复速率调到最快、重复前延迟
调到最短；关闭后恢复 macOS 常规速度，并记住关闭选择。
“一键设置全部”会依次配置代理、PATH、镜像和 Git 代理，关闭桌面四角，
开启双击桌面显示桌面，调整 Dock 与 Finder，安装 Rosetta，并设置弱密码
和自动登录。已经满足的桌面项会自动跳过。

## 安装内容

| 组件 | 来源 | 全局配置 |
| --- | --- | --- |
| Rust / rustup | rsproxy.cn | Cargo sparse 镜像 |
| Go | golang.google.cn | `GOPROXY=https://goproxy.cn,direct` |
| `goproxycn` | Go 编写的小工具 | 切换或查看 Go 代理 |
| Node.js / npm | npmmirror | `registry=https://registry.npmmirror.com` |
| Wrangler | npm 淘宝镜像 | `npm install -g` |
| cloudflared | Cloudflare 官方发布 | 校验 SHA-256 后安装 |
| Flutter / Dart | storage.flutter-io.cn | 中国镜像，不自动接受 Android SDK 许可 |

## 使用

```sh
cd toolchain-installer
cargo build --release
./target/release/toolchain-installer install all
./target/release/toolchain-installer configure
./target/release/toolchain-installer doctor
```

也可以单独安装：

```sh
./target/release/toolchain-installer install rust
./target/release/toolchain-installer install go
./target/release/toolchain-installer install node
./target/release/toolchain-installer install wrangler
./target/release/toolchain-installer install cloudflared
./target/release/toolchain-installer install flutter
```

安装后的命令位于 `~/.local/bin`，运行时位于
`~/.local/share/toolchains`，下载缓存位于
`~/.local/cache/toolchain-installer`。

安装器会维护 `~/.zshenv` 和 `~/.npmrc` 中的标记配置，不会覆盖其他设置。
安装前会以毫秒级文件检查并行确认现有组件，已安装工具不会重新下载。

## Go 代理

```sh
goproxycn on
goproxycn off
goproxycn status
```

## Android

Flutter 本体和 Android 相关命令会全局可用，但 Android SDK 许可属于需要用户确认的法律条款，安装器不会自动执行 `--android-licenses`。安装完成后运行：

```sh
flutter doctor
flutter doctor --android-licenses
```
