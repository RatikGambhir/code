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
//! The timer follows Chromium's own schedule
//! (`OnScheduleMessagePumpWork`, which may arrive on any thread): work it
//! asks for runs right away or after the delay it names, and otherwise the
//! timer only fires every [`MAX_DELAY`], as in CEF's sample application.
//! A fixed 60 Hz tick would cost CPU while nothing happens and add up to a
//! frame of latency when something does.
//!
//! Approach from bokuweb/gpui-cef (MIT OR Apache-2.0).

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::{Rc, Weak},
    sync::atomic::{AtomicPtr, Ordering},
};

use core_foundation::{
    base::TCFType as _,
    date::CFAbsoluteTimeGetCurrent,
    runloop::{
        kCFRunLoopCommonModes, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, CFRunLoopTimerGetNextFireDate,
        CFRunLoopTimerRef, CFRunLoopTimerSetNextFireDate,
    },
};

use super::shared::TabShared;

/// Longest wait between pumps, in seconds (CEF's sample uses the same).
const MAX_DELAY: f64 = 1.0 / 30.0;

/// The pump's timer, for [`schedule`] calls from Chromium's threads. Set
/// once; the timer lives (in the main thread's `PUMP`) until the process
/// exits.
static TIMER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

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
        // The first round runs straight away: initialization queues work.
        let now = unsafe { CFAbsoluteTimeGetCurrent() };
        let timer = CFRunLoopTimer::new(now, MAX_DELAY, 0, 0, on_tick, &mut context);
        unsafe { CFRunLoop::get_main().add_timer(&timer, kCFRunLoopCommonModes) };
        TIMER.store(timer.as_concrete_TypeRef() as *mut c_void, Ordering::Release);
        Self { _timer: timer, state }
    }
}

/// Chromium asks for `cef_do_message_loop_work` in `delay_ms` (now, if not
/// positive). Called from any thread; moves the timer's next firing earlier
/// when needed. Before the pump starts there is nothing to move: it starts
/// with a round.
pub(crate) fn schedule(delay_ms: i64) {
    let timer = TIMER.load(Ordering::Acquire) as CFRunLoopTimerRef;
    if timer.is_null() || super::SHUT_DOWN.load(Ordering::SeqCst) {
        return;
    }
    let delay = (delay_ms.max(0) as f64 / 1000.0).min(MAX_DELAY);
    // SAFETY: the timer is never released (see `TIMER`), and Core
    // Foundation's timer calls are thread safe.
    unsafe {
        let fire = CFAbsoluteTimeGetCurrent() + delay;
        if fire < CFRunLoopTimerGetNextFireDate(timer) {
            CFRunLoopTimerSetNextFireDate(timer, fire);
        }
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
    if super::SHUT_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
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
