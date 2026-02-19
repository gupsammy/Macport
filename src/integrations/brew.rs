use std::collections::HashMap;
use std::process::Command;

use anyhow::Result;
use log::warn;

use crate::model::KillFeedback;
use crate::utils::find_command;

pub fn query_brew_services_map() -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let out = Command::new(find_command("brew"))
        .args(["services", "list"])
        .output();
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
    for (idx, line) in stdout.lines().enumerate() {
        if idx == 0 {
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

pub fn get_brew_managed_service(
    cmd: &str,
    port: u16,
    brew_services_map: &HashMap<String, String>,
) -> Option<String> {
    let potential_service = map_brew_service_from_cmd(cmd)?;
    if let Some(status) = brew_services_map.get(&potential_service) {
        if status == "started" {
            let expected_port = get_default_port_for_service(&potential_service);
            if Some(port) == expected_port {
                return Some(potential_service);
            }
        }
    }
    None
}

pub fn run_brew_stop(service: &str) -> KillFeedback {
    let res = Command::new(find_command("brew"))
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

fn get_default_port_for_service(service: &str) -> Option<u16> {
    match service {
        "redis" => Some(6379),
        "postgresql" => Some(5432),
        "mysql" => Some(3306),
        "mongodb-community" => Some(27017),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brew_mapping_happy_and_mismatch() {
        let mut map = HashMap::new();
        map.insert("redis".to_string(), "started".to_string());
        map.insert("postgresql".to_string(), "stopped".to_string());

        // Matches default port and started
        assert_eq!(
            get_brew_managed_service("redis-server", 6379, &map),
            Some("redis".into())
        );

        // Wrong port shouldn't match
        assert_eq!(get_brew_managed_service("redis-server", 6380, &map), None);

        // Not started shouldn't match
        assert_eq!(get_brew_managed_service("postgres", 5432, &map), None);

        // Unknown service returns None
        assert_eq!(get_brew_managed_service("myapp", 3000, &map), None);
    }
}
