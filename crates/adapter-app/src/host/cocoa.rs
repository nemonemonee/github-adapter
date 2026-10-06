//! Minimal AppKit status item. All Cocoa objects and events stay on the main thread.
use std::cell::RefCell;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use adapter_protocol::Result;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSSize, NSString};

use super::{Activation, HostHandle, State, TrayAction, failure};

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

thread_local! { static HOST: RefCell<Option<HostHandle>> = const { RefCell::new(None) }; }

pub(crate) fn alert(title: &str, message: &str) {
    autoreleasepool(|_| unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: 1isize];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let alert: Retained<AnyObject> = msg_send![class!(NSAlert), new];
        let _: () = msg_send![&alert, setMessageText: &*NSString::from_str(title)];
        let _: () = msg_send![&alert, setInformativeText: &*NSString::from_str(message)];
        let _: *mut AnyObject = msg_send![&alert, addButtonWithTitle: &*NSString::from_str("OK")];
        let _: isize = msg_send![&alert, runModal];
    });
}

extern "C-unwind" fn open(_: &AnyObject, _: Sel, _: *mut AnyObject) {
    HOST.with(|slot| {
        if let Some(host) = &*slot.borrow() {
            let response = host.route_tray(TrayAction::Open);
            if let Some(error) = response.error {
                alert("Codex needs attention", &error.message);
            }
        }
    });
}
extern "C-unwind" fn quit(_: &AnyObject, _: Sel, _: *mut AnyObject) {
    HOST.with(|slot| {
        if let Some(host) = &*slot.borrow() {
            host.route_tray(TrayAction::Quit);
        }
    });
}
extern "C-unwind" fn status(_: &AnyObject, _: Sel, _: *mut AnyObject) {
    HOST.with(|slot| {
        if let Some(host) = &*slot.borrow() {
            let snapshot = host.snapshot();
            let mut message = format!(
                "Backend: {:?}\nCodex activation: {:?}",
                snapshot.state, snapshot.activation
            );
            for diagnostic in snapshot.diagnostics {
                message.push_str(&format!("\n{}: {}", diagnostic.code, diagnostic.message));
            }
            alert("GitHub Adapter", &message);
        }
    });
}
fn target_class() -> &'static AnyClass {
    static TARGET: OnceLock<&'static AnyClass> = OnceLock::new();
    TARGET.get_or_init(|| {
        let mut builder = ClassBuilder::new(c"GitHubAdapterMenuTarget", class!(NSObject))
            .expect("unique native menu target class");
        // AppKit invokes target/action methods as -(void)action:(id)sender.
        unsafe {
            builder.add_method(sel!(openCodex:), open as extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(showStatus:), status as extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(quitAdapter:), quit as extern "C-unwind" fn(_, _, _));
        }
        builder.register()
    })
}

#[derive(Clone, Default)]
pub(super) struct Notifier {
    finished: Arc<AtomicBool>,
    changed: Arc<AtomicBool>,
}
impl Notifier {
    pub(super) fn wake(&self) -> Arc<dyn Fn() + Send + Sync> {
        let changed = self.changed.clone();
        Arc::new(move || {
            changed.store(true, Ordering::Release);
        })
    }
    pub(super) fn finish(&self) {
        self.finished.store(true, Ordering::Release);
    }
}

pub(super) struct Tray {
    app: *mut AnyObject,
    bar: *mut AnyObject,
    item: Retained<AnyObject>,
    status: Retained<AnyObject>,
    _target: Retained<AnyObject>,
    host: HostHandle,
    notifier: Notifier,
}
impl Tray {
    pub(super) fn new(host: HostHandle) -> Result<Self> {
        if unsafe { libc::pthread_main_np() } != 1 {
            return Err(failure(
                "tray_unavailable",
                "The menu-bar application must start on the macOS main thread.",
            ));
        }
        autoreleasepool(|_| unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return Err(failure(
                    "tray_unavailable",
                    "AppKit could not initialize the menu-bar application.",
                ));
            }
            let _: bool = msg_send![app, setActivationPolicy: 1isize]; // accessory: menu bar, no Dock tile
            let _: () = msg_send![app, finishLaunching];
            let bar: *mut AnyObject = msg_send![class!(NSStatusBar), systemStatusBar];
            let item: *mut AnyObject = msg_send![bar, statusItemWithLength: -1.0f64];
            let item = Retained::retain(item).ok_or_else(|| {
                failure(
                    "tray_unavailable",
                    "Could not create a visible menu-bar item.",
                )
            })?;
            let button: *mut AnyObject = msg_send![&item, button];
            let _: () = msg_send![button, setTitle: &*NSString::from_str("GA")];
            let _: () = msg_send![button, setToolTip: &*NSString::from_str("GitHub Adapter")];
            if let Ok(executable) = std::env::current_exe()
                && let Some(contents) = executable.parent().and_then(std::path::Path::parent)
            {
                let path = contents.join("Resources/github-adapter-tray.png");
                if path.is_file() {
                    let image: Option<Retained<AnyObject>> = msg_send![
                        msg_send![class!(NSImage), alloc], initWithContentsOfFile: &*NSString::from_str(&path.to_string_lossy())];
                    if let Some(image) = image {
                        let _: () = msg_send![&image, setSize: NSSize::new(18.0, 18.0)];
                        let _: () = msg_send![&image, setTemplate: true];
                        let _: () = msg_send![button, setImage: &*image];
                        let _: () = msg_send![button, setTitle: &*NSString::from_str("")];
                    }
                }
            }
            let target: Retained<AnyObject> = msg_send![target_class(), new];
            let menu: Retained<AnyObject> = msg_send![class!(NSMenu), new];
            let _: () = msg_send![&menu, setAutoenablesItems: false];
            let make_item = |title: &str, action: Sel| -> Retained<AnyObject> {
                let entry: Retained<AnyObject> = msg_send![msg_send![class!(NSMenuItem), alloc],
                    initWithTitle: &*NSString::from_str(title), action: action, keyEquivalent: &*NSString::from_str("")];
                let _: () = msg_send![&entry, setTarget: &*target];
                let _: () = msg_send![&menu, addItem: &*entry];
                entry
            };
            let status = make_item("Status: starting", sel!(showStatus:));
            let _ = make_item("Open Codex", sel!(openCodex:));
            let separator: *mut AnyObject = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![&menu, addItem: separator];
            let _ = make_item("Quit — restore normal Codex", sel!(quitAdapter:));
            let _: () = msg_send![&item, setMenu: &*menu];
            HOST.with(|slot| *slot.borrow_mut() = Some(host.clone()));
            Ok(Self {
                app,
                bar,
                item,
                status,
                _target: target,
                host,
                notifier: Notifier::default(),
            })
        })
    }
    pub(super) fn notifier(&self) -> Notifier {
        self.notifier.clone()
    }
    pub(super) fn run(self) {
        let mut backend_failure_alerted = false;
        let mut activation_failure_alerted = None;
        while !self.notifier.finished.load(Ordering::Acquire) {
            autoreleasepool(|_| unsafe {
                if self.notifier.changed.swap(false, Ordering::AcqRel) {
                    let snapshot = self.host.snapshot();
                    let title = format!("Status: {:?}", snapshot.state);
                    let _: () = msg_send![&self.status, setTitle: &*NSString::from_str(&title)];
                    if snapshot.state == State::Failed && !backend_failure_alerted {
                        backend_failure_alerted = true;
                        alert(
                            "GitHub Adapter — startup needs attention",
                            "Startup did not complete. Open Status in the GitHub Adapter menu for diagnostics. Quit — restore normal Codex stops the adapter and restores its temporary routing.",
                        );
                    } else if matches!(
                        snapshot.activation,
                        Activation::Failed | Activation::Uncertain
                    ) && activation_failure_alerted != Some(snapshot.activation_attempt)
                    {
                        activation_failure_alerted = Some(snapshot.activation_attempt);
                        alert(
                            "Codex activation needs attention",
                            "Codex activation was not acknowledged. Open Status in the GitHub Adapter menu for diagnostics, then inspect Codex before trying again. Quit — restore normal Codex restores its temporary routing.",
                        );
                    }
                }
                let until: *mut AnyObject =
                    msg_send![class!(NSDate), dateWithTimeIntervalSinceNow: 0.1f64];
                let event: *mut AnyObject = msg_send![self.app,
                    nextEventMatchingMask: usize::MAX, untilDate: until,
                    inMode: &*NSString::from_str("kCFRunLoopDefaultMode"), dequeue: true];
                if !event.is_null() {
                    let _: () = msg_send![self.app, sendEvent: event];
                }
                let _: () = msg_send![self.app, updateWindows];
            });
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        HOST.with(|slot| *slot.borrow_mut() = None);
        unsafe {
            let _: () = msg_send![self.bar, removeStatusItem: &*self.item];
        }
    }
}
