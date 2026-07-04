// Hide the console window on Windows when compiling in release mode
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

slint::include_modules!();

mod scanner;

use clap::Parser;
use serde::Serialize;
use slint::{ModelRc, SharedString, VecModel};
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

// Bring winit accessor traits and raw events into scope for Drag-and-Drop
use slint::winit_030::{WinitWindowAccessor, winit};

static AUTO_SAVE_LOGS: AtomicBool = AtomicBool::new(false);

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// The target directory path to scan.
    #[arg(index = 1)]
    path: Option<String>,

    /// Run without launching the graphical user interface.
    #[arg(short, long)]
    quiet: bool,

    /// Automatically delete empty directories found during the scan.
    #[arg(short, long)]
    delete: bool,

    /// Simulate deletion without modifying any files (Dry-Run).
    #[arg(long)]
    dry_run: bool,

    /// Format CLI output as structured JSON.
    #[arg(long)]
    json: bool,

    // Configurable parameters for scripts and automation
    #[arg(long, default_value_t = -1)]
    max_depth: i32,

    #[arg(long)]
    delete_permanently: bool,

    #[arg(long, default_value = "desktop.ini,Thumbs.db,.DS_Store")]
    ignore_files: String,

    #[arg(
        long,
        default_value = "System Volume Information,RECYCLER,Recycled,$RECYCLE.BIN"
    )]
    ignore_dirs: String,

    #[arg(long, default_value_t = true)]
    ignore_hidden: bool,

    #[arg(long, default_value_t = true)]
    keep_system: bool,

    #[arg(long, default_value_t = 0)]
    min_age_hours: u32,

    #[arg(long, default_value_t = true)]
    consider_empty_files_empty: bool,

    /// Collapse access-denied style errors into a single summary line instead
    /// of printing each one.
    #[arg(long, default_value_t = true)]
    hide_search_errors: bool,
}

#[derive(Serialize)]
struct JsonReport {
    scan_path: String,
    empty_directories_found: Vec<JsonDir>,
    deletion_summary: Option<JsonDeletionSummary>,
}

#[derive(Serialize)]
struct JsonDir {
    path: String,
    status: &'static str,
}

#[derive(Serialize)]
struct JsonDeletionSummary {
    deleted: usize,
    failed: usize,
    dry_run: bool,
}

pub enum LogEvent {
    Msg(String),
    StatusChange(usize, i32), // index, status
    Progress(f32),            // Event for updating the progress bar
}

#[derive(Clone)]
pub struct UiLogger {
    sender: Option<mpsc::Sender<LogEvent>>,
}

impl UiLogger {
    pub fn log(&self, msg: &str) {
        if AUTO_SAVE_LOGS.load(Ordering::Relaxed) {
            write_to_file_log(msg);
        }
        if let Some(sender) = &self.sender {
            let _ = sender.send(LogEvent::Msg(format!("{}\n", msg)));
        } else {
            println!("{}", msg);
        }
    }

    pub fn status(&self, msg: &str, index: usize, status: i32) {
        if AUTO_SAVE_LOGS.load(Ordering::Relaxed) {
            write_to_file_log(msg);
        }
        if let Some(sender) = &self.sender {
            let _ = sender.send(LogEvent::Msg(format!("{}\n", msg)));
            let _ = sender.send(LogEvent::StatusChange(index, status));
        } else {
            println!("{}", msg);
        }
    }

    pub fn progress(&self, val: f32) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(LogEvent::Progress(val));
        }
    }
}

fn write_to_file_log(msg: &str) {
    if let Some(proj_dirs) = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        let log_dir = proj_dirs.join("RED").join("logs");
        if fs::create_dir_all(&log_dir).is_ok() {
            let log_file = log_dir.join("red.log");
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
            {
                use std::io::Write;
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                let _ = writeln!(file, "[{}] {}", timestamp, msg.trim_end());
            }
        }
    }
}

// Windows Explorer Registry helpers (written to HKCU - no Administrator UAC required)
#[cfg(target_os = "windows")]
fn check_registry_integration() -> bool {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(
        r"Software\Classes\Directory\shell\RemoveEmptyDirs\command",
        KEY_READ,
    )
    .is_ok()
}

#[cfg(target_os = "windows")]
fn set_registry_integration(integrate: bool) -> Result<(), String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = r"Software\Classes\Directory\shell\RemoveEmptyDirs";

    if integrate {
        let current_exe = std::env::current_exe()
            .map_err(|e| format!("Failed to resolve current executable path: {}", e))?;
        let command_str = format!("\"{}\" \"%1\"", current_exe.to_string_lossy());

        let (key, _) = hkcu
            .create_subkey_with_flags(path, KEY_WRITE)
            .map_err(|e| e.to_string())?;
        key.set_value("", &"Remove empty folders here")
            .map_err(|e| e.to_string())?;

        // This adds the Coffee Cup icon next to the context menu text in Windows Explorer [3]:
        key.set_value("Icon", &current_exe.to_string_lossy().as_ref())
            .map_err(|e| e.to_string())?;

        let (cmd_key, _) = key
            .create_subkey_with_flags("command", KEY_WRITE)
            .map_err(|e| e.to_string())?;
        cmd_key
            .set_value("", &command_str)
            .map_err(|e| e.to_string())?;
    } else {
        let _ = hkcu.delete_subkey_all(path);
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn check_registry_integration() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn set_registry_integration(_integrate: bool) -> Result<(), String> {
    Ok(())
}

fn rebuild_visible_items(folders: &[scanner::DirectoryNode]) -> Vec<DirectoryItem> {
    let mut result = Vec::new();
    let mut hide_depth = i32::MAX;
    let mut active_depths: Vec<bool> = Vec::new();

    for (i, node) in folders.iter().enumerate() {
        if node.depth <= hide_depth {
            hide_depth = i32::MAX;

            if node.depth > 0 {
                let idx = (node.depth - 1) as usize;
                if active_depths.len() <= idx {
                    active_depths.resize(idx + 1, false);
                }
                active_depths[idx] = !node.is_last_sibling;
            }

            let mut tree_lines_vec = Vec::new();
            for d in 0..(node.depth - 1) {
                let d = d as usize;
                if d < active_depths.len() && active_depths[d] {
                    tree_lines_vec.push(1);
                } else {
                    tree_lines_vec.push(0);
                }
            }
            if node.depth > 0 {
                if node.is_last_sibling {
                    tree_lines_vec.push(3);
                } else {
                    tree_lines_vec.push(2);
                }
            }

            let tree_lines_model = std::rc::Rc::new(slint::VecModel::from(tree_lines_vec));

            result.push(DirectoryItem {
                name: SharedString::from(&node.name),
                path: SharedString::from(node.path.to_string_lossy().into_owned()),
                depth: node.depth,
                status: node.status,
                has_children: node.has_children,
                is_expanded: node.is_expanded,
                id: i as i32,
                is_root: node.depth == 0,
                tree_prefix: SharedString::new(),
                tree_lines: tree_lines_model.into(),
            });

            if !node.is_expanded {
                hide_depth = node.depth;
            }
        }
    }
    result
}

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

    let cli = Cli::parse();

    // Full CLI Mode
    if cli.quiet || cli.delete || cli.json {
        let dummy_cancel = Arc::new(AtomicBool::new(false));
        if let Some(path_str) = cli.path {
            let path = PathBuf::from(path_str.clone());
            let scan_settings = scanner::ScanSettings {
                ignore_files: cli
                    .ignore_files
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                ignore_dirs: cli
                    .ignore_dirs
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                ignore_hidden: cli.ignore_hidden,
                keep_system: cli.keep_system,
                min_age_hours: cli.min_age_hours,
                max_depth: cli.max_depth,
                consider_empty_files_empty: cli.consider_empty_files_empty,
                hide_search_errors: cli.hide_search_errors,
            };

            if !cli.quiet && !cli.json {
                println!("[*] Scanning: {:?}", path);
            }

            let mut json_dirs = Vec::new();

            match scanner::scan_empty_dirs(
                &path,
                &scan_settings,
                &|msg| {
                    if !cli.quiet && !cli.json {
                        println!("{}", msg);
                    }
                },
                &dummy_cancel,
            ) {
                Ok(mut dirs) => {
                    let empty_count = dirs.iter().filter(|d| d.status == 1).count();
                    if !cli.quiet && !cli.json {
                        println!("[+] Found {} empty directories.", empty_count);
                    }

                    if cli.json {
                        for d in &dirs {
                            let status_str = match d.status {
                                1 => "empty",
                                3 => "protected",
                                _ => "normal",
                            };
                            json_dirs.push(JsonDir {
                                path: d.path.to_string_lossy().into_owned(),
                                status: status_str,
                            });
                        }
                    }

                    let mut deletion_summary = None;

                    if cli.delete && empty_count > 0 {
                        if !cli.quiet && !cli.json {
                            if cli.dry_run {
                                println!("[*] Simulating deletion (Dry-Run)...");
                            } else {
                                println!("[*] Deleting...");
                            }
                        }
                        let delete_settings = scanner::DeleteSettings {
                            move_to_trash: !cli.delete_permanently,
                            ignore_errors: true,
                            pause_ms: 0,
                            ignore_files: scan_settings.ignore_files.clone(),
                            consider_empty_files_empty: cli.consider_empty_files_empty,
                            dry_run: cli.dry_run,
                        };
                        let (deleted, failed) = scanner::delete_empty_dirs(
                            &mut dirs,
                            &delete_settings,
                            &|msg, _, _| {
                                if !cli.quiet && !cli.json {
                                    println!("{}", msg);
                                }
                            },
                            &|_| {},
                            &dummy_cancel,
                        );
                        if !cli.quiet && !cli.json {
                            if cli.dry_run {
                                println!(
                                    "[+] Simulation complete. Would delete: {}, Failed: {}",
                                    deleted, failed
                                );
                            } else {
                                println!(
                                    "[+] Deletion complete. Deleted: {}, Failed: {}",
                                    deleted, failed
                                );
                            }
                        }

                        if cli.json {
                            deletion_summary = Some(JsonDeletionSummary {
                                deleted,
                                failed,
                                dry_run: cli.dry_run,
                            });
                        }
                    }

                    if cli.json {
                        let report = JsonReport {
                            scan_path: path_str,
                            empty_directories_found: json_dirs,
                            deletion_summary,
                        };
                        if let Ok(json_str) = serde_json::to_string_pretty(&report) {
                            println!("{}", json_str);
                        }
                    }
                }
                Err(e) => {
                    if !cli.quiet && !cli.json {
                        eprintln!("[!] Error: {}", e);
                    } else if cli.json {
                        println!("{{\"error\": \"{}\"}}", e);
                    }
                    std::process::exit(1);
                }
            }
        } else {
            if !cli.quiet && !cli.json {
                eprintln!("[!] Error: Path is required for CLI/Quiet/JSON mode.");
            } else if cli.json {
                println!("{{\"error\": \"Path is required\"}}");
            }
            std::process::exit(1);
        }
        return Ok(());
    }

    let ui = AppWindow::new()?;
    let ui_handle = ui.as_weak();

    // Initialize OS Context Menu and Log configuration states on startup
    ui.set_is_integrated(check_registry_integration());
    ui.set_auto_save_logs(AUTO_SAVE_LOGS.load(Ordering::Relaxed));

    // DRAG-AND-DROP INTEGRATION:
    // Intercepts window events at the OS level. If a user drops a folder from
    // Windows Explorer, we automatically set the target directory in the UI. [1]
    let ui_weak_dnd = ui_handle.clone();
    ui.window().on_winit_window_event(move |_, event| {
        // Collapsed nested if let bindings using Rust let-chains:
        if let winit::event::WindowEvent::DroppedFile(path_buf) = event
            && let Some(ui) = ui_weak_dnd.upgrade()
        {
            ui.set_selected_folder(SharedString::from(path_buf.to_string_lossy().into_owned()));
        }
        slint::winit_030::EventResult::Propagate
    });

    if let Some(path) = cli.path {
        ui.set_selected_folder(SharedString::from(path));
    }

    let (log_tx, log_rx) = mpsc::channel::<LogEvent>();
    let logger = UiLogger {
        sender: Some(log_tx),
    };

    let found_folders = Arc::new(Mutex::new(Vec::<scanner::DirectoryNode>::new()));
    let cancel_flag = Arc::new(AtomicBool::new(false));

    ui.set_directories(ModelRc::from(Rc::new(VecModel::from(vec![]))));

    let ui_weak_log = ui_handle.clone();
    let found_folders_log = found_folders.clone();

    // Background thread to manage logs, progress metrics, and UI state updates
    thread::spawn(move || {
        let mut logs = VecDeque::with_capacity(300);
        let mut last_rebuild_time = std::time::Instant::now();
        let mut pending_status_updates = false;

        while let Ok(evt) = log_rx.recv() {
            let mut status_updates = Vec::new();
            let mut progress_update = None;
            let mut logs_changed = false;

            let mut process_event = |e: LogEvent| match e {
                LogEvent::Msg(msg) => {
                    logs.push_back(msg);
                    logs_changed = true;
                }
                LogEvent::StatusChange(index, status) => {
                    status_updates.push((index, status));
                    pending_status_updates = true;
                }
                LogEvent::Progress(p) => progress_update = Some(p),
            };

            process_event(evt);

            while let Ok(m) = log_rx.try_recv() {
                process_event(m);
            }

            while logs.len() > 250 {
                logs.pop_front();
                logs_changed = true;
            }

            let folders_clone = {
                let mut folders = found_folders_log.lock().unwrap();
                for &(index, status) in &status_updates {
                    if let Some(node) = folders.get_mut(index) {
                        node.status = status;
                    }
                }
                folders.clone()
            };

            let now = std::time::Instant::now();
            let elapsed_ms = now.duration_since(last_rebuild_time).as_millis();

            let is_finished = progress_update.map(|p| p >= 1.0).unwrap_or(false);

            let should_rebuild = pending_status_updates
                && (elapsed_ms >= 120 || folders_clone.len() < 150 || is_finished);

            if should_rebuild {
                last_rebuild_time = now;
                pending_status_updates = false;
            }

            let combined = if logs_changed {
                Some(logs.iter().cloned().collect::<String>())
            } else {
                None
            };

            let _ = ui_weak_log.upgrade_in_event_loop(move |ui| {
                if let Some(log_str) = combined {
                    ui.set_log_text(log_str.into());
                }

                if let Some(p) = progress_update {
                    ui.set_progress(p);
                }

                if should_rebuild {
                    let list_items = rebuild_visible_items(&folders_clone);
                    let new_model = Rc::new(VecModel::from(list_items));
                    ui.set_directories(new_model.into());
                }
            });
            thread::sleep(std::time::Duration::from_millis(16));
        }
    });

    ui.on_browse_folder(move || {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            SharedString::from(path.to_string_lossy().into_owned())
        } else {
            SharedString::new()
        }
    });

    ui.on_exit_app(move || {
        std::process::exit(0);
    });

    let ui_weak_cancel = ui_handle.clone();
    let cancel_flag_cancel = cancel_flag.clone();
    ui.on_cancel_operation(move || {
        cancel_flag_cancel.store(true, Ordering::Relaxed);
        if let Some(ui) = ui_weak_cancel.upgrade() {
            ui.set_status_msg("Cancellation requested...".into());
        }
    });

    let ui_weak_toggle = ui_handle.clone();
    let found_folders_toggle = found_folders.clone();
    ui.on_toggle_expand(move |id| {
        let mut folders = found_folders_toggle.lock().unwrap();
        if let Some(node) = folders.get_mut(id as usize) {
            node.is_expanded = !node.is_expanded;
        }
        let list_items = rebuild_visible_items(&folders);
        if let Some(ui) = ui_weak_toggle.upgrade() {
            let new_model = Rc::new(VecModel::from(list_items));
            ui.set_directories(new_model.into());
        }
    });

    // Windows Context Menu Integration Callback
    ui.on_toggle_context_menu(move |integrate| {
        if let Err(e) = set_registry_integration(integrate) {
            eprintln!("[!] Context menu integration failed: {}", e);
        }
    });

    // Auto-save Logs Callback
    ui.on_toggle_auto_save_logs(move |save| {
        AUTO_SAVE_LOGS.store(save, Ordering::Relaxed);
    });

    let ui_weak_scan = ui_handle.clone();
    let logger_scan = logger.clone();
    let found_folders_scan = found_folders.clone();
    let cancel_flag_scan = cancel_flag.clone();
    ui.on_search_folders(move || {
        let ui_weak = ui_weak_scan.clone();
        let logger = logger_scan.clone();
        let folders_state = found_folders_scan.clone();
        let cancel_flag_thread = cancel_flag_scan.clone();

        let ui = match ui_weak.upgrade() {
            Some(ui) => ui,
            None => return,
        };
        let folder_path = ui.get_selected_folder().to_string();
        let ignore_files = ui.get_ignore_files_text().to_string();
        let ignore_dirs = ui.get_ignore_list_text().to_string();
        let ignore_hidden = ui.get_ignore_hidden();
        let keep_system = ui.get_skip_system();
        let min_age_hours = ui.get_min_age_hours();
        let max_depth = ui.get_max_depth();
        let consider_empty_files_empty = ui.get_consider_empty_files_empty();
        let hide_search_errors = ui.get_hide_search_errors();

        ui.set_is_scanning(true);
        ui.set_status_msg("Scanning...".into());
        ui.set_progress(0.0);

        if folder_path.is_empty() {
            logger.log("Please select a folder first.");
            ui.set_is_scanning(false);
            return;
        }

        let path = PathBuf::from(folder_path);

        let settings = scanner::ScanSettings {
            ignore_files: ignore_files
                .split('\n')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            ignore_dirs: ignore_dirs
                .split('\n')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            ignore_hidden,
            keep_system,
            min_age_hours: min_age_hours as u32,
            max_depth,
            consider_empty_files_empty,
            hide_search_errors,
        };

        cancel_flag_thread.store(false, Ordering::Relaxed);

        let ui_weak_thread = ui.as_weak();
        thread::spawn(move || {
            logger.log(&format!(
                "[*] Scanning for empty directories in: {:?}",
                path
            ));

            match scanner::scan_empty_dirs(
                &path,
                &settings,
                &|msg| logger.log(msg),
                &cancel_flag_thread,
            ) {
                Ok(empty_dirs) => {
                    let count = empty_dirs.len();
                    let empty_count = empty_dirs.iter().filter(|d| d.status == 1).count();
                    logger.log(&format!(
                        "[+] Found {} empty directories ({} shown in tree).",
                        empty_count, count
                    ));

                    let folders_clone = {
                        let mut state = folders_state.lock().unwrap();
                        *state = empty_dirs;
                        state.clone()
                    };

                    let _ = ui_weak_thread.upgrade_in_event_loop(move |ui| {
                        let list_items = rebuild_visible_items(&folders_clone);
                        let new_model = Rc::new(VecModel::from(list_items));
                        ui.set_directories(new_model.into());
                        ui.set_empty_count(empty_count as i32);
                        ui.set_deleted_count(0);
                        ui.set_failed_count(0);
                        ui.set_is_scanning(false);
                        ui.set_status_msg(SharedString::from(format!(
                            "Found {} empty directories.",
                            empty_count
                        )));
                        ui.set_progress(1.0);
                    });
                }
                Err(e) => {
                    logger.log(&format!("[!] {}", e));
                    let status = if e.contains("cancelled") {
                        "Scan cancelled."
                    } else {
                        "Scan failed."
                    };
                    let _ = ui_weak_thread.upgrade_in_event_loop(move |ui| {
                        ui.set_is_scanning(false);
                        ui.set_status_msg(status.into());
                    });
                }
            }
        });
    });

    let ui_weak_del = ui_handle.clone();
    let logger_del = logger.clone();
    let found_folders_del = found_folders.clone();
    let cancel_flag_del = cancel_flag.clone();
    ui.on_delete_folders(move || {
        let ui_weak = ui_weak_del.clone();
        let logger = logger_del.clone();
        let folders_state = found_folders_del.clone();
        let cancel_flag_thread = cancel_flag_del.clone();

        let ui = match ui_weak.upgrade() {
            Some(ui) => ui,
            None => return,
        };
        let move_to_trash = ui.get_delete_mode() == 0;
        let ignore_errors = ui.get_ignore_errors();
        let pause_ms = ui.get_pause_ms();
        let ignore_files = ui.get_ignore_files_text().to_string();
        let consider_empty_files_empty = ui.get_consider_empty_files_empty();

        ui.set_is_deleting(true);
        ui.set_status_msg("Deleting...".into());
        ui.set_progress(0.0);

        let ui_weak_thread = ui.as_weak();
        cancel_flag_thread.store(false, Ordering::Relaxed);

        thread::spawn(move || {
            let mut dirs = {
                let state = folders_state.lock().unwrap();
                state.clone()
            };

            if dirs.is_empty() {
                let _ = ui_weak_thread.upgrade_in_event_loop(|ui| {
                    ui.set_is_deleting(false);
                });
                return;
            }

            let settings = scanner::DeleteSettings {
                move_to_trash,
                ignore_errors,
                pause_ms: pause_ms as u32,
                ignore_files: ignore_files
                    .split('\n')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                consider_empty_files_empty,
                dry_run: false, // UI deletes physically
            };

            logger.log("[*] Starting deletion process...");

            let (deleted, failed) = scanner::delete_empty_dirs(
                &mut dirs,
                &settings,
                &|msg, idx, stat| logger.status(msg, idx, stat),
                &|p| logger.progress(p),
                &cancel_flag_thread,
            );

            let was_cancelled = cancel_flag_thread.load(Ordering::Relaxed);
            logger.log(&format!(
                "[+] Deletion finished. Deleted: {}, Failed: {}",
                deleted, failed
            ));

            *folders_state.lock().unwrap() = dirs;

            let _ = ui_weak_thread.upgrade_in_event_loop(move |ui| {
                ui.set_deleted_count(deleted as i32);
                ui.set_failed_count(failed as i32);
                ui.set_is_deleting(false);
                if was_cancelled {
                    ui.set_status_msg("Deletion cancelled.".into());
                } else {
                    ui.set_status_msg("Deletion complete.".into());
                }
                ui.set_progress(1.0);
            });
        });
    });

    ui.run()
}
