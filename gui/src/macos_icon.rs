use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::{MainThreadMarker, NSData, NSSize};

fn image(mtm: MainThreadMarker, png: &[u8]) -> Option<Retained<NSImage>> {
    NSImage::initWithData(mtm.alloc(), &NSData::with_bytes(png))
}

pub(crate) fn install() {
    if let Some(mtm) = MainThreadMarker::new() {
        if let Some(icon) = image(mtm, include_bytes!("../assets/remuda.png")) {
            let app = NSApplication::sharedApplication(mtm);
            // AppKit retains the image and this function runs on the main thread.
            unsafe { app.setApplicationIconImage(Some(&icon)) };
        }
    }
}

pub(crate) fn tray(mtm: MainThreadMarker) -> Option<Retained<NSImage>> {
    let icon = image(mtm, include_bytes!("../assets/remuda-tray.png"))?;
    icon.setSize(NSSize::new(18., 18.));
    icon.setTemplate(true);
    Some(icon)
}
