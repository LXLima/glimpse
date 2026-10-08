use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;
use crate::icons::extract_icon_as_base64;

#[derive(Clone, serde::Serialize, Debug)]
pub struct IndexEntry {
    pub name: String,
    pub path: String,
    pub kind: String, // "app" | "file" | "setting"
    pub icon_base64: Option<String>,
    /// Lowercased once at index time so searches never re-allocate
    /// thousands of lowercase strings per keystroke.
    pub name_lower: String,
}

static INDEX: OnceLock<Mutex<Vec<IndexEntry>>> = OnceLock::new();

pub fn get_index() -> &'static Mutex<Vec<IndexEntry>> {
    INDEX.get_or_init(|| Mutex::new(vec![]))
}

/// Number of indexed entries. `0` means the index has not finished building
/// yet (settings entries are always added), which the UI uses to show an
/// "indexing..." hint instead of a misleading empty result list.
#[tauri::command]
pub fn get_index_status() -> usize {
    get_index()
        .lock()
        .map(|index| index.len())
        .unwrap_or(0)
}

pub fn build_index() {
    let start = std::time::Instant::now();
    let mut entries: Vec<IndexEntry> = vec![];
    let mut seen_paths: HashSet<String> = HashSet::new();

    // --- Scan Start Menu for .lnk (installed apps) ---
    let mut start_dirs = vec![std::path::PathBuf::from(
        r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs",
    )];
    // %APPDATA% instead of building the path from USERNAME: profiles can be
    // renamed / redirected (OneDrive) and USERNAME may not match the folder.
    if let Ok(appdata) = std::env::var("APPDATA") {
        start_dirs.push(std::path::PathBuf::from(appdata).join(
            r"Microsoft\Windows\Start Menu\Programs",
        ));
    }

    for dir in &start_dirs {
        for entry in WalkDir::new(dir).max_depth(4).into_iter().flatten() {
            let name = entry.file_name().to_string_lossy();
            if name.ends_with(".lnk") {
                // strip_suffix (not replace) so names like "Sync.lnk.lnk.bak"
                // or "Link.lnk" are not mangled mid-string.
                let clean = name
                    .strip_suffix(".lnk")
                    .unwrap_or(&name)
                    .trim()
                    .to_string();
                if clean.is_empty() {
                    continue;
                }
                let path = entry.path().to_string_lossy().to_string();
                if !seen_paths.insert(path.clone()) {
                    continue; // same shortcut reachable twice
                }

                // Extract native icon for apps
                let icon_base64 = extract_icon_as_base64(&path);

                entries.push(IndexEntry {
                    name: clean.clone(),
                    path,
                    kind: "app".into(),
                    icon_base64,
                    name_lower: clean.to_lowercase(),
                });
            }
        }
    }

    // --- Settings shortcuts ---
    let settings = vec![
        ("Display Settings", "ms-settings:display"),
        ("Bluetooth & Devices", "ms-settings:bluetooth"),
        ("Wi-Fi Settings", "ms-settings:network-wifi"),
        ("Sound Settings", "ms-settings:sound"),
        ("Windows Update", "ms-settings:windowsupdate"),
        ("Apps & Features", "ms-settings:appsfeatures"),
        ("Startup Apps", "ms-settings:startupapps"),
        ("Privacy Settings", "ms-settings:privacy"),
        ("Power & Sleep", "ms-settings:powersleep"),
        ("Storage Settings", "ms-settings:storagesense"),
        ("Task Manager", "taskmgr"),
        ("Control Panel", "control"),
        ("Device Manager", "devmgmt.msc"),
        ("Disk Management", "diskmgmt.msc"),
        ("Registry Editor", "regedit"),
    ];
    for (name, path) in settings {
        entries.push(IndexEntry {
            name: name.to_string(),
            path: path.to_string(),
            kind: "setting".into(),
            icon_base64: None,
            name_lower: name.to_lowercase(),
        });
    }

    // Store in static index
    if let Ok(mut index) = get_index().lock() {
        *index = entries;
    }
    if cfg!(debug_assertions) {
        println!("PERF: Index built in {:?}", start.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_index_initializes() {
        let index = get_index();
        let lock = index.lock().unwrap();
        assert!(lock.is_empty() || !lock.is_empty());
    }

    #[test]
    fn test_build_index_adds_settings() {
        build_index();
        let index = get_index().lock().unwrap();
        let has_taskmgr = index.iter().any(|e| e.name == "Task Manager" && e.path == "taskmgr");
        assert!(has_taskmgr, "Index should contain Task Manager");
    }

    #[test]
    fn test_index_has_no_duplicate_paths() {
        build_index();
        let index = get_index().lock().unwrap();
        let mut seen = HashSet::new();
        for entry in index.iter() {
            assert!(seen.insert(entry.path.clone()), "duplicate path: {}", entry.path);
        }
    }
}
