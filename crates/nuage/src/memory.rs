//! Memory: how much this server may use (its container's limit, or the
//! machine's memory), how much it uses, how much is left, the swap, and the
//! budget of the page caches.
//!
//! The page caches are the only memory Nuage DB keeps on purpose: the data
//! stays on disk and is read when needed. A server gives them one budget for
//! all its applications (`NUAGE_CACHE_MB`; by default a quarter of the
//! memory), shared equally: however many applications there are, the caches
//! never outgrow it.
//!
//! Where the figures come from, the first that answers:
//!
//! * `NUAGE_MEMORY_MB`: the memory to count on, said by hand (for a host that
//!   limits it without saying so);
//! * the container's limit and usage (cgroup v2, then v1); a host that gives
//!   the limit but not the usage (Heroku) is counted from this process's own
//!   memory, which is what the container holds;
//! * the machine (`/proc/meminfo`).
//!
//! The swap is the container's when it has a limit of its own; a container
//! whose host says nothing of it reports none, rather than the swap of the
//! whole machine it shares.

/// Memory figures of this server (Linux; `None` elsewhere, or when unknown).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryInfo {
    /// Memory this server may use: the container's limit, else the machine's.
    pub total_bytes: Option<u64>,
    /// Memory used within it: the container's usage (without the file cache
    /// the system can take back), else this process's.
    pub used_bytes: Option<u64>,
    /// Memory left for new uses: `total - used`, and no more than the machine has.
    pub available_bytes: Option<u64>,
    /// Memory this process uses (resident set).
    pub process_bytes: Option<u64>,
    /// Swap this server may use (`None` in a container whose host does not say).
    pub swap_total_bytes: Option<u64>,
    /// Swap unused.
    pub swap_free_bytes: Option<u64>,
    /// `total_bytes` is a limit set on this server (a container, a dyno, or
    /// `NUAGE_MEMORY_MB`), not the whole machine.
    pub limited: bool,
}

/// What the system says, as text and numbers: read by [`read`], turned into
/// figures by [`figures`].
#[derive(Clone, Debug, Default)]
struct Raw {
    /// `/proc/meminfo`.
    meminfo: String,
    /// `/proc/self/status`.
    status: String,
    /// `NUAGE_MEMORY_MB`, in bytes.
    declared: Option<u64>,
    cgroup: Cgroup,
}

/// The container's memory controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cgroup {
    limit: Option<u64>,
    /// Usage, file cache that can be taken back excluded.
    usage: Option<u64>,
    swap_limit: Option<u64>,
    swap_usage: Option<u64>,
}

fn meminfo_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(name)?.strip_prefix(':')?;
        let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kib * 1024)
    })
}

/// A number of bytes in a cgroup file; `max` or a huge number is no limit.
fn limit_of(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().filter(|n| *n < (1 << 50))
}

/// A line `name value` of `memory.stat`.
fn stat_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

/// The directories of this process's memory controller: its own group, then
/// the root of the mount (a container sees its group there).
fn cgroup_dirs(proc_cgroup: &str) -> (Vec<String>, Vec<String>) {
    let mut v2 = Vec::new();
    let mut v1 = Vec::new();
    for line in proc_cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let path = path.trim_end_matches('/');
        if controllers.is_empty() && !path.is_empty() {
            v2.push(format!("/sys/fs/cgroup{path}"));
        } else if controllers.split(',').any(|c| c == "memory") && !path.is_empty() {
            v1.push(format!("/sys/fs/cgroup/memory{path}"));
        }
    }
    v2.push("/sys/fs/cgroup".into());
    v1.push("/sys/fs/cgroup/memory".into());
    (v2, v1)
}

/// Reads the container's limit, usage and swap through `file` (a path to its
/// content): cgroup v2, then v1; the tightest limit found.
fn cgroup_of(proc_cgroup: &str, file: impl Fn(&str) -> Option<String>) -> Cgroup {
    let (v2, v1) = cgroup_dirs(proc_cgroup);
    let mut best: Option<Cgroup> = None;
    let mut keep = |c: Cgroup| {
        if c.limit.is_some() && best.is_none_or(|b| c.limit < b.limit) {
            best = Some(c);
        }
    };
    for dir in &v2 {
        let read = |name: &str| file(&format!("{dir}/{name}"));
        let Some(limit) = read("memory.max").as_deref().and_then(limit_of) else { continue };
        let inactive = read("memory.stat").as_deref().and_then(|s| stat_field(s, "inactive_file")).unwrap_or(0);
        keep(Cgroup {
            limit: Some(limit),
            usage: read("memory.current")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|u| u.saturating_sub(inactive)),
            swap_limit: read("memory.swap.max").as_deref().and_then(limit_of),
            swap_usage: read("memory.swap.current").and_then(|s| s.trim().parse().ok()),
        });
    }
    for dir in &v1 {
        let read = |name: &str| file(&format!("{dir}/{name}"));
        let Some(limit) = read("memory.limit_in_bytes").as_deref().and_then(limit_of) else { continue };
        let inactive = read("memory.stat").as_deref().and_then(|s| stat_field(s, "total_inactive_file")).unwrap_or(0);
        let usage = read("memory.usage_in_bytes").and_then(|s| s.trim().parse::<u64>().ok());
        // v1 counts memory and swap together.
        let both_limit = read("memory.memsw.limit_in_bytes").as_deref().and_then(limit_of);
        let both_usage = read("memory.memsw.usage_in_bytes").and_then(|s| s.trim().parse::<u64>().ok());
        keep(Cgroup {
            limit: Some(limit),
            usage: usage.map(|u| u.saturating_sub(inactive)),
            swap_limit: both_limit.map(|b| b.saturating_sub(limit)),
            swap_usage: both_usage.zip(usage).map(|(b, u)| b.saturating_sub(u)),
        });
    }
    best.unwrap_or_default()
}

fn read() -> Raw {
    let text = |path: &str| std::fs::read_to_string(path).ok();
    Raw {
        meminfo: text("/proc/meminfo").unwrap_or_default(),
        status: text("/proc/self/status").unwrap_or_default(),
        declared: std::env::var("NUAGE_MEMORY_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).map(|m| m << 20),
        cgroup: cgroup_of(&text("/proc/self/cgroup").unwrap_or_default(), text),
    }
}

fn figures(raw: &Raw) -> MemoryInfo {
    let machine_total = meminfo_field(&raw.meminfo, "MemTotal");
    let machine_available = meminfo_field(&raw.meminfo, "MemAvailable");
    let process = meminfo_field(&raw.status, "VmRSS");
    let machine_swap = (meminfo_field(&raw.meminfo, "SwapTotal"), meminfo_field(&raw.meminfo, "SwapFree"));
    // A limit counts when it is under what the machine has.
    let under = |limit: Option<u64>| limit.filter(|l| machine_total.is_none_or(|t| *l < t));
    let (limit, by_hand) = match under(raw.declared) {
        Some(l) => (Some(l), true),
        None => (under(raw.cgroup.limit), false),
    };
    let Some(limit) = limit else {
        return MemoryInfo {
            total_bytes: machine_total,
            used_bytes: machine_total.zip(machine_available).map(|(t, a)| t.saturating_sub(a)),
            available_bytes: machine_available,
            process_bytes: process,
            swap_total_bytes: machine_swap.0,
            swap_free_bytes: machine_swap.1,
            limited: false,
        };
    };
    // What the host counts against the limit; a host that does not say
    // (Heroku), or a limit said by hand: this process, the container's tenant.
    let used = if by_hand { process } else { raw.cgroup.usage.or(process) };
    let available = used.map(|u| limit.saturating_sub(u).min(machine_available.unwrap_or(u64::MAX)));
    let (swap_total, swap_free) = match raw.cgroup.swap_limit.filter(|_| !by_hand) {
        Some(total) => (Some(total), Some(total.saturating_sub(raw.cgroup.swap_usage.unwrap_or(0)))),
        // The machine's swap is shared with every other container: not this
        // server's to count on.
        None => (None, None),
    };
    MemoryInfo {
        total_bytes: Some(limit),
        used_bytes: used,
        available_bytes: available,
        process_bytes: process,
        swap_total_bytes: swap_total,
        swap_free_bytes: swap_free,
        limited: true,
    }
}

impl MemoryInfo {
    /// The figures now.
    pub fn now() -> MemoryInfo {
        figures(&read())
    }

    /// The share of the memory in use, from 0 to 1 (`None` when unknown).
    pub fn pressure(&self) -> Option<f64> {
        match (self.used_bytes, self.total_bytes) {
            (Some(used), Some(total)) if total > 0 => Some(used as f64 / total as f64),
            _ => None,
        }
    }
}

/// The budget of the page caches of a server, all applications together, for
/// `NUAGE_CACHE_MB` (`None` or `auto`: a quarter of the memory, between
/// 16 MiB and 4 GiB; 64 MiB when the memory is unknown).
pub fn cache_bytes(setting: Option<&str>) -> Result<usize, String> {
    match setting.map(str::trim) {
        None | Some("") | Some("auto") => Ok(auto_cache(MemoryInfo::now().total_bytes)),
        Some(mb) => mb
            .parse::<usize>()
            .map(|m| m << 20)
            .map_err(|_| format!("NUAGE_CACHE_MB=`{mb}` is not a number of MiB or `auto`")),
    }
}

/// The memory of each application's key-value store for `NUAGE_KV_MB`
/// (`None` or `auto`: a sixteenth of the memory, between 4 MiB and 256 MiB;
/// 32 MiB when the memory is unknown).
pub fn kv_bytes(setting: Option<&str>) -> Result<usize, String> {
    match setting.map(str::trim) {
        None | Some("") | Some("auto") => Ok(auto_kv(MemoryInfo::now().total_bytes)),
        Some(mb) => mb
            .parse::<usize>()
            .ok()
            .filter(|m| *m >= 1)
            .map(|m| m << 20)
            .ok_or_else(|| format!("NUAGE_KV_MB=`{mb}` is not a number of MiB (1 or more) or `auto`")),
    }
}

/// What the key-value stores of all the applications may hold together, when
/// `NUAGE_KV_MB` is `auto`: an eighth of the memory (and one store's part at
/// least). Each store then has the smaller of its own limit and its equal part
/// of this. `None` when the limit of each was set by hand: it is kept as said.
pub fn kv_budget(setting: Option<&str>) -> Option<usize> {
    match setting.map(str::trim) {
        None | Some("") | Some("auto") => {
            let total = MemoryInfo::now().total_bytes;
            Some(total.map_or(64 << 20, |t| (t / 8) as usize).max(auto_kv(total)))
        }
        Some(_) => None,
    }
}

/// The key-value memory of each of `applications`: its own limit, or its equal
/// part of `budget` when that is less (1 MiB at least).
pub fn kv_share(each: usize, budget: Option<usize>, applications: usize) -> usize {
    match budget {
        Some(b) => each.min((b / applications.max(1)).max(1 << 20)),
        None => each,
    }
}

fn auto_kv(total: Option<u64>) -> usize {
    match total {
        Some(t) => ((t / 16) as usize).clamp(4 << 20, 256 << 20),
        None => 32 << 20,
    }
}

fn auto_cache(total: Option<u64>) -> usize {
    match total {
        Some(t) => ((t / 4).min(4 << 30) as usize).max(16 << 20),
        None => 64 << 20,
    }
}

/// The cache of each of `applications` sharing `budget`: equal parts, 2 MiB
/// at least (below, every read would go to the disk).
pub fn cache_share(budget: usize, applications: usize) -> usize {
    (budget / applications.max(1)).max(2 << 20)
}

/// Gives back to the system the memory the allocator holds for nothing
/// (glibc keeps what was freed; other allocators return it on their own).
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` only returns free heap pages to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// On a server whose memory is limited and small (2 GiB or less), keeps the
/// allocator from spreading over too many arenas: glibc makes up to eight a
/// core, and each keeps what was freed in it. Eight in all hold a quarter
/// less memory under load, for a few percent of throughput. To call before
/// any thread starts.
pub fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let m = MemoryInfo::now();
        if m.limited && m.total_bytes.is_some_and(|t| t <= 2 << 30) && std::env::var_os("MALLOC_ARENA_MAX").is_none() {
            // SAFETY: `mallopt` sets a parameter of the allocator.
            unsafe {
                libc::mallopt(libc::M_ARENA_MAX, 8);
            }
        }
    }
}

/// Threads a server may start for blocking work (statements, backups): four
/// per core, between 16 and 64, and never more than a quarter of what the
/// system lets this user start (a Heroku dyno: 256 threads in all).
pub fn blocking_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut threads = (cores * 4).clamp(16, 64);
    if let Some(limit) = thread_limit() {
        threads = threads.min((limit / 4).max(8));
    }
    threads
}

/// How many threads (and processes) the system lets this user have, when it
/// sets a limit.
pub fn thread_limit() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `getrlimit` fills the struct it is given.
        if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut limit) } == 0 && limit.rlim_cur != libc::RLIM_INFINITY {
            return Some(limit.rlim_cur as usize);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACHINE: &str = "MemTotal:       64776276 kB\nMemFree:        16573608 kB\nMemAvailable:   41251408 kB\nSwapTotal:      127926268 kB\nSwapFree:       124741980 kB\n";
    const STATUS: &str = "Name:\tnuage\nVmRSS:\t  126000 kB\n";

    fn files(list: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let list: Vec<(String, String)> = list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |path: &str| list.iter().find(|(k, _)| k == path).map(|(_, v)| v.clone())
    }

    #[test]
    fn budgets() {
        assert_eq!(auto_cache(Some(512 << 20)), 128 << 20, "a 512 MiB dyno: a quarter");
        assert_eq!(auto_cache(Some(32 << 20)), 16 << 20, "never under 16 MiB");
        assert_eq!(auto_cache(Some(64 << 30)), 4 << 30, "never over 4 GiB");
        assert_eq!(auto_cache(None), 64 << 20);
        assert_eq!(auto_kv(Some(512 << 20)), 32 << 20, "a 512 MiB dyno");
        assert_eq!(auto_kv(Some(16 << 20)), 4 << 20);
        assert_eq!(auto_kv(Some(64 << 30)), 256 << 20);
        assert_eq!(kv_bytes(Some("8")), Ok(8 << 20));
        assert!(kv_bytes(Some("0")).is_err() && kv_bytes(Some("lots")).is_err());
        assert_eq!(cache_bytes(Some("16")), Ok(16 << 20));
        assert!(cache_bytes(Some("lots")).is_err());
        // Six applications share the budget: together they never pass it.
        assert_eq!(cache_share(128 << 20, 6) * 6, (128 << 20) / 6 * 6);
        assert_eq!(cache_share(128 << 20, 0), 128 << 20);
        assert_eq!(cache_share(16 << 20, 100), 2 << 20, "2 MiB at least");
        assert!((8..=64).contains(&blocking_threads()));
    }

    #[test]
    fn a_heroku_dyno_gives_its_limit_and_nothing_else() {
        // What a dyno shows: the limit, no usage, the host's meminfo and swap.
        let cgroup = cgroup_of("0::/\n", files(&[("/sys/fs/cgroup/memory/memory.limit_in_bytes", "536870912\n")]));
        assert_eq!(cgroup, Cgroup { limit: Some(512 << 20), ..Cgroup::default() });
        let m = figures(&Raw { meminfo: MACHINE.into(), status: STATUS.into(), declared: None, cgroup });
        assert_eq!(m.total_bytes, Some(512 << 20));
        assert_eq!(m.process_bytes, Some(126_000 * 1024));
        assert_eq!(m.used_bytes, m.process_bytes, "the dyno holds this process");
        assert_eq!(m.available_bytes, Some((512 << 20) - 126_000 * 1024));
        assert_eq!((m.swap_total_bytes, m.swap_free_bytes), (None, None), "the host's swap is not the dyno's");
        assert!(m.limited && (0.2..0.3).contains(&m.pressure().unwrap()));
    }

    #[test]
    fn containers_and_machines() {
        // Docker, cgroup v2: limit, usage without the file cache, swap of its own.
        let v2 = cgroup_of(
            "0::/\n",
            files(&[
                ("/sys/fs/cgroup/memory.max", "1073741824\n"),
                ("/sys/fs/cgroup/memory.current", "300000000\n"),
                ("/sys/fs/cgroup/memory.stat", "anon 1\ninactive_file 100000000\n"),
                ("/sys/fs/cgroup/memory.swap.max", "536870912\n"),
                ("/sys/fs/cgroup/memory.swap.current", "1048576\n"),
            ]),
        );
        let m = figures(&Raw { meminfo: MACHINE.into(), status: STATUS.into(), declared: None, cgroup: v2 });
        assert_eq!((m.total_bytes, m.used_bytes), (Some(1 << 30), Some(200_000_000)));
        assert_eq!(m.available_bytes, Some((1 << 30) - 200_000_000));
        assert_eq!((m.swap_total_bytes, m.swap_free_bytes), (Some(512 << 20), Some((512 << 20) - (1 << 20))));

        // cgroup v1, in a group of its own: memory and swap counted together.
        let v1 = cgroup_of(
            "5:memory:/docker/abc\n",
            files(&[
                ("/sys/fs/cgroup/memory/docker/abc/memory.limit_in_bytes", "268435456\n"),
                ("/sys/fs/cgroup/memory/docker/abc/memory.usage_in_bytes", "100000000\n"),
                ("/sys/fs/cgroup/memory/docker/abc/memory.memsw.limit_in_bytes", "536870912\n"),
                ("/sys/fs/cgroup/memory/docker/abc/memory.memsw.usage_in_bytes", "100000000\n"),
                ("/sys/fs/cgroup/memory/memory.limit_in_bytes", "9223372036854771712\n"),
            ]),
        );
        assert_eq!(v1.limit, Some(256 << 20));
        assert_eq!((v1.swap_limit, v1.swap_usage), (Some(256 << 20), Some(0)));

        // A service without a limit (`max`): the machine, with its swap.
        let none = cgroup_of(
            "0::/system.slice/nuage.service\n",
            files(&[("/sys/fs/cgroup/system.slice/nuage.service/memory.max", "max\n")]),
        );
        let m = figures(&Raw { meminfo: MACHINE.into(), status: STATUS.into(), declared: None, cgroup: none });
        assert!(!m.limited);
        assert_eq!(m.total_bytes, Some(64_776_276 * 1024));
        assert_eq!(m.available_bytes, Some(41_251_408 * 1024));
        assert_eq!(m.swap_total_bytes, Some(127_926_268 * 1024));

        // Said by hand: it wins, counted from this process.
        let m = figures(&Raw { meminfo: MACHINE.into(), status: STATUS.into(), declared: Some(256 << 20), cgroup: v2 });
        assert_eq!((m.total_bytes, m.used_bytes, m.limited), (Some(256 << 20), Some(126_000 * 1024), true));
        assert_eq!(m.swap_total_bytes, None);

        if cfg!(target_os = "linux") {
            let m = MemoryInfo::now();
            assert!(m.total_bytes.is_some() && m.process_bytes.is_some() && m.available_bytes.is_some(), "{m:?}");
        }
    }
}
