//! Memory: how much the machine has (its container's limit included), how
//! much the server uses, the swap, and the page cache each application's
//! database gets.
//!
//! The cache is the only memory Nuage DB keeps on purpose: the data stays on
//! disk and is read when needed. `NUAGE_CACHE_MB` sets it for each
//! application; by default (`auto`) it is a share of the memory, so a small
//! server (a 512 MiB dyno, a 1 GiB VPS) keeps most of its memory free.

/// Memory figures of the machine and of this process (Linux; `None`
/// elsewhere, or when unknown).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryInfo {
    /// Memory of the machine, or of the container when it is limited.
    pub total_bytes: Option<u64>,
    /// Memory free for new uses (`MemAvailable`, within the container's limit).
    pub available_bytes: Option<u64>,
    /// Memory this process uses (resident set).
    pub process_bytes: Option<u64>,
    /// Swap of the machine.
    pub swap_total_bytes: Option<u64>,
    /// Swap unused.
    pub swap_free_bytes: Option<u64>,
}

fn meminfo_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(name)?.strip_prefix(':')?;
        let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kib * 1024)
    })
}

/// The memory limit of this process's container (cgroup v2, then v1).
fn cgroup_limit() -> Option<u64> {
    for path in ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            // `max`, or a huge number: no limit.
            if let Ok(n) = s.parse::<u64>()
                && n < (1 << 50)
            {
                return Some(n);
            }
        }
    }
    None
}

fn cgroup_usage() -> Option<u64> {
    for path in ["/sys/fs/cgroup/memory.current", "/sys/fs/cgroup/memory/memory.usage_in_bytes"] {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(n) = s.trim().parse::<u64>()
        {
            return Some(n);
        }
    }
    None
}

impl MemoryInfo {
    /// The figures now.
    pub fn now() -> MemoryInfo {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let machine_total = meminfo_field(&meminfo, "MemTotal");
        let machine_available = meminfo_field(&meminfo, "MemAvailable");
        let (total, available) = match (cgroup_limit(), machine_total) {
            (Some(limit), Some(t)) if limit < t => {
                let used = cgroup_usage().unwrap_or(0);
                (Some(limit), Some(limit.saturating_sub(used).min(machine_available.unwrap_or(u64::MAX))))
            }
            _ => (machine_total, machine_available),
        };
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        MemoryInfo {
            total_bytes: total,
            available_bytes: available,
            process_bytes: meminfo_field(&status, "VmRSS"),
            swap_total_bytes: meminfo_field(&meminfo, "SwapTotal"),
            swap_free_bytes: meminfo_field(&meminfo, "SwapFree"),
        }
    }
}

/// The page cache of each application's database for `NUAGE_CACHE_MB`
/// (`None` or `auto`: an eighth of the memory, between 8 MiB and 512 MiB;
/// 64 MiB when the memory is unknown).
pub fn cache_bytes(setting: Option<&str>) -> Result<usize, String> {
    match setting.map(str::trim) {
        None | Some("") | Some("auto") => Ok(auto_cache(MemoryInfo::now().total_bytes)),
        Some(mb) => mb
            .parse::<usize>()
            .map(|m| m << 20)
            .map_err(|_| format!("NUAGE_CACHE_MB=`{mb}` is not a number of MiB or `auto`")),
    }
}

fn auto_cache(total: Option<u64>) -> usize {
    match total {
        Some(t) => ((t / 8) as usize).clamp(8 << 20, 512 << 20),
        None => 64 << 20,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_and_figures() {
        assert_eq!(auto_cache(Some(512 << 20)), 64 << 20, "a 512 MiB dyno");
        assert_eq!(auto_cache(Some(32 << 20)), 8 << 20, "never under 8 MiB");
        assert_eq!(auto_cache(Some(16 << 30)), 512 << 20, "never over 512 MiB");
        assert_eq!(auto_cache(None), 64 << 20);
        assert_eq!(cache_bytes(Some("16")), Ok(16 << 20));
        assert!(cache_bytes(Some("lots")).is_err());
        assert!(cache_bytes(Some("auto")).unwrap() >= 8 << 20);
        let text = "MemTotal:        2048000 kB\nSwapTotal:   1024 kB\n";
        assert_eq!(meminfo_field(text, "MemTotal"), Some(2_048_000 * 1024));
        assert_eq!(meminfo_field(text, "SwapTotal"), Some(1024 * 1024));
        assert_eq!(meminfo_field(text, "Swap"), None);
        if cfg!(target_os = "linux") {
            let m = MemoryInfo::now();
            assert!(m.total_bytes.is_some() && m.process_bytes.is_some(), "{m:?}");
        }
    }
}
