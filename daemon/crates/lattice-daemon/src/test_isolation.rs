//! Unit tests never read or write the operator's real Lattice state.
//!
//! Unit tests build daemons, shards and hook routes in-process, and those
//! find their state through `HOME` and the XDG base directories: the memory
//! retention registry, the resource budget registry, hook session state, the
//! daemon settings file, transport credentials. Left alone, every test run
//! registered its throwaway workspaces in the operator's real registries
//! (77 dead entries were found on 2026-09-19), read the operator's real
//! `daemon.toml`, and contended with the live daemon for the registry lock,
//! which failed shard bootstraps under a parallel run.
//!
//! Fixing each call site would miss the next one. Instead, before `main` and
//! before any test thread exists, each unit-test process points `HOME` and
//! the XDG roots at a private directory of its own. Children it spawns
//! inherit that. Two read-mostly locations are kept pointing at the real
//! ones, because moving them would change what is tested rather than where
//! state goes: the shared embedding model, and the Rust toolchain.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The operator's `HOME` as it was before isolation, for the tests below.
static REAL_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
static SANDBOX: OnceLock<PathBuf> = OnceLock::new();

#[used]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
#[cfg_attr(any(target_os = "linux", target_os = "android"), link_section = ".init_array")]
#[cfg_attr(windows, link_section = ".CRT$XCU")]
static ISOLATE_TEST_PROCESS: extern "C" fn() = isolate_test_process;

extern "C" fn isolate_test_process() {
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let sandbox = std::env::temp_dir().join(format!("lattice-test-home-{}", std::process::id()));
    for name in ["home", "state", "config", "cache", "data", "run"] {
        let dir = sandbox.join(name);
        // A process that cannot isolate itself must not run against the
        // operator's state instead. Aborting is loud; carrying on is not.
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("lattice tests: cannot create {}", dir.display());
            std::process::abort();
        }
        make_private(&dir);
    }
    make_private(&sandbox);
    let sandbox = sandbox.canonicalize().unwrap_or(sandbox);

    // Read-mostly locations that must keep resolving against the real home.
    if let Some(real_home) = &real_home {
        // Resolved while HOME is still the real one.
        if let Ok(model_dir) = lattice_core::embeddings::shared_embedding_model_dir() {
            keep_if_unset("LATTICE_EMBEDDING_MODEL_DIR", model_dir);
        }
        keep_if_unset("CARGO_HOME", real_home.join(".cargo"));
        keep_if_unset("RUSTUP_HOME", real_home.join(".rustup"));
    }

    for (variable, name) in [
        ("HOME", "home"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        std::env::set_var(variable, sandbox.join(name));
    }
    let _ = REAL_HOME.set(real_home);
    let _ = SANDBOX.set(sandbox);
    // SAFETY: `remove_sandbox` is a plain `extern "C" fn()` with no
    // captured state, as `atexit` requires.
    unsafe {
        libc::atexit(remove_sandbox);
    }
}

extern "C" fn remove_sandbox() {
    if let Some(sandbox) = SANDBOX.get() {
        let _ = std::fs::remove_dir_all(sandbox);
    }
}

/// Keep `variable` pointing at an existing real location, unless the
/// operator already chose one.
fn keep_if_unset(variable: &str, real: PathBuf) {
    if std::env::var_os(variable).is_none() && real.exists() {
        std::env::set_var(variable, real);
    }
}

fn make_private(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_process_runs_against_a_private_home_and_state_root() {
        let sandbox = SANDBOX.get().expect("isolation ran before main");
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert!(home.starts_with(sandbox), "{}", home.display());
        if let Some(Some(real_home)) = REAL_HOME.get() {
            assert_ne!(&home, real_home);
        }
        for variable in [
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            let value = PathBuf::from(std::env::var_os(variable).unwrap());
            assert!(value.starts_with(sandbox), "{variable}={}", value.display());
        }
    }

    #[test]
    fn registries_written_by_shards_land_in_the_sandbox() {
        let sandbox = SANDBOX.get().expect("isolation ran before main");
        let registry = crate::memory_retention_runtime::registry_path().unwrap();
        assert!(registry.starts_with(sandbox), "{}", registry.display());
        let budget = crate::disk_budget::registry_root().unwrap();
        assert!(budget.starts_with(sandbox), "{}", budget.display());
    }
}
