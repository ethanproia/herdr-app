//! The NSApplication subclass CEF requires on macOS (CefAppProtocol), adapted from cef-rs's
//! cefsimple example. The host has no windows and no Dock icon (accessory policy + LSUIElement).

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObjectProtocol};
use objc2::{ClassType, DefinedClass, MainThreadMarker, define_class, extern_methods, msg_send};
use objc2_app_kit::{NSApp, NSApplication, NSApplicationActivationPolicy, NSEvent};
use std::cell::Cell;

#[derive(Default)]
pub struct HostApplicationIvars {
    handling_send_event: Cell<Bool>,
}

define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = HostApplicationIvars]
    pub struct HostApplication;

    impl HostApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let was = self.is_handling_send_event();
            if !was {
                self.set_handling_send_event(true);
            }
            let _: () = unsafe { msg_send![super(self), sendEvent: event] };
            if !was {
                self.set_handling_send_event(false);
            }
        }
    }

    unsafe impl NSObjectProtocol for HostApplication {}

    unsafe impl CrAppControlProtocol for HostApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn _set_handling_send_event(&self, v: Bool) {
            self.ivars().handling_send_event.set(v);
        }
    }

    unsafe impl CrAppProtocol for HostApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn _is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CefAppProtocol for HostApplication {}
);

impl HostApplication {
    extern_methods! {
        #[unsafe(method(sharedApplication))]
        fn shared_application() -> Retained<Self>;

        #[unsafe(method(setHandlingSendEvent:))]
        fn set_handling_send_event(&self, v: bool);

        #[unsafe(method(isHandlingSendEvent))]
        fn is_handling_send_event(&self) -> bool;
    }
}

/// Make `NSApp` a `HostApplication` before CEF starts, with no Dock icon and no menu bar.
pub fn setup() {
    let app = HostApplication::shared_application();
    let mtm = MainThreadMarker::new().expect("CEF host runs on the main thread");
    assert!(NSApp(mtm).isKindOfClass(HostApplication::class()), "NSApp was created before the host");
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}
