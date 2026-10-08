use std::process::Command;

/// Launch through `cmd /c start`, which understands URIs, shortcuts, scripts
/// and MMC snap-ins.
fn spawn_via_shell(path: &str) -> Result<(), String> {
    Command::new("cmd")
        .args(["/c", "start", "", path])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn launch_item(path: String) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Nothing to launch.".to_string());
    }

    let lower = path.to_lowercase();
    let needs_shell = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ms-settings:")
        || lower.starts_with("ms-store:")
        || lower.ends_with(".msc")
        || lower.ends_with(".lnk")
        || lower.ends_with(".url")
        || lower.ends_with(".bat")
        || lower.ends_with(".cmd")
        || lower == "taskmgr"
        || lower == "control"
        || lower == "regedit";

    if needs_shell {
        return spawn_via_shell(path);
    }

    let target = std::path::Path::new(path);
    if target.is_dir() {
        Command::new("explorer")
            .arg(target)
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    if !target.exists() {
        return Err(format!("Not found: {}", path));
    }

    // Run the executable from its own folder: plenty of portable apps break
    // when the working directory is inherited from the launcher.
    let mut command = Command::new(target);
    if let Some(parent) = target.parent() {
        command.current_dir(parent);
    }
    command.spawn().map_err(|e| format!("Could not start {}: {}", path, e))?;
    Ok(())
}

#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    let target = std::path::Path::new(path.trim());
    if !target.exists() {
        return Err(format!("Folder not found: {}", path));
    }
    Command::new("explorer")
        .arg(target)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn kill_process(name: String) -> Result<(), String> {
    let pid: u32 = name
        .trim()
        .parse()
        .map_err(|_| format!("Invalid process id: {}", name))?;
    if pid == 0 {
        return Err("Invalid process id: 0".to_string());
    }
    if pid == std::process::id() {
        return Err("Refusing to terminate Glimpse itself.".to_string());
    }

    // taskkill only reports failures through its exit status/stdout, so we have
    // to wait for it - spawning and dropping the child hides every error.
    let output = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("Could not run taskkill: {}", e))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = [stdout.trim(), stderr.trim()]
        .iter()
        .find(|s| !s.is_empty())
        .copied()
        .unwrap_or("unknown error");
    Err(format!("Could not terminate process {}: {}", pid, detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kill_rejects_garbage_pid() {
        assert!(kill_process("definitely-not-a-pid".to_string()).is_err());
    }

    #[test]
    fn test_kill_refuses_own_process() {
        assert!(kill_process(std::process::id().to_string()).is_err());
    }

    #[test]
    fn test_open_path_reports_missing_folder() {
        assert!(open_path(r"C:\definitely-missing\folder".to_string()).is_err());
    }
}
