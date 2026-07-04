// Hide the console window on Windows when compiling in release mode
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

slint::include_modules!();

mod scanner;

use clap::Parser;
use slint::{ModelRc, SharedString, VecModel};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

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
        if let Some(sender) = &self.sender {
            let _ = sender.send(LogEvent::Msg(format!("{}\n", msg)));
        } else {
            println!("{}", msg);
        }
    }

    pub fn status(&self, msg: &str, index: usize, status: i32) {
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

fn rebuild_visible_items(folders: &[scanner::DirectoryNode]) -> Vec<DirectoryItem> {
    let mut result = Vec::new();
    let mut hide_depth = i32::MAX;
    // Grows on demand instead of the old fixed `[false; 256]` buffer, which
    // silently mis-rendered the tree lines for anything nested deeper than
    // 256 levels (e.g. some node_modules-style trees).
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
                    tree_lines_vec.push(1); // vertical
                } else {
                    tree_lines_vec.push(0); // empty
                }
            }
            if node.depth > 0 {
                if node.is_last_sibling {
                    tree_lines_vec.push(3); // L-junction
                } else {
                    tree_lines_vec.push(2); // T-junction
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
    let cli = Cli::parse();

    // Full console mode (CLI) without launching the graphical user interface
    if cli.quiet || cli.delete {
        let dummy_cancel = Arc::new(AtomicBool::new(false));
        if let Some(path_str) = cli.path {
            let path = PathBuf::from(path_str);
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

            if !cli.quiet {
                println!("[*] Scanning: {:?}", path);
            }
            match scanner::scan_empty_dirs(
                &path,
                &scan_settings,
                &|msg| {
                    if !cli.quiet {
                        println!("{}", msg);
                    }
                },
                &dummy_cancel,
            ) {
                Ok(mut dirs) => {
                    let empty_count = dirs.iter().filter(|d| d.status == 1).count();
                    if !cli.quiet {
                        println!("[+] Found {} empty directories.", empty_count);
                    }
                    if cli.delete && empty_count > 0 {
                        if !cli.quiet {
                            println!("[*] Deleting...");
                        }
                        let delete_settings = scanner::DeleteSettings {
                            move_to_trash: !cli.delete_permanently,
                            ignore_errors: true,
                            pause_ms: 0,
                            ignore_files: scan_settings.ignore_files.clone(),
                            consider_empty_files_empty: cli.consider_empty_files_empty,
                        };
                        let (deleted, failed) = scanner::delete_empty_dirs(
                            &mut dirs,
                            &delete_settings,
                            &|msg, _, _| {
                                if !cli.quiet {
                                    println!("{}", msg);
                                }
                            },
                            &|_| {},
                            &dummy_cancel,
                        );
                        if !cli.quiet {
                            println!("[+] Finished. Deleted: {}, Failed: {}", deleted, failed);
                        }
                    }
                }
                Err(e) => {
                    if !cli.quiet {
                        eprintln!("[!] Error: {}", e);
                    }
                }
            }
        } else {
            eprintln!("[!] Error: Path is required for CLI/Quiet mode.");
        }
        return Ok(());
    }

    let ui = AppWindow::new()?;
    let ui_handle = ui.as_weak();

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

            let mut process_event = |e: LogEvent| match e {
                LogEvent::Msg(msg) => logs.push_back(msg),
                LogEvent::StatusChange(index, status) => {
                    status_updates.push((index, status));
                    pending_status_updates = true;
                }
                LogEvent::Progress(p) => progress_update = Some(p),
            };

            process_event(evt);

            // Drain remaining events in the channel queue
            while let Ok(m) = log_rx.try_recv() {
                process_event(m);
            }

            while logs.len() > 250 {
                logs.pop_front();
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

            // Throttle tree view model rebuilding
            let now = std::time::Instant::now();
            let elapsed_ms = now.duration_since(last_rebuild_time).as_millis();

            let is_finished = progress_update.map(|p| p >= 1.0).unwrap_or(false);

            // Rebuild tree if status changed AND either 120ms passed, or we finished, or total size is small (< 150 items)
            let should_rebuild = pending_status_updates
                && (elapsed_ms >= 120 || folders_clone.len() < 150 || is_finished);

            if should_rebuild {
                last_rebuild_time = now;
                pending_status_updates = false;
            }

            let combined = logs.iter().cloned().collect::<String>();
            let _ = ui_weak_log.upgrade_in_event_loop(move |ui| {
                ui.set_log_text(combined.into());

                if let Some(p) = progress_update {
                    ui.set_progress(p);
                }

                if should_rebuild {
                    let list_items = rebuild_visible_items(&folders_clone);
                    let new_model = Rc::new(VecModel::from(list_items));
                    ui.set_directories(new_model.into());
                }
            });
            thread::sleep(std::time::Duration::from_millis(16)); // ~60fps target
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
