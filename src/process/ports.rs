use std::collections::HashSet;
use std::process::Command;

use anyhow::{anyhow, Context, Result};

use crate::model::ProcessInfo;

pub fn scan_ports(port_ranges: &[(u16, u16)]) -> Result<Vec<ProcessInfo>> {
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
                if let (Some(pid), Some(cmd)) = (current_pid, current_cmd.as_ref()) {
                    if let Some(port) = parse_port_from_lsof(val.trim()) {
                        if in_ranges(port, port_ranges) && seen.insert((port, pid)) {
                            results.push(ProcessInfo {
                                port,
                                pid,
                                command: cmd.clone(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    results.sort();
    Ok(results)
}

/// Verify that a PID is still associated with a TCP listener.
/// Used to mitigate TOCTOU race conditions before killing a process.
pub fn verify_pid_is_listener(pid: i32) -> bool {
    let output = Command::new("lsof")
        .args([
            "-nP",
            "-p",
            &pid.to_string(),
            "-iTCP",
            "-sTCP:LISTEN",
            "-Fn",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            // If lsof returns any "n" lines, PID is still listening
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.starts_with('n'))
        }
        _ => false,
    }
}

// Extract a port number from an lsof name field.
// Handles "*:3000", "127.0.0.1:5173", and "[::1]:8000".
pub fn parse_port_from_lsof(name: &str) -> Option<u16> {
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

#[cfg(test)]
mod tests {
    use super::parse_port_from_lsof;

    #[test]
    fn parses_ipv4_wildcard() {
        assert_eq!(parse_port_from_lsof("*:3000"), Some(3000));
    }

    #[test]
    fn parses_ipv4_localhost() {
        assert_eq!(parse_port_from_lsof("127.0.0.1:5173"), Some(5173));
    }

    #[test]
    fn parses_ipv6_localhost() {
        assert_eq!(parse_port_from_lsof("[::1]:8000"), Some(8000));
    }

    #[test]
    fn rejects_non_listen_or_flow() {
        assert_eq!(parse_port_from_lsof("127.0.0.1:abcd"), None);
        assert_eq!(parse_port_from_lsof("127.0.0.1->192.168.0.1:1234"), None);
        assert_eq!(parse_port_from_lsof("garbage"), None);
    }
}
