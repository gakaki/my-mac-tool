use std::cell::RefCell;

use nsh_platform_macos::objc2::{
    AnyThread, MainThreadMarker, MainThreadOnly, define_class, rc::Retained, sel,
};
use nsh_platform_macos::objc2_app_kit::{
    NSApplication, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use nsh_platform_macos::objc2_foundation::{
    NSBundle, NSObject, NSObjectProtocol, NSSize, NSString,
};

const APP_TITLE: &str = "我的 Mac 设置和开发工具安装";

thread_local! {
    static STATUS_ITEM: RefCell<Option<(Retained<NSStatusItem>, Retained<MenuBarController>)>> =
        const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MyMacMenuBarController"]
    struct MenuBarController;

    unsafe impl NSObjectProtocol for MenuBarController {}

    impl MenuBarController {
        #[unsafe(method(showMainWindow:))]
        fn show_main_window(&self, _sender: &NSMenuItem) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let app = NSApplication::sharedApplication(mtm);
            let windows = app.windows();
            if let Some(window) = windows
                .iter()
                .find(|window| window.isMainWindow())
                .or_else(|| windows.iter().next())
            {
                if window.isMiniaturized() {
                    window.deminiaturize(None);
                }
                window.makeKeyAndOrderFront(None);
            }
            app.activate();
        }

        #[unsafe(method(quitApplication:))]
        fn quit_application(&self, _sender: &NSMenuItem) {
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }
    }
);

fn menu_bar_image() -> Retained<NSImage> {
    let bundle = NSBundle::mainBundle();
    let name = NSString::from_str("MenuBarIcon");
    let extension = NSString::from_str("png");
    let image = bundle
        .pathForResource_ofType(Some(&name), Some(&extension))
        .and_then(|path| NSImage::initWithContentsOfFile(NSImage::alloc(), &path))
        .or_else(|| {
            NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str("terminal"),
                Some(&NSString::from_str(APP_TITLE)),
            )
        })
        .expect("macOS provides the terminal symbol");
    image.setSize(NSSize::new(18.0, 18.0));
    image.setTemplate(true);
    image
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: nsh_platform_macos::objc2::runtime::Sel,
    target: Option<&nsh_platform_macos::objc2::runtime::AnyObject>,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    unsafe { item.setTarget(target) };
    item
}

pub fn install_status_item() -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| "状态栏必须在主线程创建".to_string())?;
    if STATUS_ITEM.with(|item| item.borrow().is_some()) {
        return Ok(());
    }

    let controller: Retained<MenuBarController> =
        unsafe { nsh_platform_macos::objc2::msg_send![MenuBarController::alloc(mtm), init] };
    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let image = menu_bar_image();
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&image));
        button.setToolTip(Some(&NSString::from_str(APP_TITLE)));
    }

    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str(APP_TITLE));
    menu.addItem(&menu_item(
        mtm,
        "打开主窗口",
        sel!(showMainWindow:),
        Some(&controller),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(
        mtm,
        "退出程序",
        sel!(quitApplication:),
        Some(&controller),
    ));
    status_item.setMenu(Some(&menu));
    STATUS_ITEM.with(|item| *item.borrow_mut() = Some((status_item, controller)));
    Ok(())
}
