//! Menu-bar and system-tray icon.

use tauri::AppHandle;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

const ICON: &[u8] = include_bytes!("../icons/tray@2x.png");

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Fonsviva", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Fonsviva", true, Some("CmdOrCtrl+Q"))?;
    let menu = Menu::with_items(app, &[&open, &PredefinedMenuItem::separator(app)?, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(ICON)?)
        .icon_as_template(true)
        .tooltip("Fonsviva")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => crate::show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                if !cfg!(target_os = "macos") {
                    crate::show_main(tray.app_handle());
                }
            }
        })
        .build(app)?;
    Ok(())
}
