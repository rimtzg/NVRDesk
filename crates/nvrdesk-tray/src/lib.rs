//! Cross-platform System Tray icon and quick-control menu for NVRDesk.

use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use nvrdesk_daemon::DaemonHandle;
use tracing::{info, warn};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

struct TrayMenuHandles {
    _tray: TrayIcon,
    open_id: MenuId,
    pause_id: MenuId,
    quit_id: MenuId,
    pause_item: MenuItem,
    status_item: MenuItem,
}

/// Runs the platform System Tray loop on the calling thread until `shutdown` becomes `true`.
///
/// On macOS this MUST be called from the process main thread (`NSStatusBar` requirement).
pub fn run_tray_loop(handle: DaemonHandle, shutdown: Arc<AtomicBool>) {
    #[cfg(target_os = "macos")]
    {
        run_macos_tray(handle, shutdown);
    }
    #[cfg(target_os = "windows")]
    {
        run_windows_tray(handle, shutdown);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        run_linux_tray(handle, shutdown);
    }
}

fn build_tray_icon(handle: &DaemonHandle) -> Result<TrayMenuHandles, String> {
    let icon = build_camera_icon();
    let status_item = MenuItem::new("NVRDesk: Ready", false, None);
    let open_item = MenuItem::new("Open Web Control Panel", true, None);
    let pause_item = MenuItem::new(
        if handle.is_paused() {
            "Resume Streaming"
        } else {
            "Pause Streaming"
        },
        true,
        None,
    );
    let quit_item = MenuItem::new("Quit NVRDesk", true, None);

    let open_id = open_item.id().clone();
    let pause_id = pause_item.id().clone();
    let quit_id = quit_item.id().clone();

    let menu = Menu::new();
    let _ = menu.append(&status_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&open_item);
    let _ = menu.append(&pause_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("NVRDesk Virtual IP Camera")
        .with_icon(icon)
        .build()
        .map_err(|e| format!("failed to build tray icon: {e}"))?;

    Ok(TrayMenuHandles {
        _tray: tray,
        open_id,
        pause_id,
        quit_id,
        pause_item,
        status_item,
    })
}

fn handle_menu_event(
    event_id: &MenuId,
    menu: &TrayMenuHandles,
    handle: &DaemonHandle,
    shutdown: &AtomicBool,
) -> bool {
    if event_id == &menu.open_id {
        open_browser(&handle.web_ui_url());
    } else if event_id == &menu.pause_id {
        if handle.is_paused() {
            handle.resume();
            menu.pause_item.set_text("Pause Streaming");
            menu.status_item.set_text("NVRDesk: Active");
        } else {
            handle.pause();
            menu.pause_item.set_text("Resume Streaming");
            menu.status_item.set_text("NVRDesk: Paused");
        }
    } else if event_id == &menu.quit_id {
        info!("quit selected from system tray");
        handle.request_shutdown();
        shutdown.store(true, Ordering::Release);
        return true;
    }
    false
}

fn build_camera_icon() -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = ((y * SIZE + x) * 4) as usize;
            let in_body = (4..28).contains(&x) && (7..25).contains(&y);
            let dx = x as f32 - 16.0;
            let dy = y as f32 - 16.0;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= 5.0 {
                // Lens center
                rgba[idx..idx + 4].copy_from_slice(&[59, 130, 246, 255]);
            } else if dist <= 7.2 {
                // Lens ring
                rgba[idx..idx + 4].copy_from_slice(&[240, 246, 255, 255]);
            } else if in_body {
                // Camera housing
                rgba[idx..idx + 4].copy_from_slice(&[30, 41, 59, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid 32x32 RGBA tray icon")
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let res = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let res = Command::new("xdg-open").arg(url).spawn();

    if let Err(err) = res {
        warn!(%url, error = %err, "failed to launch browser for Web UI");
    }
}

fn sync_tray_visibility(
    menu: &mut Option<TrayMenuHandles>,
    handle: &DaemonHandle,
    tray_failed: &mut bool,
) {
    if !handle.tray_enabled() {
        if menu.take().is_some() {
            info!("system tray icon hidden by configuration");
        }
        *tray_failed = false;
        return;
    }

    if menu.is_none() && !*tray_failed {
        match build_tray_icon(handle) {
            Ok(built) => {
                info!(web_ui = %handle.web_ui_url(), "system tray icon ready");
                *menu = Some(built);
            }
            Err(err) => {
                warn!(error = %err, "system tray unavailable; continuing without tray");
                *tray_failed = true;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn run_macos_tray(handle: DaemonHandle, shutdown: Arc<AtomicBool>) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        warn!("macOS tray requires main thread; falling back to headless wait");
        wait_for_shutdown(&shutdown);
        return;
    };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let mut menu: Option<TrayMenuHandles> = None;
    let mut tray_failed = false;
    sync_tray_visibility(&mut menu, &handle, &mut tray_failed);

    // Pump NSApplication events periodically so Ctrl+C shutdown and dynamic tray toggling work cleanly.
    while !shutdown.load(Ordering::Acquire) {
        sync_tray_visibility(&mut menu, &handle, &mut tray_failed);
        if let Some(m) = menu.as_ref() {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if handle_menu_event(&ev.id, m, &handle, &shutdown) {
                    return;
                }
            }
        }
        unsafe {
            use objc2_app_kit::NSEventMask;
            use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
            let until = NSDate::dateWithTimeIntervalSinceNow(0.1);
            while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                NSDefaultRunLoopMode,
                true,
            ) {
                app.sendEvent(&event);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn run_windows_tray(handle: DaemonHandle, shutdown: Arc<AtomicBool>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };

    let mut menu: Option<TrayMenuHandles> = None;
    let mut tray_failed = false;
    sync_tray_visibility(&mut menu, &handle, &mut tray_failed);

    let mut msg = MSG::default();
    while !shutdown.load(Ordering::Acquire) {
        sync_tray_visibility(&mut menu, &handle, &mut tray_failed);
        unsafe {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Some(m) = menu.as_ref() {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if handle_menu_event(&ev.id, m, &handle, &shutdown) {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn run_linux_tray(handle: DaemonHandle, shutdown: Arc<AtomicBool>) {
    let mut menu: Option<TrayMenuHandles> = None;
    let mut tray_failed = false;
    sync_tray_visibility(&mut menu, &handle, &mut tray_failed);

    while !shutdown.load(Ordering::Acquire) {
        sync_tray_visibility(&mut menu, &handle, &mut tray_failed);
        if let Some(m) = menu.as_ref() {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if handle_menu_event(&ev.id, m, &handle, &shutdown) {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_shutdown(shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
    }
}
