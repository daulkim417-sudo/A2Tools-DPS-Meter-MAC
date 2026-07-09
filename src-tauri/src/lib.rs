pub mod capture;
pub mod combat;
pub mod config;
pub mod entity;
pub mod history;
pub mod i18n;
pub mod logging;
pub mod platform;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tauri::{Emitter, Manager};
use tokio::sync::mpsc;

use capture::captured_payload::CapturedPayload;
use capture::combat_port_detector::CombatPortDetector;
use capture::pcap_capturer::PcapCapturer;
use combat::capture_dispatcher::CaptureDispatcher;
use combat::data_storage::DataStorage;
use combat::dps_calculator::DpsCalculator;
use combat::ping_tracker::PingTracker;
use config::settings::Settings;
use entity::dps_data::DpsData;
use entity::fight_record::{FightRecord, FightSummary};
use entity::details_context::{DetailsContext, TargetDetailsResponse};
use history::fight_history::FightHistoryManager;
use i18n::lookup::{NpcLookup, SkillLookup};

/// Shared application state.
pub struct AppState {
    pub data_storage: Arc<DataStorage>,
    pub dps_calculator: Mutex<DpsCalculator>,
    pub ping_tracker: Arc<PingTracker>,
    pub port_detector: Arc<CombatPortDetector>,
    pub fight_history: FightHistoryManager,
    pub settings: Settings,
    pub skill_lookup: Arc<SkillLookup>,
    pub npc_lookup: Arc<NpcLookup>,
    pub app_data_dir: std::path::PathBuf,
    pub i18n_data_dir: Option<std::path::PathBuf>,
}

// ===== TAURI COMMANDS =====

#[tauri::command]
fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
fn get_dps_snapshot(state: tauri::State<'_, AppState>) -> DpsData {
    state.dps_calculator.lock().get_dps()
}

#[tauri::command]
fn get_skill_details(state: tauri::State<'_, AppState>, target_id: i32, actor_ids: Option<Vec<i32>>) -> TargetDetailsResponse {
    state.dps_calculator.lock().get_target_details(target_id, actor_ids.as_deref())
}

#[tauri::command]
fn get_details_context(state: tauri::State<'_, AppState>) -> DetailsContext {
    state.dps_calculator.lock().get_details_context()
}

#[tauri::command]
fn get_fight_history(state: tauri::State<'_, AppState>) -> Vec<FightSummary> {
    state.fight_history.list_fights()
}

#[tauri::command]
fn save_fight(state: tauri::State<'_, AppState>, record: FightRecord) -> Result<(), String> {
    state.fight_history.save_fight(&record)
}

#[tauri::command]
fn load_fight(state: tauri::State<'_, AppState>, id: String) -> Result<FightRecord, String> {
    state.fight_history.load_fight(&id)
}

#[tauri::command]
fn delete_fight(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    state.fight_history.delete_fight(&id)
}

#[tauri::command]
fn export_fight_json(state: tauri::State<'_, AppState>, record: FightRecord) -> Result<String, String> {
    state.fight_history.export_fight_json(&record)
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> std::collections::HashMap<String, String> {
    state.settings.get_all()
}

#[tauri::command]
fn update_settings(state: tauri::State<'_, AppState>, key: String, value: String) {
    state.settings.set(&key, &value);
}

#[tauri::command]
fn clear_settings(state: tauri::State<'_, AppState>) {
    state.settings.clear();
}

#[tauri::command]
fn get_ping(state: tauri::State<'_, AppState>) -> Option<i32> {
    state.ping_tracker.current_ping_ms()
}

#[tauri::command]
fn get_capture_status(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let port = state.port_detector.current_port();
    let device_opt = state.port_detector.current_device();
    let local_id = state.data_storage.local_player_id();
    let char_name = state.data_storage.local_character_name();

    // macOS에서 device가 비어있을 때 fallback 처리
    let device = device_opt.clone().unwrap_or_else(|| {
        // PcapCapturer가 en 인터페이스를 사용 중인지 확인
        if let Ok(devices) = crate::capture::pcap_capturer::list_device_labels() {
            if let Some(first_en) = devices.into_iter().find(|d| d.to_lowercase().starts_with("en")) {
                return first_en;
            }
        }
        "en0".to_string() // 기본값
    });

    let ip = if device_opt.is_some() {
        // 실제 연결된 IP가 있으면 사용, 없으면 localhost
        "127.0.0.1".to_string()
    } else {
        "127.0.0.1".to_string()
    };

    serde_json::json!({
        "locked": port.is_some(),
        "port": port,
        "device": device,
        "ip": ip,
        "localPlayerId": local_id,
        "characterName": char_name,
    })
}

#[tauri::command]
fn set_target_mode(state: tauri::State<'_, AppState>, mode: String) {
    state.dps_calculator.lock().set_target_selection_mode(&mode);
}

#[tauri::command]
fn set_character_name(state: tauri::State<'_, AppState>, name: String) {
    let trimmed = name.trim().to_string();
    state.data_storage.set_local_character_name(Some(name));
    // If an actor ID was already bound, propagate the new character name
    // into nickname_storage immediately so the main meter window updates.
    if !trimmed.is_empty() {
        if let Some(id) = state.data_storage.local_player_id() {
            state.data_storage.set_permanent_nickname(id as i32, &trimmed);
        }
    }
}

#[tauri::command]
fn bind_local_actor_id(state: tauri::State<'_, AppState>, actor_id: i64) {
    if actor_id <= 0 {
        // Clear manual binding — auto-detection will take over
        tracing::info!("bind_local_actor_id: cleared");
        state.data_storage.set_local_player_id(None);
        return;
    }
    let already_bound = state.data_storage.local_player_id() == Some(actor_id);
    if !already_bound {
        tracing::info!("bind_local_actor_id: {}", actor_id);
        state.data_storage.set_local_player_id(Some(actor_id));
    }
    // Always (re)apply the permanent nickname if we have a character name,
    // even when the actor_id was already bound — this handles the case where
    // the character name was set AFTER the actor_id binding.
    if let Some(name) = state.data_storage.local_character_name() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            let current = state.data_storage.get_nickname(actor_id as i32);
            if current.as_deref() != Some(trimmed) {
                state.data_storage.set_permanent_nickname(actor_id as i32, trimmed);
            }
        }
    }
}

#[tauri::command]
fn bind_local_nickname(state: tauri::State<'_, AppState>, actor_id: i64, nickname: String) {
    // Always update if the stored nickname differs from the requested one.
    // Previously we skipped if the actor had ANY nickname, which left stale
    // false-positive scan results stuck in place.
    let current = state.data_storage.get_nickname(actor_id as i32);
    if state.data_storage.local_player_id() == Some(actor_id)
        && current.as_deref() == Some(nickname.as_str())
    {
        return;
    }
    tracing::info!("bind_local_nickname: {} -> '{}' (was {:?})", actor_id, nickname, current);
    state.data_storage.set_local_player_id(Some(actor_id));
    // Use set_permanent_nickname so it survives reset_nicknames() calls
    state.data_storage.set_permanent_nickname(actor_id as i32, &nickname);
}

#[tauri::command]
fn reset_combat(state: tauri::State<'_, AppState>) {
    state.dps_calculator.lock().restart_target_selection(true);
    // Don't reset port detector or ping — keep the network connection alive
    // Only clear combat data and re-learn nicknames from future packets
    state.data_storage.reset_nicknames();
}

#[tauri::command]
fn is_admin() -> bool {
    platform::admin::is_admin()
}

#[tauri::command]
fn set_language(state: tauri::State<'_, AppState>, language: String) {
    tracing::info!("Language change requested: {}", language);
    if let Some(ref data_dir) = state.i18n_data_dir {
        i18n::lookup::load_language(&state.skill_lookup, &state.npc_lookup, data_dir, &language);
    } else {
        tracing::warn!("No i18n data dir available for language reload");
    }
    state.settings.set("dpsMeter.language", &language);
}

#[tauri::command]
fn set_debug_logging(state: tauri::State<'_, AppState>, enabled: bool) {
    logging::logger::set_debug_enabled(enabled, &state.app_data_dir);
    state.settings.set("dpsMeter.debugLoggingEnabled", if enabled { "true" } else { "false" });
}

#[tauri::command]
fn set_packet_logging(state: tauri::State<'_, AppState>, enabled: bool) {
    logging::logger::set_packet_log_enabled(enabled, &state.app_data_dir);
    state.settings.set("dpsMeter.saveRawPackets", if enabled { "true" } else { "false" });
}

#[tauri::command]
fn reset_auto_detection(state: tauri::State<'_, AppState>) {
    state.port_detector.reset();
    state.ping_tracker.reset();
}

#[tauri::command]
fn get_available_devices() -> Vec<String> {
    // Load wpcap.dll and enumerate devices
    match crate::capture::pcap_capturer::list_device_labels() {
        Ok(labels) => labels,
        Err(_) => Vec::new(),
    }
}

#[tauri::command]
fn set_manual_device(state: tauri::State<'_, AppState>, device: String) {
    let dev = if device.trim().is_empty() { None } else { Some(device) };
    state.port_detector.set_preferred_device(dev);
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn read_cached_icon(state: tauri::State<'_, AppState>, key: String) -> Option<String> {
    let path = state.app_data_dir.join("icon_cache").join(&key);
    std::fs::read_to_string(&path).ok()
}

#[tauri::command]
fn write_cached_icon(state: tauri::State<'_, AppState>, key: String, data: String) {
    let cache_dir = state.app_data_dir.join("icon_cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let path = cache_dir.join(&key);
    let _ = std::fs::write(&path, &data);
}


#[tauri::command]
async fn show_update_window(app: tauri::AppHandle, current: String, latest: String, msi_url: String) -> Result<bool, String> {
    let msg = format!("A new update is available!\n\nCurrent: {}\nLatest: {}\n\nDownload and install now?", current, latest);

    let accepted = tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        {
            use windows::Win32::UI::WindowsAndMessaging::*;
            use windows::core::PCWSTR;
            let msg_w: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
            let title: Vec<u16> = "A2Tools - Update Available".encode_utf16().chain(std::iter::once(0)).collect();
            let result = unsafe {
                MessageBoxW(None, PCWSTR(msg_w.as_ptr()), PCWSTR(title.as_ptr()), MB_YESNO | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND)
            };
            result == IDYES
        }
        #[cfg(not(windows))]
        { false }
    }).await.unwrap_or(false);

    if accepted && !msi_url.is_empty() {
        // Download and install in background
        let app2 = app.clone();
        let url = msi_url.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = download_and_install_msi_inner(&app2, &url).await {
                tracing::error!("Update download failed: {}", e);
                // Show error dialog
                let _ = tokio::task::spawn_blocking(move || {
                    #[cfg(windows)]
                    {
                        use windows::Win32::UI::WindowsAndMessaging::*;
                        use windows::core::PCWSTR;
                        let msg: Vec<u16> = format!("Download failed: {}\n\nPlease download manually.", e)
                            .encode_utf16().chain(std::iter::once(0)).collect();
                        let title: Vec<u16> = "A2Tools - Update Error".encode_utf16().chain(std::iter::once(0)).collect();
                        unsafe { MessageBoxW(None, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR | MB_TOPMOST); }
                    }
                }).await;
            }
        });
    } else if accepted {
        // No MSI URL, open releases page
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", "https://github.com/taengu/A2Tools-DPS-Meter/releases"]).spawn();
    }

    Ok(accepted)
}

async fn download_and_install_msi_inner(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use futures_util::StreamExt;

    // Show progress dialog on a blocking thread
    let app_clone = app.clone();
    let url_owned = url.to_string();

    let response = reqwest::get(&url_owned).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let total_size = response.content_length().unwrap_or(0);
    let file_name = url_owned.rsplit('/').next().unwrap_or("update.msi");
    let msi_path = std::env::temp_dir().join(file_name);

    let mut file = tokio::fs::File::create(&msi_path).await.map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();
    let mut last_pct: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if total_size > 0 {
            let pct = (downloaded * 100 / total_size).min(100);
            if pct != last_pct {
                last_pct = pct;
                let _ = app_clone.emit("download-progress", pct);
                tracing::info!("Download: {}%", pct);
            }
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    tracing::info!("Download complete, launching installer: {}", msi_path.display());

    // Detect current install directory from the running executable's location
    let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let install_dir = current_exe.parent()
        .ok_or("Could not determine install directory")?
        .to_string_lossy()
        .into_owned();
    // Strip a trailing backslash so msiexec doesn't interpret \" as an escape
    let install_dir = install_dir.trim_end_matches('\\').to_string();

    // Launch the MSI installer. msiexec.exe uses its own non-standard command line
    // parser, so PROPERTY="value" pairs with spaces require literal embedded quotes
    // — not what std::process::Command's normal arg quoting produces. We use raw_arg
    // (Windows-only) to control the exact command line.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("msiexec");
        cmd.raw_arg("/i")
            .raw_arg(format!("\"{}\"", msi_path.display()))
            .raw_arg("/passive")
            .raw_arg(format!("INSTALLDIR=\"{}\"", install_dir))
            .raw_arg("AUTOLAUNCHAPP=1");
        tracing::info!("msiexec args: /i \"{}\" /passive INSTALLDIR=\"{}\" AUTOLAUNCHAPP=1",
            msi_path.display(), install_dir);
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        return Err("MSI install only supported on Windows".to_string());
    }

    // Give installer time to start, then exit
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    app_clone.exit(0);
    Ok(())
}

#[tauri::command]
async fn fetch_url(url: String) -> Result<String, String> {
    reqwest::get(&url).await.map_err(|e| e.to_string())?
        .text().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn open_url(url: String) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
}

#[tauri::command]
fn resize_window(app: tauri::AppHandle, width: f64, height: f64) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
    }
}

#[tauri::command]
fn capture_screenshot(app: tauri::AppHandle, x: i32, y: i32, width: i32, height: i32) {
    #[cfg(windows)]
    {
        if let Some(window) = app.get_webview_window("main") {
            if let Ok(raw) = window.hwnd() {
                let hwnd_val = raw.0 as isize;
                std::thread::spawn(move || {
                    platform::screenshot::capture_to_clipboard(hwnd_val, x, y, width, height);
                });
            }
        }
    }
}

#[tauri::command]
fn start_drag(app: tauri::AppHandle) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{HWND, WPARAM, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_NCLBUTTONDOWN};
        use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;

        if let Some(window) = app.get_webview_window("main") {
            if let Ok(raw) = window.hwnd() {
                unsafe {
                    let _ = ReleaseCapture();
                    const HTCAPTION: usize = 2;
                    let hwnd = HWND(raw.0);
                    let _ = PostMessageW(Some(hwnd), WM_NCLBUTTONDOWN, WPARAM(HTCAPTION), LPARAM(0));
                }
            }
        }
    }
}

#[tauri::command]
fn get_aion2_window_title() -> Option<String> {
    platform::window_detector::find_aion2_window_title()
}

#[tauri::command]
fn test_auto_hide() -> serde_json::Value {
    let aion_fg = platform::window_detector::is_aion2_foreground();
    let aion_title = platform::window_detector::find_aion2_window_title();
    serde_json::json!({
        "aion2_foreground": aion_fg,
        "aion2_title": aion_title,
    })
}

#[tauri::command]
fn debug_status(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let port = state.port_detector.current_port();
    let device = state.port_detector.current_device();
    let ping = state.ping_tracker.current_ping_ms();
    let dmg_gen = state.data_storage.damage_generation();
    let window = platform::window_detector::find_aion2_window_title();
    let admin = platform::admin::is_admin();
    serde_json::json!({
        "port": port,
        "device": device,
        "ping": ping,
        "damageGeneration": dmg_gen,
        "aion2Window": window,
        "isAdmin": admin,
    })
}

#[tauri::command]
async fn replay_file(state: tauri::State<'_, AppState>, file_path: String) -> Result<String, String> {
    // Reset existing data before replay
    state.dps_calculator.lock().restart_target_selection(true);
    state.data_storage.reset_nicknames();

    // Feed packets directly to StreamProcessor, bypassing CaptureDispatcher
    // (no AION2 window check, no port detection needed for replay)
    let data_storage = state.data_storage.clone();
    let skill_lookup = state.skill_lookup.clone();
    let npc_lookup = state.npc_lookup.clone();
    let i18n_dir = state.i18n_data_dir.clone();

    let count = tokio::task::spawn_blocking(move || {
        use crate::capture::stream_processor::StreamProcessor;

        let mut processor = StreamProcessor::new(data_storage.clone(), skill_lookup, npc_lookup);
        // Load DOT IDs
        if let Some(ref data_dir) = i18n_dir {
            let mut dot_ids = std::collections::HashSet::new();
            if let Ok(text) = std::fs::read_to_string(data_dir.join("dot_skill_ids.json")) {
                if let Ok(ids) = serde_json::from_str::<Vec<i32>>(&text) {
                    for id in ids { dot_ids.insert(id); }
                }
            }
            processor.set_dot_skill_ids(dot_ids);
        }

        // Each line in the replay file is a complete game payload — process directly
        // without TCP reassembly (the assembler would incorrectly concatenate payloads)
        let text = match std::fs::read_to_string(&file_path) {
            Ok(t) => t.trim_start_matches('\u{feff}').to_string(), // Strip BOM
            Err(e) => return Err(format!("Failed to read file: {}", e)),
        };

        let mut packet_count = 0;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() != 3 { continue; }
            // Use capture-time timestamp from the row, not wall clock
            if let Some(ts) = parse_replay_timestamp(parts[0].trim()) {
                processor.set_override_timestamp(Some(ts));
            }
            let hex = parts[2];
            let data = match decode_replay_hex(hex) {
                Some(d) => d,
                None => continue,
            };
            packet_count += 1;
            processor.consume_stream(&data);
        }

        let dmg = data_storage.damage_generation();
        Ok(format!("Replay complete. {} packets, {} damage events.", packet_count, dmg))
    }).await.map_err(|e| format!("Replay task failed: {}", e))?;

    // Force snapshot boss fights from the replay
    {
        let mut calc = state.dps_calculator.lock();
        let records = calc.snapshot_boss_fights_force();
        let mut sorted = records;
        sorted.sort_by(|a, b| b.total_damage.cmp(&a.total_damage));
        for record in sorted.iter().take(10) {
            if let Err(e) = state.fight_history.save_fight(record) {
                tracing::warn!("Failed to save replay fight: {}", e);
            } else {
                tracing::info!("Saved replay fight: {} ({})", record.boss_name, record.id);
            }
        }
        // Mark all targets as saved so the periodic auto-save loop doesn't re-process them
        calc.mark_all_targets_saved();
    }

    count
}

/// Parse an ISO 8601 timestamp (or plain epoch millis) into epoch milliseconds.
fn parse_replay_timestamp(s: &str) -> Option<i64> {
    // Try plain integer first (epoch millis)
    if let Ok(ms) = s.parse::<i64>() {
        return Some(ms);
    }
    // Parse ISO 8601: "2026-04-01T14:08:18.447814200-03:00"
    // Manual parse to avoid adding a chrono dependency
    // Format: YYYY-MM-DDTHH:MM:SS.fractional[+-]HH:MM
    let t_pos = s.find('T')?;
    let date_part = &s[..t_pos];
    let time_and_tz = &s[t_pos + 1..];

    let date_parts: Vec<&str> = date_part.split('-').collect();
    if date_parts.len() != 3 { return None; }
    let year: i64 = date_parts[0].parse().ok()?;
    let month: i64 = date_parts[1].parse().ok()?;
    let day: i64 = date_parts[2].parse().ok()?;

    // Split time from timezone offset (look for + or - after the seconds)
    let (time_part, tz_offset_mins) = if let Some(plus_pos) = time_and_tz.rfind('+') {
        if plus_pos > 6 { // Must be after HH:MM:SS
            let tz = &time_and_tz[plus_pos + 1..];
            let tz_parts: Vec<&str> = tz.split(':').collect();
            let h: i64 = tz_parts.first()?.parse().ok()?;
            let m: i64 = tz_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            (&time_and_tz[..plus_pos], h * 60 + m)
        } else {
            (time_and_tz, 0i64)
        }
    } else if let Some(minus_pos) = time_and_tz.rfind('-') {
        if minus_pos > 6 {
            let tz = &time_and_tz[minus_pos + 1..];
            let tz_parts: Vec<&str> = tz.split(':').collect();
            let h: i64 = tz_parts.first()?.parse().ok()?;
            let m: i64 = tz_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            (&time_and_tz[..minus_pos], -(h * 60 + m))
        } else {
            (time_and_tz, 0i64)
        }
    } else {
        // No timezone, treat as UTC
        let tp = time_and_tz.trim_end_matches('Z');
        (tp, 0i64)
    };

    // Parse time: HH:MM:SS.fractional
    let colon_parts: Vec<&str> = time_part.split(':').collect();
    if colon_parts.len() < 3 { return None; }
    let hour: i64 = colon_parts[0].parse().ok()?;
    let minute: i64 = colon_parts[1].parse().ok()?;
    let sec_parts: Vec<&str> = colon_parts[2].split('.').collect();
    let second: i64 = sec_parts[0].parse().ok()?;
    let millis: i64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1];
        // Take first 3 digits for milliseconds
        let padded = if frac.len() >= 3 { &frac[..3] } else { frac };
        let mut ms: i64 = padded.parse().ok()?;
        if frac.len() < 3 {
            for _ in 0..(3 - frac.len()) { ms *= 10; }
        }
        ms
    } else {
        0
    };

    // Convert to Unix epoch using a simplified algorithm
    // Days from epoch (1970-01-01)
    let days = days_from_civil(year, month, day);
    let total_secs = days * 86400 + hour * 3600 + minute * 60 + second - tz_offset_mins * 60;
    Some(total_secs * 1000 + millis)
}

/// Days from 1970-01-01 for a given civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn decode_replay_hex(hex: &str) -> Option<Vec<u8>> {
    let clean: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 { return None; }
    let mut bytes = Vec::with_capacity(clean.len() / 2);
    for chunk in clean.as_bytes().chunks(2) {
        let h = match chunk[0] {
            b'0'..=b'9' => chunk[0] - b'0',
            b'a'..=b'f' => chunk[0] - b'a' + 10,
            b'A'..=b'F' => chunk[0] - b'A' + 10,
            _ => return None,
        };
        let l = match chunk[1] {
            b'0'..=b'9' => chunk[1] - b'0',
            b'a'..=b'f' => chunk[1] - b'a' + 10,
            b'A'..=b'F' => chunk[1] - b'A' + 10,
            _ => return None,
        };
        bytes.push((h << 4) | l);
    }
    Some(bytes)
}

// ===== APP SETUP =====

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::logger::init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // Resolve data directory
            let app_data_dir = app.path().app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let _ = std::fs::create_dir_all(&app_data_dir);

            // Load resources — try multiple paths (dev vs production)
            let skill_lookup = SkillLookup::new();
            let npc_lookup = NpcLookup::new();
            let mut dot_ids: HashSet<i32> = HashSet::new();

            let resource_dir = app.path().resource_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let candidate_dirs = [
                resource_dir.join("data"),                        // production: resources/data
                resource_dir.join("_up_").join("src").join("data"), // production: resources/_up_/src/data (from ../src/data)
                resource_dir.join("..").join("src").join("data"), // dev: src-tauri/../src/data
                std::path::PathBuf::from("src/data"),             // dev: cwd fallback
                std::path::PathBuf::from("../src/data"),          // dev: from src-tauri/
            ];

            // Find the data directory
            let mut found_data_dir: Option<std::path::PathBuf> = None;
            for data_dir in &candidate_dirs {
                if data_dir.exists() && data_dir.join("i18n").join("skills").exists() {
                    found_data_dir = Some(data_dir.clone());
                    break;
                }
            }

            if let Some(ref data_dir) = found_data_dir {
                // Load DOT skill IDs (language-independent)
                if let Ok(text) = std::fs::read_to_string(data_dir.join("dot_skill_ids.json")) {
                    if let Ok(ids) = serde_json::from_str::<Vec<i32>>(&text) {
                        for id in ids { dot_ids.insert(id); }
                        tracing::info!("Loaded {} DOT skill IDs", dot_ids.len());
                    }
                }

                // Load skill/NPC data in the user's language
                let language = Settings::new(app_data_dir.clone())
                    .get("dpsMeter.language")
                    .unwrap_or_else(|| "en".to_string());
                i18n::lookup::load_language(&skill_lookup, &npc_lookup, data_dir, &language);
            } else {
                tracing::warn!("Failed to find data directory!");
            }

            let skill_lookup = Arc::new(skill_lookup);
            let npc_lookup = Arc::new(npc_lookup);

            let data_storage = Arc::new(DataStorage::new());
            let ping_tracker = Arc::new(PingTracker::new());
            let port_detector = Arc::new(CombatPortDetector::new());

            let dps_calculator = DpsCalculator::new(
                data_storage.clone(),
                skill_lookup.clone(),
                npc_lookup.clone(),
                ping_tracker.clone(),
            );

            let settings = Settings::new(app_data_dir.clone());

            // Load logging settings from saved state
            if settings.get("dpsMeter.debugLoggingEnabled").as_deref() == Some("true") {
                logging::logger::set_debug_enabled(true, &app_data_dir);
            }
            if settings.get("dpsMeter.saveRawPackets").as_deref() == Some("true") {
                logging::logger::set_packet_log_enabled(true, &app_data_dir);
            }

            let state = AppState {
                data_storage: data_storage.clone(),
                dps_calculator: Mutex::new(dps_calculator),
                ping_tracker: ping_tracker.clone(),
                port_detector: port_detector.clone(),
                fight_history: FightHistoryManager::new(app_data_dir.clone()),
                settings,
                skill_lookup: skill_lookup.clone(),
                npc_lookup: npc_lookup.clone(),
                app_data_dir: app_data_dir.clone(),
                i18n_data_dir: found_data_dir.clone(),
            };

            app.manage(state);

            // Restore saved window position and ensure always-on-top
            if let Some(window) = app.get_webview_window("main") {
                let state_ref = app.state::<AppState>();
                if let (Some(x), Some(y)) = (state_ref.settings.get("window.x"), state_ref.settings.get("window.y")) {
                    if let (Ok(x), Ok(y)) = (x.parse::<i32>(), y.parse::<i32>()) {
                        // Don't restore minimized positions (Windows uses -32000,-32000)
                        if x > -10000 && y > -10000 {
                            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }));
                        }
                    }
                }
                let _ = window.set_always_on_top(true);
            }

            // 운영체제별 패킷 캡처 드라이버/라이브러리 존재 여부 체크
            #[cfg(windows)]
            let pcap_available = unsafe { libloading::Library::new("wpcap.dll").is_ok() };
            #[cfg(target_os = "macos")]
            let pcap_available = unsafe {
                libloading::Library::new("libpcap.dylib").is_ok() 
                    || libloading::Library::new("/usr/lib/libpcap.A.dylib").is_ok()
            };
            #[cfg(not(any(windows, target_os = "macos")))]
            let pcap_available = true;

            if !pcap_available {
                #[cfg(windows)]
                tracing::error!("Npcap is not installed — packet capture disabled");
                #[cfg(not(windows))]
                tracing::error!("libpcap is not installed — packet capture disabled");
                
                let handle_npcap = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = handle_npcap.emit("npcap-missing", ());
                });
            }

            // Start capture pipeline
            let (tx, rx) = mpsc::channel::<CapturedPayload>(4096);

            let capturer = PcapCapturer::new(tx);
            if pcap_available {
                capturer.start();
            }

            let mut dispatcher = CaptureDispatcher::new(
                data_storage.clone(),
                skill_lookup.clone(),
                npc_lookup.clone(),
                port_detector.clone(),
                ping_tracker.clone(),
            );
            dispatcher.set_dot_skill_ids(dot_ids);

            tauri::async_runtime::spawn(async move {
                dispatcher.run(rx).await;
            });

            let hotkey_handle = app.handle().clone();
            let hotkey_manager = platform::hotkeys::HotkeyManager::new();

            let reload_label = app.state::<AppState>().settings
                .get("dpsMeter.hotkey").unwrap_or_default();
            let toggle_label = app.state::<AppState>().settings
                .get("dpsMeter.toggleWindowHotkey").unwrap_or_default();

            let (reload_mods, reload_vk) = platform::hotkeys::parse_hotkey_label(&reload_label)
                .unwrap_or((0x0002 | 0x0001, 0x52)); 
            let (toggle_mods, toggle_vk) = platform::hotkeys::parse_hotkey_label(&toggle_label)
                .unwrap_or((0x0002 | 0x0001, 0x26));

            hotkey_manager.start(
                reload_mods, reload_vk,
                toggle_mods, toggle_vk,
                {
                    let h = hotkey_handle.clone();
                    move || {
                        tracing::info!("Hotkey: reload triggered");
                        if let Some(state) = h.try_state::<AppState>() {
                            state.dps_calculator.lock().restart_target_selection(true);
                            state.data_storage.reset_nicknames();
                        }
                        let _ = h.emit("combat-reset", ());
                        let _ = h.emit("dps-update", &entity::dps_data::DpsData::new());
                    }
                },
                {
                    let h = hotkey_handle;
                    move || {
                        if let Some(window) = h.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_always_on_top(true);
                                let _ = window.set_focus();
                            }
                        }
                    }
                },
            );

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(500));
                let mut tick_count: u64 = 0;
                let mut hide_delay: u64 = 0;
                loop {
                    interval.tick().await;
                    tick_count += 1;

                    if let Some(state) = handle.try_state::<AppState>() {
                        let t0 = std::time::Instant::now();
                        let lock_guard = state.dps_calculator.lock();
                        let lock_ms = t0.elapsed().as_millis();
                        let dps = {
                            let mut calc = lock_guard;
                            calc.get_dps()
                        };
                        let calc_ms = t0.elapsed().as_millis();
                        let _ = handle.emit("dps-update", &dps);
                        let total_ms = t0.elapsed().as_millis();
                        if total_ms > 200 {
                            tracing::warn!("Slow: lock={}ms calc={}ms emit={}ms total={}ms gen={}",
                                lock_ms, calc_ms - lock_ms, total_ms - calc_ms, total_ms,
                                state.data_storage.damage_generation());
                        }

                        if let Some(ping) = state.ping_tracker.current_ping_ms() {
                            let _ = handle.emit("ping-update", ping);
                        }

                        let auto_hide = tick_count > 20
                            && state.settings.get("dpsMeter.autoHideMeter")
                                .unwrap_or_default() == "true";
                        if auto_hide {
                            if let Some(window) = handle.get_webview_window("main") {
                                let aion_fg = platform::window_detector::is_aion2_foreground();
                                let is_self_fg = window.is_focused().unwrap_or(false);
                                let is_visible = window.is_visible().unwrap_or(true);
                                let is_minimized = window.is_minimized().unwrap_or(false);
                                #[cfg(windows)]
                                {
                                    use windows::Win32::Foundation::HWND;
                                    use windows::Win32::UI::WindowsAndMessaging::*;
                                    if let Ok(raw) = window.hwnd() {
                                        let hwnd = HWND(raw.0);
                                        if aion_fg || is_self_fg {
                                            hide_delay = 0;
                                            if !is_visible || is_minimized {
                                                unsafe {
                                                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                                                    let _ = SetWindowPos(
                                                        hwnd, Some(HWND_TOPMOST),
                                                        0, 0, 0, 0,
                                                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                                                    );
                                                }
                                                let _ = window.emit("force-resize", ());
                                            }
                                        } else if is_visible && !is_minimized {
                                            hide_delay += 1;
                                            if hide_delay >= 3 {
                                                unsafe {
                                                    let _ = SetWindowPos(
                                                        hwnd, Some(HWND_NOTOPMOST),
                                                        0, 0, 0, 0,
                                                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                                    );
                                                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if tick_count % 10 == 0 {
                            if let Some(window) = handle.get_webview_window("main") {
                                if let Ok(pos) = window.outer_position() {
                                    if pos.x > -10000 && pos.y > -10000 {
                                        state.settings.set("window.x", &pos.x.to_string());
                                        state.settings.set("window.y", &pos.y.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            });

            let handle_save = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if let Some(state) = handle_save.try_state::<AppState>() {
                        if state.data_storage.damage_generation() > 0 {
                            // Run on blocking thread to avoid starving the async runtime
                            // snapshot_boss_fights acquires the dps_calculator lock
                            // Run synchronously but only if lock is available
                            if let Some(mut calc) = state.dps_calculator.try_lock() {
                                let records = calc.snapshot_boss_fights();
                                drop(calc);
                                for record in &records {
                                    let _ = state.fight_history.save_fight(record);
                                }
                            }
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_version,
            get_dps_snapshot,
            get_skill_details,
            get_details_context,
            get_fight_history,
            save_fight,
            load_fight,
            delete_fight,
            export_fight_json,
            get_settings,
            update_settings,
            get_ping,
            get_capture_status,
            set_target_mode,
            set_character_name,
            bind_local_actor_id,
            bind_local_nickname,
            clear_settings,
            reset_combat,
            is_admin,
            set_language,
            set_debug_logging,
            set_packet_logging,
            get_aion2_window_title,
            debug_status,
            quit_app,
            open_url,
            read_cached_icon,
            write_cached_icon,
            resize_window,
            capture_screenshot,
            start_drag,
            reset_auto_detection,
            get_available_devices,
            set_manual_device,
            replay_file,
            test_auto_hide,
            fetch_url,
            show_update_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
