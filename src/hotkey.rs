#[cfg(target_os = "macos")]
use std::{
    ptr::NonNull,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{rc::Retained, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

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

        let is_fn_down = modifier_flags.contains(NSEventModifierFlags::Function);
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
    _monitor: Retained<AnyObject>,
    _monitor_handler: RcBlock<dyn Fn(NonNull<NSEvent>)>,
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

    let monitor_handler: RcBlock<dyn Fn(NonNull<NSEvent>)> =
        RcBlock::new(move |event_ptr: NonNull<NSEvent>| {
        let event = unsafe { event_ptr.as_ref() };
        let mut detector = detector_for_monitor
            .lock()
            .expect("Fn tap detector mutex poisoned");

        if detector.register_event(event.keyCode(), event.modifierFlags()) {
            on_double_press_for_monitor();
        }
    });

    let monitor =
        NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::FlagsChanged, &monitor_handler)
            .ok_or_else(|| "failed to install Fn double-press monitor".to_string())?;

    Ok(HotkeyMonitor {
        _monitor: monitor,
        _monitor_handler: monitor_handler,
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
