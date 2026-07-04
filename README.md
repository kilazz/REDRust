# RED (Remove Empty Directory)
RED is a fast, parallelized GUI tool written in Rust for finding and cleaning up empty directories on your filesystem. Built with [Slint](https://slint.dev/) for a lightweight, native user interface and [Rayon](https://github.com/rayon-rs/rayon) for high-performance parallel scanning.

## Features
- **Fast Parallel Scanning:** Utilizes multi-threading to traverse the filesystem quickly.
- **Interactive UI:** A responsive tree-view GUI to review empty directories before deleting them.
- **Smart Filtering:** 
  - Ignore specific files or directories by name/pattern.
  - Option to skip hidden or system directories.
  - Option to consider directories with only "empty files" (0 bytes) as empty.
  - Configure minimum age for directories to prevent deleting newly created ones.
  - Set maximum scan depth.
- **Safe Deletion:**
  - Move to Trash/Recycle Bin by default (uses the `trash` crate).
  - Option to permanently delete directories.
  - Configurable pause between deletions.

## Build Instructions
To compile the project from source, clone the repository and run:

```bash
cargo build --release
```
