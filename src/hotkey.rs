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
    fn register_event(&mut self, key_code: u16, modifier_flags: NSEventModifierFlags) -> bool {
        if key_code != FN_KEYCODE {
            return false;
        }

        let is_fn_down = modifier_flags.contains(NSEventModifierFlags::NSFunctionKeyMask);
        let mut detected = false;

        if is_fn_down && !self.previous_fn_down {
            let now = Instant::now();

            if self
                .last_press
                .is_some_and(|last_press| now.duration_since(last_press) <= DOUBLE_TAP_WINDOW)
            {
                self.last_press = None;
                detected = true;
            } else {
                self.last_press = Some(now);
            }
        }

        self.previous_fn_down = is_fn_down;
        detected
    }
}

#[cfg(target_os = "macos")]
pub struct HotkeyMonitor {
    _monitor: id,
    _monitor_block: RcBlock<(id,), ()>,
    _detector: Arc<Mutex<FnTapDetector>>,
}

#[cfg(target_os = "macos")]
pub fn install_double_fn_monitor(
    on_double_press: impl Fn() + Send + Sync + 'static,
) -> Result<HotkeyMonitor, String> {
    let detector = Arc::new(Mutex::new(FnTapDetector::default()));
    let detector_for_monitor = detector.clone();
    let on_double_press = Arc::new(on_double_press);
    let on_double_press_for_monitor = on_double_press.clone();

    let monitor_block = ConcreteBlock::new(move |event: id| {
        let key_code = unsafe { event.keyCode() };
        let modifier_flags = unsafe { event.modifierFlags() };
        let mut detector = detector_for_monitor
            .lock()
            .expect("Fn tap detector mutex poisoned");

        if detector.register_event(key_code, modifier_flags) {
            on_double_press_for_monitor();
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
pub fn install_double_fn_monitor(
    _on_double_press: impl Fn() + Send + Sync + 'static,
) -> Result<HotkeyMonitor, String> {
    Err("double Fn hotkey is only supported on macOS".into())
}
