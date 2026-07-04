use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
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
}

#[derive(Clone, Debug)]
pub struct DirectoryNode {
    pub path: PathBuf,
    pub name: String,
    pub depth: i32,
    pub status: i32,
    pub has_children: bool,
    pub is_expanded: bool,
    pub is_last_sibling: bool,
}

pub fn scan_empty_dirs<F: Fn(&str)>(
    root: &Path,
    settings: &ScanSettings,
    _log: &F,
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

    let (tx, rx) = std::sync::mpsc::channel();
    builder.build_parallel().run(|| {
        let tx = tx.clone();
        Box::new(move |result| {
            if let Ok(entry) = result {
                let _ = tx.send(entry);
            }
            ignore::WalkState::Continue
        })
    });
    drop(tx);

    let entries: Vec<ignore::DirEntry> = rx.into_iter().collect();

    // Parallel metadata gathering
    let mut entry_states: Vec<_> = entries
        .into_par_iter()
        .map(|entry| {
            let p = entry.path().to_path_buf();
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

    let mut dir_status: std::collections::HashMap<PathBuf, bool> = std::collections::HashMap::new();
    let mut included_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut empty_dirs_found: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for (p, depth, is_dir, is_file, child_name, is_young_dir, is_empty_file) in entry_states {
        if settings.max_depth >= 0 && (depth as i32) > settings.max_depth {
            if let Some(parent) = p.parent() {
                dir_status.insert(parent.to_path_buf(), false);
            }
            continue;
        }

        if is_dir {
            let dir_name = &child_name;
            let full_path_lower = p.to_string_lossy().replace('\\', "/").to_lowercase();

            let mut is_empty = *dir_status.get(&p).unwrap_or(&true);

            if is_empty
                && (dir_matchers.iter().any(|m| full_path_lower.contains(m))
                    || ((settings.ignore_hidden || settings.keep_system)
                        && dir_name.starts_with('.'))
                    || is_young_dir)
            {
                is_empty = false;
            }

            dir_status.insert(p.clone(), is_empty);

            if is_empty && p != root {
                empty_dirs_found.insert(p.clone());
                included_dirs.insert(p.clone());

                // Optimization: stop parent traversal early if it was already added to the set
                let mut parent = p.parent();
                while let Some(par) = parent {
                    if !included_dirs.insert(par.to_path_buf()) {
                        break;
                    }
                    if par == root {
                        break;
                    }
                    parent = par.parent();
                }
            } else if !is_empty
                && p != root
                && let Some(parent) = p.parent()
            {
                dir_status.insert(parent.to_path_buf(), false);
            }
        } else if is_file
            && !is_empty_file
            && !file_matchers.iter().any(|m| m.matches(&child_name))
            && let Some(parent) = p.parent()
        {
            dir_status.insert(parent.to_path_buf(), false);
        }
    }

    let mut sorted_paths: Vec<PathBuf> = included_dirs.into_iter().collect();
    sorted_paths.sort();

    let mut result = Vec::new();
    for p in sorted_paths {
        let is_empty = empty_dirs_found.contains(&p);
        let depth = (p.components().count() as i32) - root_depth;
        let name = if p == root {
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
            status: if is_empty { 1 } else { 0 },
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
        // If it's a symlink, treat it as an "empty" element so we safely delete the link, not its contents
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

                if is_ignored || is_empty_file {
                    // Symlinks inside the dir are safely deleted without traversing them
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
            total_items += 1; // Count total files for progress calculation
        }
    }

    let file_matchers: Vec<WildMatch> = settings
        .ignore_files
        .iter()
        .map(|s| WildMatch::new(s))
        .collect();

    for (_depth, indices) in depths.into_iter().rev() {
        if settings.pause_ms == 0 {
            // Fast parallel path
            let results: Vec<_> = {
                // Borrow immutably in a restricted scope to satisfy Sync bounds
                let nodes_ref: &[DirectoryNode] = nodes;
                indices
                    .par_iter()
                    .map(|&i| {
                        let dir = &nodes_ref[i].path;

                        match clean_and_verify_empty(dir, settings, &file_matchers) {
                            Ok(true) => {
                                let meta = fs::symlink_metadata(dir);
                                let is_symlink = meta.map(|m| m.is_symlink()).unwrap_or(false);

                                let res = if settings.move_to_trash && !is_symlink {
                                    trash::delete(dir).map_err(|e| e.to_string())
                                } else {
                                    // If it's a symlink, aggressively use fs::remove_dir/file to avoid trashing its real contents
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
            }; // Immutable borrow of 'nodes' drops here

            let mut abort = false;
            for (i, status, msg, _err) in results {
                processed_items += 1;
                progress_cb(processed_items as f32 / total_items as f32);

                log(&msg, i, status);
                nodes[i].status = status; // Safe to mutate sequentially now

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
            // Slow sequential path with pause
            let mut abort = false;
            for &i in &indices {
                let dir = nodes[i].path.clone(); // Clone path to detach lifetime from 'nodes'

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
                                nodes[i].status = 2; // Mutate status safely
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
