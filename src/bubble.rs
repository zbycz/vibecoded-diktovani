//! A small always-on-top popup ("bubble") that hangs from the menu-bar icon and
//! tells the user what is happening (recording / transcribing), how to proceed,
//! and offers a big "Zrušit" (Cancel) button.
//!
//! Implemented as a non-activating `NSPanel` so it never steals key focus from
//! the app the user is typing into — that would break paste-at-cursor.
#![allow(unexpected_cfgs, deprecated, non_upper_case_globals)]

#[derive(Clone, PartialEq)]
pub enum BubbleState {
    /// Downloading the Whisper model on startup. `fraction` is the completed
    /// ratio (0.0..=1.0) when the total size is known, else `None` for an
    /// indeterminate bar. `detail` is the second-line text (percent + ETA).
    DownloadingModel {
        fraction: Option<f64>,
        detail: String,
    },
    Recording,
    /// Transcription in progress. `submit` is true once the user has armed
    /// "paste + Enter" mode, which changes the bubble's icon/title/hint.
    Transcribing { submit: bool },
}

/// Screen rect of the menu-bar icon: (x, y, width, height) in AppKit screen
/// coordinates (origin bottom-left).
pub type AnchorRect = (f64, f64, f64, f64);

#[cfg(target_os = "macos")]
pub use macos::Bubble;

#[cfg(not(target_os = "macos"))]
pub struct Bubble;

#[cfg(not(target_os = "macos"))]
impl Bubble {
    pub fn new(_on_cancel: Box<dyn Fn()>) -> Bubble {
        Bubble
    }
    pub fn show(&self, _state: BubbleState, _anchor: AnchorRect) {}
    pub fn update(&self, _state: BubbleState) {}
    pub fn hide(&self) {}
}

#[cfg(target_os = "macos")]
mod macos {
    #![allow(unexpected_cfgs, deprecated, unsafe_op_in_unsafe_fn)]
    use super::{AnchorRect, BubbleState};
    use cocoa::base::{NO, YES, id, nil};
    use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
    use objc::declare::ClassDecl;
    use objc::runtime::{Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};
    use std::os::raw::c_void;
    use std::sync::Once;

    const W: f64 = 300.0;
    const CH: f64 = 112.0; // height of the rounded card
    const H: f64 = 124.0; // window height (card + tail room)
    const TAIL: f64 = 14.0; // diagonal of the rotated pointer square

    pub struct Bubble {
        panel: id,
        card: id,
        tail: id,
        icon_view: id,
        title_label: id,
        info_label: id,
        progress_bar: id,
        button: id,
        _cancel_target: id,
    }

    // Info-label frame for the wrapping hint text used by most states.
    const INFO_FRAME: NSRect = NSRect {
        origin: NSPoint { x: 16.0, y: 46.0 },
        size: NSSize {
            width: W - 32.0,
            height: 34.0,
        },
    };
    // Tighter single-line info frame used while downloading, leaving room below
    // it for the progress bar.
    const DOWNLOAD_INFO_FRAME: NSRect = NSRect {
        origin: NSPoint { x: 16.0, y: 62.0 },
        size: NSSize {
            width: W - 32.0,
            height: 18.0,
        },
    };
    const PROGRESS_FRAME: NSRect = NSRect {
        origin: NSPoint { x: 16.0, y: 48.0 },
        size: NSSize {
            width: W - 32.0,
            height: 12.0,
        },
    };

    impl Bubble {
        pub fn new(on_cancel: Box<dyn Fn()>) -> Bubble {
            unsafe {
                let style: u64 = NS_NONACTIVATING_PANEL_MASK; // borderless (0) | nonactivating
                let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, H));
                let panel: id = msg_send![class!(NSPanel), alloc];
                let panel: id = msg_send![panel,
                    initWithContentRect: rect
                    styleMask: style
                    backing: 2u64 // NSBackingStoreBuffered
                    defer: NO];

                let _: () = msg_send![panel, setLevel: 25isize]; // NSStatusWindowLevel
                let _: () = msg_send![panel, setOpaque: NO];
                let clear: id = msg_send![class!(NSColor), clearColor];
                let _: () = msg_send![panel, setBackgroundColor: clear];
                let _: () = msg_send![panel, setHasShadow: YES];
                let _: () = msg_send![panel, setHidesOnDeactivate: NO];
                let _: () = msg_send![panel, setFloatingPanel: YES];
                let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: YES];
                // canJoinAllSpaces | fullScreenAuxiliary
                let _: () = msg_send![panel, setCollectionBehavior: (1u64 | (1u64 << 8))];

                let content: id = msg_send![panel, contentView];

                // Pointer tail: a small square rotated 45° peeking above the card.
                let tail_frame = NSRect::new(
                    NSPoint::new(W / 2.0 - TAIL / 2.0, CH - TAIL / 2.0),
                    NSSize::new(TAIL, TAIL),
                );
                let tail = make_filled_box(tail_frame, 0.0);
                let _: () = msg_send![tail, setFrameCenterRotation: 45.0f64];
                let _: () = msg_send![content, addSubview: tail];

                // Rounded card on top, covering the lower half of the tail.
                let card_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, CH));
                let card = make_filled_box(card_frame, 12.0);
                let _: () = msg_send![content, addSubview: card];
                let card_content: id = msg_send![card, contentView];

                // Icon (system symbol), title, info, cancel button.
                let icon_view: id = msg_send![class!(NSImageView), alloc];
                let icon_view: id = msg_send![icon_view,
                    initWithFrame: NSRect::new(NSPoint::new(14.0, CH - 33.0), NSSize::new(26.0, 26.0))];
                let _: () = msg_send![card_content, addSubview: icon_view];

                let title_label = make_label(
                    NSRect::new(NSPoint::new(48.0, CH - 36.0), NSSize::new(W - 64.0, 26.0)),
                    15.0,
                    true,
                    false,
                );
                let _: () = msg_send![card_content, addSubview: title_label];

                let info_label = make_label(INFO_FRAME, 12.0, false, true);
                let _: () = msg_send![card_content, addSubview: info_label];

                // Determinate horizontal progress bar, shown only while the model
                // is downloading.
                let progress_bar: id = msg_send![class!(NSProgressIndicator), alloc];
                let progress_bar: id = msg_send![progress_bar, initWithFrame: PROGRESS_FRAME];
                let _: () = msg_send![progress_bar, setStyle: 0u64]; // NSProgressIndicatorStyleBar
                let _: () = msg_send![progress_bar, setIndeterminate: NO];
                let _: () = msg_send![progress_bar, setMinValue: 0.0f64];
                let _: () = msg_send![progress_bar, setMaxValue: 1.0f64];
                let _: () = msg_send![progress_bar, setHidden: YES];
                let _: () = msg_send![card_content, addSubview: progress_bar];

                // Action button wired to a small Objective-C target object. Its
                // title and color change per state ("Zrušit" vs "Skrýt dialog").
                let target = make_cancel_target(on_cancel);
                let button: id = msg_send![class!(NSButton), alloc];
                let button: id = msg_send![button,
                    initWithFrame: NSRect::new(NSPoint::new(16.0, 10.0), NSSize::new(W - 32.0, 30.0))];
                let _: () = msg_send![button, setBezelStyle: 1u64]; // rounded
                let _: () = msg_send![button, setButtonType: 7u64]; // momentary push-in
                let _: () = msg_send![button, setTarget: target];
                let _: () = msg_send![button, setAction: sel!(onCancel:)];
                let _: () = msg_send![card_content, addSubview: button];

                Bubble {
                    panel,
                    card,
                    tail,
                    icon_view,
                    title_label,
                    info_label,
                    progress_bar,
                    button,
                    _cancel_target: target,
                }
            }
        }

        pub fn show(&self, state: BubbleState, anchor: AnchorRect) {
            let (ix, iy, iw, _ih) = anchor;
            let icon_center_x = ix + iw / 2.0;
            let mut x = icon_center_x - W / 2.0;
            let y = iy - H; // hang below the icon, tail tip touching it

            unsafe {
                // Keep the bubble on the icon's screen.
                let screen: id = msg_send![class!(NSScreen), mainScreen];
                if screen != nil {
                    let sframe: NSRect = msg_send![screen, frame];
                    let min_x = sframe.origin.x + 8.0;
                    let max_x = sframe.origin.x + sframe.size.width - W - 8.0;
                    if x > max_x {
                        x = max_x;
                    }
                    if x < min_x {
                        x = min_x;
                    }
                }

                // Re-aim the tail at the icon after any horizontal clamping.
                let tail_x = (icon_center_x - x).clamp(16.0, W - 16.0);
                let _: () = msg_send![self.tail, setFrameCenterRotation: 0.0f64];
                let tail_frame = NSRect::new(
                    NSPoint::new(tail_x - TAIL / 2.0, CH - TAIL / 2.0),
                    NSSize::new(TAIL, TAIL),
                );
                let _: () = msg_send![self.tail, setFrame: tail_frame];
                let _: () = msg_send![self.tail, setFrameCenterRotation: 45.0f64];

                let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(W, H));
                let _: () = msg_send![self.panel, setFrame: frame display: YES];
                let _: () = msg_send![self.panel, orderFrontRegardless];
            }

            self.update(state);
        }

        pub fn update(&self, state: BubbleState) {
            let downloading = matches!(state, BubbleState::DownloadingModel { .. });
            let (symbol, title, info): (&str, &str, &str) = match &state {
                BubbleState::DownloadingModel { detail, .. } => {
                    ("arrow.down.circle.fill", "Stahuji model…", detail.as_str())
                }
                BubbleState::Recording => (
                    "checkmark.circle.fill",
                    "Nahrávám…",
                    "Klikni na háček ✓ v liště nebo stiskni Fn pro spuštění přepisu.",
                ),
                BubbleState::Transcribing { submit: false } => (
                    "waveform",
                    "Přepisuji…",
                    "Klávesou Fn (nebo klikem na lištu) zapneš odeslání Enterem.",
                ),
                BubbleState::Transcribing { submit: true } => (
                    "play.fill",
                    "Po dokončení rovnou odešlu",
                    "Klikni znovu na lištu nebo stiskni Fn pro zrušení odeslání.",
                ),
            };
            unsafe {
                let sym = NSString::alloc(nil).init_str(symbol);
                let img: id =
                    msg_send![class!(NSImage), imageWithSystemSymbolName: sym accessibilityDescription: nil];
                let _: () = msg_send![self.icon_view, setImage: img];
                let t = NSString::alloc(nil).init_str(title);
                let _: () = msg_send![self.title_label, setStringValue: t];
                let i = NSString::alloc(nil).init_str(info);
                let _: () = msg_send![self.info_label, setStringValue: i];
                let _: () = msg_send![self.info_label,
                    setFrame: if downloading { DOWNLOAD_INFO_FRAME } else { INFO_FRAME }];

                // Progress bar: visible (and indeterminate when total unknown)
                // only while downloading.
                if let BubbleState::DownloadingModel { fraction, .. } = &state {
                    let _: () = msg_send![self.progress_bar, setHidden: NO];
                    match fraction {
                        Some(value) => {
                            let _: () = msg_send![self.progress_bar, stopAnimation: nil];
                            let _: () = msg_send![self.progress_bar, setIndeterminate: NO];
                            let _: () = msg_send![self.progress_bar, setDoubleValue: *value];
                        }
                        None => {
                            let _: () = msg_send![self.progress_bar, setIndeterminate: YES];
                            let _: () = msg_send![self.progress_bar, startAnimation: nil];
                        }
                    }
                } else {
                    let _: () = msg_send![self.progress_bar, stopAnimation: nil];
                    let _: () = msg_send![self.progress_bar, setHidden: YES];
                }

                // Button: a neutral "Skrýt dialog" while downloading (the download
                // keeps running in the background), a red "Zrušit" otherwise.
                let (btitle, bezel): (&str, id) = if downloading {
                    ("Skrýt dialog", nil)
                } else {
                    ("Zrušit", msg_send![class!(NSColor), systemRedColor])
                };
                let bt = NSString::alloc(nil).init_str(btitle);
                let _: () = msg_send![self.button, setTitle: bt];
                let _: () = msg_send![self.button, setBezelColor: bezel];

                // Repaint the card in case appearance changed.
                let _: () = msg_send![self.card, setNeedsDisplay: YES];
            }
        }

        pub fn hide(&self) {
            unsafe {
                let _: () = msg_send![self.panel, orderOut: nil];
            }
        }
    }

    // Borderless (0) | NSWindowStyleMaskNonactivatingPanel (1 << 7)
    const NS_NONACTIVATING_PANEL_MASK: u64 = 1 << 7;

    /// Build an `NSBox` that draws a filled, optionally rounded rectangle with no
    /// border or title — used for both the card and the pointer tail.
    unsafe fn make_filled_box(frame: NSRect, corner_radius: f64) -> id {
        let b: id = msg_send![class!(NSBox), alloc];
        let b: id = msg_send![b, initWithFrame: frame];
        let _: () = msg_send![b, setBoxType: 4u64]; // NSBoxCustom
        let _: () = msg_send![b, setTitlePosition: 0u64]; // NSNoTitle
        let _: () = msg_send![b, setBorderType: 0u64]; // NSNoBorder
        let _: () = msg_send![b, setBorderWidth: 0.0f64];
        let _: () = msg_send![b, setCornerRadius: corner_radius];
        let fill: id = msg_send![class!(NSColor), windowBackgroundColor];
        let _: () = msg_send![b, setFillColor: fill];
        let _: () = msg_send![b, setContentViewMargins: NSSize::new(0.0, 0.0)];
        b
    }

    unsafe fn make_label(frame: NSRect, size: f64, bold: bool, secondary: bool) -> id {
        let label: id = msg_send![class!(NSTextField), alloc];
        let label: id = msg_send![label, initWithFrame: frame];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let font: id = if bold {
            msg_send![class!(NSFont), boldSystemFontOfSize: size]
        } else {
            msg_send![class!(NSFont), systemFontOfSize: size]
        };
        let _: () = msg_send![label, setFont: font];
        let color: id = if secondary {
            msg_send![class!(NSColor), secondaryLabelColor]
        } else {
            msg_send![class!(NSColor), labelColor]
        };
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setLineBreakMode: 0u64]; // wrap by word
        let _: () = msg_send![label, setMaximumNumberOfLines: 0isize];
        label
    }

    extern "C" fn on_cancel(this: &Object, _cmd: Sel, _sender: id) {
        unsafe {
            let ptr: *mut c_void = *this.get_ivar("cb");
            if !ptr.is_null() {
                let cb = &*(ptr as *const Box<dyn Fn()>);
                cb();
            }
        }
    }

    fn cancel_target_class() -> &'static objc::runtime::Class {
        static mut CLASS: *const objc::runtime::Class = std::ptr::null();
        static INIT: Once = Once::new();
        unsafe {
            INIT.call_once(|| {
                let superclass = class!(NSObject);
                let mut decl = ClassDecl::new("DiktovaniCancelTarget", superclass)
                    .expect("register cancel target class");
                decl.add_ivar::<*mut c_void>("cb");
                decl.add_method(
                    sel!(onCancel:),
                    on_cancel as extern "C" fn(&Object, Sel, id),
                );
                CLASS = decl.register();
            });
            &*CLASS
        }
    }

    unsafe fn make_cancel_target(on_cancel: Box<dyn Fn()>) -> id {
        let target: id = msg_send![cancel_target_class(), new];
        // Double-box so the ivar holds a thin pointer; leaked for app lifetime.
        let boxed: Box<Box<dyn Fn()>> = Box::new(on_cancel);
        let ptr = Box::into_raw(boxed) as *mut c_void;
        let obj: &mut Object = &mut *target;
        obj.set_ivar::<*mut c_void>("cb", ptr);
        target
    }
}
