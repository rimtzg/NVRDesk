//! Global keyboard & mouse input capture and POS privacy-aware overlay state tracker.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nvrdesk_core::{
    ClickRipple, InputOverlayConfig, InputOverlaySnapshot, KeystrokeMode, MonitorBounds,
    MouseButton,
};
use rdevin::{Button, Event as RdevEvent, EventType as RdevEventType, Key};
use tracing::{info, warn};

const MAX_KEYSTROKE_CHARS: usize = 48;
const PRIVACY_CHECK_INTERVAL: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKind {
    Ctrl,
    Alt,
    Shift,
    Meta,
}

/// Platform-independent input event fed into `InputTracker`.
#[derive(Debug, Clone)]
pub enum TrackedEvent {
    MouseMove {
        x: f64,
        y: f64,
    },
    MouseDown {
        button: MouseButton,
    },
    MouseUp {
        button: MouseButton,
    },
    KeyPress {
        key_name: String,
        printable: Option<char>,
        is_special: bool,
        modifier: Option<ModifierKind>,
    },
    KeyRelease {
        modifier: Option<ModifierKind>,
    },
}

#[derive(Debug, Clone)]
struct ClickEntry {
    global_x: f64,
    global_y: f64,
    button: MouseButton,
    at: Instant,
}

#[derive(Debug, Clone)]
struct KeyToken {
    text: String,
    is_digit: bool,
    at: Instant,
}

#[derive(Debug, Default)]
struct Modifiers {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
}

struct TrackerState {
    config: InputOverlayConfig,
    cursor_pos: Option<(f64, f64)>,
    clicks: VecDeque<ClickEntry>,
    tokens: VecDeque<KeyToken>,
    modifiers: Modifiers,
    privacy_paused: bool,
    last_privacy_check: Instant,
}

/// Thread-safe tracker aggregating mouse position, click ripples, and keystroke OSD tokens.
#[derive(Clone)]
pub struct InputTracker {
    state: Arc<Mutex<TrackerState>>,
    window_title_fn: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
}

impl InputTracker {
    /// Creates a new tracker with optional focused-window title provider for privacy exclusions.
    #[must_use]
    pub fn new(
        config: InputOverlayConfig,
        window_title_fn: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(TrackerState {
                config,
                cursor_pos: None,
                clicks: VecDeque::new(),
                tokens: VecDeque::new(),
                modifiers: Modifiers::default(),
                privacy_paused: false,
                last_privacy_check: Instant::now() - PRIVACY_CHECK_INTERVAL,
            })),
            window_title_fn,
        }
    }

    /// Starts the OS-wide `rdevin` listener on a dedicated background thread.
    ///
    /// If the host OS denies accessibility / X11 hooks, logs a warning and degrades gracefully
    /// without interrupting video streaming.
    pub fn start_os_listener(&self) {
        let tracker = self.clone();
        if let Err(err) = thread::Builder::new()
            .name("nvrdesk-input-hook".into())
            .spawn(move || {
                info!("global keyboard/mouse input listener starting");
                #[cfg(target_os = "macos")]
                info!("macOS hint: grant Accessibility / Input Monitoring permission for global keystroke & click overlays");

                if let Err(err) = rdevin::listen(move |ev| {
                    if let Some(tracked) = convert_rdev_event(&ev) {
                        tracker.push_event(tracked, Instant::now());
                    }
                }) {
                    warn!(error = ?err, "global input listener exited; click/keystroke overlays disabled");
                }
            })
        {
            warn!(error = %err, "failed to spawn input listener thread");
        }
    }

    /// Feeds a `TrackedEvent` into the tracker (also used directly in deterministic unit tests).
    pub fn push_event(&self, event: TrackedEvent, now: Instant) {
        let mut st = self.state.lock().expect("input tracker lock");
        if !st.config.enabled {
            return;
        }

        // Refresh window privacy status periodically without blocking every mouse move.
        if !st.config.excluded_windows.is_empty()
            && !matches!(event, TrackedEvent::MouseMove { .. })
            && now.saturating_duration_since(st.last_privacy_check) >= PRIVACY_CHECK_INTERVAL
        {
            st.last_privacy_check = now;
            let title = self
                .window_title_fn
                .as_ref()
                .and_then(|f| f())
                .unwrap_or_default()
                .to_lowercase();
            let excluded = st
                .config
                .excluded_windows
                .iter()
                .any(|pat| !pat.is_empty() && title.contains(&pat.to_lowercase()));
            if excluded && !st.privacy_paused {
                st.tokens.clear();
            }
            st.privacy_paused = excluded;
        }

        match event {
            TrackedEvent::MouseMove { x, y } => {
                st.cursor_pos = Some((x, y));
            }
            TrackedEvent::MouseDown { button } => {
                if st.config.show_clicks
                    && let Some((gx, gy)) = st.cursor_pos
                {
                    st.clicks.push_back(ClickEntry {
                        global_x: gx,
                        global_y: gy,
                        button,
                        at: now,
                    });
                    while st.clicks.len() > 16 {
                        st.clicks.pop_front();
                    }
                }
            }
            TrackedEvent::MouseUp { .. } => {}
            TrackedEvent::KeyRelease { modifier } => {
                if let Some(m) = modifier {
                    set_modifier(&mut st.modifiers, m, false);
                }
            }
            TrackedEvent::KeyPress {
                key_name,
                printable,
                is_special,
                modifier,
            } => {
                if let Some(m) = modifier {
                    set_modifier(&mut st.modifiers, m, true);
                    return;
                }

                if !st.config.show_keystrokes
                    || st.config.keystroke_mode == KeystrokeMode::Off
                    || st.privacy_paused
                {
                    return;
                }

                let has_shortcut_mod = st.modifiers.ctrl || st.modifiers.alt || st.modifiers.meta;

                if st.config.keystroke_mode == KeystrokeMode::ShortcutsAndSpecial
                    && !has_shortcut_mod
                    && !is_special
                {
                    return;
                }

                let (text, is_digit) = if has_shortcut_mod {
                    (format_shortcut(&st.modifiers, &key_name), false)
                } else if is_special {
                    (format!("[{key_name}]"), false)
                } else if let Some(ch) = printable {
                    (ch.to_string(), ch.is_ascii_digit())
                } else {
                    (format!("[{key_name}]"), false)
                };

                st.tokens.push_back(KeyToken {
                    text,
                    is_digit,
                    at: now,
                });
                while st.tokens.len() > 64 {
                    st.tokens.pop_front();
                }
            }
        }
    }

    /// Produces a snapshot of active cursor, click ripples, and keystroke OSD normalized to `bounds`.
    #[must_use]
    pub fn snapshot(&self, bounds: Option<MonitorBounds>, now: Instant) -> InputOverlaySnapshot {
        let mut st = self.state.lock().expect("input tracker lock");
        if !st.config.enabled {
            return InputOverlaySnapshot::default();
        }

        let click_ttl = Duration::from_millis(st.config.click_ttl_ms.max(50));
        let key_ttl = Duration::from_millis(st.config.keystroke_ttl_ms.max(200));

        while let Some(front) = st.clicks.front() {
            if now.saturating_duration_since(front.at) >= click_ttl {
                st.clicks.pop_front();
            } else {
                break;
            }
        }

        while let Some(front) = st.tokens.front() {
            if now.saturating_duration_since(front.at) >= key_ttl {
                st.tokens.pop_front();
            } else {
                break;
            }
        }

        let cursor_norm = st
            .cursor_pos
            .and_then(|(gx, gy)| normalize_point(gx, gy, bounds));

        let clicks = if st.config.show_clicks {
            st.clicks
                .iter()
                .filter_map(|c| {
                    let (norm_x, norm_y) = normalize_point(c.global_x, c.global_y, bounds)?;
                    let progress = (now.saturating_duration_since(c.at).as_secs_f32()
                        / click_ttl.as_secs_f32())
                    .clamp(0.0, 1.0);
                    Some(ClickRipple {
                        norm_x,
                        norm_y,
                        button: c.button,
                        progress,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        let keystroke_text = if st.config.show_keystrokes && !st.privacy_paused {
            render_keystroke_tokens(&st.tokens, st.config.keystroke_mode)
        } else {
            String::new()
        };

        InputOverlaySnapshot {
            cursor_norm,
            show_cursor_highlight: st.config.show_cursor_highlight,
            clicks,
            keystroke_text,
            privacy_paused: st.privacy_paused,
        }
    }
}

fn normalize_point(gx: f64, gy: f64, bounds: Option<MonitorBounds>) -> Option<(f32, f32)> {
    let b = bounds?;
    if b.width == 0 || b.height == 0 {
        return None;
    }
    let rx = (gx - f64::from(b.x)) / f64::from(b.width);
    let ry = (gy - f64::from(b.y)) / f64::from(b.height);
    if (0.0..=1.0).contains(&rx) && (0.0..=1.0).contains(&ry) {
        Some((rx as f32, ry as f32))
    } else {
        None
    }
}

fn set_modifier(mods: &mut Modifiers, kind: ModifierKind, pressed: bool) {
    match kind {
        ModifierKind::Ctrl => mods.ctrl = pressed,
        ModifierKind::Alt => mods.alt = pressed,
        ModifierKind::Shift => mods.shift = pressed,
        ModifierKind::Meta => mods.meta = pressed,
    }
}

fn format_shortcut(mods: &Modifiers, key_name: &str) -> String {
    let mut parts = Vec::with_capacity(4);
    if mods.ctrl {
        parts.push("Ctrl");
    }
    if mods.alt {
        parts.push("Alt");
    }
    if mods.meta {
        parts.push("Cmd");
    }
    if mods.shift {
        parts.push("Shift");
    }
    parts.push(key_name);
    format!("[{}]", parts.join("+"))
}

fn render_keystroke_tokens(tokens: &VecDeque<KeyToken>, mode: KeystrokeMode) -> String {
    if tokens.is_empty() {
        return String::new();
    }

    // In MaskedDigits mode, mask any contiguous digit run of length >= 5 except the last 2 digits
    // (or all digits when >= 6 so card numbers and PINs cannot be read from video).
    let mut chars_out: Vec<(String, bool)> = tokens
        .iter()
        .map(|t| (t.text.clone(), t.is_digit))
        .collect();

    if mode == KeystrokeMode::MaskedDigits {
        let mut idx = 0;
        while idx < chars_out.len() {
            if !chars_out[idx].1 {
                idx += 1;
                continue;
            }
            let start = idx;
            while idx < chars_out.len() && chars_out[idx].1 {
                idx += 1;
            }
            let run_len = idx - start;
            if run_len >= 5 {
                for item in &mut chars_out[start..idx.saturating_sub(2)] {
                    item.0 = "*".to_string();
                }
            }
        }
    }

    let mut combined = String::new();
    for (txt, _) in chars_out {
        combined.push_str(&txt);
    }

    let char_count = combined.chars().count();
    if char_count > MAX_KEYSTROKE_CHARS {
        combined
            .chars()
            .skip(char_count - MAX_KEYSTROKE_CHARS)
            .collect()
    } else {
        combined
    }
}

fn convert_rdev_event(ev: &RdevEvent) -> Option<TrackedEvent> {
    match &ev.event_type {
        RdevEventType::MouseMove { x, y } => Some(TrackedEvent::MouseMove { x: *x, y: *y }),
        RdevEventType::ButtonPress(btn) => Some(TrackedEvent::MouseDown {
            button: map_button(*btn),
        }),
        RdevEventType::ButtonRelease(btn) => Some(TrackedEvent::MouseUp {
            button: map_button(*btn),
        }),
        RdevEventType::KeyPress(key) => {
            let (key_name, is_special, modifier) = classify_key(*key);
            let printable = if modifier.is_some() || is_special {
                None
            } else {
                ev.unicode
                    .as_ref()
                    .and_then(|u| u.name.as_deref())
                    .and_then(|s| s.chars().next())
                    .filter(|c| !c.is_control())
                    .or_else(|| fallback_printable(*key))
            };
            Some(TrackedEvent::KeyPress {
                key_name,
                printable,
                is_special,
                modifier,
            })
        }
        RdevEventType::KeyRelease(key) => {
            let (_, _, modifier) = classify_key(*key);
            Some(TrackedEvent::KeyRelease { modifier })
        }
        RdevEventType::Wheel { .. } => None,
    }
}

const fn map_button(btn: Button) -> MouseButton {
    match btn {
        Button::Left => MouseButton::Left,
        Button::Right => MouseButton::Right,
        Button::Middle | Button::Unknown(_) => MouseButton::Middle,
    }
}

fn classify_key(key: Key) -> (String, bool, Option<ModifierKind>) {
    match key {
        Key::ControlLeft | Key::ControlRight => ("Ctrl".into(), true, Some(ModifierKind::Ctrl)),
        Key::Alt | Key::AltGr => ("Alt".into(), true, Some(ModifierKind::Alt)),
        Key::ShiftLeft | Key::ShiftRight => ("Shift".into(), true, Some(ModifierKind::Shift)),
        Key::MetaLeft | Key::MetaRight => ("Cmd".into(), true, Some(ModifierKind::Meta)),
        Key::Return | Key::KpReturn => ("ENTER".into(), true, None),
        Key::Escape => ("ESC".into(), true, None),
        Key::Tab => ("TAB".into(), true, None),
        Key::Backspace => ("BS".into(), true, None),
        Key::Delete => ("DEL".into(), true, None),
        Key::Space => (" ".into(), false, None),
        Key::UpArrow => ("UP".into(), true, None),
        Key::DownArrow => ("DOWN".into(), true, None),
        Key::LeftArrow => ("LEFT".into(), true, None),
        Key::RightArrow => ("RIGHT".into(), true, None),
        Key::Home => ("HOME".into(), true, None),
        Key::End => ("END".into(), true, None),
        Key::PageUp => ("PGUP".into(), true, None),
        Key::PageDown => ("PGDN".into(), true, None),
        Key::F1 => ("F1".into(), true, None),
        Key::F2 => ("F2".into(), true, None),
        Key::F3 => ("F3".into(), true, None),
        Key::F4 => ("F4".into(), true, None),
        Key::F5 => ("F5".into(), true, None),
        Key::F6 => ("F6".into(), true, None),
        Key::F7 => ("F7".into(), true, None),
        Key::F8 => ("F8".into(), true, None),
        Key::F9 => ("F9".into(), true, None),
        Key::F10 => ("F10".into(), true, None),
        Key::F11 => ("F11".into(), true, None),
        Key::F12 => ("F12".into(), true, None),
        other => (
            format!("{other:?}").trim_start_matches("Key").to_string(),
            false,
            None,
        ),
    }
}

fn fallback_printable(key: Key) -> Option<char> {
    match key {
        Key::Space => Some(' '),
        Key::Num0 | Key::Kp0 => Some('0'),
        Key::Num1 | Key::Kp1 => Some('1'),
        Key::Num2 | Key::Kp2 => Some('2'),
        Key::Num3 | Key::Kp3 => Some('3'),
        Key::Num4 | Key::Kp4 => Some('4'),
        Key::Num5 | Key::Kp5 => Some('5'),
        Key::Num6 | Key::Kp6 => Some('6'),
        Key::Num7 | Key::Kp7 => Some('7'),
        Key::Num8 | Key::Kp8 => Some('8'),
        Key::Num9 | Key::Kp9 => Some('9'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_clicks_and_expires_after_ttl() {
        let tracker = InputTracker::new(
            InputOverlayConfig {
                click_ttl_ms: 400,
                ..Default::default()
            },
            None,
        );
        let bounds = Some(MonitorBounds {
            x: 100,
            y: 50,
            width: 1000,
            height: 500,
        });
        let t0 = Instant::now();
        tracker.push_event(TrackedEvent::MouseMove { x: 600.0, y: 300.0 }, t0);
        tracker.push_event(
            TrackedEvent::MouseDown {
                button: MouseButton::Left,
            },
            t0,
        );

        let snap = tracker.snapshot(bounds, t0 + Duration::from_millis(200));
        assert_eq!(snap.cursor_norm, Some((0.5, 0.5)));
        assert_eq!(snap.clicks.len(), 1);
        assert!((snap.clicks[0].progress - 0.5).abs() < 0.05);

        let expired = tracker.snapshot(bounds, t0 + Duration::from_millis(450));
        assert!(expired.clicks.is_empty());
    }

    #[test]
    fn masks_long_digit_sequences_in_masked_digits_mode() {
        let tracker = InputTracker::new(
            InputOverlayConfig {
                keystroke_mode: KeystrokeMode::MaskedDigits,
                ..Default::default()
            },
            None,
        );
        let t0 = Instant::now();
        for ch in "4111111111111234".chars() {
            tracker.push_event(
                TrackedEvent::KeyPress {
                    key_name: ch.to_string(),
                    printable: Some(ch),
                    is_special: false,
                    modifier: None,
                },
                t0,
            );
        }
        let snap = tracker.snapshot(None, t0);
        assert_eq!(snap.keystroke_text, "**************34");
    }

    #[test]
    fn filters_shortcuts_and_excludes_sensitive_windows() {
        let tracker = InputTracker::new(
            InputOverlayConfig {
                keystroke_mode: KeystrokeMode::ShortcutsAndSpecial,
                excluded_windows: vec!["1Password".to_string()],
                ..Default::default()
            },
            Some(Arc::new(|| Some("My 1Password Vault".to_string()))),
        );
        let t0 = Instant::now();
        tracker.push_event(
            TrackedEvent::KeyPress {
                key_name: "a".into(),
                printable: Some('a'),
                is_special: false,
                modifier: None,
            },
            t0,
        );
        let snap = tracker.snapshot(None, t0);
        assert!(snap.privacy_paused);
        assert!(snap.keystroke_text.is_empty());
    }
}
