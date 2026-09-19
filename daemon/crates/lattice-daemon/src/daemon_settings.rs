//! User-level daemon settings.
//!
//! The daemon is started on demand by whichever stdio proxy first finds it
//! missing, and it inherits that client's environment. An environment
//! variable therefore configures one start of the daemon at best. Settings
//! that must survive a restart live in a file the daemon reads itself:
//!
//! ```toml
//! # $XDG_CONFIG_HOME/lattice/daemon.toml, default ~/.config/lattice/daemon.toml
//! memory_budget_mb = 6144     # optional; default is a third of physical memory
//! max_loaded_shards = 6       # optional hard ceiling; default is no ceiling
//! ```
//!
//! Capacity follows demand: every workspace with a connected agent gets a
//! shard, and real memory is the limit. `max_loaded_shards` is only a
//! ceiling an operator may impose. See `docs/shard-capacity.md`.
//!
//! Precedence is environment, then file, then built-in default. An invalid
//! value from either source stops the daemon with an error that names the
//! source. Silently falling back would run with a setting the operator did
//! not choose, which is how a cap of 3 starved a workspace for days.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item};

pub(crate) const SETTINGS_FILE_NAME: &str = "daemon.toml";
pub(crate) const MAX_LOADED_SHARDS_KEY: &str = "max_loaded_shards";
pub(crate) const MAX_LOADED_SHARDS_ENV: &str = "LATTICE_MAX_LOADED_SHARDS";
/// Older name for the same setting. Still honoured, after the current one.
pub(crate) const MAX_LOADED_WORKSPACES_ENV: &str = "LATTICE_MAX_LOADED_WORKSPACES";
/// Each loaded shard holds a graph, an index and caches in memory. A cap
/// beyond this is a typo, not a plan.
pub(crate) const MAX_LOADED_SHARDS_LIMIT: usize = 64;
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
pub(crate) const MEMORY_BUDGET_KEY: &str = "memory_budget_mb";
pub(crate) const MEMORY_BUDGET_ENV: &str = "LATTICE_MEMORY_BUDGET_MB";
/// Below this the daemon cannot hold even one large workspace.
pub(crate) const MIN_MEMORY_BUDGET_MB: u64 = 512;
pub(crate) const MAX_MEMORY_BUDGET_MB: u64 = 4 * 1024 * 1024;
/// Used when physical memory cannot be read.
const FALLBACK_MEMORY_BUDGET_MB: u64 = 4 * 1024;
const MIN_DEFAULT_MEMORY_BUDGET_MB: u64 = 2 * 1024;
const KNOWN_KEYS: [&str; 2] = [MAX_LOADED_SHARDS_KEY, MEMORY_BUDGET_KEY];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SettingSource {
    Environment(&'static str),
    SettingsFile(PathBuf),
    Default,
}

impl SettingSource {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Environment(name) => format!("environment variable {name}"),
            Self::SettingsFile(path) => format!("settings file {}", path.display()),
            Self::Default => "built-in default".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DaemonSettings {
    /// An optional hard ceiling on loaded shards. `None` is the default:
    /// a count is what starved a connected workspace for days, and memory,
    /// not a number of workspaces, is what actually runs out.
    pub(crate) max_loaded_shards: Option<usize>,
    pub(crate) max_loaded_shards_source: SettingSource,
    /// Budget for the daemon's real memory footprint, in bytes.
    pub(crate) memory_budget_bytes: u64,
    pub(crate) memory_budget_source: SettingSource,
    /// Where the settings file is looked for, whether or not it exists.
    pub(crate) settings_file: Option<PathBuf>,
    pub(crate) settings_file_present: bool,
}

impl DaemonSettings {
    /// What `status` and `doctor` show: the effective value and where it
    /// came from, so an operator never has to guess which source won.
    pub(crate) fn report(&self) -> Value {
        json!({
            "max_loaded_shards": self.max_loaded_shards,
            "max_loaded_shards_source": self.max_loaded_shards_source.describe(),
            "memory_budget_bytes": self.memory_budget_bytes,
            "memory_budget_source": self.memory_budget_source.describe(),
            "settings_file": self.settings_file.as_ref().map(|path| path.to_string_lossy()),
            "settings_file_present": self.settings_file_present,
        })
    }

    /// Settings for a daemon built with an explicit ceiling and no sources.
    pub(crate) fn unconfigured(max_loaded_shards: Option<usize>) -> Self {
        Self {
            max_loaded_shards,
            max_loaded_shards_source: SettingSource::Default,
            memory_budget_bytes: default_memory_budget_mb(physical_memory_bytes()) * 1024 * 1024,
            memory_budget_source: SettingSource::Default,
            settings_file: None,
            settings_file_present: false,
        }
    }
}

/// `$XDG_CONFIG_HOME/lattice/daemon.toml`, else `~/.config/lattice/daemon.toml`.
/// Mirrors how Lattice places state under `XDG_STATE_HOME`.
pub(crate) fn settings_file_path(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<PathBuf>> {
    if let Some(config_home) = env("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let config_home = PathBuf::from(config_home);
        if !config_home.is_absolute() {
            bail!(
                "XDG_CONFIG_HOME must be an absolute path, got `{}`",
                config_home.display()
            );
        }
        return Ok(Some(config_home.join("lattice").join(SETTINGS_FILE_NAME)));
    }
    Ok(env("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| {
            home.join(".config")
                .join("lattice")
                .join(SETTINGS_FILE_NAME)
        }))
}

/// Load settings for this process from its real environment and home.
pub(crate) fn load() -> Result<DaemonSettings> {
    let env = |name: &str| std::env::var(name).ok();
    let path = settings_file_path(&env)?;
    let text = match &path {
        Some(path) => read_settings_file(path)?,
        None => None,
    };
    resolve(&env, path, text.as_deref(), physical_memory_bytes())
}

/// A third of physical memory, and never less than 2 GiB.
///
/// Measured on macOS in 2026-09, a freshly loaded workspace costs about
/// 0.12 MB of real footprint per indexed file: 360 MB for 3,066 files, so six
/// mid-sized repositories need roughly 3 to 4 GiB. A third of a 16 GiB machine
/// is 5.3 GiB, which holds that with room for growth between sweeps, and
/// leaves two thirds for the editor, the agents and the builds they run.
pub(crate) fn default_memory_budget_mb(physical_memory_bytes: Option<u64>) -> u64 {
    match physical_memory_bytes {
        Some(bytes) => (bytes / (1024 * 1024) / 3).max(MIN_DEFAULT_MEMORY_BUDGET_MB),
        None => FALLBACK_MEMORY_BUDGET_MB,
    }
}

pub(crate) fn physical_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut value: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: the name is a NUL-terminated literal, `value` is a writable
        // u64 and `size` holds its length, as `hw.memsize` requires.
        let ok = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut value as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            ) == 0
        };
        (ok && value > 0).then_some(value)
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kib = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        Some(kib * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn read_settings_file(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read daemon settings file `{}`", path.display()))
        }
    };
    if !metadata.is_file() {
        bail!(
            "daemon settings file `{}` is not a regular file",
            path.display()
        );
    }
    if metadata.len() > MAX_SETTINGS_BYTES {
        bail!(
            "daemon settings file `{}` is larger than {MAX_SETTINGS_BYTES} bytes",
            path.display()
        );
    }
    std::fs::read_to_string(path)
        .map(Some)
        .with_context(|| format!("cannot read daemon settings file `{}`", path.display()))
}

/// Pure resolution. `file_text` is the settings file's content when present.
pub(crate) fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    settings_file: Option<PathBuf>,
    file_text: Option<&str>,
    physical_memory_bytes: Option<u64>,
) -> Result<DaemonSettings> {
    // The file is validated even when the environment wins. A broken file
    // that only surfaces after the variable disappears is a trap.
    let file = match (&settings_file, file_text) {
        (Some(path), Some(text)) => parse_settings_file(path, text)?,
        _ => FileSettings::default(),
    };
    let from_file = file.max_loaded_shards;
    let budget_from_env = env(MEMORY_BUDGET_ENV)
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            let parsed = value.trim().parse::<i64>().map_err(|_| {
                anyhow!("environment variable {MEMORY_BUDGET_ENV}=`{value}` is not a whole number")
            })?;
            validate_memory_budget_mb(
                parsed,
                &format!("environment variable {MEMORY_BUDGET_ENV}"),
            )
        })
        .transpose()?;
    let (memory_budget_mb, memory_budget_source) = match (budget_from_env, file.memory_budget_mb)
    {
        (Some(mb), _) => (mb, SettingSource::Environment(MEMORY_BUDGET_ENV)),
        (None, Some(mb)) => (
            mb,
            SettingSource::SettingsFile(settings_file.clone().expect("file value implies a path")),
        ),
        (None, None) => (
            default_memory_budget_mb(physical_memory_bytes),
            SettingSource::Default,
        ),
    };
    let from_env = [MAX_LOADED_SHARDS_ENV, MAX_LOADED_WORKSPACES_ENV]
        .into_iter()
        .find_map(|name| {
            env(name)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (name, value))
        })
        .map(|(name, value)| {
            let parsed = value.trim().parse::<i64>().map_err(|_| {
                anyhow!("environment variable {name}=`{value}` is not a whole number")
            })?;
            validate_max_loaded_shards(parsed, &format!("environment variable {name}"))
                .map(|shards| (shards, SettingSource::Environment(name)))
        })
        .transpose()?;

    let (max_loaded_shards, max_loaded_shards_source) = match (from_env, from_file) {
        (Some((shards, source)), _) => (Some(shards), source),
        (None, Some(shards)) => (
            Some(shards),
            SettingSource::SettingsFile(settings_file.clone().expect("file value implies a path")),
        ),
        (None, None) => (None, SettingSource::Default),
    };
    Ok(DaemonSettings {
        max_loaded_shards,
        max_loaded_shards_source,
        memory_budget_bytes: memory_budget_mb * 1024 * 1024,
        memory_budget_source,
        settings_file_present: settings_file.is_some() && file_text.is_some(),
        settings_file,
    })
}

#[derive(Debug, Default)]
struct FileSettings {
    max_loaded_shards: Option<usize>,
    memory_budget_mb: Option<u64>,
}

fn whole_number(document: &DocumentMut, key: &str, path: &Path) -> Result<Option<(i64, String)>> {
    let Some(item) = document.get(key) else {
        return Ok(None);
    };
    let source = format!("`{key}` in `{}`", path.display());
    let value = match item {
        Item::Value(value) => value.as_integer().ok_or_else(|| {
            anyhow!(
                "{source} must be a whole number, got `{}`",
                value.to_string().trim()
            )
        })?,
        _ => bail!("{source} must be a whole number, not a table"),
    };
    Ok(Some((value, source)))
}

fn parse_settings_file(path: &Path, text: &str) -> Result<FileSettings> {
    let document = text.parse::<DocumentMut>().map_err(|error| {
        anyhow!(
            "daemon settings file `{}` is not valid TOML: {error}",
            path.display()
        )
    })?;
    // An unknown key is almost always a misspelt known one, and ignoring it
    // would leave the operator believing a setting is in force.
    if let Some((key, _)) = document.iter().find(|(key, _)| !KNOWN_KEYS.contains(key)) {
        bail!(
            "daemon settings file `{}` has unknown key `{key}`; known keys: {}",
            path.display(),
            KNOWN_KEYS.join(", ")
        );
    }
    Ok(FileSettings {
        max_loaded_shards: whole_number(&document, MAX_LOADED_SHARDS_KEY, path)?
            .map(|(value, source)| validate_max_loaded_shards(value, &source))
            .transpose()?,
        memory_budget_mb: whole_number(&document, MEMORY_BUDGET_KEY, path)?
            .map(|(value, source)| validate_memory_budget_mb(value, &source))
            .transpose()?,
    })
}

fn validate_memory_budget_mb(value: i64, source: &str) -> Result<u64> {
    if !(MIN_MEMORY_BUDGET_MB as i64..=MAX_MEMORY_BUDGET_MB as i64).contains(&value) {
        bail!(
            "{source} must be between {MIN_MEMORY_BUDGET_MB} and {MAX_MEMORY_BUDGET_MB} (MiB), got {value}"
        );
    }
    Ok(value as u64)
}

fn validate_max_loaded_shards(value: i64, source: &str) -> Result<usize> {
    if !(1..=MAX_LOADED_SHARDS_LIMIT as i64).contains(&value) {
        bail!("{source} must be between 1 and {MAX_LOADED_SHARDS_LIMIT}, got {value}");
    }
    Ok(value as usize)
}

/// The memory this process really occupies, in bytes.
///
/// On macOS this is the physical footprint, which includes compressed pages.
/// Resident size does not: a daemon that has been idle for days can show tens
/// of megabytes resident while holding gigabytes compressed, which is how a
/// 8.8 GiB daemon was first measured as 191 MB. On Linux it is resident size.
/// Elsewhere, or if the platform call fails, it is unknown rather than zero.
pub(crate) fn process_memory_footprint_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: `info` is a correctly sized, writable `rusage_info_v2`, the
        // flavor passed is the one that fills exactly that struct, and the
        // pid is this process. The value is read only after success.
        let filled = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V2,
                info.as_mut_ptr().cast::<libc::rusage_info_t>(),
            ) == 0
        };
        // SAFETY: initialised by the successful call above.
        filled.then(|| unsafe { info.assume_init() }.ri_phys_footprint)
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        // SAFETY: `sysconf` has no preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        u64::try_from(page_size)
            .ok()
            .map(|size| resident_pages * size)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn file() -> Option<PathBuf> {
        Some(PathBuf::from("/home/op/.config/lattice/daemon.toml"))
    }

    const GIB: u64 = 1024 * 1024 * 1024;
    const RAM_16G: Option<u64> = Some(16 * GIB);

    #[test]
    fn by_default_there_is_no_shard_ceiling_and_memory_is_a_third_of_ram() {
        let settings = resolve(&env_of(&[]), file(), None, RAM_16G).unwrap();
        assert_eq!(settings.max_loaded_shards, None);
        assert_eq!(settings.max_loaded_shards_source, SettingSource::Default);
        assert_eq!(settings.memory_budget_bytes, 5461 * 1024 * 1024);
        assert_eq!(settings.memory_budget_source, SettingSource::Default);
        assert!(!settings.settings_file_present);
        let report = settings.report();
        assert!(report["max_loaded_shards"].is_null());
        assert_eq!(report["max_loaded_shards_source"], "built-in default");

        // Small machines keep a usable floor; unknown memory gets a fixed value.
        assert_eq!(default_memory_budget_mb(Some(4 * GIB)), 2048);
        assert_eq!(default_memory_budget_mb(Some(64 * GIB)), 21_845);
        assert_eq!(default_memory_budget_mb(None), 4096);
    }

    #[test]
    fn a_ceiling_written_before_demand_driven_capacity_still_works() {
        // The file rolled out on 2026-09-19 holds exactly this.
        let settings =
            resolve(&env_of(&[]), file(), Some("max_loaded_shards = 6\n"), RAM_16G).unwrap();
        assert_eq!(settings.max_loaded_shards, Some(6));
        assert_eq!(
            settings.max_loaded_shards_source,
            SettingSource::SettingsFile(file().unwrap())
        );
        assert!(settings.settings_file_present);
        assert_eq!(settings.memory_budget_source, SettingSource::Default);
        let report = settings.report();
        assert_eq!(report["max_loaded_shards"], 6);
        assert_eq!(
            report["max_loaded_shards_source"],
            "settings file /home/op/.config/lattice/daemon.toml"
        );
        assert_eq!(report["settings_file_present"], true);
    }

    #[test]
    fn precedence_is_environment_then_file_then_default_for_both_keys() {
        let text = "max_loaded_shards = 6\nmemory_budget_mb = 3000\n";
        let from_file = resolve(&env_of(&[]), file(), Some(text), RAM_16G).unwrap();
        assert_eq!(from_file.max_loaded_shards, Some(6));
        assert_eq!(from_file.memory_budget_bytes, 3000 * 1024 * 1024);
        assert_eq!(
            from_file.memory_budget_source,
            SettingSource::SettingsFile(file().unwrap())
        );

        let env = env_of(&[(MAX_LOADED_SHARDS_ENV, " 9 "), (MEMORY_BUDGET_ENV, "8192")]);
        let from_env = resolve(&env, file(), Some(text), RAM_16G).unwrap();
        assert_eq!(from_env.max_loaded_shards, Some(9));
        assert_eq!(
            from_env.max_loaded_shards_source.describe(),
            "environment variable LATTICE_MAX_LOADED_SHARDS"
        );
        assert_eq!(from_env.memory_budget_bytes, 8192 * 1024 * 1024);
        assert_eq!(
            from_env.memory_budget_source.describe(),
            "environment variable LATTICE_MEMORY_BUDGET_MB"
        );

        // The older variable still works, and the current one beats it.
        let legacy = env_of(&[(MAX_LOADED_WORKSPACES_ENV, "5")]);
        assert_eq!(
            resolve(&legacy, file(), None, RAM_16G).unwrap().max_loaded_shards,
            Some(5)
        );
        let both = env_of(&[(MAX_LOADED_WORKSPACES_ENV, "5"), (MAX_LOADED_SHARDS_ENV, "7")]);
        assert_eq!(
            resolve(&both, file(), None, RAM_16G).unwrap().max_loaded_shards,
            Some(7)
        );
        // An empty variable is unset, not zero.
        let empty = env_of(&[(MAX_LOADED_SHARDS_ENV, "  "), (MEMORY_BUDGET_ENV, "")]);
        let settings = resolve(&empty, file(), Some(text), RAM_16G).unwrap();
        assert_eq!(settings.max_loaded_shards, Some(6));
        assert_eq!(settings.memory_budget_bytes, 3000 * 1024 * 1024);
    }

    #[test]
    fn a_file_without_keys_or_with_only_comments_falls_through_to_the_defaults() {
        for text in ["", "# nothing set yet\n", "\n\n"] {
            let settings = resolve(&env_of(&[]), file(), Some(text), RAM_16G).unwrap();
            assert_eq!(settings.max_loaded_shards, None);
            assert_eq!(settings.memory_budget_source, SettingSource::Default);
            assert!(settings.settings_file_present);
        }
    }

    #[test]
    fn invalid_file_values_are_rejected_with_the_file_and_key_named() {
        for (text, expected) in [
            ("max_loaded_shards = 0", "must be between 1 and 64, got 0"),
            ("max_loaded_shards = -2", "must be between 1 and 64, got -2"),
            ("max_loaded_shards = 65", "must be between 1 and 64, got 65"),
            ("max_loaded_shards = \"6\"", "must be a whole number"),
            ("max_loaded_shards = 6.5", "must be a whole number"),
            ("max_loaded_shards = true", "must be a whole number"),
            ("[max_loaded_shards]\nvalue = 6", "not a table"),
            ("max_loaded_shard = 6", "unknown key `max_loaded_shard`"),
            ("max_loaded_shards = 6\nidle_ttl = 5", "unknown key `idle_ttl`"),
            ("max_loaded_shards = ", "not valid TOML"),
            ("max_loaded_shards = 6\nmax_loaded_shards = 7", "not valid TOML"),
            ("memory_budget_mb = 100", "must be between 512 and"),
            ("memory_budget_mb = -1", "must be between 512 and"),
            ("memory_budget_mb = \"6GB\"", "must be a whole number"),
            ("memory_budget = 4096", "unknown key `memory_budget`"),
        ] {
            let error = format!(
                "{:#}",
                resolve(&env_of(&[]), file(), Some(text), RAM_16G).unwrap_err()
            );
            assert!(error.contains(expected), "{text:?} gave {error}");
            assert!(error.contains("daemon.toml"), "{error}");
        }
    }

    #[test]
    fn invalid_environment_values_are_rejected_rather_than_ignored() {
        for (name, value, expected) in [
            (MAX_LOADED_SHARDS_ENV, "six", "is not a whole number"),
            (MAX_LOADED_SHARDS_ENV, "6.0", "is not a whole number"),
            (MAX_LOADED_SHARDS_ENV, "0", "must be between 1 and 64, got 0"),
            (MAX_LOADED_SHARDS_ENV, "-1", "must be between 1 and 64, got -1"),
            (MAX_LOADED_SHARDS_ENV, "1000", "must be between 1 and 64, got 1000"),
            (MEMORY_BUDGET_ENV, "lots", "is not a whole number"),
            (MEMORY_BUDGET_ENV, "64", "must be between 512 and"),
        ] {
            let env = env_of(&[(name, value)]);
            let error = format!("{:#}", resolve(&env, file(), None, RAM_16G).unwrap_err());
            assert!(error.contains(expected), "{value:?} gave {error}");
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn a_broken_file_is_an_error_even_when_the_environment_would_win() {
        let env = env_of(&[(MAX_LOADED_SHARDS_ENV, "9")]);
        let error = resolve(&env, file(), Some("max_loaded_shards = 0"), RAM_16G).unwrap_err();
        assert!(format!("{error:#}").contains("daemon.toml"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn physical_memory_is_readable_and_plausible_here() {
        let bytes = physical_memory_bytes().expect("physical memory is readable here");
        assert!(bytes >= GIB, "{bytes}");
        assert!(bytes < 64 * 1024 * GIB, "{bytes}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_process_footprint_is_known_plausible_and_grows_with_allocation() {
        let before = process_memory_footprint_bytes().expect("footprint is readable here");
        assert!(
            before > 1024 * 1024,
            "{before} bytes is too small to be real"
        );
        // Touch every page so the allocation is actually backed.
        let mut ballast = vec![0_u8; 64 * 1024 * 1024];
        for index in (0..ballast.len()).step_by(4096) {
            ballast[index] = (index % 251) as u8;
        }
        let after = process_memory_footprint_bytes().unwrap();
        assert!(ballast.iter().step_by(4096).any(|byte| *byte != 0));
        assert!(
            after >= before + 32 * 1024 * 1024,
            "footprint went from {before} to {after} after touching 64 MiB"
        );
    }

    #[test]
    fn the_settings_file_follows_xdg_config_home_then_home() {
        let xdg = env_of(&[("XDG_CONFIG_HOME", "/etc/xdg-op"), ("HOME", "/home/op")]);
        assert_eq!(
            settings_file_path(&xdg).unwrap(),
            Some(PathBuf::from("/etc/xdg-op/lattice/daemon.toml"))
        );
        let home = env_of(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/op")]);
        assert_eq!(
            settings_file_path(&home).unwrap(),
            Some(PathBuf::from("/home/op/.config/lattice/daemon.toml"))
        );
        assert_eq!(settings_file_path(&env_of(&[])).unwrap(), None);
        assert_eq!(
            settings_file_path(&env_of(&[("HOME", "relative")])).unwrap(),
            None
        );
        assert!(settings_file_path(&env_of(&[("XDG_CONFIG_HOME", "relative")])).is_err());
    }

    #[test]
    fn loading_reads_a_real_file_and_rejects_a_directory_in_its_place() {
        let root = std::env::temp_dir().join(format!(
            "lattice-daemon-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("lattice").join(SETTINGS_FILE_NAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(read_settings_file(&path).unwrap(), None);
        std::fs::write(&path, "max_loaded_shards = 6\n").unwrap();
        assert_eq!(
            read_settings_file(&path).unwrap().as_deref(),
            Some("max_loaded_shards = 6\n")
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            format!("{:#}", read_settings_file(&path).unwrap_err()).contains("not a regular file")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
