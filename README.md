# Deox

Deox scans a disk or folder on Windows 11 and shows what takes space and is probably unnecessary: temporary files, application caches, development artifacts such as `node_modules`, duplicate files and preinstalled bloatware. Every item comes with a reason and a safety level.

**Deox suggests, you decide.** Nothing is deleted without your confirmation, and deletion only moves items to the Recycle Bin. Everything runs locally; there is no network access and no telemetry.

Results open in **Disk Explorer**. Above is a directory tree of every folder and file, largest first, with sizes, shares, file counts and the time of the last change; arrow keys walk it. Below is a treemap: folders and files are rectangles sized by the space they take, tinted where Deox suggests removing them (Safe or Review). Selecting in one shows the same place in the other. Click a rectangle to open its folder, or send it to the **Cleanup** tab as a filter. The Cleanup tab lists the suggestions, and its title shows how many items are selected.

## Safety levels

| Level | Meaning |
|-------|---------|
| Safe | Regenerated automatically; preselected. |
| Review | Probably unneeded, but worth a look first; never preselected. |
| Info | Installed programs. Deox only reports them; removal goes through the program's own uninstaller. |

System and personal folders (Windows, Program Files, ProgramData, Documents, Pictures, Desktop, Videos, AppData\Roaming, OneDrive) are protected and are checked again at the moment of deletion. Links and junctions are never followed. An item that changed since the scan is skipped. Every action is appended to `%LOCALAPPDATA%\Deox\data\actions.log`.

"Preview only" is on by default in the confirmation window: it runs every check and reports what would happen without touching anything.

## Build and run

Requires a recent stable Rust toolchain on Windows.

```bash
cargo run --release -p deox
cargo test --workspace
```

## Extending the rules

Rules are data, not code. Edit [core/rules/categories.toml](core/rules/categories.toml) for cleanup rules and [core/rules/bloatware.toml](core/rules/bloatware.toml) for programs worth a second look, then rebuild (the files are embedded at compile time).

```toml
[[rule]]
name = "User temporary files"
category = "Temp"            # Temp, AppCache, DevArtifact
confidence = "Safe"          # Safe or Review
path_glob = "%TEMP%\*"      # case-insensitive; * crosses folders; %VAR% expands
min_age_days = 7             # leave anything touched more recently
reason = "Shown to the user next to the item."
```

With `kind = "dir"` a matching folder becomes a single finding, offered only when nothing around it changed for `min_age_days`. A rule that points into a protected folder has no effect unless the folder is on the allowlist in [core/src/protect.rs](core/src/protect.rs).
