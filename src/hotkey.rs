#![allow(deprecated, unexpected_cfgs)]

#[cfg(target_os = "macos")]
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use block::{Block, ConcreteBlock, RcBlock};
#[cfg(target_os = "macos")]
use cocoa::{
    appkit::{NSEvent, NSEventMask, NSEventModifierFlags},
    base::{id, nil},
};
#[cfg(target_os = "macos")]
use objc::{class, msg_send, sel, sel_impl};

/// A detected Fn key tap. A double tap is reported on the second of two quick
/// taps; the first one is reported as a `Single` just before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnTap {
    Single,
    Double,
}

#[cfg(target_os = "macos")]
const FN_KEYCODE: u16 = 63;
#[cfg(target_os = "macos")]
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

#[cfg(target_os = "macos")]
#[derive(Default)]
struct FnTapDetector {
    last_press: Option<Instant>,
    previous_fn_down: bool,
}

#[cfg(target_os = "macos")]
impl FnTapDetector {
    fn register_event(
        &mut self,
        key_code: u16,
        modifier_flags: NSEventModifierFlags,
    ) -> Option<FnTap> {
        if key_code != FN_KEYCODE {
            return None;
        }

        let is_fn_down = modifier_flags.contains(NSEventModifierFlags::NSFunctionKeyMask);
        let mut tap = None;

        if is_fn_down && !self.previous_fn_down {
            let now = Instant::now();

            if self
                .last_press
                .is_some_and(|last_press| now.duration_since(last_press) <= DOUBLE_TAP_WINDOW)
            {
                self.last_press = None;
                tap = Some(FnTap::Double);
            } else {
                self.last_press = Some(now);
                tap = Some(FnTap::Single);
            }
        }

        self.previous_fn_down = is_fn_down;
        tap
    }
}

#[cfg(target_os = "macos")]
pub struct HotkeyMonitor {
    _monitor: id,
    _monitor_block: RcBlock<(id,), ()>,
    _detector: Arc<Mutex<FnTapDetector>>,
}

#[cfg(target_os = "macos")]
pub fn install_fn_tap_monitor(
    on_tap: impl Fn(FnTap) + Send + Sync + 'static,
) -> Result<HotkeyMonitor, String> {
    let detector = Arc::new(Mutex::new(FnTapDetector::default()));
    let detector_for_monitor = detector.clone();
    let on_tap = Arc::new(on_tap);
    let on_tap_for_monitor = on_tap.clone();

    let monitor_block = ConcreteBlock::new(move |event: id| {
        let key_code = unsafe { event.keyCode() };
        let modifier_flags = unsafe { event.modifierFlags() };
        let mut detector = detector_for_monitor
            .lock()
            .expect("Fn tap detector mutex poisoned");

        if let Some(tap) = detector.register_event(key_code, modifier_flags) {
            on_tap_for_monitor(tap);
        }
    })
    .copy();

    let handler: *const Block<(id,), ()> = &*monitor_block;
    let monitor: id = unsafe {
        msg_send![
            class!(NSEvent),
            addGlobalMonitorForEventsMatchingMask: NSEventMask::NSFlagsChangedMask.bits()
            handler: handler
        ]
    };

    if monitor == nil {
        return Err("failed to install Fn double-press monitor".into());
    }

    Ok(HotkeyMonitor {
        _monitor: monitor,
        _monitor_block: monitor_block,
        _detector: detector,
    })
}

#[cfg(not(target_os = "macos"))]
pub struct HotkeyMonitor;

#[cfg(not(target_os = "macos"))]
pub fn install_fn_tap_monitor(
    _on_tap: impl Fn(FnTap) + Send + Sync + 'static,
) -> Result<HotkeyMonitor, String> {
    Err("Fn hotkey is only supported on macOS".into())
}
