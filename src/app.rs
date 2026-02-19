use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use log::{error, warn};
use nix::errno::Errno;
use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tray_icon::menu::MenuEvent;
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::event::{Event, StartCause};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};

use crate::config::{
    get_config_path, load_and_validate_config, load_or_create_config, save_config,
};
use crate::integrations::brew::{query_brew_services_map, run_brew_stop};
use crate::integrations::docker::{query_docker_port_map, run_docker_stop};
use crate::model::*;
use crate::notify::{maybe_notify_changes, notify_update_available};
use crate::process::kill::terminate_pid;
use crate::process::ports::scan_ports;
use crate::ui::icon::{create_template_icon, IconVariant};
use crate::ui::menu::{
    build_menu_with_context, build_tooltip, collect_targets_for_all, format_command_label,
    parse_menu_action,
};
use crate::update::check_for_update;

const IDLE_THRESHOLD: Duration = Duration::from_secs(30);
const IDLE_MULTIPLIER: u64 = 2; // Idle poll interval = base * IDLE_MULTIPLIER
const INTEGRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MENU_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UPDATE_CHECK_DELAY: Duration = Duration::from_secs(5);
const DOWNLOAD_URL: &str =
    "https://github.com/gupsammy/PortKiller/releases/latest/download/PortKiller.dmg";
// menu constants moved under ui::menu

pub fn run() -> Result<()> {
    let config = load_or_create_config().context("failed to load configuration")?;
    let shared_config = Arc::new(RwLock::new(config.clone()));

    let mut state = AppState {
        processes: Vec::new(),
        last_feedback: None,
        config: config.clone(),
        project_cache: HashMap::new(),
        docker_port_map: HashMap::new(),
        brew_services_map: HashMap::new(),
        available_update: None,
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("failed to create event loop")?;
    let proxy = event_loop.create_proxy();
    let (worker_tx, worker_rx) = crossbeam_channel::unbounded();

    let _monitor_thread = spawn_monitor_thread(proxy.clone(), shared_config.clone());
    let _config_watcher = spawn_config_watcher(proxy.clone(), shared_config.clone());
    let _worker = spawn_worker(worker_rx, proxy.clone());
    let _update_checker = spawn_update_checker(proxy.clone(), shared_config.clone());
    let menu_receiver = MenuEvent::receiver().clone();

    let icon =
        create_template_icon(IconVariant::Inactive).context("failed to create tray icon image")?;
    let initial_menu = build_menu_with_context(&state).context("failed to build initial menu")?;
    let tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_menu(Box::new(initial_menu))
        .with_tooltip("No dev port listeners detected.")
        .build()
        .context("failed to create tray icon")?;
    tray_icon
        .set_visible(true)
        .context("failed to show tray icon")?;

    update_tray_display(&tray_icon, &state);
    let mut worker_sender: Option<Sender<WorkerCommand>> = Some(worker_tx);
    // Initialize to past time to force first integration refresh
    let mut last_integration_refresh = Instant::now() - INTEGRATION_REFRESH_INTERVAL;
    // Clone shared_config for use in event loop (for manual reload)
    let shared_config_for_loop = shared_config.clone();

    #[allow(deprecated)]
    let run_result = event_loop.run(move |event, event_loop| match event {
        Event::NewEvents(StartCause::Init) => {
            // Use WaitUntil to periodically check for menu events
            event_loop
                .set_control_flow(ControlFlow::WaitUntil(Instant::now() + MENU_POLL_INTERVAL));
        }
        Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
            // Poll for menu events (replaces dedicated menu listener thread)
            while let Ok(event) = menu_receiver.try_recv() {
                if let Some(action) = parse_menu_action(event.id()) {
                    let _ = proxy.send_event(UserEvent::MenuAction(action));
                }
            }
            event_loop
                .set_control_flow(ControlFlow::WaitUntil(Instant::now() + MENU_POLL_INTERVAL));
        }
        Event::UserEvent(user_event) => match user_event {
            UserEvent::ProcessesUpdated(processes) => {
                let prev = std::mem::take(&mut state.processes);
                state.processes = processes;
                // Detect if ports changed (not just process list) to trigger integration refresh
                let prev_ports: HashSet<u16> = prev.iter().map(|p| p.port).collect();
                let curr_ports: HashSet<u16> = state.processes.iter().map(|p| p.port).collect();
                let ports_changed = prev_ports != curr_ports;
                // Refresh integrations when ports change OR on timer (to catch external changes)
                let timer_refresh =
                    last_integration_refresh.elapsed() >= INTEGRATION_REFRESH_INTERVAL;
                if ports_changed || timer_refresh {
                    last_integration_refresh = Instant::now();
                    if state.config.integrations.docker_enabled {
                        state.docker_port_map = query_docker_port_map().unwrap_or_default();
                    }
                    if state.config.integrations.brew_enabled {
                        state.brew_services_map = query_brew_services_map().unwrap_or_default();
                    }
                }
                // Clear maps if integrations disabled (check every time)
                if !state.config.integrations.docker_enabled {
                    state.docker_port_map.clear();
                }
                if !state.config.integrations.brew_enabled {
                    state.brew_services_map.clear();
                }
                // Derive project info in best-effort mode
                refresh_projects_for(&mut state);
                // Notifications on change (before cache cleanup so stopped ports still have project info)
                maybe_notify_changes(&state, &prev);
                // Clean up stale cache entries for terminated processes
                let active_pids: HashSet<i32> = state.processes.iter().map(|p| p.pid).collect();
                state
                    .project_cache
                    .retain(|pid, _| active_pids.contains(pid));
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
                MenuAction::LaunchAtLogin => {
                    use crate::launch::{
                        disable_launch_at_login, enable_launch_at_login, is_launch_at_login_enabled,
                    };

                    // Toggle the current state
                    let currently_enabled = state.config.system.launch_at_login;
                    let result = if currently_enabled {
                        disable_launch_at_login()
                    } else {
                        enable_launch_at_login()
                    };

                    match result {
                        Ok(()) => {
                            // Verify actual system state and update config based on reality
                            match is_launch_at_login_enabled() {
                                Ok(actual_state) => {
                                    state.config.system.launch_at_login = actual_state;
                                    if let Err(e) = save_config(&state.config) {
                                        state.last_feedback = Some(KillFeedback::error(format!(
                                            "Failed to save config: {}",
                                            e
                                        )));
                                    } else {
                                        state.last_feedback =
                                            Some(KillFeedback::info(if actual_state {
                                                "Launch at login enabled".to_string()
                                            } else {
                                                "Launch at login disabled".to_string()
                                            }));
                                    }
                                }
                                Err(e) => {
                                    state.last_feedback = Some(KillFeedback::error(format!(
                                        "Failed to verify launch-at-login state: {}",
                                        e
                                    )));
                                }
                            }
                        }
                        Err(e) => {
                            // Check if it's the "requires approval" case
                            let msg = e.to_string();
                            if msg.contains("approve") || msg.contains("Login Items") {
                                // Treat as partial success - verify actual state
                                match is_launch_at_login_enabled() {
                                    Ok(actual_state) => {
                                        state.config.system.launch_at_login = actual_state;
                                        let _ = save_config(&state.config);
                                        state.last_feedback = Some(KillFeedback::warning(
                                            "Please approve in System Settings > Login Items"
                                                .to_string(),
                                        ));
                                    }
                                    Err(_) => {
                                        state.last_feedback = Some(KillFeedback::warning(
                                            "Please approve in System Settings > Login Items"
                                                .to_string(),
                                        ));
                                    }
                                }
                            } else {
                                state.last_feedback = Some(KillFeedback::error(format!(
                                    "Failed to toggle launch-at-login: {}",
                                    e
                                )));
                            }
                        }
                    }

                    sync_menu_with_context(&tray_icon, &state);
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
                    // Filter to only regular processes (exclude Docker and Brew)
                    let regular_processes: Vec<ProcessInfo> = state
                        .processes
                        .iter()
                        .filter(|p| {
                            // Exclude Docker containers
                            if state.docker_port_map.contains_key(&p.port) {
                                return false;
                            }
                            // Exclude Brew services
                            if crate::integrations::brew::get_brew_managed_service(
                                &p.command,
                                p.port,
                                &state.brew_services_map,
                            )
                            .is_some()
                            {
                                return false;
                            }
                            true
                        })
                        .cloned()
                        .collect();

                    let targets = collect_targets_for_all(&regular_processes);
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
                MenuAction::DockerStopAll => {
                    if let Some(sender) = worker_sender.as_ref() {
                        // Collect all unique Docker containers from current processes
                        let containers: Vec<String> = state
                            .docker_port_map
                            .values()
                            .map(|dc| dc.name.clone())
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .collect();

                        for container in containers {
                            let _ = sender.send(WorkerCommand::DockerStop {
                                container: container.clone(),
                            });
                        }
                    }
                }
                MenuAction::BrewStop { service } => {
                    if let Some(sender) = worker_sender.as_ref() {
                        let _ = sender.send(WorkerCommand::BrewStop { service });
                    }
                }
                MenuAction::BrewStopAll => {
                    if let Some(sender) = worker_sender.as_ref() {
                        // Collect all unique brew services from current processes
                        let services: Vec<String> = state
                            .processes
                            .iter()
                            .filter_map(|p| {
                                crate::integrations::brew::get_brew_managed_service(
                                    &p.command,
                                    p.port,
                                    &state.brew_services_map,
                                )
                            })
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .collect();

                        for service in services {
                            let _ = sender.send(WorkerCommand::BrewStop {
                                service: service.clone(),
                            });
                        }
                    }
                }
                MenuAction::ReloadConfig => {
                    match load_and_validate_config() {
                        Ok(new_config) => {
                            // Update shared config for monitor thread
                            if let Ok(mut cfg) = shared_config_for_loop.write() {
                                *cfg = new_config.clone();
                            }
                            state.config = new_config;
                            state.last_feedback =
                                Some(KillFeedback::info("Configuration reloaded".to_string()));
                        }
                        Err(e) => {
                            state.last_feedback =
                                Some(KillFeedback::error(format!("Reload failed: {}", e)));
                        }
                    }
                    sync_menu_with_context(&tray_icon, &state);
                    update_tray_display(&tray_icon, &state);
                }
                MenuAction::CheckForUpdates => {
                    state.last_feedback =
                        Some(KillFeedback::info("Checking for updates...".to_string()));
                    update_tray_display(&tray_icon, &state);

                    // Spawn a thread to check for updates
                    let proxy_clone = proxy.clone();
                    thread::spawn(move || {
                        let result = check_for_update().ok().flatten();
                        let _ = proxy_clone.send_event(UserEvent::UpdateCheckResult(result));
                    });
                }
                MenuAction::ToggleAutoUpdate => {
                    state.config.updates.check_enabled = !state.config.updates.check_enabled;
                    if let Err(e) = save_config(&state.config) {
                        state.last_feedback =
                            Some(KillFeedback::error(format!("Failed to save config: {}", e)));
                    } else {
                        state.last_feedback =
                            Some(KillFeedback::info(if state.config.updates.check_enabled {
                                "Auto-update check enabled".to_string()
                            } else {
                                "Auto-update check disabled".to_string()
                            }));
                    }
                    // Update shared config
                    if let Ok(mut cfg) = shared_config_for_loop.write() {
                        *cfg = state.config.clone();
                    }
                    sync_menu_with_context(&tray_icon, &state);
                    update_tray_display(&tray_icon, &state);
                }
                MenuAction::DownloadUpdate => {
                    if state.available_update.is_some() {
                        let _ = Command::new("open").arg(DOWNLOAD_URL).spawn();
                        state.last_feedback =
                            Some(KillFeedback::info("Opening download...".to_string()));
                    }
                    update_tray_display(&tray_icon, &state);
                }
                MenuAction::DismissUpdate => {
                    if let Some(ref update_info) = state.available_update {
                        state.config.updates.dismissed_version = Some(update_info.version.clone());
                        let _ = save_config(&state.config);
                        state.available_update = None;
                        state.last_feedback =
                            Some(KillFeedback::info("Update dismissed".to_string()));
                        // Update shared config
                        if let Ok(mut cfg) = shared_config_for_loop.write() {
                            *cfg = state.config.clone();
                        }
                    }
                    sync_menu_with_context(&tray_icon, &state);
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
            UserEvent::ConfigReloaded(new_config) => {
                state.config = new_config;
                state.last_feedback =
                    Some(KillFeedback::info("Configuration reloaded".to_string()));
                sync_menu_with_context(&tray_icon, &state);
                update_tray_display(&tray_icon, &state);
            }
            UserEvent::ConfigReloadFailed(message) => {
                state.last_feedback = Some(KillFeedback::error(message));
                update_tray_display(&tray_icon, &state);
            }
            UserEvent::UpdateCheckResult(result) => {
                if let Some(ref update_info) = result {
                    // Check if this version was dismissed
                    let dismissed = state
                        .config
                        .updates
                        .dismissed_version
                        .as_ref()
                        .map(|v| v == &update_info.version)
                        .unwrap_or(false);

                    if !dismissed {
                        state.available_update = result.clone();
                        notify_update_available(&update_info.version, &update_info.download_url);
                        state.last_feedback = Some(KillFeedback::info(format!(
                            "Update available: v{}",
                            update_info.version
                        )));
                    }
                } else {
                    state.available_update = None;
                }
                sync_menu_with_context(&tray_icon, &state);
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
    shared_config: Arc<RwLock<crate::config::Config>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut previous: Vec<ProcessInfo> = Vec::new();
        let mut last_change = Instant::now();
        loop {
            // Read current config at each iteration to pick up hot-reloaded changes
            let (port_ranges, poll_interval_secs) = {
                let cfg = shared_config.read().unwrap();
                (
                    cfg.monitoring.port_ranges.clone(),
                    cfg.monitoring.poll_interval_secs,
                )
            };
            let poll_interval_active = Duration::from_secs(poll_interval_secs);
            let poll_interval_idle = Duration::from_secs(poll_interval_secs * IDLE_MULTIPLIER);

            let scan_start = Instant::now();
            match scan_ports(&port_ranges) {
                Ok(mut processes) => {
                    let scan_duration = scan_start.elapsed();
                    processes.sort();
                    if processes != previous {
                        log::debug!(
                            "Change detected (scan took {:?}). Polling immediately for rapid changes.",
                            scan_duration
                        );
                        last_change = Instant::now();
                        previous = processes.clone();
                        if proxy
                            .send_event(UserEvent::ProcessesUpdated(processes))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    } else {
                        // Adaptive polling: use longer interval when idle
                        let poll_interval = if last_change.elapsed() > IDLE_THRESHOLD {
                            poll_interval_idle
                        } else {
                            poll_interval_active
                        };
                        log::trace!(
                            "No change (scan took {:?}). Sleeping {}s (idle: {}).",
                            scan_duration,
                            poll_interval.as_secs(),
                            last_change.elapsed() > IDLE_THRESHOLD
                        );
                        thread::sleep(poll_interval);
                    }
                }
                Err(err) => {
                    let message = format!("{}", err);
                    if proxy.send_event(UserEvent::MonitorError(message)).is_err() {
                        break;
                    }
                    thread::sleep(poll_interval_active);
                }
            }
        }
    })
}

const CONFIG_DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

fn spawn_update_checker(
    proxy: EventLoopProxy<UserEvent>,
    shared_config: Arc<RwLock<crate::config::Config>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Initial delay before first check
        thread::sleep(UPDATE_CHECK_DELAY);

        loop {
            // Read config to check if updates are enabled
            let (check_enabled, check_interval_hours, last_check, dismissed_version) = {
                let cfg = shared_config.read().unwrap();
                (
                    cfg.updates.check_enabled,
                    cfg.updates.check_interval_hours,
                    cfg.updates.last_check_timestamp,
                    cfg.updates.dismissed_version.clone(),
                )
            };

            if !check_enabled {
                // Sleep for an hour and check again if enabled
                thread::sleep(Duration::from_secs(3600));
                continue;
            }

            // Check if enough time has passed since last check
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let should_check = match last_check {
                Some(last) => now - last >= (check_interval_hours as i64 * 3600),
                None => true,
            };

            if should_check {
                log::debug!("Performing scheduled update check");
                match check_for_update() {
                    Ok(Some(update_info)) => {
                        // Check if dismissed
                        let is_dismissed = dismissed_version
                            .as_ref()
                            .map(|v| v == &update_info.version)
                            .unwrap_or(false);

                        if !is_dismissed {
                            let _ =
                                proxy.send_event(UserEvent::UpdateCheckResult(Some(update_info)));
                        }

                        // Update last check timestamp
                        if let Ok(mut cfg) = shared_config.write() {
                            cfg.updates.last_check_timestamp = Some(now);
                            let _ = save_config(&cfg);
                        }
                    }
                    Ok(None) => {
                        log::debug!("No update available");
                        // Update last check timestamp
                        if let Ok(mut cfg) = shared_config.write() {
                            cfg.updates.last_check_timestamp = Some(now);
                            let _ = save_config(&cfg);
                        }
                    }
                    Err(e) => {
                        log::warn!("Update check failed: {}", e);
                        // Don't update timestamp on failure, retry next interval
                    }
                }
            }

            // Sleep for the configured interval
            thread::sleep(Duration::from_secs(check_interval_hours * 3600));
        }
    })
}

fn spawn_config_watcher(
    proxy: EventLoopProxy<UserEvent>,
    shared_config: Arc<RwLock<crate::config::Config>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let config_path = get_config_path();
        let (tx, rx) = std::sync::mpsc::channel();

        let mut watcher: RecommendedWatcher = match Watcher::new(
            move |res: Result<NotifyEvent, notify::Error>| {
                let _ = tx.send(res);
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::error!("Failed to create config watcher: {}", e);
                return;
            }
        };

        if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
            log::error!("Failed to watch config file: {}", e);
            return;
        }

        log::debug!("Config watcher started for {:?}", config_path);

        // Debounce: track last reload time
        let mut last_reload = Instant::now() - CONFIG_DEBOUNCE_DURATION;

        for result in rx {
            match result {
                Ok(event) => {
                    // Only react to modify/create events
                    if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                        // Debounce rapid changes (editors may write in multiple ops)
                        if last_reload.elapsed() < CONFIG_DEBOUNCE_DURATION {
                            continue;
                        }
                        last_reload = Instant::now();

                        log::debug!("Config file changed, attempting reload");

                        // Attempt to load and validate
                        match load_and_validate_config() {
                            Ok(new_config) => {
                                // Update shared config for monitor thread
                                if let Ok(mut cfg) = shared_config.write() {
                                    *cfg = new_config.clone();
                                }
                                let _ = proxy.send_event(UserEvent::ConfigReloaded(new_config));
                            }
                            Err(e) => {
                                let msg = format!("Config reload failed: {}", e);
                                log::warn!("{}", msg);
                                let _ = proxy.send_event(UserEvent::ConfigReloadFailed(msg));
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("Config watch error: {}", e);
                }
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

// removed: local terminate/scan helpers moved under process::{kill,ports}

// menu id parsing moved to ui::menu

// collect_targets_for_all now in ui::menu

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

// format_command_label now in ui::menu

// build_menu_with_context moved to ui::menu

// process_menu_id moved to ui::menu

fn sync_menu_with_context(tray_icon: &TrayIcon, state: &AppState) {
    match build_menu_with_context(state) {
        Ok(menu) => tray_icon.set_menu(Some(Box::new(menu))),
        Err(err) => error!("Failed to rebuild menu: {}", err),
    }
}

fn update_tray_display(tray_icon: &TrayIcon, state: &AppState) {
    // Swap icon based on whether ports are active
    let variant = if state.processes.is_empty() {
        IconVariant::Inactive
    } else {
        IconVariant::Active
    };

    if let Ok(icon) = create_template_icon(variant) {
        let _ = tray_icon.set_icon(Some(icon));
        tray_icon.set_icon_as_template(true);
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
    let path = get_process_cwd(pid)?;
    // Validate path is in safe location (home dir or /tmp)
    if !is_safe_path(&path) {
        log::debug!("Skipping project resolution for unsafe path: {:?}", path);
        return None;
    }
    let name = get_git_repo_name(&path)
        .or_else(|| dir_name(&path))
        .unwrap_or_else(|| "(unknown)".to_string());
    Some(ProjectInfo { name, path })
}

fn get_process_cwd(pid: i32) -> Option<std::path::PathBuf> {
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .map(std::path::PathBuf::from)
}

fn is_safe_path(path: &std::path::Path) -> bool {
    // Resolve to canonical path to prevent traversal attacks
    let canonical = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    // Allow paths under home directory
    if let Ok(home) = std::env::var("HOME") {
        if canonical.starts_with(&home) {
            return true;
        }
    }
    // Allow /tmp and /var/folders (macOS temp)
    // Note: On macOS, /tmp -> /private/tmp and /var -> /private/var after canonicalization
    if canonical.starts_with("/tmp")
        || canonical.starts_with("/private/tmp")
        || canonical.starts_with("/var/folders")
        || canonical.starts_with("/private/var/folders")
    {
        return true;
    }
    false
}

fn get_git_repo_name(path: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout);
    std::path::Path::new(root.trim())
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

fn dir_name(path: &std::path::Path) -> Option<String> {
    path.file_name().map(|s| s.to_string_lossy().to_string())
}

// docker/brew integrations moved to crate::integrations::{docker,brew}

// notifications moved to crate::notify

// build_tooltip and create_template_icon moved under ui::{menu,icon}
