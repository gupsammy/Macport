use std::collections::HashSet;
use std::process::Command;

use crate::model::{AppState, ProcessInfo};
use crate::utils::find_command;

const BUNDLE_ID: &str = "com.samarthgupta.portkiller";

pub fn maybe_notify_changes(state: &AppState, prev: &[ProcessInfo]) {
    if !state.config.notifications.enabled {
        return;
    }

    let prev_ports: HashSet<u16> = prev.iter().map(|p| p.port).collect();
    let curr_ports: HashSet<u16> = state.processes.iter().map(|p| p.port).collect();

    // Notify for added ports
    let added: Vec<u16> = curr_ports.difference(&prev_ports).copied().collect();
    for port in added {
        if let Some(process) = state.processes.iter().find(|p| p.port == port) {
            let (title, body) = format_notification(port, process, state, true);
            notify(&title, &body);
        }
    }

    // Notify for removed ports
    let removed: Vec<u16> = prev_ports.difference(&curr_ports).copied().collect();
    for port in removed {
        if let Some(process) = prev.iter().find(|p| p.port == port) {
            let (title, body) = format_notification(port, process, state, false);
            notify(&title, &body);
        }
    }
}

fn format_notification(
    port: u16,
    process: &ProcessInfo,
    state: &AppState,
    is_start: bool,
) -> (String, String) {
    let title = if is_start {
        format!("Port {} Started", port)
    } else {
        format!("Port {} Stopped", port)
    };

    let command = truncate_command(&process.command, 40);

    let body = if let Some(project) = state.project_cache.get(&process.pid) {
        format!("{} ({}) • {}", command, process.pid, project.name)
    } else {
        format!("{} ({})", command, process.pid)
    };

    (title, body)
}

fn truncate_command(command: &str, max_len: usize) -> String {
    if command.len() <= max_len {
        command.to_string()
    } else {
        format!("{}...", &command[..max_len.saturating_sub(3)])
    }
}

fn notify(title: &str, body: &str) {
    // Use terminal-notifier only - osascript fallback removed due to command injection risk
    // (malicious process names could contain AppleScript syntax)
    notify_with_terminal_notifier(title, body);
}

fn notify_with_terminal_notifier(title: &str, body: &str) {
    let cmd = find_command("terminal-notifier");
    // Check if terminal-notifier exists (find_command falls back to name if not found)
    if !std::path::Path::new(&cmd).exists() && Command::new(&cmd).arg("-help").output().is_err() {
        return;
    }

    let _ = Command::new(&cmd)
        .args([
            "-title", title, "-message", body, "-sender", BUNDLE_ID, "-sound", "Glass",
        ])
        .spawn();
}

/// Notify user that an update is available with clickable download link
pub fn notify_update_available(version: &str, download_url: &str) {
    let cmd = find_command("terminal-notifier");
    if !std::path::Path::new(&cmd).exists() && Command::new(&cmd).arg("-help").output().is_err() {
        return;
    }

    let title = format!("PortKiller v{} Available", version);
    let body = "Click to download the update";

    let _ = Command::new(&cmd)
        .args([
            "-title",
            &title,
            "-message",
            body,
            "-sender",
            BUNDLE_ID,
            "-sound",
            "Glass",
            "-open",
            download_url,
        ])
        .spawn();
}
