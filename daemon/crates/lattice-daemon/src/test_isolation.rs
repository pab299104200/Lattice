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
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    link_section = ".init_array"
)]
#[cfg_attr(windows, link_section = ".CRT$XCU")]
static ISOLATE_TEST_PROCESS: extern "C" fn() = isolate_test_process;

/// Set on every isolated process, and so inherited by its children.
const SANDBOX_ENV: &str = "LATTICE_TEST_SANDBOX";
const SANDBOX_PREFIX: &str = "lattice-test-home-";

extern "C" fn isolate_test_process() {
    // Some tests run this test binary again as a child, and time-out tests
    // kill it, so it never reaches `atexit`. A child therefore keeps the
    // sandbox it inherited, already isolated, and leaves removal to the
    // process that created it.
    if let Some(inherited) = std::env::var_os(SANDBOX_ENV).map(PathBuf::from) {
        if inherited.is_dir() {
            let _ = REAL_HOME.set(None);
            let _ = SANDBOX.set(inherited);
            return;
        }
    }
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    remove_orphaned_sandboxes();
    let sandbox = std::env::temp_dir().join(format!("{SANDBOX_PREFIX}{}", std::process::id()));
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
    std::env::set_var(SANDBOX_ENV, &sandbox);
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

/// Remove sandboxes whose process has gone without reaching `atexit`.
///
/// Trusted checks run their command with a cleared environment, so a test
/// binary run as a check cannot inherit its parent's sandbox, and the
/// time-out tests then kill it. Its sandbox is empty but would otherwise be
/// left behind on every run.
fn remove_orphaned_sandboxes() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(SANDBOX_PREFIX))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if !process_is_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 performs only the existence and permission check.
    // Anything but "no such process" is treated as alive, so a sandbox is
    // never removed from under a process that may still own it.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
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
    fn a_child_test_process_reuses_its_parents_sandbox() {
        let sandbox = SANDBOX.get().expect("isolation ran before main");
        if let Some(parent) = std::env::var_os("LATTICE_TEST_ISOLATION_PARENT") {
            // In the child: a sandbox of its own would be left behind if the
            // parent killed it, as time-out tests do.
            assert_eq!(sandbox, &PathBuf::from(parent));
            assert_eq!(
                PathBuf::from(std::env::var_os("HOME").unwrap()),
                sandbox.join("home")
            );
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "test_isolation::tests::a_child_test_process_reuses_its_parents_sandbox",
                "--exact",
                "--quiet",
            ])
            .env("LATTICE_TEST_ISOLATION_PARENT", sandbox)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "the child did not reuse its parent's sandbox"
        );
        assert!(sandbox.is_dir(), "and must not remove it");
    }

    #[cfg(unix)]
    #[test]
    fn a_sandbox_left_by_a_dead_process_is_removed_and_a_live_one_is_not() {
        // A pid that is certainly gone: a child that has been reaped.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let orphan = std::env::temp_dir().join(format!("{SANDBOX_PREFIX}{dead}"));
        std::fs::create_dir_all(orphan.join("home")).unwrap();
        let ours = SANDBOX.get().expect("isolation ran before main").clone();

        remove_orphaned_sandboxes();
        assert!(!orphan.exists(), "{}", orphan.display());
        assert!(ours.is_dir(), "a live process keeps its sandbox");
        assert!(process_is_alive(std::process::id()));
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
