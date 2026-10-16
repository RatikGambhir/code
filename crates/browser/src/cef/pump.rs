//! Driving Chromium's message loop from the main run loop.
//!
//! GPUI owns `[NSApp run]`, so Chromium runs with `external_message_pump`
//! and needs `cef_do_message_loop_work()` called regularly on the main
//! thread. That call cannot live on GPUI's executor: when the app is idle the
//! foreground executor barely ticks, and calling into Chromium while GPUI's
//! `App` is borrowed lets GPUI re-enter from the nested run loop and panic.
//! A `CFRunLoopTimer` in the common modes fires straight from the run loop —
//! also during window resizes and menu tracking — with nothing borrowed.
//!
//! One timer serves every tab. After pumping it tells resized tabs and wakes
//! the listeners of tabs with something new to show.
//!
//! Approach from bokuweb/gpui-cef (MIT OR Apache-2.0).

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::{Rc, Weak},
};

use core_foundation::{
    date::CFAbsoluteTimeGetCurrent,
    runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, CFRunLoopTimerRef},
};

use super::shared::TabShared;

/// Seconds between pumps: 60 Hz.
const INTERVAL: f64 = 1.0 / 60.0;

#[derive(Default)]
struct PumpState {
    tabs: RefCell<Vec<Weak<TabShared>>>,
    ticking: Cell<bool>,
}

struct Pump {
    _timer: CFRunLoopTimer,
    state: Rc<PumpState>,
}

thread_local! {
    static PUMP: RefCell<Option<Pump>> = const { RefCell::new(None) };
}

/// Starts the timer if it is not running yet.
pub(crate) fn start() {
    PUMP.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(Pump::start());
        }
    });
}

/// Adds a tab to the pump's rounds. Dead tabs drop out on their own.
pub(crate) fn register(tab: &Rc<TabShared>) {
    start();
    PUMP.with(|slot| {
        if let Some(pump) = slot.borrow().as_ref() {
            pump.state.tabs.borrow_mut().push(Rc::downgrade(tab));
        }
    });
}

impl Pump {
    fn start() -> Self {
        let state = Rc::new(PumpState::default());
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: Rc::as_ptr(&state) as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        let now = unsafe { CFAbsoluteTimeGetCurrent() };
        let timer = CFRunLoopTimer::new(now + INTERVAL, INTERVAL, 0, 0, on_tick, &mut context);
        unsafe { CFRunLoop::get_main().add_timer(&timer, kCFRunLoopCommonModes) };
        Self { _timer: timer, state }
    }
}

extern "C" fn on_tick(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    // The state outlives the timer: both live in the thread-local `PUMP`,
    // which is never cleared.
    let state = unsafe { &*(info as *const PumpState) };
    // Chromium can spin a nested run loop (a modal dialog), which would fire
    // this timer again from inside `do_message_loop_work`.
    if state.ticking.replace(true) {
        return;
    }

    cef::do_message_loop_work();

    let tabs: Vec<Rc<TabShared>> = {
        let mut weak = state.tabs.borrow_mut();
        weak.retain(|tab| tab.strong_count() > 0);
        weak.iter().filter_map(Weak::upgrade).collect()
    };
    for tab in tabs {
        tab.after_pump();
    }

    state.ticking.set(false);
}
