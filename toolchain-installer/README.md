# 国内镜像工具链安装器

这是一个用 Rust 编写的 macOS 用户级全局安装器。它不依赖 Homebrew，也不会改动系统目录。

## GUI

构建并打开“我的 Mac 设置和开发工具安装”：

```sh
./packaging/package_app.sh
open "dist/我的 Mac 设置和开发工具安装.app"
```

GUI 首页是“安装编程工具”。按下“检测并安装”后先配置
`127.0.0.1:7890` 系统代理、终端代理、PATH 和镜像，再并行安装各组件。
Rust、Go、Node、cloudflared 和 Flutter 会同时下载；Wrangler 在 Node
完成后自动开始。所有工具在同一页显示单独进度、当前阶段和日志；
已安装的工具会快速跳过，只安装缺失项。

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
