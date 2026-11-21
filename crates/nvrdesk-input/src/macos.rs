//! macOS background-thread global input listener using `CFRunLoop::get_current()`.
//!
//! `rdevin::listen` attaches its `CGEventTap` to `CFRunLoopGetMain()`, which never pumps
//! when the main thread runs a `tokio` runtime. Running `CGEventTap` on the current thread's
//! `CFRunLoop` captures global mouse and keyboard events cleanly.

use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField, KeyCode,
};
use foreign_types::ForeignType;
use nvrdesk_core::MouseButton;
use rdevin::key_from_code;
use tracing::{info, warn};

use crate::{InputTracker, ModifierKind, TrackedEvent, classify_key, fallback_printable};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGEventKeyboardGetUnicodeString(
        event: *mut std::ffi::c_void,
        max_string_length: std::ffi::c_ulong,
        actual_string_length: *mut std::ffi::c_ulong,
        unicode_string: *mut u16,
    );
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Runs a blocking macOS `CGEventTap` loop on the calling thread's `CFRunLoop`.
pub fn listen(tracker: InputTracker) {
    let listen_access = unsafe { CGPreflightListenEventAccess() };
    let ax_trusted = unsafe { AXIsProcessTrusted() };
    if !listen_access && !ax_trusted {
        warn!(
            listen_access,
            ax_trusted,
            "macOS Input Monitoring / Accessibility permission is NOT granted to this terminal/binary — keystroke and mouse click overlays will not receive events until enabled in System Settings -> Privacy & Security"
        );
    } else {
        info!(
            listen_access,
            ax_trusted, "macOS input permissions verified"
        );
    }

    let events = vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];

    let tap_port = std::sync::Arc::new(AtomicPtr::new(std::ptr::null_mut()));
    let tap_port_cb = std::sync::Arc::clone(&tap_port);
    let prev_flags = Cell::new(CGEventFlags::CGEventFlagNull);

    let make_tap = |location: CGEventTapLocation| {
        let tracker = tracker.clone();
        let tap_port_cb = std::sync::Arc::clone(&tap_port_cb);
        let prev_flags = prev_flags.clone();
        CGEventTap::new(
            location,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            events.clone(),
            move |_proxy, etype, event| {
                handle_cg_event(&tracker, &tap_port_cb, &prev_flags, etype, event);
                None
            },
        )
    };

    let tap = match make_tap(CGEventTapLocation::HID)
        .or_else(|_| make_tap(CGEventTapLocation::Session))
    {
        Ok(tap) => tap,
        Err(()) => {
            warn!(
                "failed to create macOS CGEventTap; grant Input Monitoring / Accessibility permission"
            );
            return;
        }
    };

    use core_foundation::base::TCFType;
    tap_port.store(
        tap.mach_port.as_concrete_TypeRef() as *mut _,
        Ordering::Release,
    );

    let Ok(loop_source) = tap.mach_port.create_runloop_source(0) else {
        warn!("failed to create macOS CFRunLoopSource for CGEventTap");
        return;
    };

    let current = CFRunLoop::get_current();
    unsafe {
        current.add_source(&loop_source, kCFRunLoopCommonModes);
    }
    tap.enable();
    info!("macOS CGEventTap input listener active on background runloop");
    CFRunLoop::run_current();
}

fn handle_cg_event(
    tracker: &InputTracker,
    tap_port: &AtomicPtr<std::ffi::c_void>,
    prev_flags: &Cell<CGEventFlags>,
    etype: CGEventType,
    event: &CGEvent,
) {
    let now = Instant::now();
    match etype {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            let ptr = tap_port.load(Ordering::Acquire);
            if !ptr.is_null() {
                unsafe {
                    CGEventTapEnable(ptr as CFMachPortRef, true);
                }
            }
        }
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let loc = event.location();
            tracker.push_event(TrackedEvent::MouseMove { x: loc.x, y: loc.y }, now);
        }
        CGEventType::LeftMouseDown => {
            let loc = event.location();
            tracker.push_event(TrackedEvent::MouseMove { x: loc.x, y: loc.y }, now);
            tracker.push_event(
                TrackedEvent::MouseDown {
                    button: MouseButton::Left,
                },
                now,
            );
        }
        CGEventType::RightMouseDown => {
            let loc = event.location();
            tracker.push_event(TrackedEvent::MouseMove { x: loc.x, y: loc.y }, now);
            tracker.push_event(
                TrackedEvent::MouseDown {
                    button: MouseButton::Right,
                },
                now,
            );
        }
        CGEventType::OtherMouseDown => {
            let loc = event.location();
            tracker.push_event(TrackedEvent::MouseMove { x: loc.x, y: loc.y }, now);
            tracker.push_event(
                TrackedEvent::MouseDown {
                    button: MouseButton::Middle,
                },
                now,
            );
        }
        CGEventType::LeftMouseUp => {
            tracker.push_event(
                TrackedEvent::MouseUp {
                    button: MouseButton::Left,
                },
                now,
            );
        }
        CGEventType::RightMouseUp => {
            tracker.push_event(
                TrackedEvent::MouseUp {
                    button: MouseButton::Right,
                },
                now,
            );
        }
        CGEventType::OtherMouseUp => {
            tracker.push_event(
                TrackedEvent::MouseUp {
                    button: MouseButton::Middle,
                },
                now,
            );
        }
        CGEventType::KeyDown => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let key = key_from_code(keycode);
            let (key_name, is_special, modifier) = classify_key(key);
            let printable = if modifier.is_some() || is_special {
                None
            } else {
                extract_unicode_char(event).or_else(|| fallback_printable(key))
            };
            tracker.push_event(
                TrackedEvent::KeyPress {
                    key_name,
                    printable,
                    is_special,
                    modifier,
                },
                now,
            );
        }
        CGEventType::KeyUp => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let key = key_from_code(keycode);
            let (_, _, modifier) = classify_key(key);
            tracker.push_event(TrackedEvent::KeyRelease { modifier }, now);
        }
        CGEventType::FlagsChanged => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let flags = event.get_flags();
            let old = prev_flags.replace(flags);

            let (flag, mod_kind) = match keycode {
                KeyCode::CONTROL | KeyCode::RIGHT_CONTROL => {
                    (CGEventFlags::CGEventFlagControl, ModifierKind::Ctrl)
                }
                KeyCode::OPTION | KeyCode::RIGHT_OPTION => {
                    (CGEventFlags::CGEventFlagAlternate, ModifierKind::Alt)
                }
                KeyCode::SHIFT | KeyCode::RIGHT_SHIFT => {
                    (CGEventFlags::CGEventFlagShift, ModifierKind::Shift)
                }
                KeyCode::COMMAND | KeyCode::RIGHT_COMMAND => {
                    (CGEventFlags::CGEventFlagCommand, ModifierKind::Meta)
                }
                _ => return,
            };

            let pressed = flags.contains(flag);
            let was_pressed = old.contains(flag);
            if pressed && !was_pressed {
                tracker.push_event(
                    TrackedEvent::KeyPress {
                        key_name: String::new(),
                        printable: None,
                        is_special: true,
                        modifier: Some(mod_kind),
                    },
                    now,
                );
            } else if !pressed && was_pressed {
                tracker.push_event(
                    TrackedEvent::KeyRelease {
                        modifier: Some(mod_kind),
                    },
                    now,
                );
            }
        }
        _ => {}
    }
}

fn extract_unicode_char(event: &CGEvent) -> Option<char> {
    let mut buf = [0u16; 8];
    let mut actual_len: std::ffi::c_ulong = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(
            event.as_ptr() as *mut _,
            buf.len() as std::ffi::c_ulong,
            &mut actual_len,
            buf.as_mut_ptr(),
        );
    }
    let len = (actual_len as usize).min(buf.len());
    if len == 0 {
        return None;
    }
    String::from_utf16(&buf[..len])
        .ok()?
        .chars()
        .next()
        .filter(|c| !c.is_control())
}
