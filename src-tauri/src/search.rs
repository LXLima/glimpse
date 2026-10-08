use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use crate::indexer::{get_index};
use meval::eval_str;
use tauri::Manager;
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible, GetWindowThreadProcessId};
use windows::Win32::Foundation::{HWND, LPARAM, BOOL};

/// Hard cap on query length to bound the work per keystroke (chars, never
/// bytes - slicing bytes could split a UTF-8 sequence and panic).
const MAX_QUERY_CHARS: usize = 512;
const MAX_INDEX_RESULTS: usize = 12;
const MAX_KILL_RESULTS: usize = 5;

struct WindowInfo {
    title: String,
    pid: u32,
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if IsWindowVisible(hwnd).as_bool() {
        let length = GetWindowTextLengthW(hwnd);
        if length > 0 {
            let mut buffer = vec![0u16; (length + 1) as usize];
            GetWindowTextW(hwnd, &mut buffer);
            if let Ok(title) = String::from_utf16(&buffer[..length as usize]) {
                let title = title.trim_end_matches('\0').to_string();
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                // Never offer to kill ourselves (the palette and the settings
                // window both live in this process).
                let is_own_window = pid == 0 || pid == std::process::id();
                if !is_own_window && !title.is_empty() && title != "Program Manager" {
                    let windows = &mut *(lparam.0 as *mut Vec<WindowInfo>);
                    windows.push(WindowInfo { title, pid });
                }
            }
        }
    }
    BOOL(1)
}

fn get_open_windows() -> Vec<WindowInfo> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

#[derive(serde::Serialize)]
pub struct SearchResult {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub score: i64,
    pub icon_base64: Option<String>,
}

/// The actual search. Kept free of Tauri types so it can be unit tested.
pub fn perform_search(config_dir: Option<std::path::PathBuf>, query: &str) -> Vec<SearchResult> {
    let query = query.trim();
    if query.is_empty() {
        return vec![];
    }
    // Bound the work per keystroke (chars, never bytes - slicing bytes could
    // split a UTF-8 sequence and panic).
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();

    let matcher = SkimMatcherV2::default();
    let q = query.to_lowercase();

    let index = get_index().lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // Score first, clone icons only for the rows we actually return: icons are
    // ~1-3 KB of base64 each and the index can hold thousands of entries.
    // Names are pre-lowercased at index time (`name_lower`) - allocating a
    // lowercase copy per entry per keystroke used to dominate search time.
    let mut scored: Vec<(&crate::indexer::IndexEntry, i64)> = index
        .iter()
        .filter_map(|entry| {
            let score = matcher.fuzzy_match(&entry.name_lower, &q)?;
            Some((entry, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.name.cmp(&b.0.name)));
    scored.truncate(MAX_INDEX_RESULTS);

    let mut results: Vec<SearchResult> = scored
        .into_iter()
        .map(|(entry, score)| SearchResult {
            name: entry.name.clone(),
            path: entry.path.clone(),
            kind: entry.kind.clone(),
            score,
            icon_base64: entry.icon_base64.clone(),
        })
        .collect();
    drop(index);

    if q.contains("config") || q.contains("settings") || q.contains("hotkey") {
        if let Some(config_dir) = config_dir {
            results.insert(0, SearchResult {
                name: "Open Settings / Config Folder".to_string(),
                path: config_dir.to_string_lossy().to_string(),
                kind: "setting".to_string(),
                score: i64::MAX - 1,
                icon_base64: None,
            });
        }
    }

    if q.starts_with("kill ") {
        let search_term = q.strip_prefix("kill ").unwrap().trim();
        if !search_term.is_empty() {
            let open_windows = get_open_windows();
            let mut kill_results = vec![];
            for win in open_windows {
                if let Some(score) = matcher.fuzzy_match(&win.title.to_lowercase(), search_term) {
                    kill_results.push(SearchResult {
                        name: format!("Kill Application: {}", win.title),
                        path: win.pid.to_string(),
                        kind: "kill".to_string(),
                        score: i64::MAX - 100 + score,
                        icon_base64: None,
                    });
                }
            }
            kill_results.sort_by(|a, b| b.score.cmp(&a.score));
            // Keep top window matches to prevent clutter
            kill_results.truncate(MAX_KILL_RESULTS);
            for result in kill_results.into_iter().rev() {
                results.insert(0, result);
            }
        }
    }

    if let Ok(res) = eval_str(&query) {
        if query.chars().any(|c| "+-*/()^".contains(c)) {
            results.insert(0, SearchResult {
                name: format!("= {}", res),
                path: res.to_string(),
                kind: "math".to_string(),
                score: i64::MAX,
                icon_base64: None,
            });
        }
    }

    results
}

#[tauri::command]
pub fn search_items(app: tauri::AppHandle, query: String) -> Vec<SearchResult> {
    let start = std::time::Instant::now();
    let config_dir = app.path().app_config_dir().ok();
    let results = perform_search(config_dir, &query);

    if cfg!(debug_assertions) {
        println!("PERF: Search for '{}' took {:?}", query, start.elapsed());
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_finds_settings() {
        crate::indexer::build_index();
        let results = perform_search(None, "task");
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.name == "Task Manager"));
    }

    #[test]
    fn test_blank_query_returns_nothing() {
        assert!(perform_search(None, "   ").is_empty());
    }

    #[test]
    fn test_math_expression_is_detected() {
        crate::indexer::build_index();
        let results = perform_search(None, "21*2");
        assert_eq!(results.first().map(|r| r.kind.as_str()), Some("math"));
        assert_eq!(results.first().map(|r| r.name.as_str()), Some("= 42"));
    }

    #[test]
    fn test_very_long_query_does_not_panic() {
        crate::indexer::build_index();
        let query = "a".repeat(5_000);
        let _ = perform_search(None, &query);
    }
}
