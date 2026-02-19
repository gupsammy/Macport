use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const COMMAND_CACHE_TTL: Duration = Duration::from_secs(300);

struct CacheEntry {
    path: String,
    cached_at: Instant,
}

static COMMAND_CACHE: OnceLock<Mutex<HashMap<&'static str, CacheEntry>>> = OnceLock::new();

/// Find an executable in common Homebrew locations, falling back to PATH.
/// Results are cached for efficiency.
pub fn find_command(name: &str) -> String {
    if let Some(key) = tracked_command_key(name) {
        let cache = COMMAND_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(entry) = guard.get(key) {
            if entry.cached_at.elapsed() < COMMAND_CACHE_TTL && is_cached_path_valid(&entry.path) {
                return entry.path.clone();
            }
        }

        let resolved = find_in_paths(name, HOMEBREW_PATHS);
        guard.insert(
            key,
            CacheEntry {
                path: resolved.clone(),
                cached_at: Instant::now(),
            },
        );
        return resolved;
    }

    find_in_paths(name, HOMEBREW_PATHS)
}

const HOMEBREW_PATHS: &[&str] = &[
    "/opt/homebrew/bin", // Apple Silicon
    "/usr/local/bin",    // Intel Mac
];

fn find_in_paths(name: &str, prefix_paths: &[&str]) -> String {
    for prefix in prefix_paths {
        let full_path = format!("{}/{}", prefix, name);
        if Path::new(&full_path).exists() {
            return full_path;
        }
    }
    name.to_string()
}

fn tracked_command_key(name: &str) -> Option<&'static str> {
    match name {
        "docker" => Some("docker"),
        "brew" => Some("brew"),
        "terminal-notifier" => Some("terminal-notifier"),
        _ => None,
    }
}

fn is_cached_path_valid(path: &str) -> bool {
    if !Path::new(path).is_absolute() {
        return true;
    }
    Path::new(path).exists()
}
