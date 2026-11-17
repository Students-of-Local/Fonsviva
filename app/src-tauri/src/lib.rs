pub mod daemon;
mod tray;

use tauri::{AppHandle, Manager};

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The daemon's address and this app's tokens. Starts the daemon when none serves this data dir.
#[tauri::command]
async fn daemon_info() -> Result<daemon::DaemonInfo, String> {
    tauri::async_runtime::spawn_blocking(daemon::ensure)
        .await
        .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_main(app)
        }))
        .setup(|app| {
            tray::build(app.handle())?;
            show_main(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps Fonsviva in the tray; models keep serving wired apps.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![daemon_info])
        .build(tauri::generate_context!())
        .expect("error while building Fonsviva")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main(app);
            }
            let _ = (app, event);
        });
}
