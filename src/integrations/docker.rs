use std::collections::HashMap;
use std::process::Command;

use anyhow::Result;
use log::warn;

use crate::model::{DockerContainerInfo, KillFeedback};
use crate::utils::find_command;

pub fn query_docker_port_map() -> Result<HashMap<u16, DockerContainerInfo>> {
    let mut map = HashMap::new();
    let out = Command::new(find_command("docker"))
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
        for seg in ports.split(',') {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            if let Some((left, _right)) = seg.split_once("->") {
                if let Some((_, host)) = left.rsplit_once(':') {
                    if host.contains('-') {
                        continue;
                    }
                    if let Ok(p) = host.parse::<u16>() {
                        map.insert(
                            p,
                            DockerContainerInfo {
                                name: name.clone(),
                                id: id.clone(),
                            },
                        );
                    }
                }
            }
        }
    }
    Ok(map)
}

pub fn run_docker_stop(container: &str) -> KillFeedback {
    let res = Command::new(find_command("docker"))
        .args(["stop", container])
        .output();
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
