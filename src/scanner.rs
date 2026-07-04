use ignore::WalkBuilder;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wildmatch::WildMatch;

#[derive(Clone, Debug)]
pub struct ScanSettings {
    pub ignore_files: Vec<String>,
    pub ignore_dirs: Vec<String>,
    pub ignore_hidden: bool,
    pub keep_system: bool,
    pub min_age_hours: u32,
    pub max_depth: i32,
    pub consider_empty_files_empty: bool,
    /// If true, access-error details (e.g., "permission denied") are collapsed
    /// into a single summary line instead of being logged individually.
    pub hide_search_errors: bool,
}

#[derive(Clone, Debug)]
pub struct DirectoryNode {
    pub path: Arc<Path>,
    pub name: String,
    pub depth: i32,
    pub status: i32, // 0: Normal, 1: Empty, 2: Deleted, 3: Protected, 4: Failed
    pub has_children: bool,
    pub is_expanded: bool,
    pub is_last_sibling: bool,
}

/// Messages coming out of the parallel filesystem walk: either a successfully
/// read entry, or an error (e.g., permission denied) that we still want to
/// surface to the user instead of silently dropping.
enum WalkMsg {
    Entry(ignore::DirEntry),
    Error(String),
}

/// Walks up from `start` toward `root`, adding every ancestor to `included`
/// so it appears in the tree view. Stops as soon as an ancestor is already
/// present (it and everything above it was already added).
fn add_ancestors(included: &mut FxHashSet<Arc<Path>>, start: &Path, root: &Path) {
    let mut parent = start.parent();
    while let Some(par) = parent {
        if !included.insert(Arc::from(par)) {
            break;
        }
        if par == root {
            break;
        }
        parent = par.parent();
    }
}

/// Checks the Windows "system" file attribute. On non-Windows platforms there
/// is no equivalent concept, so this always returns false there.
#[cfg(windows)]
fn is_system_dir(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    fs::metadata(path)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_SYSTEM != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_system_dir(_path: &Path) -> bool {
    false
}

pub fn scan_empty_dirs(
    root: &Path,
    settings: &ScanSettings,
    log: &dyn Fn(&str),
    cancel_flag: &Arc<AtomicBool>,
) -> Result<Vec<DirectoryNode>, String> {
    let file_matchers: Vec<WildMatch> = settings
        .ignore_files
        .iter()
        .map(|s| WildMatch::new(s))
        .collect();
    let dir_matchers: Vec<String> = settings
        .ignore_dirs
        .iter()
        .map(|s| s.replace('\\', "/").to_lowercase())
        .collect();

    let root_depth = root.components().count() as i32;

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false);

    let (tx, rx) = std::sync::mpsc::channel::<WalkMsg>();
    let cancel_walk = cancel_flag.clone();
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        let cancel_inner = cancel_walk.clone();
        Box::new(move |result| {
            // Abort parallel walk immediately if cancellation was requested
            if cancel_inner.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }
            match result {
                Ok(entry) => {
                    let _ = tx.send(WalkMsg::Entry(entry));
                }
                Err(err) => {
                    let _ = tx.send(WalkMsg::Error(err.to_string()));
                }
            }
            ignore::WalkState::Continue
        })
    });
    drop(tx);

    let mut entries: Vec<ignore::DirEntry> = Vec::new();
    let mut walk_errors: Vec<String> = Vec::new();
    for msg in rx {
        match msg {
            WalkMsg::Entry(e) => entries.push(e),
            WalkMsg::Error(e) => walk_errors.push(e),
        }
    }

    // Abort early before doing heavy metadata computation
    if cancel_flag.load(Ordering::Relaxed) {
        return Err("Operation cancelled by user".to_string());
    }

    if !walk_errors.is_empty() {
        if settings.hide_search_errors {
            log(&format!(
                "[!] {} item(s) skipped due to access errors.",
                walk_errors.len()
            ));
        } else {
            for e in &walk_errors {
                log(&format!("[!] Access error: {}", e));
            }
        }
    }

    // Parallel metadata gathering
    let mut entry_states: Vec<_> = entries
        .into_par_iter()
        .map(|entry| {
            let p = Arc::<Path>::from(entry.path());
            let depth = entry.depth();
            let file_type = entry.file_type();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let mut is_young_dir = false;
            let mut is_empty_file = false;

            let is_dir = file_type.as_ref().map(|ft| ft.is_dir()).unwrap_or(false);
            let is_file = file_type.as_ref().map(|ft| ft.is_file()).unwrap_or(false);

            if is_dir && settings.min_age_hours > 0 {
                if let Ok(metadata) = fs::metadata(&p)
                    && let Ok(created) = metadata.created().or_else(|_| metadata.modified())
                    && let Ok(elapsed) = created.elapsed()
                    && elapsed.as_secs() < (settings.min_age_hours as u64 * 3600)
                {
                    is_young_dir = true;
                }
            } else if is_file
                && settings.consider_empty_files_empty
                && let Ok(meta) = fs::metadata(&p)
                && meta.len() == 0
            {
                is_empty_file = true;
            }

            (
                p,
                depth,
                is_dir,
                is_file,
                file_name,
                is_young_dir,
                is_empty_file,
            )
        })
        .collect();

    // Sort bottom-up for correct analysis of cascading emptiness
    entry_states.sort_by_key(|(_, d, _, _, _, _, _)| std::cmp::Reverse(*d));

    // FxHashMap/FxHashSet: faster non-cryptographic hashing for Arc<Path> keys
    // than the default SipHash-based std collections, meaningful on large trees.
    let mut dir_status: FxHashMap<Arc<Path>, bool> = FxHashMap::default();
    let mut included_dirs: FxHashSet<Arc<Path>> = FxHashSet::default();
    let mut empty_dirs_found: FxHashSet<Arc<Path>> = FxHashSet::default();
    let mut protected_dirs: FxHashSet<Arc<Path>> = FxHashSet::default();

    for (p, depth, is_dir, is_file, child_name, is_young_dir, is_empty_file) in entry_states {
        // Stop sweep reduction immediately if cancellation is requested
        if cancel_flag.load(Ordering::Relaxed) {
            return Err("Operation cancelled by user".to_string());
        }

        if settings.max_depth >= 0 && (depth as i32) > settings.max_depth {
            if let Some(parent) = p.parent() {
                dir_status.insert(Arc::from(parent), false);
            }
            continue;
        }

        if is_dir {
            let dir_name = &child_name;
            let mut is_empty = *dir_status.get(&p).unwrap_or(&true);
            let mut is_protected = false;

            if is_empty {
                // Skip building the lowercase path string entirely when there is
                // nothing to match against (common case: empty ignore list).
                let matches_ignore_dir = if dir_matchers.is_empty() {
                    false
                } else {
                    let full_path_lower = p.to_string_lossy().replace('\\', "/").to_lowercase();
                    dir_matchers.iter().any(|m| full_path_lower.contains(m))
                };
                let matches_hidden = settings.ignore_hidden && dir_name.starts_with('.');
                let matches_system = settings.keep_system && is_system_dir(&p);

                if matches_ignore_dir || matches_hidden || matches_system || is_young_dir {
                    is_empty = false;
                    is_protected = true;
                }
            }

            dir_status.insert(p.clone(), is_empty);

            if p.as_ref() != root {
                if is_empty {
                    empty_dirs_found.insert(p.clone());
                    included_dirs.insert(p.clone());
                    add_ancestors(&mut included_dirs, &p, root);
                } else {
                    if let Some(parent) = p.parent() {
                        dir_status.insert(Arc::from(parent), false);
                    }
                    // A directory that would otherwise be empty, but is protected
                    // (ignore list / hidden / system / too young), is now surfaced
                    // in the tree with status 3 instead of silently vanishing.
                    if is_protected {
                        protected_dirs.insert(p.clone());
                        included_dirs.insert(p.clone());
                        add_ancestors(&mut included_dirs, &p, root);
                    }
                }
            }
        } else if is_file
            && !is_empty_file
            && !file_matchers.iter().any(|m| m.matches(&child_name))
            && let Some(parent) = p.parent()
        {
            dir_status.insert(Arc::from(parent), false);
        } else if !is_dir
            && !is_file
            && !file_matchers.iter().any(|m| m.matches(&child_name))
            && let Some(parent) = p.parent()
        {
            // Symlinks (and any other special entry type) fall through both
            // is_dir and is_file when the walker doesn't follow links. Treating
            // them as real content keeps scan-time and delete-time behavior consistent.
            dir_status.insert(Arc::from(parent), false);
        }
    }

    let mut sorted_paths: Vec<Arc<Path>> = included_dirs.into_iter().collect();
    sorted_paths.sort();

    let mut result = Vec::new();
    for p in sorted_paths {
        let is_empty = empty_dirs_found.contains(&p);
        let is_protected = protected_dirs.contains(&p);
        let depth = (p.components().count() as i32) - root_depth;
        let name = if p.as_ref() == root {
            p.to_string_lossy().into_owned()
        } else {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        result.push(DirectoryNode {
            path: p,
            name,
            depth,
            status: if is_empty {
                1
            } else if is_protected {
                3
            } else {
                0
            },
            has_children: false,
            is_expanded: true,
            is_last_sibling: false,
        });
    }

    // Determine tree relationships
    for i in 0..result.len() {
        if i + 1 < result.len() && result[i + 1].depth > result[i].depth {
            result[i].has_children = true;
        }

        let mut last = true;
        for j in (i + 1)..result.len() {
            if result[j].depth < result[i].depth {
                break;
            }
            if result[j].depth == result[i].depth {
                last = false;
                break;
            }
        }
        result[i].is_last_sibling = last;
    }

    Ok(result)
}

#[derive(Clone, Debug)]
pub struct DeleteSettings {
    pub move_to_trash: bool,
    pub ignore_errors: bool,
    pub pause_ms: u32,
    pub ignore_files: Vec<String>,
    pub consider_empty_files_empty: bool,
    pub dry_run: bool,
}

// Helper function to safely clean up directories and protect against symbolic link pitfalls
fn clean_and_verify_empty(
    dir: &Path,
    settings: &DeleteSettings,
    file_matchers: &[WildMatch],
) -> Result<bool, String> {
    // 1. Check if the directory itself is a symlink/junction
    let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    if meta.is_symlink() {
        // If it is a symlink, treat it as an "empty" element and allow deleting the link itself
        return Ok(true);
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for child in entries.flatten() {
            let cp = child.path();
            let meta = fs::symlink_metadata(&cp);
            let is_symlink = meta.as_ref().map(|m| m.is_symlink()).unwrap_or(false);

            if is_symlink || cp.is_file() {
                let child_name = cp.file_name().unwrap_or_default().to_string_lossy();
                let is_ignored = file_matchers.iter().any(|m| m.matches(&child_name));
                let is_empty_file = !is_symlink
                    && settings.consider_empty_files_empty
                    && fs::metadata(&cp).map(|m| m.len() == 0).unwrap_or(false);

                if (is_ignored || is_empty_file) && !settings.dry_run {
                    // Symlinks are safely deleted via remove_file/remove_dir without traversing
                    let _ = fs::remove_file(&cp).or_else(|_| fs::remove_dir(&cp));
                }
            }
        }
    }

    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_none() {
                Ok(true)
            } else {
                Err("Directory is not empty (contains non-ignored files)".to_string())
            }
        }
        Err(e) => Err(format!("Failed to verify directory: {}", e)),
    }
}

pub fn delete_empty_dirs<F, P>(
    nodes: &mut [DirectoryNode],
    settings: &DeleteSettings,
    log: &F,
    progress_cb: &P,
    cancel_flag: &Arc<AtomicBool>,
) -> (usize, usize)
where
    F: Fn(&str, usize, i32),
    P: Fn(f32),
{
    let mut deleted = 0;
    let mut failed = 0;
    let mut processed_items = 0;

    let mut depths: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    let mut total_items = 0;

    for (i, node) in nodes.iter().enumerate() {
        if node.status == 1 {
            depths.entry(node.depth).or_default().push(i);
            total_items += 1;
        }
    }

    let file_matchers: Vec<WildMatch> = settings
        .ignore_files
        .iter()
        .map(|s| WildMatch::new(s))
        .collect();

    for (_depth, indices) in depths.into_iter().rev() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        // Rayon Optimization: if there is only 1 directory on this level, delete it sequentially to avoid thread spawning overhead
        if settings.pause_ms == 0 && indices.len() > 1 {
            let results: Vec<_> = {
                let nodes_ref: &[DirectoryNode] = nodes;
                let cancel_inner = cancel_flag.clone();
                indices
                    .par_iter()
                    .map(|&i| {
                        if cancel_inner.load(Ordering::Relaxed) {
                            return (i, 4, "Cancelled".to_string(), None);
                        }
                        let dir = &nodes_ref[i].path;

                        if settings.dry_run {
                            return (
                                i,
                                2,
                                format!("[Dry-Run] Would delete: {}", dir.display()),
                                None,
                            );
                        }

                        match clean_and_verify_empty(dir, settings, &file_matchers) {
                            Ok(true) => {
                                let meta = fs::symlink_metadata(dir);
                                let is_symlink = meta.map(|m| m.is_symlink()).unwrap_or(false);

                                let res = if settings.move_to_trash && !is_symlink {
                                    trash::delete(dir).map_err(|e| e.to_string())
                                } else {
                                    if is_symlink {
                                        fs::remove_dir(dir)
                                            .or_else(|_| fs::remove_file(dir))
                                            .map_err(|e| e.to_string())
                                    } else {
                                        fs::remove_dir(dir).map_err(|e| e.to_string())
                                    }
                                };

                                match res {
                                    Ok(_) => (i, 2, format!("Deleted: {}", dir.display()), None),
                                    Err(e) => (
                                        i,
                                        4,
                                        format!("Failed to delete {}: {}", dir.display(), e),
                                        Some(e),
                                    ),
                                }
                            }
                            Ok(false) => (
                                i,
                                4,
                                format!(
                                    "Failed to delete {}: Directory is not empty",
                                    dir.display()
                                ),
                                None,
                            ),
                            Err(e) => (
                                i,
                                4,
                                format!("Failed to delete {}: {}", dir.display(), e),
                                None,
                            ),
                        }
                    })
                    .collect()
            };

            let mut abort = false;
            for (i, status, msg, _err) in results {
                if cancel_flag.load(Ordering::Relaxed) {
                    abort = true;
                    break;
                }
                if msg == "Cancelled" {
                    continue;
                }

                processed_items += 1;
                progress_cb(processed_items as f32 / total_items as f32);

                log(&msg, i, status);
                nodes[i].status = status;

                if status == 2 {
                    deleted += 1;
                } else {
                    failed += 1;
                    if !settings.ignore_errors {
                        log("Aborting deletion due to error.", i, 4);
                        abort = true;
                        break;
                    }
                }
            }
            if abort {
                break;
            }
        } else {
            // Slow sequential path with pause, or single directory fallback
            let mut abort = false;
            for &i in &indices {
                if cancel_flag.load(Ordering::Relaxed) {
                    abort = true;
                    break;
                }

                let dir = nodes[i].path.clone();

                if settings.dry_run {
                    log(&format!("[Dry-Run] Would delete: {}", dir.display()), i, 2);
                    nodes[i].status = 2;
                    deleted += 1;
                    processed_items += 1;
                    progress_cb(processed_items as f32 / total_items as f32);
                    continue;
                }

                match clean_and_verify_empty(&dir, settings, &file_matchers) {
                    Ok(true) => {
                        let meta = fs::symlink_metadata(&dir);
                        let is_symlink = meta.map(|m| m.is_symlink()).unwrap_or(false);

                        let res = if settings.move_to_trash && !is_symlink {
                            trash::delete(&dir).map_err(|e| e.to_string())
                        } else {
                            if is_symlink {
                                fs::remove_dir(&dir)
                                    .or_else(|_| fs::remove_file(&dir))
                                    .map_err(|e| e.to_string())
                            } else {
                                fs::remove_dir(&dir).map_err(|e| e.to_string())
                            }
                        };

                        match res {
                            Ok(_) => {
                                log(&format!("Deleted: {}", dir.display()), i, 2);
                                nodes[i].status = 2;
                                deleted += 1;
                            }
                            Err(e) => {
                                log(&format!("Failed to delete {}: {}", dir.display(), e), i, 4);
                                nodes[i].status = 4;
                                failed += 1;
                                if !settings.ignore_errors {
                                    log("Aborting deletion due to error.", i, 4);
                                    abort = true;
                                    break;
                                }
                            }
                        }
                    }
                    _ => {
                        log(
                            &format!(
                                "Failed to delete {}: Not empty or inaccessible",
                                dir.display()
                            ),
                            i,
                            4,
                        );
                        nodes[i].status = 4;
                        failed += 1;
                        if !settings.ignore_errors {
                            log("Aborting deletion due to error.", i, 4);
                            abort = true;
                            break;
                        }
                    }
                }

                processed_items += 1;
                progress_cb(processed_items as f32 / total_items as f32);

                if settings.pause_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(settings.pause_ms as u64));
                }
            }
            if abort {
                break;
            }
        }
    }

    (deleted, failed)
}
