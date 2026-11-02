use crate::privileged::{run as run_privileged, shell_quote};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayInfo {
    pub name: String,
    pub vendor_hex: String,
    pub product_hex: String,
    pub vendor_id: u32,
    pub product_id: u32,
    pub current_width: u32,
    pub current_height: u32,
    pub current_refresh: u32,
    pub better_display_running: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetMode {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub macos_version: String,
    pub display: DisplayInfo,
    pub target: TargetMode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstallOutcome {
    pub override_path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

pub fn detect() -> Result<Detection, String> {
    let output = run(
        "/usr/sbin/system_profiler",
        &["SPDisplaysDataType", "-json", "-detailLevel", "full"],
    )?;
    let json = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|error| format!("无法解析显示器信息：{error}"))?;
    let mut display = parse_system_profiler(&json)?;
    display.better_display_running = Command::new("/usr/bin/pgrep")
        .args(["-x", "BetterDisplay"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    // CS40X advertises 5120x2160 at 120 Hz in EDID. macOS may hide that mode
    // after an OS upgrade unless a product override is present.
    let target = if is_cs40x(&display) {
        TargetMode {
            width: 5120,
            height: 2160,
            refresh: 120,
        }
    } else {
        TargetMode {
            width: display.current_width.max(2),
            height: display.current_height.max(2),
            refresh: display.current_refresh.max(60),
        }
    };

    let version_output = run("/usr/bin/sw_vers", &["-productVersion"])?;
    let macos_version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_owned();

    Ok(Detection {
        macos_version,
        display,
        target,
    })
}

pub fn parse_system_profiler(json: &Value) -> Result<DisplayInfo, String> {
    let displays = json
        .get("SPDisplaysDataType")
        .and_then(Value::as_array)
        .ok_or_else(|| "系统没有返回显示器数据".to_string())?
        .iter()
        .filter_map(|gpu| gpu.get("spdisplays_ndrvs").and_then(Value::as_array))
        .flatten()
        .filter(|display| {
            display
                .get("spdisplays_online")
                .and_then(Value::as_str)
                .map(|value| value == "spdisplays_yes")
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();

    let display = displays
        .iter()
        .copied()
        .find(|display| {
            display
                .get("spdisplays_main")
                .and_then(Value::as_str)
                .map(|value| value == "spdisplays_yes")
                .unwrap_or(false)
        })
        .or_else(|| displays.first().copied())
        .ok_or_else(|| "没有找到已连接的显示器".to_string())?;

    let name = display
        .get("_name")
        .and_then(Value::as_str)
        .unwrap_or("Unknown Display")
        .to_owned();
    let vendor_hex = display
        .get("_spdisplays_display-vendor-id")
        .and_then(Value::as_str)
        .ok_or_else(|| "显示器没有厂商 ID".to_string())?
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    let product_hex = display
        .get("_spdisplays_display-product-id")
        .and_then(Value::as_str)
        .ok_or_else(|| "显示器没有产品 ID".to_string())?
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    let vendor_id = parse_hex_id(&vendor_hex)?;
    let product_id = parse_hex_id(&product_hex)?;

    let pixels = display
        .get("_spdisplays_pixels")
        .and_then(Value::as_str)
        .unwrap_or("0 x 0");
    let (current_width, current_height) = parse_resolution(pixels);
    let resolution = display
        .get("spdisplays_resolution")
        .and_then(Value::as_str)
        .unwrap_or("");
    let current_refresh = parse_refresh(resolution).unwrap_or(60);

    Ok(DisplayInfo {
        name,
        vendor_hex,
        product_hex,
        vendor_id,
        product_id,
        current_width,
        current_height,
        current_refresh,
        better_display_running: false,
    })
}

pub fn install_override(
    display: &DisplayInfo,
    target: TargetMode,
) -> Result<InstallOutcome, String> {
    validate_target(target)?;
    let override_path = override_path(display.vendor_id, display.product_id);
    let state = state_dir()?;
    fs::create_dir_all(&state).map_err(|error| format!("无法创建配置目录：{error}"))?;

    let original = state.join(format!(
        "original-{:x}-{:x}.plist",
        display.vendor_id, display.product_id
    ));
    let missing = state.join(format!(
        "original-{:x}-{:x}.missing",
        display.vendor_id, display.product_id
    ));
    if !original.exists() && !missing.exists() {
        if override_path.exists() {
            fs::copy(&override_path, &original)
                .map_err(|error| format!("无法备份现有显示覆盖：{error}"))?;
        } else {
            fs::write(&missing, b"No override existed before Display Doctor.\n")
                .map_err(|error| format!("无法记录初始配置状态：{error}"))?;
        }
    }

    let generated = state.join(format!(
        "installed-{:x}-{:x}.plist",
        display.vendor_id, display.product_id
    ));
    fs::write(&generated, override_xml(display, target))
        .map_err(|error| format!("无法生成显示覆盖：{error}"))?;

    let temporary = env::temp_dir().join(format!(
        "display-doctor-{}-{}.plist",
        std::process::id(),
        display.product_id
    ));
    fs::copy(&generated, &temporary).map_err(|error| format!("无法准备安装文件：{error}"))?;
    let copy_result = privileged_copy(&temporary, &override_path);
    let _ = fs::remove_file(&temporary);
    copy_result?;

    Ok(InstallOutcome {
        override_path,
        backup_path: original.exists().then_some(original),
    })
}

pub fn restore_override(display: &DisplayInfo) -> Result<PathBuf, String> {
    let state = state_dir()?;
    let original = state.join(format!(
        "original-{:x}-{:x}.plist",
        display.vendor_id, display.product_id
    ));
    let missing = state.join(format!(
        "original-{:x}-{:x}.missing",
        display.vendor_id, display.product_id
    ));
    let target = override_path(display.vendor_id, display.product_id);

    if original.exists() {
        let temporary = env::temp_dir().join(format!(
            "display-doctor-restore-{}-{}.plist",
            std::process::id(),
            display.product_id
        ));
        fs::copy(&original, &temporary).map_err(|error| format!("无法准备恢复文件：{error}"))?;
        let result = privileged_copy(&temporary, &target);
        let _ = fs::remove_file(&temporary);
        result?;
    } else if missing.exists() {
        privileged_remove(&target)?;
    } else {
        return Err("没有找到这台显示器的安装前备份".to_string());
    }
    Ok(target)
}

pub fn apply_mode(
    target: TargetMode,
    logical_width: u32,
    logical_height: u32,
) -> Result<(), String> {
    validate_target(target)?;
    if logical_width < 640 || logical_height < 360 {
        return Err("桌面分辨率太小，至少需要 640×360".to_string());
    }
    apply_coregraphics_mode(target, logical_width, logical_height)
}

pub fn open_display_settings() -> Result<(), String> {
    let status = Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.Displays-Settings.extension")
        .status()
        .map_err(|error| format!("无法打开系统显示设置：{error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("无法打开系统显示设置".to_string())
    }
}

pub fn override_xml(display: &DisplayInfo, target: TargetMode) -> String {
    let pixel_dimensions = base64_encode(&mode_bytes(target.width, target.height, None));
    let default_resolution = base64_encode(&mode_bytes(
        target.width,
        target.height,
        Some(target.refresh),
    ));
    let mut scale_entries = vec![pixel_dimensions.clone()];
    scale_entries.extend(
        logical_candidates(target)
            .into_iter()
            .filter(|(width, height)| *width != target.width || *height != target.height)
            .map(|(width, height)| base64_encode(&hidpi_mode_bytes(width, height))),
    );
    let scale_xml = scale_entries
        .into_iter()
        .map(|entry| format!("\t\t<data>{entry}</data>\n"))
        .collect::<String>();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>DisplayProductID</key>
	<integer>{}</integer>
	<key>DisplayVendorID</key>
	<integer>{}</integer>
	<key>DisplayProductName</key>
	<string>{}</string>
	<key>DisplayPixelDimensions</key>
	<data>{}</data>
	<key>default-resolution</key>
	<data>{}</data>
	<key>scale-resolutions</key>
	<array>
{}	</array>
</dict>
</plist>
"#,
        display.product_id,
        display.vendor_id,
        xml_escape(&display.name),
        pixel_dimensions,
        default_resolution,
        scale_xml
    )
}

pub fn is_cs40x(display: &DisplayInfo) -> bool {
    (display.vendor_id == 0x0e66 && display.product_id == 0x0120)
        || display.name.to_ascii_uppercase().starts_with("CS40X")
}

pub fn logical_candidates(target: TargetMode) -> Vec<(u32, u32)> {
    let widths = [3840_u32, 3200, 2880, 2560, 2304, 2048, 1920, 1600, 1280];
    let maximum = (target.width.saturating_mul(3) / 4)
        .min(3840)
        .max(target.width / 2);
    let mut candidates = widths
        .into_iter()
        .filter(|width| *width < target.width && *width <= maximum)
        .filter_map(|width| {
            let height = ((target.height as u64 * width as u64 + target.width as u64 / 2)
                / target.width as u64) as u32;
            (height >= 360 && height < target.height).then_some((width, height))
        })
        .collect::<Vec<_>>();
    let half_width = target.width / 2;
    let half_height = target.height / 2;
    if half_width < target.width && half_height >= 360 {
        candidates.push((half_width, half_height));
    }
    candidates.sort_unstable_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates.dedup();
    candidates
}

fn mode_bytes(width: u32, height: u32, tail: Option<u32>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(if tail.is_some() { 12 } else { 8 });
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    if let Some(value) = tail {
        if value <= u16::MAX as u32 {
            bytes.extend_from_slice(&(value as u16).to_be_bytes());
            bytes.extend_from_slice(&u16::MAX.to_be_bytes());
        } else {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }
    bytes
}

fn hidpi_mode_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes
}

fn parse_hex_id(value: &str) -> Result<u32, String> {
    u32::from_str_radix(value, 16).map_err(|error| format!("无效的显示器 ID {value}: {error}"))
}

fn parse_resolution(value: &str) -> (u32, u32) {
    let dimensions = value
        .split_once('@')
        .map(|(dimensions, _)| dimensions)
        .unwrap_or(value);
    let mut parts = dimensions
        .split('x')
        .map(|part| part.trim().parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn parse_refresh(value: &str) -> Option<u32> {
    value
        .split_once('@')?
        .1
        .trim()
        .trim_end_matches("Hz")
        .parse::<f64>()
        .ok()
        .map(|refresh| refresh.round() as u32)
}

fn validate_target(target: TargetMode) -> Result<(), String> {
    if target.width < 640 || target.height < 360 {
        return Err("目标分辨率太小".to_string());
    }
    if target.width > 16384 || target.height > 16384 {
        return Err("目标分辨率超出 macOS 显示覆盖的合理范围".to_string());
    }
    if !target.width.is_multiple_of(2) || !target.height.is_multiple_of(2) {
        return Err("目标宽高必须是偶数".to_string());
    }
    if !(1..=240).contains(&target.refresh) {
        return Err("刷新率应在 1 到 240 Hz 之间".to_string());
    }
    Ok(())
}

fn override_path(vendor_id: u32, product_id: u32) -> PathBuf {
    PathBuf::from("/Library/Displays/Contents/Resources/Overrides")
        .join(format!("DisplayVendorID-{vendor_id:x}"))
        .join(format!("DisplayProductID-{product_id:x}"))
}

fn state_dir() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or_else(|| "无法确定当前用户目录".to_string())?;
    Ok(PathBuf::from(home).join("Library/Application Support/Display Doctor"))
}

fn run(program: &str, args: &[&str]) -> Result<Output, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("无法运行 {program}：{error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{program} 执行失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn privileged_copy(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "显示覆盖路径没有父目录".to_string())?;
    let command = format!(
        "/bin/mkdir -p {} && /bin/cp {} {} && /usr/sbin/chown root:wheel {} && /bin/chmod 644 {} && /usr/bin/defaults write /Library/Preferences/com.apple.windowserver DisplayResolutionEnabled -bool YES",
        shell_quote(parent),
        shell_quote(source),
        shell_quote(destination),
        shell_quote(destination),
        shell_quote(destination)
    );
    run_privileged(&command)
}

fn privileged_remove(destination: &Path) -> Result<(), String> {
    run_privileged(&format!("/bin/rm -f {}", shell_quote(destination)))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(target_os = "macos")]
fn apply_coregraphics_mode(
    target: TargetMode,
    logical_width: u32,
    logical_height: u32,
) -> Result<(), String> {
    use std::ffi::c_void;
    use std::ptr;

    type DisplayId = u32;
    type ModeRef = *mut c_void;
    type ArrayRef = *const c_void;

    #[allow(clippy::duplicated_attributes)]
    #[link(name = "CoreGraphics", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CGGetActiveDisplayList(
            max_displays: u32,
            displays: *mut DisplayId,
            display_count: *mut u32,
        ) -> i32;
        fn CGDisplayCopyAllDisplayModes(display: DisplayId, options: *const c_void) -> ArrayRef;
        fn CGDisplayModeGetWidth(mode: ModeRef) -> usize;
        fn CGDisplayModeGetHeight(mode: ModeRef) -> usize;
        fn CGDisplayModeGetPixelWidth(mode: ModeRef) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: ModeRef) -> usize;
        fn CGDisplayModeGetRefreshRate(mode: ModeRef) -> f64;
        fn CGDisplaySetDisplayMode(
            display: DisplayId,
            mode: ModeRef,
            options: *const c_void,
        ) -> i32;
        fn CFArrayGetCount(array: ArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(array: ArrayRef, index: isize) -> *const c_void;
        fn CFRelease(value: *const c_void);
    }

    let mut displays = [0_u32; 16];
    let mut count = 0_u32;
    let result = unsafe {
        CGGetActiveDisplayList(
            displays.len() as u32,
            displays.as_mut_ptr(),
            &mut count as *mut u32,
        )
    };
    if result != 0 || count == 0 {
        return Err(format!("无法枚举活动显示器（CoreGraphics {result}）"));
    }

    let mut best: Option<(u32, ModeRef, f64)> = None;
    let mut available = Vec::new();
    for display in displays.into_iter().take(count as usize) {
        let modes = unsafe { CGDisplayCopyAllDisplayModes(display, ptr::null()) };
        if modes.is_null() {
            continue;
        }
        let mode_count = unsafe { CFArrayGetCount(modes) };
        for index in 0..mode_count {
            let mode = unsafe { CFArrayGetValueAtIndex(modes, index) } as ModeRef;
            if mode.is_null() {
                continue;
            }
            let width = unsafe { CGDisplayModeGetWidth(mode) } as u32;
            let height = unsafe { CGDisplayModeGetHeight(mode) } as u32;
            let pixel_width = unsafe { CGDisplayModeGetPixelWidth(mode) } as u32;
            let pixel_height = unsafe { CGDisplayModeGetPixelHeight(mode) } as u32;
            let refresh = unsafe { CGDisplayModeGetRefreshRate(mode) };
            if pixel_width == target.width && pixel_height == target.height {
                available.push(format!("{width}×{height}@{refresh:.0}"));
                if width == logical_width && height == logical_height {
                    let score = (refresh - target.refresh as f64).abs();
                    if best
                        .as_ref()
                        .map(|(_, _, best_score)| score < *best_score)
                        .unwrap_or(true)
                    {
                        best = Some((display, mode, score));
                    }
                }
            }
        }
        unsafe { CFRelease(modes) };
    }

    let Some((display, mode, _)) = best else {
        available.sort_unstable();
        available.dedup();
        let detail = if available.is_empty() {
            "系统尚未载入这个物理输出模式".to_string()
        } else {
            format!("已载入的桌面档位：{}", available.join("、"))
        };
        return Err(format!(
            "找不到 {}×{} 输出、桌面 {}×{} 的模式。{}\n请先确认已安装原生覆盖；新安装时请重启 Mac。",
            target.width, target.height, logical_width, logical_height, detail
        ));
    };

    let result = unsafe { CGDisplaySetDisplayMode(display, mode, ptr::null()) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!("macOS 拒绝切换显示模式（CoreGraphics {result}）"))
    }
}

#[cfg(not(target_os = "macos"))]
fn apply_coregraphics_mode(
    _target: TargetMode,
    _logical_width: u32,
    _logical_height: u32,
) -> Result<(), String> {
    Err("这个功能只能在 macOS 上运行".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_display() -> DisplayInfo {
        DisplayInfo {
            name: "CS40X".to_string(),
            vendor_hex: "e66".to_string(),
            product_hex: "120".to_string(),
            vendor_id: 0x0e66,
            product_id: 0x0120,
            current_width: 3840,
            current_height: 2160,
            current_refresh: 144,
            better_display_running: false,
        }
    }

    #[test]
    fn encodes_the_known_native_mode_exactly() {
        assert_eq!(base64_encode(&mode_bytes(5120, 2160, None)), "AAAUAAAACHA=");
        assert_eq!(
            base64_encode(&mode_bytes(5120, 2160, Some(120))),
            "AAAUAAAACHAAeP//"
        );
        assert_eq!(
            base64_encode(&hidpi_mode_bytes(3840, 1620)),
            "AAAPAAAABlQAAAAB"
        );
    }

    #[test]
    fn generates_cs40x_hidpi_candidates() {
        let target = TargetMode {
            width: 5120,
            height: 2160,
            refresh: 120,
        };
        let candidates = logical_candidates(target);
        assert!(candidates.contains(&(3840, 1620)));
        assert!(candidates.contains(&(2560, 1080)));
        assert!(candidates.contains(&(1280, 540)));
        assert_eq!(candidates.len(), 9);
    }

    #[test]
    fn parses_system_profiler_json() {
        let json = serde_json::json!({
            "SPDisplaysDataType": [{
                "spdisplays_ndrvs": [{
                    "_name": "CS40X",
                    "_spdisplays_display-product-id": "120",
                    "_spdisplays_display-vendor-id": "e66",
                    "_spdisplays_pixels": "3840 x 2160",
                    "spdisplays_resolution": "3840 x 2160 @ 144.00Hz",
                    "spdisplays_online": "spdisplays_yes",
                    "spdisplays_main": "spdisplays_yes"
                }]
            }]
        });
        let display = parse_system_profiler(&json).unwrap();
        assert_eq!(display.name, "CS40X");
        assert_eq!(display.vendor_id, 0x0e66);
        assert_eq!(display.product_id, 0x0120);
        assert_eq!(display.current_width, 3840);
        assert_eq!(display.current_refresh, 144);
    }

    #[test]
    fn produces_a_macos_display_override() {
        let target = TargetMode {
            width: 5120,
            height: 2160,
            refresh: 120,
        };
        let plist = override_xml(&sample_display(), target);
        assert!(plist.contains("<integer>288</integer>"));
        assert!(plist.contains("<integer>3686</integer>"));
        assert!(plist.contains("<string>CS40X</string>"));
        assert!(plist.contains("AAAUAAAACHAAeP//"));
        assert!(plist.contains("AAAPAAAABlQAAAAB"));
    }

    #[test]
    #[ignore = "requires an active display and briefly selects its current mode"]
    fn coregraphics_mode_selection_smoke_test() {
        let detection = detect().unwrap();
        apply_mode(
            detection.target,
            detection.display.current_width,
            detection.display.current_height,
        )
        .unwrap();
    }
}
