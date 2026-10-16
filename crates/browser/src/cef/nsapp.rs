//! Making GPUI's `NSApplication` subclass acceptable to Chromium.
//!
//! Chromium CHECKs that `NSApp` conforms to `CrAppProtocol`,
//! `CrAppControlProtocol` and `CefAppProtocol` and aborts otherwise. NSApp is
//! GPUI's own class (`GPUIApplication`), which knows nothing of them — and
//! the protocols only exist in CEF's headers, not in the Objective-C runtime.
//! So, after GPUI has created NSApp and before `cef_initialize`:
//!
//! - register protocols with those names (conformance is checked by name),
//! - add `isHandlingSendEvent` / `setHandlingSendEvent:`,
//! - wrap `sendEvent:` so the flag is raised while an event is dispatched.
//!
//! Approach from bokuweb/gpui-cef (MIT OR Apache-2.0).

use std::{cell::Cell, ffi::c_void};

use objc2::{
    ffi::{
        class_addMethod, class_addProtocol, class_conformsToProtocol, class_getInstanceMethod,
        method_getImplementation, method_setImplementation, objc_allocateProtocol, objc_getClass,
        objc_getProtocol, objc_registerProtocol, protocol_addMethodDescription, sel_registerName,
    },
    runtime::{AnyClass, AnyObject, Bool, Sel},
};

thread_local! {
    /// Whether `sendEvent:` is running. NSApp only lives on the main thread.
    static HANDLING_SEND_EVENT: Cell<bool> = const { Cell::new(false) };
}

type SendEventImp = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);

thread_local! {
    static ORIGINAL_SEND_EVENT: Cell<Option<SendEventImp>> = const { Cell::new(None) };
}

extern "C-unwind" fn is_handling_send_event(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.with(Cell::get))
}

extern "C-unwind" fn set_handling_send_event(_this: *mut AnyObject, _sel: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.with(|flag| flag.set(handling.as_bool()));
}

extern "C-unwind" fn send_event(this: *mut AnyObject, sel: Sel, event: *mut AnyObject) {
    // Restore the previous value afterwards so nested dispatch stays correct.
    let previous = HANDLING_SEND_EVENT.with(|flag| flag.replace(true));
    if let Some(original) = ORIGINAL_SEND_EVENT.with(Cell::get) {
        unsafe { original(this, sel, event) };
    }
    HANDLING_SEND_EVENT.with(|flag| flag.set(previous));
}

const GPUI_APPLICATION_CLASS: &std::ffi::CStr = c"GPUIApplication";

/// Retrofits the CEF application protocols onto GPUI's NSApp class. Call
/// once, on the main thread, after GPUI created NSApp.
pub(crate) fn adopt_cef_app_protocol() -> Result<(), &'static str> {
    unsafe {
        let class = objc_getClass(GPUI_APPLICATION_CLASS.as_ptr());
        if class.is_null() {
            return Err("GPUIApplication class not found; start Chromium after GPUI created NSApp");
        }
        let class = class as *mut AnyClass;

        let sel = |name: &std::ffi::CStr| -> Result<Sel, &'static str> {
            sel_registerName(name.as_ptr()).ok_or("failed to register a selector")
        };
        // An IMP is an opaque function pointer; the real signatures are
        // restored by the Objective-C runtime's calling convention.
        let as_imp = |f: *const c_void| -> unsafe extern "C-unwind" fn() {
            std::mem::transmute::<*const c_void, unsafe extern "C-unwind" fn()>(f)
        };

        let is_handling = sel(c"isHandlingSendEvent")?;
        let set_handling = sel(c"setHandlingSendEvent:")?;
        // Adding fails if the method already exists (a second runtime start);
        // that is fine, the existing one is ours.
        class_addMethod(class, is_handling, as_imp(is_handling_send_event as *const c_void), c"c@:".as_ptr());
        class_addMethod(class, set_handling, as_imp(set_handling_send_event as *const c_void), c"v@:c".as_ptr());

        if ORIGINAL_SEND_EVENT.with(Cell::get).is_none() {
            let method = class_getInstanceMethod(class, sel(c"sendEvent:")?);
            if method.is_null() {
                return Err("-sendEvent: not found on GPUIApplication");
            }
            let original = method_getImplementation(method)
                .map(|imp| std::mem::transmute::<unsafe extern "C-unwind" fn(), SendEventImp>(imp));
            ORIGINAL_SEND_EVENT.with(|slot| slot.set(original));
            method_setImplementation(method, as_imp(send_event as *const c_void));
        }

        for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
            let mut protocol = objc_getProtocol(name.as_ptr());
            if protocol.is_null() {
                let allocated = objc_allocateProtocol(name.as_ptr());
                if allocated.is_null() {
                    return Err("failed to allocate an Objective-C protocol");
                }
                protocol_addMethodDescription(allocated, is_handling, c"c@:".as_ptr(), Bool::YES, Bool::YES);
                protocol_addMethodDescription(allocated, set_handling, c"v@:c".as_ptr(), Bool::YES, Bool::YES);
                objc_registerProtocol(allocated);
                protocol = allocated;
            }
            class_addProtocol(class, protocol);
            if !class_conformsToProtocol(class, protocol).as_bool() {
                return Err("NSApp does not conform to the protocols Chromium requires");
            }
        }
        Ok(())
    }
}
