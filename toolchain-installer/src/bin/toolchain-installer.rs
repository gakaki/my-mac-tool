use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const RUSTUP_SERVER: &str = "https://rsproxy.cn";
const RUSTUP_UPDATE_ROOT: &str = "https://rsproxy.cn/rustup";
const CRATES_MIRROR: &str = "sparse+https://rsproxy.cn/index/";
const GOPROXY: &str = "https://goproxy.cn,direct";
const NPM_REGISTRY: &str = "https://registry.npmmirror.com";
const FLUTTER_STORAGE: &str = "https://storage.flutter-io.cn";
const GO_DOWNLOAD_JSON: &str = "https://golang.google.cn/dl/?mode=json";
const NODE_INDEX: &str = "https://npmmirror.com/mirrors/node/index.json";
const FLUTTER_RELEASES: &str =
    "https://storage.flutter-io.cn/flutter_infra_release/releases/releases_macos.json";
const PROXY_HOST: &str = "127.0.0.1";
const PROXY_PORT: &str = "7890";
const PROXY_URL: &str = "http://127.0.0.1:7890";

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy)]
enum Component {
    Rust,
    Go,
    Node,
    Wrangler,
    Cloudflared,
    Flutter,
}

struct Layout {
    home: PathBuf,
    local: PathBuf,
    bin: PathBuf,
    tools: PathBuf,
    cache: PathBuf,
}

impl Layout {
    fn detect() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_string())?;
        let local = home.join(".local");
        Ok(Self {
            bin: local.join("bin"),
            tools: local.join("share/toolchains"),
            cache: local.join("cache/toolchain-installer"),
            home,
            local,
        })
    }

    fn prepare(&self) -> Result<()> {
        for path in [&self.bin, &self.tools, &self.cache] {
            fs::create_dir_all(path)
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        }
        Ok(())
    }
}

fn main() {
    if let Err(error) = entrypoint() {
        eprintln!("\nerror: {error}");
        std::process::exit(1);
    }
}

fn entrypoint() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            print_help();
            Ok(())
        }
        [command] if command == "help" || command == "--help" || command == "-h" => {
            print_help();
            Ok(())
        }
        [command] if command == "doctor" => doctor(),
        [command] if command == "status" => fast_status(),
        [command, name] if command == "check" => fast_check(name),
        [command] if command == "configure" => configure_only(),
        [command] if command == "setup" => setup_environment(),
        [command, rest @ ..] if command == "install" => {
            let components = parse_components(rest)?;
            install(components)
        }
        _ => {
            print_help();
            Err("invalid arguments".to_string())
        }
    }
}

fn print_help() {
    println!(
        r#"toolchain-installer

Usage:
  toolchain-installer install rust
  toolchain-installer install go
  toolchain-installer install node
  toolchain-installer install wrangler
  toolchain-installer install cloudflared
  toolchain-installer install flutter
  toolchain-installer install all
  toolchain-installer configure
  toolchain-installer setup
  toolchain-installer doctor
  toolchain-installer status
  toolchain-installer check rust|go|node|wrangler|cloudflared|flutter

Mirrors:
  Rust     https://rsproxy.cn
  Go       https://goproxy.cn
  Node npm https://registry.npmmirror.com
  Flutter  https://storage.flutter-io.cn

Layout:
  commands  ~/.local/bin
  runtimes  ~/.local/share/toolchains
  cache     ~/.local/cache/toolchain-installer

The installer is user-global, so npm -g and flutter pub do not require sudo.
"#
    );
}

fn parse_components(args: &[String]) -> Result<Vec<Component>> {
    if args.is_empty() {
        return Err("choose at least one component".to_string());
    }
    let mut result = Vec::new();
    for value in args {
        match value.as_str() {
            "rust" | "rsproxy" => result.push(Component::Rust),
            "go" | "goproxy" | "goproxycn" => result.push(Component::Go),
            "node" | "nodejs" => result.push(Component::Node),
            "wrangler" => {
                result.push(Component::Node);
                result.push(Component::Wrangler);
            }
            "wrangler-only" => result.push(Component::Wrangler),
            "cloudflared" => result.push(Component::Cloudflared),
            "cloudflare" => {
                result.push(Component::Node);
                result.push(Component::Wrangler);
                result.push(Component::Cloudflared);
            }
            "flutter" | "android" => result.push(Component::Flutter),
            "all" => result.extend([
                Component::Rust,
                Component::Go,
                Component::Node,
                Component::Wrangler,
                Component::Cloudflared,
                Component::Flutter,
            ]),
            other => return Err(format!("unknown component: {other}")),
        }
    }
    let mut unique = Vec::new();
    for component in result {
        if !unique.iter().any(|existing: &Component| {
            std::mem::discriminant(existing) == std::mem::discriminant(&component)
        }) {
            unique.push(component);
        }
    }
    Ok(unique)
}

fn install(components: Vec<Component>) -> Result<()> {
    require_commands(&["curl", "jq", "shasum", "tar", "unzip"])?;
    if env::consts::OS != "macos" {
        return Err("this installer currently targets macOS only".to_string());
    }

    let layout = Layout::detect()?;
    layout.prepare()?;
    if env::var_os("TOOLCHAIN_SKIP_CONFIGURE").is_none() {
        configure_shell(&layout)?;
    }

    for component in components {
        match component {
            Component::Rust => install_rust(&layout)?,
            Component::Go => install_go(&layout)?,
            Component::Node => install_node(&layout)?,
            Component::Wrangler => install_wrangler(&layout)?,
            Component::Cloudflared => install_cloudflared(&layout)?,
            Component::Flutter => install_flutter(&layout)?,
        }
    }

    println!("\nInstalled commands are available in:");
    println!("  {}", layout.bin.display());
    println!("Open a new terminal or run: source ~/.zshenv");
    Ok(())
}

fn configure_only() -> Result<()> {
    let layout = Layout::detect()?;
    layout.prepare()?;
    configure_shell(&layout)?;
    write_npm_config(&layout.home, &layout.local)?;
    println!("Mirror and PATH configuration refreshed.");
    Ok(())
}

fn setup_environment() -> Result<()> {
    report_stage("Preparing local directories and PATH");
    let layout = Layout::detect()?;
    layout.prepare()?;
    configure_shell(&layout)?;
    report_stage("Writing npm mirror configuration");
    write_npm_config(&layout.home, &layout.local)?;
    let cargo_home = layout.tools.join("rustup/cargo");
    fs::create_dir_all(&cargo_home).map_err(io_error)?;
    report_stage("Writing Cargo rsproxy mirror configuration");
    write_cargo_mirror(&cargo_home)?;
    report_stage("Configuring Git proxy");
    configure_git_proxy()?;
    report_stage(format!("Configuring system proxy {PROXY_URL}"));
    configure_system_proxy()?;
    report_stage("Environment setup complete");
    Ok(())
}

fn install_rust(layout: &Layout) -> Result<()> {
    if component_installed(layout, Component::Rust) {
        println!("==> Rust already installed; skipping.");
        return Ok(());
    }
    println!("\n==> Rust via rsproxy");
    let kind = "rustup";
    let root = layout.tools.join(kind);
    let cargo_home = root.join("cargo");
    let rustup_home = root.join("rustup");
    fs::create_dir_all(&cargo_home).map_err(io_error)?;
    fs::create_dir_all(&rustup_home).map_err(io_error)?;
    write_cargo_mirror(&cargo_home)?;

    let host = rust_host()?;
    let download_root = format!("{RUSTUP_SERVER}/rustup/dist/{host}");
    let binary = layout.cache.join("rustup-init");
    download(&format!("{download_root}/rustup-init"), &binary)?;
    let checksum_text = curl_text(&format!("{download_root}/rustup-init.sha256"))?;
    let expected = checksum_text
        .split_whitespace()
        .next()
        .ok_or_else(|| "rustup checksum is empty".to_string())?;
    verify_sha256(&binary, expected)?;
    let mut permissions = fs::metadata(&binary).map_err(io_error)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&binary, permissions).map_err(io_error)?;

    let mut command = Command::new(&binary);
    command.args(["-y", "--profile", "minimal", "--no-modify-path"]);
    command.env("CARGO_HOME", &cargo_home);
    command.env("RUSTUP_HOME", &rustup_home);
    command.env("RUSTUP_DIST_SERVER", RUSTUP_SERVER);
    command.env("RUSTUP_UPDATE_ROOT", RUSTUP_UPDATE_ROOT);
    run(&mut command)?;

    for name in ["cargo", "rustc", "rustup"] {
        link_command(&cargo_home.join("bin").join(name), &layout.bin.join(name))?;
    }
    println!("Rust installed through rsproxy.");
    Ok(())
}

fn install_go(layout: &Layout) -> Result<()> {
    if component_installed(layout, Component::Go) {
        println!("==> Go and goproxycn already installed; skipping.");
        return Ok(());
    }
    println!("\n==> Go via golang.google.cn + goproxy.cn");
    let json = curl_text(GO_DOWNLOAD_JSON)?;
    let version = jq(&json, ".[0].version")?;
    let filename = jq(
        &json,
        ".[0].files[] | select(.os==\"darwin\" and .arch==\"arm64\" and .kind==\"archive\") | .filename",
    )?;
    let expected = jq(
        &json,
        ".[0].files[] | select(.os==\"darwin\" and .arch==\"arm64\" and .kind==\"archive\") | .sha256",
    )?;
    let archive = layout.cache.join(&filename);
    download(&format!("https://golang.google.cn/dl/{filename}"), &archive)?;
    verify_sha256(&archive, &expected)?;

    let staging = layout.tools.join(format!("go-{version}.staging"));
    remove_if_exists(&staging)?;
    fs::create_dir_all(&staging).map_err(io_error)?;
    run(Command::new("tar").args(["-xzf", path_str(&archive)?, "-C", path_str(&staging)?]))?;
    let target = layout.tools.join(format!("go-{version}"));
    replace_owned_dir(&staging.join("go"), &target)?;

    for name in ["go", "gofmt"] {
        link_command(&target.join("bin").join(name), &layout.bin.join(name))?;
    }
    let mut go_config = Command::new(target.join("bin/go"));
    go_config.args(["env", "-w", &format!("GOPROXY={GOPROXY}")]);
    run(&mut go_config)?;
    run(Command::new(target.join("bin/go")).args(["env", "-w", "GOSUMDB=sum.golang.google.cn"]))?;
    build_goproxycn(layout, &target)?;
    println!("Go {version} installed; goproxycn helper installed.");
    Ok(())
}

fn install_node(layout: &Layout) -> Result<()> {
    if component_installed(layout, Component::Node) {
        println!("==> Node.js and npm already installed; skipping.");
        return Ok(());
    }
    println!("\n==> Node.js via npmmirror");
    let json = curl_text(NODE_INDEX)?;
    let suffix = match arch_name()?.as_str() {
        "aarch64" => "osx-arm64-tar",
        "x86_64" => "osx-x64-tar",
        _ => return Err("unsupported macOS architecture".to_string()),
    };
    let version = jq(
        &json,
        &format!("[.[] | select(.lts != false and (.files | index(\"{suffix}\")))] | .[0].version"),
    )?;
    let archive_name = format!("node-{version}-darwin-{}.tar.gz", node_arch()?);
    let archive = layout.cache.join(&archive_name);
    download(
        &format!("https://npmmirror.com/mirrors/node/{version}/{archive_name}"),
        &archive,
    )?;
    let checksums = curl_text(&format!(
        "https://mirrors.tuna.tsinghua.edu.cn/nodejs-release/{version}/SHASUMS256.txt"
    ))?;
    let expected = checksums
        .lines()
        .find(|line| line.ends_with(&archive_name))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| format!("checksum for {archive_name} not found"))?;
    verify_sha256(&archive, expected)?;

    let staging = layout.tools.join(format!("node-{version}.staging"));
    remove_if_exists(&staging)?;
    fs::create_dir_all(&staging).map_err(io_error)?;
    run(Command::new("tar").args(["-xzf", path_str(&archive)?, "-C", path_str(&staging)?]))?;
    let extracted = staging.join(format!("node-{version}-darwin-{}", node_arch()?));
    let target = layout.tools.join(format!("node-{version}"));
    replace_owned_dir(&extracted, &target)?;

    for name in ["node", "npm", "npx", "corepack"] {
        let source = target.join("bin").join(name);
        if source.exists() {
            link_command(&source, &layout.bin.join(name))?;
        }
    }
    write_npm_config(&layout.home, &layout.local)?;
    println!("Node {version}, npm, and Taobao mirror configuration installed.");
    Ok(())
}

fn install_wrangler(layout: &Layout) -> Result<()> {
    if component_installed(layout, Component::Wrangler) {
        println!("==> Wrangler already installed; skipping.");
        return Ok(());
    }
    println!("\n==> Wrangler via npm mirror");
    let node = find_command(layout, "node")?;
    let npm = find_command(layout, "npm")?;
    if node.is_none() || npm.is_none() {
        return Err("install the node component before wrangler".to_string());
    }
    let mut command = Command::new(npm.expect("checked"));
    command.args([
        "install",
        "--global",
        "--allow-scripts=esbuild,workerd",
        "--registry",
        NPM_REGISTRY,
        "wrangler",
    ]);
    command.env("PATH", tool_path(layout));
    command.env("npm_config_prefix", &layout.local);
    run(&mut command)?;
    println!("Wrangler installed globally.");
    Ok(())
}

fn install_cloudflared(layout: &Layout) -> Result<()> {
    if component_installed(layout, Component::Cloudflared) {
        println!("==> cloudflared already installed; skipping.");
        return Ok(());
    }
    println!("\n==> cloudflared");
    let json = curl_text("https://api.github.com/repos/cloudflare/cloudflared/releases/latest")?;
    let asset = if arch_name()? == "aarch64" {
        "cloudflared-darwin-arm64.tgz"
    } else {
        "cloudflared-darwin-amd64.tgz"
    };
    let url = jq(
        &json,
        &format!(".assets[] | select(.name==\"{asset}\") | .browser_download_url"),
    )?;
    let digest = jq(
        &json,
        &format!(".assets[] | select(.name==\"{asset}\") | .digest"),
    )?;
    let expected = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| "cloudflared asset has no SHA-256 digest".to_string())?;
    let archive = layout.cache.join(asset);
    download_github(&url, &archive)?;
    verify_sha256(&archive, expected)?;

    let staging = layout.tools.join("cloudflared.staging");
    remove_if_exists(&staging)?;
    fs::create_dir_all(&staging).map_err(io_error)?;
    run(Command::new("tar").args(["-xzf", path_str(&archive)?, "-C", path_str(&staging)?]))?;
    let target = layout.tools.join("cloudflared");
    replace_owned_file(&staging.join("cloudflared"), &target)?;
    link_command(&target, &layout.bin.join("cloudflared"))?;
    println!("cloudflared installed.");
    Ok(())
}

fn install_flutter(layout: &Layout) -> Result<()> {
    if component_installed(layout, Component::Flutter) {
        println!("==> Flutter and Dart already installed; skipping.");
        return Ok(());
    }
    println!("\n==> Flutter stable via flutter-io.cn");
    let json = curl_text(FLUTTER_RELEASES)?;
    let arch_filter = if arch_name()? == "aarch64" {
        "(.archive | contains(\"arm64\"))"
    } else {
        "(.archive | contains(\"arm64\") | not)"
    };
    let filter = format!(
        ".current_release.stable as $h | .releases[] | select(.hash==$h and {arch_filter}) | .version"
    );
    let version = jq(&json, &filter)?;
    let archive_name = jq(
        &json,
        &format!(".releases[] | select(.version==\"{version}\" and {arch_filter}) | .archive"),
    )?;
    let expected = jq(
        &json,
        &format!(".releases[] | select(.version==\"{version}\" and {arch_filter}) | .sha256"),
    )?;
    let archive = layout.cache.join(
        Path::new(&archive_name)
            .file_name()
            .ok_or_else(|| "invalid Flutter archive name".to_string())?,
    );
    download(
        &format!("{FLUTTER_STORAGE}/flutter_infra_release/releases/{archive_name}"),
        &archive,
    )?;
    verify_sha256(&archive, &expected)?;

    let staging = layout.tools.join(format!("flutter-{version}.staging"));
    remove_if_exists(&staging)?;
    fs::create_dir_all(&staging).map_err(io_error)?;
    run(Command::new("unzip").args(["-q", path_str(&archive)?, "-d", path_str(&staging)?]))?;
    let target = layout.tools.join(format!("flutter-{version}"));
    replace_owned_dir(&staging.join("flutter"), &target)?;

    link_command(&target.join("bin/flutter"), &layout.bin.join("flutter"))?;
    link_command(&target.join("bin/dart"), &layout.bin.join("dart"))?;
    let mut command = Command::new(target.join("bin/flutter"));
    command.args(["config", "--no-analytics"]);
    command.env("PATH", tool_path(layout));
    command.env("FLUTTER_STORAGE_BASE_URL", FLUTTER_STORAGE);
    command.env("PUB_HOSTED_URL", "https://pub.flutter-io.cn");
    run(&mut command)?;
    println!("Flutter {version} installed. Android SDK licenses are not auto-accepted.");
    Ok(())
}

fn build_goproxycn(layout: &Layout, go_dir: &Path) -> Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("support/goproxycn");
    let helper = layout.bin.join("goproxycn");
    let mut command = Command::new(go_dir.join("bin/go"));
    command
        .args(["build", "-trimpath", "-o", path_str(&helper)?, "."])
        .current_dir(&source);
    run(&mut command)?;
    Ok(())
}

fn configure_shell(layout: &Layout) -> Result<()> {
    let cargo = layout.tools.join("rustup/cargo");
    let rustup = layout.tools.join("rustup/rustup");
    let block = format!(
        r#"export CARGO_HOME="{}"
export RUSTUP_HOME="{}"
export PATH="{}:{}:$PATH"
export RUSTUP_DIST_SERVER={}
export RUSTUP_UPDATE_ROOT={}
export GOPROXY={}
export GOSUMDB=sum.golang.google.cn
export PUB_HOSTED_URL=https://pub.flutter-io.cn
export FLUTTER_STORAGE_BASE_URL={}
export HTTP_PROXY={}
export HTTPS_PROXY={}
export ALL_PROXY={}
export http_proxy={}
export https_proxy={}
export all_proxy={}
export NO_PROXY=localhost,127.0.0.1,::1
export no_proxy=localhost,127.0.0.1,::1"#,
        cargo.display(),
        rustup.display(),
        layout.bin.display(),
        cargo.join("bin").display(),
        RUSTUP_SERVER,
        RUSTUP_UPDATE_ROOT,
        GOPROXY,
        FLUTTER_STORAGE,
        PROXY_URL,
        PROXY_URL,
        PROXY_URL,
        PROXY_URL,
        PROXY_URL,
        PROXY_URL
    );
    update_marker_block(
        &layout.home.join(".zshenv"),
        "# >>> toolchain-installer >>>",
        "# <<< toolchain-installer <<<",
        &block,
    )
}

fn write_cargo_mirror(cargo_home: &Path) -> Result<()> {
    let block = format!(
        r#"[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "{CRATES_MIRROR}"

[http]
proxy = "{PROXY_URL}"

[net]
git-fetch-with-cli = true"#
    );
    update_marker_block(
        &cargo_home.join("config.toml"),
        "# >>> toolchain-installer >>>",
        "# <<< toolchain-installer <<<",
        &block,
    )
}

fn write_npm_config(home: &Path, prefix: &Path) -> Result<()> {
    let path = home.join(".npmrc");
    let mut content = fs::read_to_string(&path).unwrap_or_default();
    content = replace_property(&content, "registry", &format!("{NPM_REGISTRY}/"));
    content = replace_property(&content, "prefix", path_str(prefix)?);
    content = replace_property(&content, "proxy", PROXY_URL);
    content = replace_property(&content, "https-proxy", PROXY_URL);
    content = replace_property(&content, "allow-scripts", "esbuild,workerd");
    if !content.ends_with('\n') {
        content.push('\n');
    }
    fs::write(&path, content).map_err(io_error)
}

fn configure_git_proxy() -> Result<()> {
    run(Command::new("git").args(["config", "--global", "http.proxy", PROXY_URL]))?;
    run(Command::new("git").args(["config", "--global", "https.proxy", PROXY_URL]))
}

fn configure_system_proxy() -> Result<()> {
    let service = default_network_service()
        .ok_or_else(|| "cannot determine the active network service".to_string())?;
    report_stage(format!(
        "Checking macOS proxy for network service: {service}"
    ));
    if system_proxy_is_configured(&service) {
        report_stage(format!(
            "System proxy already points to {PROXY_HOST}:{PROXY_PORT}; skipping authorization"
        ));
        return Ok(());
    }
    let quoted_service = shell_quote(&service);
    let commands = [
        format!("networksetup -setwebproxy {quoted_service} {PROXY_HOST} {PROXY_PORT}"),
        format!("networksetup -setsecurewebproxy {quoted_service} {PROXY_HOST} {PROXY_PORT}"),
        format!("networksetup -setsocksfirewallproxy {quoted_service} {PROXY_HOST} {PROXY_PORT}"),
        format!("networksetup -setwebproxystate {quoted_service} on"),
        format!("networksetup -setsecurewebproxystate {quoted_service} on"),
        format!("networksetup -setsocksfirewallproxystate {quoted_service} on"),
    ];

    let mut direct_ok = true;
    for command in &commands {
        let status = Command::new("/bin/sh")
            .args(["-c", command])
            .status()
            .map_err(io_error)?;
        if !status.success() {
            direct_ok = false;
            break;
        }
    }
    if direct_ok {
        return Ok(());
    }

    report_stage("Administrator authorization is required for system proxy settings");
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        commands
            .join(" && ")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    );
    run(Command::new("osascript").args(["-e", &script]))
}

fn system_proxy_is_configured(service: &str) -> bool {
    ["webproxy", "securewebproxy", "socksfirewallproxy"]
        .into_iter()
        .all(|kind| {
            let output = Command::new("networksetup")
                .arg(format!("-get{kind}"))
                .arg(service)
                .output();
            let Ok(output) = output else {
                return false;
            };
            if !output.status.success() {
                return false;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let enabled = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("Enabled:"))
                .map(str::trim)
                .is_some_and(|value| value.eq_ignore_ascii_case("yes"));
            let server = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("Server:"))
                .map(str::trim)
                .is_some_and(|value| value == PROXY_HOST);
            let port = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("Port:"))
                .map(str::trim)
                .is_some_and(|value| value == PROXY_PORT);
            enabled && server && port
        })
}

fn report_stage(message: impl std::fmt::Display) {
    println!("==> {message}");
    let _ = io::stdout().flush();
}

fn default_network_service() -> Option<String> {
    let route = Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    let interface = String::from_utf8_lossy(&route.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(str::trim)?
        .to_string();
    let order = Command::new("networksetup")
        .arg("-listnetworkserviceorder")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&order.stdout);
    let mut current = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(numbered) = trimmed.strip_prefix('(')
            && let Some(close) = numbered.find(')')
        {
            let index = &numbered[..close];
            if index.chars().all(|character| character.is_ascii_digit()) {
                current = Some(numbered[close + 1..].trim().to_string());
            }
        }
        if trimmed.contains(&format!("Device: {interface})")) {
            return current;
        }
    }
    None
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn replace_property(content: &str, key: &str, value: &str) -> String {
    let mut output = String::new();
    let prefix = format!("{key}=");
    for line in content.lines() {
        if !line.trim_start().starts_with(&prefix) {
            output.push_str(line);
            output.push('\n');
        }
    }
    output.push_str(&prefix);
    output.push_str(value);
    output.push('\n');
    output
}

fn update_marker_block(path: &Path, begin: &str, end: &str, block: &str) -> Result<()> {
    let old = fs::read_to_string(path).unwrap_or_default();
    let mut output = if let (Some(start), Some(stop)) = (old.find(begin), old.find(end)) {
        if stop > start {
            let mut result = old[..start].to_string();
            result.push_str(&old[stop + end.len()..]);
            while result.ends_with("\n\n") {
                result.pop();
            }
            result
        } else {
            old
        }
    } else {
        old
    };
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(begin);
    output.push('\n');
    output.push_str(block);
    output.push('\n');
    output.push_str(end);
    output.push('\n');
    fs::write(path, output).map_err(io_error)
}

fn doctor() -> Result<()> {
    let layout = Layout::detect()?;
    let checks = [
        ("rustc", vec!["--version"]),
        ("cargo", vec!["--version"]),
        ("go", vec!["version"]),
        ("goproxycn", vec!["status"]),
        ("node", vec!["--version"]),
        ("npm", vec!["--version"]),
        ("wrangler", vec!["--version"]),
        ("cloudflared", vec!["--version"]),
        ("flutter", vec!["--version"]),
        ("dart", vec!["--version"]),
    ];
    let path = tool_path(&layout);
    for (command, args) in checks {
        let Some(binary) = find_command(&layout, command)? else {
            println!("{command:<14} missing");
            continue;
        };
        let output = Command::new(binary)
            .args(args)
            .env("PATH", &path)
            .env("FLUTTER_STORAGE_BASE_URL", FLUTTER_STORAGE)
            .env("PUB_HOSTED_URL", "https://pub.flutter-io.cn")
            .output()
            .map_err(io_error)?;
        let first = String::from_utf8_lossy(&output.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&output.stderr).lines())
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no version output")
            .trim()
            .to_string();
        println!("{command:<14} {first}");
    }
    println!("\nMirror configuration:");
    println!("  Rust  {RUSTUP_SERVER}");
    println!("  Go    {GOPROXY}");
    println!("  npm   {NPM_REGISTRY}");
    println!("  Flutter {FLUTTER_STORAGE}");
    Ok(())
}

impl Component {
    fn key(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Node => "node",
            Self::Wrangler => "wrangler",
            Self::Cloudflared => "cloudflared",
            Self::Flutter => "flutter",
        }
    }

    fn parse_exact(value: &str) -> Result<Self> {
        match value {
            "rust" | "rsproxy" => Ok(Self::Rust),
            "go" | "goproxycn" => Ok(Self::Go),
            "node" | "nodejs" => Ok(Self::Node),
            "wrangler" => Ok(Self::Wrangler),
            "cloudflared" => Ok(Self::Cloudflared),
            "flutter" | "android" => Ok(Self::Flutter),
            other => Err(format!("unknown component: {other}")),
        }
    }
}

fn component_installed(layout: &Layout, component: Component) -> bool {
    let required: &[&str] = match component {
        Component::Rust => &["cargo", "rustc", "rustup"],
        Component::Go => &["go", "gofmt", "goproxycn"],
        Component::Node => &["node", "npm", "npx"],
        Component::Wrangler => &["wrangler"],
        Component::Cloudflared => &["cloudflared"],
        Component::Flutter => &["flutter", "dart"],
    };
    required.iter().all(|name| layout.bin.join(name).exists())
}

fn fast_check(name: &str) -> Result<()> {
    let layout = Layout::detect()?;
    let component = Component::parse_exact(name)?;
    if component_installed(&layout, component) {
        println!("{}: installed", component.key());
        Ok(())
    } else {
        Err(format!("{}: not installed", component.key()))
    }
}

fn fast_status() -> Result<()> {
    let layout = Layout::detect()?;
    let statuses = [
        Component::Rust,
        Component::Go,
        Component::Node,
        Component::Wrangler,
        Component::Cloudflared,
        Component::Flutter,
    ]
    .into_iter()
    .map(|component| {
        format!(
            "\"{}\":{}",
            component.key(),
            component_installed(&layout, component)
        )
    })
    .collect::<Vec<_>>()
    .join(",");
    println!("{{{statuses}}}");
    Ok(())
}

fn require_commands(commands: &[&str]) -> Result<()> {
    for command in commands {
        if Command::new(command)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return Err(format!("required command not found: {command}"));
        }
    }
    Ok(())
}

fn curl_text(url: &str) -> Result<String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
            "--retry-all-errors",
            "--connect-timeout",
            "15",
            "--max-time",
            "60",
            url,
        ])
        .output()
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(format!(
            "download failed: {url}\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 from {url}: {e}"))
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let (remote_size, supports_ranges) = remote_download_info(url);
    let remote_size = remote_size.filter(|size| *size > 0);
    if let Ok(metadata) = fs::metadata(destination) {
        match remote_size {
            Some(size) if metadata.len() == size => {
                println!("Already downloaded: {}", destination.display());
                return Ok(());
            }
            Some(size) if supports_ranges && metadata.len() < size => {}
            _ => remove_if_exists(destination)?,
        }
    }

    println!("Downloading {url}");
    let mut command = Command::new("curl");
    command.args([
        "--fail",
        "--location",
        "--retry",
        "4",
        "--retry-all-errors",
        "--connect-timeout",
        "15",
    ]);
    if destination.exists() && supports_ranges {
        command.args(["--continue-at", "-"]);
    }
    command.args(["--output", path_str(destination)?, url]);
    run(&mut command)
}

fn remote_download_info(url: &str) -> (Option<u64>, bool) {
    let Ok(output) = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--head",
            "--connect-timeout",
            "15",
            url,
        ])
        .output()
    else {
        return (None, false);
    };
    if !output.status.success() {
        return (None, false);
    }
    let headers = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let size = headers
        .lines()
        .filter_map(|line| line.strip_prefix("content-length:"))
        .filter_map(|value| value.trim().parse::<u64>().ok())
        .next_back();
    let supports_ranges = headers
        .lines()
        .filter_map(|line| line.strip_prefix("accept-ranges:"))
        .any(|value| value.trim() == "bytes");
    (size, supports_ranges)
}

fn download_github(url: &str, destination: &Path) -> Result<()> {
    if url.contains("github.com/") {
        let proxied = format!("https://gh-proxy.com/{url}");
        let (size, _) = remote_download_info(&proxied);
        if let (Some(size), Ok(metadata)) =
            (size.filter(|size| *size > 0), fs::metadata(destination))
            && metadata.len() == size
        {
            println!("Already downloaded: {}", destination.display());
            return Ok(());
        }
        download_segmented(&proxied, destination, 8)
    } else {
        download(url, destination)
    }
}

fn download_segmented(url: &str, destination: &Path, parts: usize) -> Result<()> {
    let (size, ranges) = remote_download_info(url);
    let size = size
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("remote size unavailable for segmented download: {url}"))?;
    if !ranges {
        return Err(format!(
            "server does not advertise ranges for segmented download: {url}"
        ));
    }

    let parts = parts.max(1);
    let chunk = size.div_ceil(parts as u64);
    let directory = destination.with_extension(format!("segments-{}", std::process::id()));
    remove_if_exists(&directory)?;
    fs::create_dir_all(&directory).map_err(io_error)?;
    println!("Downloading {url} in {parts} segments");

    let mut handles = Vec::new();
    for index in 0..parts {
        let start = index as u64 * chunk;
        if start >= size {
            break;
        }
        let end = (start + chunk - 1).min(size - 1);
        let segment = directory.join(format!("part-{index:02}"));
        let url = url.to_string();
        handles.push(thread::spawn(move || -> Result<()> {
            let mut command = Command::new("curl");
            command.args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "4",
                "--retry-all-errors",
                "--connect-timeout",
                "15",
                "--range",
                &format!("{start}-{end}"),
                "--output",
                path_str(&segment)?,
                &url,
            ]);
            run(&mut command)
        }));
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| "segmented download worker panicked".to_string())??;
    }

    remove_if_exists(destination)?;
    let mut output = File::create(destination).map_err(io_error)?;
    for index in 0..parts {
        let segment = directory.join(format!("part-{index:02}"));
        if !segment.exists() {
            break;
        }
        let mut input = File::open(&segment).map_err(io_error)?;
        io::copy(&mut input, &mut output).map_err(io_error)?;
    }
    output.flush().map_err(io_error)?;
    let actual_size = fs::metadata(destination).map_err(io_error)?.len();
    if actual_size != size {
        return Err(format!(
            "segmented download size mismatch: expected {size}, got {actual_size}"
        ));
    }
    remove_if_exists(&directory)?;
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let output = Command::new("shasum")
        .args(["-a", "256", path_str(path)?])
        .output()
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(format!("cannot checksum {}", path.display()));
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if actual != expected.to_ascii_lowercase() {
        let _ = remove_if_exists(path);
        return Err(format!(
            "checksum mismatch for {}\nexpected {}\nactual   {}",
            path.display(),
            expected,
            actual
        ));
    }
    println!(
        "SHA-256 verified: {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(())
}

fn jq(json: &str, filter: &str) -> Result<String> {
    let mut child = Command::new("jq")
        .args(["-r", filter])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_error)?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "cannot open jq stdin".to_string())?;
        stdin.write_all(json.as_bytes()).map_err(io_error)?;
    }
    let output = child.wait_with_output().map_err(io_error)?;
    if !output.status.success() {
        return Err(format!(
            "jq failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|e| format!("jq returned invalid UTF-8: {e}"))?
        .trim()
        .to_string();
    if value.is_empty() || value == "null" {
        return Err(format!("jq query returned no value: {filter}"));
    }
    Ok(value)
}

fn run(command: &mut Command) -> Result<()> {
    eprintln!("$ {:?}", command);
    let status = command.status().map_err(io_error)?;
    if !status.success() {
        return Err(format!("command failed with {status}"));
    }
    Ok(())
}

fn link_command(target: &Path, link: &Path) -> Result<()> {
    if !target.exists() {
        return Err(format!("cannot link missing {}", target.display()));
    }
    if let Ok(metadata) = fs::symlink_metadata(link) {
        if metadata.file_type().is_symlink() {
            let current = fs::read_link(link).map_err(io_error)?;
            if current == target {
                return Ok(());
            }
            fs::remove_file(link).map_err(io_error)?;
        } else {
            return Err(format!(
                "refusing to replace non-symlink {}",
                link.display()
            ));
        }
    }
    symlink(target, link).map_err(io_error)?;
    Ok(())
}

fn replace_owned_dir(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        println!("Keeping existing {}", target.display());
        remove_if_exists(source)?;
        return Ok(());
    }
    fs::rename(source, target).map_err(io_error)
}

fn replace_owned_file(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        println!("Keeping existing {}", target.display());
        remove_if_exists(source)?;
        return Ok(());
    }
    fs::rename(source, target).map_err(io_error)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).map_err(io_error),
        Ok(_) => fs::remove_file(path).map_err(io_error),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn find_command(layout: &Layout, command: &str) -> Result<Option<PathBuf>> {
    let local = layout.bin.join(command);
    if local.exists() {
        return Ok(Some(local));
    }
    Ok(env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|path| path.join(command))
            .find(|candidate| candidate.is_file())
    }))
}

fn tool_path(layout: &Layout) -> String {
    format!(
        "{}:{}:{}",
        layout.bin.display(),
        layout.home.join(".cargo/bin").display(),
        env::var("PATH").unwrap_or_default()
    )
}

fn arch_name() -> Result<String> {
    let output = Command::new("uname").arg("-m").output().map_err(io_error)?;
    if !output.status.success() {
        return Err("cannot determine CPU architecture".to_string());
    }
    let arch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(match arch.as_str() {
        "arm64" => "aarch64".to_string(),
        _ => arch,
    })
}

fn rust_host() -> Result<&'static str> {
    match arch_name()?.as_str() {
        "aarch64" => Ok("aarch64-apple-darwin"),
        "x86_64" => Ok("x86_64-apple-darwin"),
        _ => Err("unsupported Rust host architecture".to_string()),
    }
}

fn node_arch() -> Result<&'static str> {
    match arch_name()?.as_str() {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x64"),
        _ => Err("unsupported Node architecture".to_string()),
    }
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

fn io_error(error: io::Error) -> String {
    error.to_string()
}
