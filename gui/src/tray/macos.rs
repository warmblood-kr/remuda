use objc2::{define_class, msg_send, rc::Retained, runtime::AnyObject, sel, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSStatusBar, NSVariableStatusItemLength,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSObject};
use std::io;

define_class!(
    // SAFETY: NSObject imposes no subclass requirements. AppKit calls this
    // target on the main thread.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct TrayActions;

    impl TrayActions {
        #[unsafe(method(openRemuda:))]
        fn open_remuda(&self, _sender: &AnyObject) {
            super::open_remuda();
        }

        #[unsafe(method(openRemudaBrowser:))]
        fn open_browser(&self, _sender: &AnyObject) {
            super::open_browser();
        }

        #[unsafe(method(quitRemudaTray:))]
        fn quit_tray(&self, _sender: &AnyObject) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl TrayActions {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: NSObject's designated initializer requires no arguments.
        unsafe { msg_send![this, init] }
    }
}

pub(super) fn run() -> io::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("the tray requires the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        return Err(io::Error::other("cannot activate the macOS status menu"));
    }

    let target = TrayActions::new(mtm);
    let menu = NSMenu::new(mtm);
    // SAFETY: Each action is a selector implemented by the retained target
    // above and takes the sender supplied by AppKit.
    unsafe {
        let open = menu.addItemWithTitle_action_keyEquivalent(
            ns_string!("Open Remuda"),
            Some(sel!(openRemuda:)),
            ns_string!(""),
        );
        open.setTarget(Some(&target));
        let browser = menu.addItemWithTitle_action_keyEquivalent(
            ns_string!("Open Browser…"),
            Some(sel!(openRemudaBrowser:)),
            ns_string!(""),
        );
        browser.setTarget(Some(&target));
        let quit = menu.addItemWithTitle_action_keyEquivalent(
            ns_string!("Quit Tray"),
            Some(sel!(quitRemudaTray:)),
            ns_string!(""),
        );
        quit.setTarget(Some(&target));
    }
    let status_bar = NSStatusBar::systemStatusBar();
    let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
    let button = item
        .button(mtm)
        .ok_or_else(|| io::Error::other("cannot create the macOS status button"))?;
    if let Some(icon) = crate::macos_icon::tray(mtm) {
        button.setImage(Some(&icon));
    } else {
        button.setTitle(ns_string!("Remuda"));
    }
    item.setMenu(Some(&menu));
    app.run();
    Ok(())
}
