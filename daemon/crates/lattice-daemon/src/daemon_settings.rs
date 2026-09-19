//! User-level daemon settings.
//!
//! The daemon is started on demand by whichever stdio proxy first finds it
//! missing, and it inherits that client's environment. An environment
//! variable therefore configures one start of the daemon at best. Settings
//! that must survive a restart live in a file the daemon reads itself:
//!
//! ```toml
//! # $XDG_CONFIG_HOME/lattice/daemon.toml, default ~/.config/lattice/daemon.toml
//! max_loaded_shards = 6
//! ```
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
const KNOWN_KEYS: [&str; 1] = [MAX_LOADED_SHARDS_KEY];

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
    pub(crate) max_loaded_shards: usize,
    pub(crate) max_loaded_shards_source: SettingSource,
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
            "settings_file": self.settings_file.as_ref().map(|path| path.to_string_lossy()),
            "settings_file_present": self.settings_file_present,
        })
    }

    /// Settings for a daemon built with an explicit cap and no sources.
    pub(crate) fn unconfigured(max_loaded_shards: usize) -> Self {
        Self {
            max_loaded_shards,
            max_loaded_shards_source: SettingSource::Default,
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
pub(crate) fn load(default_max_loaded_shards: usize) -> Result<DaemonSettings> {
    let env = |name: &str| std::env::var(name).ok();
    let path = settings_file_path(&env)?;
    let text = match &path {
        Some(path) => read_settings_file(path)?,
        None => None,
    };
    resolve(&env, path, text.as_deref(), default_max_loaded_shards)
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
    default_max_loaded_shards: usize,
) -> Result<DaemonSettings> {
    // The file is validated even when the environment wins. A broken file
    // that only surfaces after the variable disappears is a trap.
    let from_file = match (&settings_file, file_text) {
        (Some(path), Some(text)) => parse_settings_file(path, text)?,
        _ => None,
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
        (Some(from_env), _) => from_env,
        (None, Some(shards)) => (
            shards,
            SettingSource::SettingsFile(settings_file.clone().expect("file value implies a path")),
        ),
        (None, None) => (
            default_max_loaded_shards.clamp(1, MAX_LOADED_SHARDS_LIMIT),
            SettingSource::Default,
        ),
    };
    Ok(DaemonSettings {
        max_loaded_shards,
        max_loaded_shards_source,
        settings_file_present: settings_file.is_some() && file_text.is_some(),
        settings_file,
    })
}

fn parse_settings_file(path: &Path, text: &str) -> Result<Option<usize>> {
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
    let Some(item) = document.get(MAX_LOADED_SHARDS_KEY) else {
        return Ok(None);
    };
    let source = format!("`{MAX_LOADED_SHARDS_KEY}` in `{}`", path.display());
    let value = match item {
        Item::Value(value) => value.as_integer().ok_or_else(|| {
            anyhow!(
                "{source} must be a whole number, got `{}`",
                value.to_string().trim()
            )
        })?,
        _ => bail!("{source} must be a whole number, not a table"),
    };
    validate_max_loaded_shards(value, &source).map(Some)
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

    #[test]
    fn precedence_is_environment_then_file_then_default_and_the_source_is_reported() {
        let none = env_of(&[]);
        let default = resolve(&none, file(), None, 4).unwrap();
        assert_eq!(default.max_loaded_shards, 4);
        assert_eq!(default.max_loaded_shards_source, SettingSource::Default);
        assert!(!default.settings_file_present);
        assert_eq!(
            default.report()["max_loaded_shards_source"],
            "built-in default"
        );

        let from_file = resolve(&none, file(), Some("max_loaded_shards = 6\n"), 4).unwrap();
        assert_eq!(from_file.max_loaded_shards, 6);
        assert_eq!(
            from_file.max_loaded_shards_source,
            SettingSource::SettingsFile(file().unwrap())
        );
        assert!(from_file.settings_file_present);
        assert_eq!(
            from_file.report(),
            json!({
                "max_loaded_shards": 6,
                "max_loaded_shards_source":
                    "settings file /home/op/.config/lattice/daemon.toml",
                "settings_file": "/home/op/.config/lattice/daemon.toml",
                "settings_file_present": true,
            })
        );

        let env = env_of(&[(MAX_LOADED_SHARDS_ENV, " 9 ")]);
        let from_env = resolve(&env, file(), Some("max_loaded_shards = 6\n"), 4).unwrap();
        assert_eq!(from_env.max_loaded_shards, 9);
        assert_eq!(
            from_env.max_loaded_shards_source.describe(),
            "environment variable LATTICE_MAX_LOADED_SHARDS"
        );

        // The older variable still works, and the current one beats it.
        let legacy = env_of(&[(MAX_LOADED_WORKSPACES_ENV, "5")]);
        assert_eq!(
            resolve(&legacy, file(), None, 4).unwrap().max_loaded_shards,
            5
        );
        let both = env_of(&[
            (MAX_LOADED_WORKSPACES_ENV, "5"),
            (MAX_LOADED_SHARDS_ENV, "7"),
        ]);
        assert_eq!(
            resolve(&both, file(), None, 4).unwrap().max_loaded_shards,
            7
        );
        // An empty variable is unset, not zero.
        let empty = env_of(&[(MAX_LOADED_SHARDS_ENV, "  ")]);
        assert_eq!(
            resolve(&empty, file(), Some("max_loaded_shards = 6"), 4)
                .unwrap()
                .max_loaded_shards,
            6
        );
    }

    #[test]
    fn a_file_without_the_key_or_with_only_comments_falls_through_to_the_default() {
        let none = env_of(&[]);
        for text in ["", "# nothing set yet\n", "\n\n"] {
            let settings = resolve(&none, file(), Some(text), 3).unwrap();
            assert_eq!(settings.max_loaded_shards, 3);
            assert_eq!(settings.max_loaded_shards_source, SettingSource::Default);
            assert!(settings.settings_file_present);
        }
        assert_eq!(resolve(&none, None, None, 0).unwrap().max_loaded_shards, 1);
        assert_eq!(
            resolve(&none, None, None, 10_000)
                .unwrap()
                .max_loaded_shards,
            MAX_LOADED_SHARDS_LIMIT
        );
    }

    #[test]
    fn invalid_file_values_are_rejected_with_the_file_and_key_named() {
        let none = env_of(&[]);
        for (text, expected) in [
            ("max_loaded_shards = 0", "must be between 1 and 64, got 0"),
            ("max_loaded_shards = -2", "must be between 1 and 64, got -2"),
            ("max_loaded_shards = 65", "must be between 1 and 64, got 65"),
            ("max_loaded_shards = \"6\"", "must be a whole number"),
            ("max_loaded_shards = 6.5", "must be a whole number"),
            ("max_loaded_shards = true", "must be a whole number"),
            ("[max_loaded_shards]\nvalue = 6", "not a table"),
            ("max_loaded_shard = 6", "unknown key `max_loaded_shard`"),
            (
                "max_loaded_shards = 6\nidle_ttl = 5",
                "unknown key `idle_ttl`",
            ),
            ("max_loaded_shards = ", "not valid TOML"),
            (
                "max_loaded_shards = 6\nmax_loaded_shards = 7",
                "not valid TOML",
            ),
        ] {
            let error = format!("{:#}", resolve(&none, file(), Some(text), 4).unwrap_err());
            assert!(error.contains(expected), "{text:?} gave {error}");
            assert!(error.contains("daemon.toml"), "{error}");
        }
    }

    #[test]
    fn invalid_environment_values_are_rejected_rather_than_ignored() {
        for (value, expected) in [
            ("six", "is not a whole number"),
            ("6.0", "is not a whole number"),
            ("0", "must be between 1 and 64, got 0"),
            ("-1", "must be between 1 and 64, got -1"),
            ("1000", "must be between 1 and 64, got 1000"),
        ] {
            let env = env_of(&[(MAX_LOADED_SHARDS_ENV, value)]);
            let error = format!("{:#}", resolve(&env, file(), None, 4).unwrap_err());
            assert!(error.contains(expected), "{value:?} gave {error}");
            assert!(error.contains(MAX_LOADED_SHARDS_ENV), "{error}");
        }
    }

    #[test]
    fn a_broken_file_is_an_error_even_when_the_environment_would_win() {
        let env = env_of(&[(MAX_LOADED_SHARDS_ENV, "9")]);
        let error = resolve(&env, file(), Some("max_loaded_shards = 0"), 4).unwrap_err();
        assert!(format!("{error:#}").contains("daemon.toml"));
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
