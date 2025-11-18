use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender};
use log::{error, warn};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getpgid};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::event::{Event, StartCause};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const SIGTERM_GRACE: Duration = Duration::from_secs(2);
const SIGKILL_GRACE: Duration = Duration::from_secs(1);
const POLL_STEP: Duration = Duration::from_millis(200);
const MAX_TOOLTIP_ENTRIES: usize = 5;
const MENU_ID_KILL_ALL: &str = "kill_all";
const MENU_ID_QUIT: &str = "quit";
const MENU_ID_EDIT_CONFIG: &str = "edit_config";
const MENU_ID_PROCESS_PREFIX: &str = "process_";
const MENU_ID_DOCKER_STOP_PREFIX: &str = "docker_stop_";
const MENU_ID_BREW_STOP_PREFIX: &str = "brew_stop_";
const MENU_ID_EMPTY: &str = "empty";
const MENU_ID_SNOOZE_30M: &str = "snooze_30m";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Config {
    port_ranges: Vec<(u16, u16)>,
    #[serde(default = "default_inactive_color")]
    inactive_color: (u8, u8, u8),
    #[serde(default = "default_active_color")]
    active_color: (u8, u8, u8),
    #[serde(default = "default_notifications_enabled")]
    notifications_enabled: bool,
}

fn default_inactive_color() -> (u8, u8, u8) {
    (255, 255, 255) // White - matches other macOS menu bar icons
}

fn default_active_color() -> (u8, u8, u8) {
    (255, 69, 58) // Red - SF Symbols system red color
}

fn default_notifications_enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port_ranges: vec![
                (3000, 3010),   // Node.js, React, Next.js, Vite
                (3306, 3306),   // MySQL
                (4000, 4010),   // Alternative Node servers
                (5001, 5010),   // Flask, general dev servers (excluding 5000)
                (5173, 5173),   // Vite default
                (5432, 5432),   // PostgreSQL
                (6379, 6380),   // Redis (6379 default, 6380 for testing)
                (8000, 8100),   // Django, Python HTTP servers
                (8080, 8090),   // Tomcat, alternative HTTP
                (9000, 9010),   // Various dev tools
                (27017, 27017), // MongoDB
            ],
            inactive_color: default_inactive_color(),
            active_color: default_active_color(),
            notifications_enabled: default_notifications_enabled(),
        }
    }
}

fn get_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".macport.json")
}

fn load_or_create_config() -> Result<Config> {
    let path = get_config_path();

    if path.exists() {
        let content = fs::read_to_string(&path).context("failed to read config file")?;
        serde_json::from_str(&content).context("failed to parse config file")
    } else {
        let config = Config::default();
        save_config(&config)?;
        Ok(config)
    }
}

fn save_config(config: &Config) -> Result<()> {
    let path = get_config_path();
    let content = serde_json::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(&path, content).context("failed to write config file")?;
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();

    let config = load_or_create_config().context("failed to load configuration")?;

    let mut state = AppState {
        processes: Vec::new(),
        last_feedback: None,
        config: config.clone(),
        project_cache: HashMap::new(),
        docker_port_map: HashMap::new(),
        brew_services_map: HashMap::new(),
        snooze_until: None,
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("failed to create event loop")?;
    let proxy = event_loop.create_proxy();
    let (worker_tx, worker_rx) = crossbeam_channel::unbounded();

    let _monitor_thread = spawn_monitor_thread(proxy.clone(), config.clone());
    let _menu_thread = spawn_menu_listener(proxy.clone());
    let _worker = spawn_worker(worker_rx, proxy.clone());

    let icon = create_icon(config.inactive_color).context("failed to create tray icon image")?;
    let initial_menu = build_menu_with_context(&state).context("failed to build initial menu")?;
    let tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(initial_menu))
        .with_tooltip("No dev port listeners detected.")
        .build()
        .context("failed to create tray icon")?;
    tray_icon
        .set_visible(true)
        .context("failed to show tray icon")?;

    update_tray_display(&tray_icon, &state);
    let mut worker_sender: Option<Sender<WorkerCommand>> = Some(worker_tx);

    #[allow(deprecated)]
    let run_result = event_loop.run(move |event, event_loop| match event {
        Event::NewEvents(StartCause::Init) => {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
        Event::UserEvent(user_event) => match user_event {
            UserEvent::ProcessesUpdated(processes) => {
                let prev = std::mem::take(&mut state.processes);
                state.processes = processes;
                // Refresh docker port map when we have listeners
                state.docker_port_map = query_docker_port_map().unwrap_or_default();
                // Refresh brew services map when we have listeners
                state.brew_services_map = query_brew_services_map().unwrap_or_default();
                // Derive project info in best-effort mode
                refresh_projects_for(&mut state);
                // Clean up stale cache entries for terminated processes
                let active_pids: HashSet<i32> = state.processes.iter().map(|p| p.pid).collect();
                state
                    .project_cache
                    .retain(|pid, _| active_pids.contains(pid));
                // Notifications on change
                maybe_notify_changes(&state, &prev);
                sync_menu_with_context(&tray_icon, &state);
                update_tray_display(&tray_icon, &state);
            }
            UserEvent::MenuAction(action) => match action {
                MenuAction::EditConfig => {
                    let config_path = get_config_path();
                    let path_str = config_path.to_string_lossy().to_string();
                    let _ = Command::new("open").arg("-t").arg(&path_str).spawn();
                    state.last_feedback = Some(KillFeedback::info(format!(
                        "Opened config file: {}",
                        path_str
                    )));
                    update_tray_display(&tray_icon, &state);
                }
                MenuAction::KillPid { pid, .. } => {
                    if let Some(target) = describe_pid(pid, &state.processes) {
                        if let Some(sender) = worker_sender.as_ref() {
                            if let Err(err) = sender.send(WorkerCommand::KillPid(target)) {
                                let feedback = KillFeedback::error(format!(
                                    "Unable to dispatch kill command: {}",
                                    err
                                ));
                                worker_sender = None;
                                state.last_feedback = Some(feedback);
                                update_tray_display(&tray_icon, &state);
                            }
                        } else {
                            let feedback =
                                KillFeedback::error(format!("Worker unavailable for PID {}.", pid));
                            state.last_feedback = Some(feedback);
                            update_tray_display(&tray_icon, &state);
                        }
                    } else {
                        state.last_feedback = Some(KillFeedback::info(format!(
                            "PID {} is no longer active.",
                            pid
                        )));
                        update_tray_display(&tray_icon, &state);
                    }
                }
                MenuAction::KillAll => {
                    let targets = collect_targets_for_all(&state.processes);
                    if targets.is_empty() {
                        state.last_feedback = Some(KillFeedback::info(
                            "No dev port listeners to terminate.".to_string(),
                        ));
                        update_tray_display(&tray_icon, &state);
                    } else if let Some(sender) = worker_sender.as_ref() {
                        if let Err(err) = sender.send(WorkerCommand::KillAll(targets)) {
                            let feedback = KillFeedback::error(format!(
                                "Unable to dispatch kill-all command: {}",
                                err
                            ));
                            worker_sender = None;
                            state.last_feedback = Some(feedback);
                            update_tray_display(&tray_icon, &state);
                        }
                    } else {
                        let feedback = KillFeedback::error(
                            "Worker unavailable for batch request.".to_string(),
                        );
                        state.last_feedback = Some(feedback);
                        update_tray_display(&tray_icon, &state);
                    }
                }
                MenuAction::Quit => {
                    event_loop.exit();
                }
                MenuAction::DockerStop { container } => {
                    if let Some(sender) = worker_sender.as_ref() {
                        let _ = sender.send(WorkerCommand::DockerStop { container });
                    }
                }
                MenuAction::BrewStop { service } => {
                    if let Some(sender) = worker_sender.as_ref() {
                        let _ = sender.send(WorkerCommand::BrewStop { service });
                    }
                }
                MenuAction::Snooze30m => {
                    state.snooze_until = Some(Instant::now() + Duration::from_secs(30 * 60));
                    state.last_feedback = Some(KillFeedback::info(
                        "Notifications snoozed for 30 minutes.".into(),
                    ));
                    update_tray_display(&tray_icon, &state);
                }
            },
            UserEvent::KillFeedback(feedback) => {
                state.last_feedback = Some(feedback);
                update_tray_display(&tray_icon, &state);
            }
            UserEvent::MonitorError(message) => {
                warn!("Monitor error: {}", message);
                state.last_feedback = Some(KillFeedback::error(message));
                update_tray_display(&tray_icon, &state);
            }
        },
        Event::LoopExiting => {
            worker_sender.take();
        }
        _ => {}
    });

    run_result.context("event loop terminated with error")?;
    Ok(())
}

fn spawn_monitor_thread(
    proxy: EventLoopProxy<UserEvent>,
    config: Config,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut previous: Vec<ProcessInfo> = Vec::new();
        loop {
            let scan_start = std::time::Instant::now();
            match scan_ports(&config.port_ranges) {
                Ok(mut processes) => {
                    let scan_duration = scan_start.elapsed();
                    processes.sort();
                    if processes != previous {
                        log::debug!(
                            "Change detected (scan took {:?}). Polling immediately for rapid changes.",
                            scan_duration
                        );
                        previous = processes.clone();
                        if proxy
                            .send_event(UserEvent::ProcessesUpdated(processes))
                            .is_err()
                        {
                            break;
                        }
                        // Poll immediately to catch rapid changes (0ms delay)
                        continue;
                    } else {
                        log::trace!(
                            "No change (scan took {:?}). Sleeping {}s.",
                            scan_duration,
                            POLL_INTERVAL.as_secs()
                        );
                        // Back to normal interval when stable
                        thread::sleep(POLL_INTERVAL);
                    }
                }
                Err(err) => {
                    let message = format!("{}", err);
                    if proxy.send_event(UserEvent::MonitorError(message)).is_err() {
                        break;
                    }
                    thread::sleep(POLL_INTERVAL);
                }
            }
        }
    })
}

fn spawn_menu_listener(proxy: EventLoopProxy<UserEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let receiver = MenuEvent::receiver().clone();
        for event in receiver.iter() {
            let Some(action) = parse_menu_action(event.id()) else {
                continue;
            };
            if proxy.send_event(UserEvent::MenuAction(action)).is_err() {
                break;
            }
        }
    })
}

fn spawn_worker(
    rx: Receiver<WorkerCommand>,
    proxy: EventLoopProxy<UserEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for command in rx.iter() {
            let should_continue = match command {
                WorkerCommand::KillPid(target) => handle_single_kill(&proxy, target),
                WorkerCommand::KillAll(targets) => handle_batch_kill(&proxy, targets),
                WorkerCommand::DockerStop { container } => {
                    let feedback = run_docker_stop(&container);
                    proxy.send_event(UserEvent::KillFeedback(feedback)).is_ok()
                }
                WorkerCommand::BrewStop { service } => {
                    let feedback = run_brew_stop(&service);
                    proxy.send_event(UserEvent::KillFeedback(feedback)).is_ok()
                }
            };
            if !should_continue {
                break;
            }
        }
    })
}

fn handle_single_kill(proxy: &EventLoopProxy<UserEvent>, target: KillTarget) -> bool {
    let outcome = terminate_pid(target.pid);
    let feedback = match outcome {
        KillOutcome::Success => {
            KillFeedback::info(format!("Terminated {} (PID {}).", target.label, target.pid))
        }
        KillOutcome::AlreadyExited => KillFeedback::warning(format!(
            "{} (PID {}) was already stopped.",
            target.label, target.pid
        )),
        KillOutcome::PermissionDenied => KillFeedback::error(format!(
            "Permission denied terminating {} (PID {}).",
            target.label, target.pid
        )),
        KillOutcome::TimedOut => KillFeedback::error(format!(
            "Timed out terminating {} (PID {}).",
            target.label, target.pid
        )),
        KillOutcome::Failed(err) => KillFeedback::error(format!(
            "Failed to terminate {} (PID {}): {}.",
            target.label, target.pid, err
        )),
    };

    proxy.send_event(UserEvent::KillFeedback(feedback)).is_ok()
}

fn handle_batch_kill(proxy: &EventLoopProxy<UserEvent>, targets: Vec<KillTarget>) -> bool {
    if targets.is_empty() {
        return proxy
            .send_event(UserEvent::KillFeedback(KillFeedback::info(
                "No dev port listeners to terminate.".to_string(),
            )))
            .is_ok();
    }

    let mut successes = 0usize;
    let mut already = 0usize;
    let mut denied = 0usize;
    let mut timed_out = 0usize;
    let mut failures: Vec<(KillTarget, Errno)> = Vec::new();

    for target in targets {
        match terminate_pid(target.pid) {
            KillOutcome::Success => successes += 1,
            KillOutcome::AlreadyExited => already += 1,
            KillOutcome::PermissionDenied => {
                denied += 1;
                failures.push((target, Errno::EPERM));
            }
            KillOutcome::TimedOut => {
                timed_out += 1;
                failures.push((target, Errno::ETIMEDOUT));
            }
            KillOutcome::Failed(err) => failures.push((target, err)),
        }
    }

    let failure_count = failures.len();
    let severity = if successes > 0 && failure_count == 0 && denied == 0 && timed_out == 0 {
        FeedbackSeverity::Info
    } else if successes > 0 {
        FeedbackSeverity::Warning
    } else {
        FeedbackSeverity::Error
    };

    let mut parts = Vec::new();
    if successes > 0 {
        parts.push(format!("terminated {}", successes));
    }
    if already > 0 {
        parts.push(format!("{} already stopped", already));
    }
    if denied > 0 {
        parts.push(format!("{} permission denied", denied));
    }
    if timed_out > 0 {
        parts.push(format!("{} timed out", timed_out));
    }
    if failure_count > 0 {
        parts.push(format!("{} failed", failure_count));
    }

    if parts.is_empty() {
        parts.push("no action taken".to_string());
    }

    let mut message = format!("Kill all: {}.", parts.join(", "));
    if let Some((failed_target, err)) = failures.first() {
        message.push_str(&format!(
            " First failure: {} (PID {}) — {}.",
            failed_target.label, failed_target.pid, err
        ));
    }

    let feedback = KillFeedback::new(message, severity);
    proxy.send_event(UserEvent::KillFeedback(feedback)).is_ok()
}

fn terminate_pid(pid_raw: i32) -> KillOutcome {
    let pid = Pid::from_raw(pid_raw);

    // Ensure the PID exists
    match kill(pid, None) {
        Err(Errno::ESRCH) => return KillOutcome::AlreadyExited,
        Err(err) => return KillOutcome::Failed(err),
        Ok(()) => {}
    }

    let mut last_perm_denied = false;
    let mut tried_group = false;

    // Prefer process group termination first
    if let Ok(pgid) = getpgid(Some(pid)) {
        let raw = pgid.as_raw();
        if raw > 0 {
            tried_group = true;
            let gpid = Pid::from_raw(-raw);
            match kill(gpid, Signal::SIGTERM) {
                Ok(()) => {}
                Err(Errno::ESRCH) => {}
                Err(Errno::EPERM) => last_perm_denied = true,
                Err(err) => return KillOutcome::Failed(err),
            }
            match wait_for_exit(pid, SIGTERM_GRACE) {
                Ok(true) => return KillOutcome::Success,
                Ok(false) => {}
                Err(err) => return KillOutcome::Failed(err),
            }
            match kill(gpid, Signal::SIGKILL) {
                Ok(()) => {}
                Err(Errno::ESRCH) => {}
                Err(Errno::EPERM) => last_perm_denied = true,
                Err(err) => return KillOutcome::Failed(err),
            }
            match wait_for_exit(pid, SIGKILL_GRACE) {
                Ok(true) => return KillOutcome::Success,
                Ok(false) => {}
                Err(err) => return KillOutcome::Failed(err),
            }
        }
    }

    // Fallback to targeting the single PID
    match kill(pid, Signal::SIGTERM) {
        Ok(()) => {}
        Err(Errno::ESRCH) => return KillOutcome::AlreadyExited,
        Err(Errno::EPERM) => last_perm_denied = true,
        Err(err) => return KillOutcome::Failed(err),
    }
    match wait_for_exit(pid, SIGTERM_GRACE) {
        Ok(true) => return KillOutcome::Success,
        Ok(false) => {}
        Err(err) => return KillOutcome::Failed(err),
    }
    match kill(pid, Signal::SIGKILL) {
        Ok(()) => {}
        Err(Errno::ESRCH) => return KillOutcome::Success,
        Err(Errno::EPERM) => last_perm_denied = true,
        Err(err) => return KillOutcome::Failed(err),
    }
    match wait_for_exit(pid, SIGKILL_GRACE) {
        Ok(true) => KillOutcome::Success,
        Ok(false) => {
            if tried_group && last_perm_denied {
                KillOutcome::PermissionDenied
            } else {
                KillOutcome::TimedOut
            }
        }
        Err(err) => KillOutcome::Failed(err),
    }
}

fn wait_for_exit(pid: Pid, timeout: Duration) -> Result<bool, Errno> {
    let deadline = Instant::now() + timeout;
    loop {
        match kill(pid, None) {
            Err(Errno::ESRCH) => return Ok(true),
            Err(err) => return Err(err),
            Ok(()) => {}
        }

        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL_STEP);
    }
}

fn scan_ports(port_ranges: &[(u16, u16)]) -> Result<Vec<ProcessInfo>> {
    fn in_ranges(port: u16, ranges: &[(u16, u16)]) -> bool {
        ranges.iter().any(|(s, e)| port >= *s && port <= *e)
    }

    let output = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-FpcnPT"])
        .output()
        .context("failed to execute lsof sweep")?;

    if !output.status.success() {
        return Err(anyhow!(
            "lsof sweep failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut current_pid: Option<i32> = None;
    let mut current_cmd: Option<String> = None;
    let mut results: Vec<ProcessInfo> = Vec::new();
    let mut seen: HashSet<(u16, i32)> = HashSet::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (tag, val) = line.split_at(1);
        match tag {
            "p" => {
                current_pid = val.trim().parse::<i32>().ok();
                current_cmd = None;
            }
            "c" => {
                current_cmd = Some(val.trim().to_string());
            }
            "n" => {
                if let (Some(pid), Some(cmd)) = (current_pid, current_cmd.as_ref())
                    && let Some(port) = parse_port_from_lsof(val.trim())
                    && in_ranges(port, port_ranges)
                    && seen.insert((port, pid))
                {
                    results.push(ProcessInfo {
                        port,
                        pid,
                        command: cmd.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    results.sort();
    Ok(results)
}

// Extract a port number from an lsof name field.
// Handles "*:3000", "127.0.0.1:5173", and "[::1]:8000".
fn parse_port_from_lsof(name: &str) -> Option<u16> {
    if name.contains("->") {
        return None;
    }
    let mut digits = String::new();
    for ch in name.chars().rev() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if ch == ':' {
            break;
        } else if ch == ']' {
            // IPv6 end bracket; continue to previous ':'
            continue;
        } else {
            return None;
        }
    }
    if digits.is_empty() {
        return None;
    }
    digits = digits.chars().rev().collect();
    digits.parse::<u16>().ok()
}

// query_port removed with single-pass sweep

// resolve_command removed with single-pass sweep

fn sanitize_identifier(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect()
}

fn parse_menu_action(id: &MenuId) -> Option<MenuAction> {
    let raw = id.as_ref();
    if raw == MENU_ID_KILL_ALL {
        Some(MenuAction::KillAll)
    } else if raw == MENU_ID_QUIT {
        Some(MenuAction::Quit)
    } else if raw == MENU_ID_EDIT_CONFIG {
        Some(MenuAction::EditConfig)
    } else if raw == MENU_ID_SNOOZE_30M {
        Some(MenuAction::Snooze30m)
    } else if let Some(rest) = raw.strip_prefix(MENU_ID_DOCKER_STOP_PREFIX) {
        Some(MenuAction::DockerStop {
            container: sanitize_identifier(rest),
        })
    } else if let Some(rest) = raw.strip_prefix(MENU_ID_BREW_STOP_PREFIX) {
        Some(MenuAction::BrewStop {
            service: sanitize_identifier(rest),
        })
    } else if let Some(remainder) = raw.strip_prefix(MENU_ID_PROCESS_PREFIX) {
        let mut parts = remainder.split('_');
        let pid = parts.next()?.parse::<i32>().ok()?;
        let _port = parts.next()?.parse::<u16>().ok()?;
        Some(MenuAction::KillPid { pid })
    } else {
        None
    }
}

fn collect_targets_for_all(processes: &[ProcessInfo]) -> Vec<KillTarget> {
    let mut map: BTreeMap<i32, (String, Vec<u16>)> = BTreeMap::new();

    for process in processes {
        let entry = map
            .entry(process.pid)
            .or_insert_with(|| (process.command.clone(), Vec::new()));
        if !entry.1.contains(&process.port) {
            entry.1.push(process.port);
        }
        if entry.0.is_empty() {
            entry.0 = process.command.clone();
        }
    }

    map.into_iter()
        .filter_map(|(pid, (command, mut ports))| {
            if ports.is_empty() {
                return None;
            }
            ports.sort();
            let label = format_command_label(&command, &ports);
            Some(KillTarget { pid, label })
        })
        .collect()
}

fn describe_pid(pid: i32, processes: &[ProcessInfo]) -> Option<KillTarget> {
    let mut ports = Vec::new();
    let mut command: Option<String> = None;
    for process in processes.iter().filter(|p| p.pid == pid) {
        if !ports.contains(&process.port) {
            ports.push(process.port);
        }
        if command.is_none() || command.as_deref().unwrap().starts_with("pid ") {
            command = Some(process.command.clone());
        }
    }

    if ports.is_empty() {
        return None;
    }

    ports.sort();
    let label = format_command_label(command.as_deref().unwrap_or(""), &ports);
    Some(KillTarget { pid, label })
}

fn format_command_label(command: &str, ports: &[u16]) -> String {
    let mut label = if command.is_empty() {
        "Unknown".to_string()
    } else {
        command.to_string()
    };
    if !ports.is_empty() {
        label.push_str(" (port");
        if ports.len() > 1 {
            label.push('s');
        }
        label.push(' ');
        for (idx, port) in ports.iter().enumerate() {
            if idx > 0 {
                label.push_str(", ");
            }
            label.push_str(&port.to_string());
        }
        label.push(')');
    }
    label
}

fn build_menu_with_context(state: &AppState) -> Result<Menu> {
    let menu = Menu::new();
    let processes = &state.processes;
    if processes.is_empty() {
        let item = MenuItem::with_id(MENU_ID_EMPTY, "No dev ports listening", false, None);
        menu.append(&item)?;
    } else {
        // Group by project name
        let mut by_project: BTreeMap<String, Vec<&ProcessInfo>> = BTreeMap::new();
        for p in processes {
            let key = state
                .project_cache
                .get(&p.pid)
                .map(|pi| pi.name.clone())
                .unwrap_or_else(|| "(unknown project)".to_string());
            by_project.entry(key).or_default().push(p);
        }

        let mut total = 0usize;
        for (project, items) in by_project {
            let header = MenuItem::with_id(
                format!("header_{}", project),
                format!("— {} —", project),
                false,
                None,
            );
            menu.append(&header)?;
            for process in items {
                total += 1;

                // Prioritize: Docker > Brew > Kill
                // Only show ONE option per port
                if let Some(dc) = state.docker_port_map.get(&process.port) {
                    // Docker-aware: show "Stop container" option
                    let dlabel = format!("Stop container {} (port {})", dc.name, process.port);
                    let did = format!("{}{}", MENU_ID_DOCKER_STOP_PREFIX, dc.name);
                    let ditem = MenuItem::with_id(&did, &dlabel, true, None);
                    menu.append(&ditem)?;
                } else if let Some(service) = get_brew_managed_service(
                    &process.command,
                    process.port,
                    &state.brew_services_map,
                ) {
                    // Brew-aware: show "Stop via brew" option (only if actually managed by brew)
                    let blabel = format!("Stop via brew {}", service);
                    let bid = format!("{}{}", MENU_ID_BREW_STOP_PREFIX, service);
                    let bitem = MenuItem::with_id(&bid, &blabel, true, None);
                    menu.append(&bitem)?;
                } else {
                    // Default: show regular "Kill" option
                    let label = format!(
                        "Kill {} (PID {}, port {})",
                        process.command, process.pid, process.port
                    );
                    let item = MenuItem::with_id(
                        MenuId::new(process_menu_id(process.pid, process.port)),
                        label,
                        true,
                        None,
                    );
                    menu.append(&item)?;
                }
            }
            menu.append(&PredefinedMenuItem::separator())?;
        }

        let kill_all_label = format!("Kill all ({})", total);
        let kill_all_item = MenuItem::with_id(MENU_ID_KILL_ALL, kill_all_label, true, None);
        menu.append(&kill_all_item)?;
    }

    menu.append(&PredefinedMenuItem::separator())?;
    let snooze_item = MenuItem::with_id(MENU_ID_SNOOZE_30M, "Snooze notifications 30m", true, None);
    menu.append(&snooze_item)?;
    let edit_config_item =
        MenuItem::with_id(MENU_ID_EDIT_CONFIG, "Edit Configuration...", true, None);
    menu.append(&edit_config_item)?;
    let quit_item = MenuItem::with_id(MENU_ID_QUIT, "Quit", true, None);
    menu.append(&quit_item)?;
    Ok(menu)
}

fn process_menu_id(pid: i32, port: u16) -> MenuId {
    MenuId::new(format!("{}{}_{}", MENU_ID_PROCESS_PREFIX, pid, port))
}

fn sync_menu_with_context(tray_icon: &TrayIcon, state: &AppState) {
    match build_menu_with_context(state) {
        Ok(menu) => tray_icon.set_menu(Some(Box::new(menu))),
        Err(err) => error!("Failed to rebuild menu: {}", err),
    }
}

fn update_tray_display(tray_icon: &TrayIcon, state: &AppState) {
    // Update icon color based on whether ports are active
    let color = if state.processes.is_empty() {
        state.config.inactive_color
    } else {
        state.config.active_color
    };

    if let Ok(icon) = create_icon(color) {
        let _ = tray_icon.set_icon(Some(icon));
    }

    let tooltip = build_tooltip(&state.processes, state.last_feedback.as_ref());
    if let Err(err) = tray_icon.set_tooltip(Some(tooltip.as_str())) {
        error!("Failed to update tooltip: {}", err);
    }
}

fn refresh_projects_for(state: &mut AppState) {
    let mut missing: HashSet<i32> = HashSet::new();
    for p in &state.processes {
        if !state.project_cache.contains_key(&p.pid) {
            missing.insert(p.pid);
        }
    }
    for pid in missing {
        if let Some(info) = resolve_project_info(pid) {
            state.project_cache.insert(pid, info);
        }
    }
}

fn resolve_project_info(pid: i32) -> Option<ProjectInfo> {
    // Get cwd via lsof -a -p <pid> -d cwd -Fn
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut cwd: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('n') {
            cwd = Some(rest.to_string());
            break;
        }
    }
    let cwd = cwd?;
    let path = PathBuf::from(cwd.clone());
    // Try git root
    let git = Command::new("git")
        .args(["-C", &cwd, "rev-parse", "--show-toplevel"])
        .output()
        .ok();
    let name = if let Some(gitout) = git {
        if gitout.status.success() {
            let root = String::from_utf8_lossy(&gitout.stdout).trim().to_string();
            PathBuf::from(root)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| {
                    path.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "(unknown)".into())
                })
        } else {
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "(unknown)".into())
        }
    } else {
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "(unknown)".into())
    };
    Some(ProjectInfo { name, path })
}

fn query_docker_port_map() -> Result<HashMap<u16, DockerContainerInfo>> {
    let mut map = HashMap::new();
    let out = Command::new("docker")
        .args(["ps", "--format", "{{.ID}}\t{{.Names}}\t{{.Ports}}"])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(err) => {
            warn!("Docker command failed (docker not installed?): {}", err);
            return Ok(map);
        }
    };
    if !out.status.success() {
        warn!(
            "Docker ps command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return Ok(map);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let id = parts[0].to_string();
        let name = parts[1].to_string();
        let ports = parts[2];
        // PORTS usually like: 0.0.0.0:5432->5432/tcp, :::5432->5432/tcp
        for seg in ports.split(',') {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }

            // Parse the port mapping format
            if let Some((left, _right)) = seg.split_once("->") {
                if let Some((_, host)) = left.rsplit_once(':') {
                    // Check for port ranges (not yet supported)
                    if host.contains('-') {
                        warn!(
                            "Docker container '{}' exposes port range '{}' - port ranges not yet supported",
                            name, host
                        );
                        continue;
                    }

                    // Try to parse single port
                    match host.parse::<u16>() {
                        Ok(p) => {
                            map.insert(
                                p,
                                DockerContainerInfo {
                                    name: name.clone(),
                                    id: id.clone(),
                                },
                            );
                        }
                        Err(_) => {
                            log::debug!(
                                "Failed to parse port '{}' from Docker mapping '{}' for container '{}'",
                                host,
                                seg,
                                name
                            );
                        }
                    }
                } else {
                    log::debug!(
                        "Failed to extract host port from Docker mapping '{}' for container '{}'",
                        seg,
                        name
                    );
                }
            } else {
                log::debug!(
                    "Failed to parse Docker port mapping '{}' for container '{}'",
                    seg,
                    name
                );
            }
        }
    }
    Ok(map)
}

fn query_brew_services_map() -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let out = Command::new("brew").args(["services", "list"]).output();
    let out = match out {
        Ok(o) => o,
        Err(err) => {
            warn!("Brew command failed (brew not installed?): {}", err);
            return Ok(map);
        }
    };
    if !out.status.success() {
        warn!(
            "Brew services list command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return Ok(map);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Parse brew services list output
    // Format: Name    Status    User    File
    // Skip header line
    for (idx, line) in stdout.lines().enumerate() {
        if idx == 0 {
            // Skip header
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let name = parts[0].to_string();
        let status = parts[1].to_string();

        log::debug!("Brew service detected: {} -> {}", name, status);
        map.insert(name, status);
    }
    Ok(map)
}

fn map_brew_service_from_cmd(cmd: &str) -> Option<String> {
    let lc = cmd.to_lowercase();
    if lc.contains("redis") {
        return Some("redis".into());
    }
    if lc.contains("postgres") {
        return Some("postgresql".into());
    }
    if lc.contains("mysqld") || lc.contains("mysql") {
        return Some("mysql".into());
    }
    if lc.contains("mongod") {
        return Some("mongodb-community".into());
    }
    None
}

fn get_brew_managed_service(
    cmd: &str,
    port: u16,
    brew_services_map: &HashMap<String, String>,
) -> Option<String> {
    // First, pattern match to identify what service this might be
    let potential_service = map_brew_service_from_cmd(cmd)?;

    // Then verify it's actually managed by brew and running
    if let Some(status) = brew_services_map.get(&potential_service)
        && status == "started"
    {
        // Verify the port matches the expected default port for this brew service
        // This prevents showing "Stop via brew" for manually-started instances on non-default ports
        let expected_port = get_default_port_for_service(&potential_service);
        if Some(port) == expected_port {
            return Some(potential_service);
        }
    }

    None
}

fn get_default_port_for_service(service: &str) -> Option<u16> {
    match service {
        "redis" => Some(6379),
        "postgresql" => Some(5432),
        "mysql" => Some(3306),
        "mongodb-community" => Some(27017),
        _ => None,
    }
}

fn run_docker_stop(container: &str) -> KillFeedback {
    let res = Command::new("docker").args(["stop", container]).output();
    match res {
        Ok(out) if out.status.success() => {
            KillFeedback::info(format!("Stopped container {}.", container))
        }
        Ok(out) => KillFeedback::error(format!(
            "Failed to stop container {}: {}",
            container,
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(err) => KillFeedback::error(format!("docker stop error: {}", err)),
    }
}

fn run_brew_stop(service: &str) -> KillFeedback {
    let res = Command::new("brew")
        .args(["services", "stop", service])
        .output();
    match res {
        Ok(out) if out.status.success() => {
            KillFeedback::info(format!("Stopped brew service {}.", service))
        }
        Ok(out) => KillFeedback::error(format!(
            "Failed to stop brew service {}: {}",
            service,
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(err) => KillFeedback::error(format!("brew services error: {}", err)),
    }
}

fn maybe_notify_changes(state: &AppState, prev: &[ProcessInfo]) {
    if !state.config.notifications_enabled {
        return;
    }
    if let Some(until) = state.snooze_until
        && Instant::now() < until
    {
        return;
    }
    let prev_ports: HashSet<u16> = prev.iter().map(|p| p.port).collect();
    let curr_ports: HashSet<u16> = state.processes.iter().map(|p| p.port).collect();
    let added: Vec<u16> = curr_ports.difference(&prev_ports).copied().collect();
    let removed: Vec<u16> = prev_ports.difference(&curr_ports).copied().collect();
    if !added.is_empty() {
        notify(&format!("Ports now listening: {:?}", added));
    }
    if !removed.is_empty() {
        notify(&format!("Ports freed: {:?}", removed));
    }
}

fn notify(message: &str) {
    // Best-effort macOS notification via AppleScript; ignores errors
    let msg = message.replace('"', "'");
    let script = format!("display notification \"{}\" with title \"Macport\"", msg);
    let _ = Command::new("osascript").args(["-e", &script]).spawn();
}

fn build_tooltip(processes: &[ProcessInfo], feedback: Option<&KillFeedback>) -> String {
    let mut lines = Vec::new();
    if processes.is_empty() {
        lines.push("No dev port listeners detected.".to_string());
    } else {
        lines.push(format!("Active listeners: {}", processes.len()));
        for process in processes.iter().take(MAX_TOOLTIP_ENTRIES) {
            lines.push(format!(
                "Port {} → {} (PID {})",
                process.port, process.command, process.pid
            ));
        }
        if processes.len() > MAX_TOOLTIP_ENTRIES {
            lines.push(format!(
                "…and {} more",
                processes.len() - MAX_TOOLTIP_ENTRIES
            ));
        }
    }

    if let Some(feedback) = feedback {
        let prefix = match feedback.severity {
            FeedbackSeverity::Info => "",
            FeedbackSeverity::Warning => "⚠️ ",
            FeedbackSeverity::Error => "⛔ ",
        };
        lines.push(format!("Last action: {}{}", prefix, feedback.message));
    }

    lines.join("\n")
}

fn create_icon(color: (u8, u8, u8)) -> Result<Icon> {
    // Create a 22x22 icon (standard macOS menu bar size)
    let size = 22;
    let mut pixels = vec![0u8; (size * size * 4) as usize];

    let (r, g, b) = color;

    let draw_pixel = |pixels: &mut [u8], x: i32, y: i32, alpha: u8| {
        if x >= 0 && x < size && y >= 0 && y < size {
            let idx = ((y * size + x) * 4) as usize;
            pixels[idx] = r;
            pixels[idx + 1] = g;
            pixels[idx + 2] = b;
            pixels[idx + 3] = alpha;
        }
    };

    // Draw a circular port icon with connection lines
    // Outer circle (port)
    for angle in 0..360 {
        let rad = (angle as f32).to_radians();
        let x = 11 + (7.0 * rad.cos()) as i32;
        let y = 11 + (7.0 * rad.sin()) as i32;
        draw_pixel(&mut pixels, x, y, 255);
    }

    // Inner circle fill (lighter)
    for dy in -5..=5 {
        for dx in -5..=5 {
            if dx * dx + dy * dy <= 25 {
                draw_pixel(&mut pixels, 11 + dx, 11 + dy, 180);
            }
        }
    }

    // Center dot (darker)
    for dy in -2..=2 {
        for dx in -2..=2 {
            if dx * dx + dy * dy <= 4 {
                draw_pixel(&mut pixels, 11 + dx, 11 + dy, 255);
            }
        }
    }

    // Connection lines (top and bottom)
    for i in 0..3 {
        draw_pixel(&mut pixels, 11, 3 + i, 255); // Top line
        draw_pixel(&mut pixels, 11, 16 + i, 255); // Bottom line
    }

    Icon::from_rgba(pixels, size as u32, size as u32)
        .map_err(|err| anyhow!("failed to build icon: {err}"))
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct ProcessInfo {
    port: u16,
    pid: i32,
    command: String,
}

#[derive(Clone, Debug)]
enum UserEvent {
    ProcessesUpdated(Vec<ProcessInfo>),
    MenuAction(MenuAction),
    KillFeedback(KillFeedback),
    MonitorError(String),
}

#[derive(Clone, Debug)]
enum MenuAction {
    KillPid { pid: i32 },
    KillAll,
    EditConfig,
    DockerStop { container: String },
    BrewStop { service: String },
    Snooze30m,
    Quit,
}

#[derive(Clone, Debug)]
enum WorkerCommand {
    KillPid(KillTarget),
    KillAll(Vec<KillTarget>),
    DockerStop { container: String },
    BrewStop { service: String },
}

#[derive(Clone, Debug)]
struct KillTarget {
    pid: i32,
    label: String,
}

#[derive(Clone, Debug)]
struct KillFeedback {
    message: String,
    severity: FeedbackSeverity,
}

impl KillFeedback {
    fn new(message: String, severity: FeedbackSeverity) -> Self {
        Self { message, severity }
    }

    fn info(message: String) -> Self {
        Self::new(message, FeedbackSeverity::Info)
    }

    fn warning(message: String) -> Self {
        Self::new(message, FeedbackSeverity::Warning)
    }

    fn error(message: String) -> Self {
        Self::new(message, FeedbackSeverity::Error)
    }
}

#[derive(Clone, Copy, Debug)]
enum FeedbackSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Default)]
struct AppState {
    processes: Vec<ProcessInfo>,
    last_feedback: Option<KillFeedback>,
    config: Config,
    project_cache: HashMap<i32, ProjectInfo>,
    docker_port_map: HashMap<u16, DockerContainerInfo>,
    brew_services_map: HashMap<String, String>, // service_name -> status
    snooze_until: Option<Instant>,
}

#[derive(Clone, Copy, Debug)]
enum KillOutcome {
    Success,
    AlreadyExited,
    PermissionDenied,
    TimedOut,
    Failed(Errno),
}

#[derive(Clone, Debug)]
struct ProjectInfo {
    name: String,
    #[allow(dead_code)]
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct DockerContainerInfo {
    name: String,
    #[allow(dead_code)]
    id: String,
}
