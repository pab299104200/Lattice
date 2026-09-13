//! Public command-line adapter for explicit repository storage operations.

use anyhow::{bail, Context, Result};
use lattice_core::storage::{
    relocate_repository_home, AccountingLimits, CacheMaintenancePlan, CachePolicy,
    HistoricalRetirementPlan, KnowledgeBackupRequest, KnowledgeRestoreRequest,
    RepositoryRelocationRequest, StorageOperator,
};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Read-only accounting shared by warm and bootstrapping MCP handlers.
pub(crate) fn status_payload(workspace: &Path) -> serde_json::Value {
    let result = (|| -> Result<serde_json::Value> {
        let identity = crate::workspace_identity::WorkspaceIdentity::resolve(workspace)?;
        if !identity.is_git_repository {
            bail!("storage accounting requires a proven Git repository");
        }
        let operator = StorageOperator::open_existing(
            &identity.repository_lattice_dir,
            &identity.repository_id,
        )?;
        Ok(serde_json::to_value(operator.status(
            now_epoch_secs(),
            &CachePolicy::default(),
            AccountingLimits::default(),
        )?)?)
    })();
    match result {
        Ok(status) => serde_json::json!({"scope":"storage", "available":true, "storage":status}),
        Err(error) => {
            serde_json::json!({"scope":"storage", "available":false, "complete":false, "error":format!("{error:#}"), "recovery":"Inspect lattice storage status in this workspace; accounting does not create or repair storage."})
        }
    }
}

pub(crate) fn is_storage_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("storage")
}

pub(crate) fn run_from_env() -> Result<i32> {
    run(std::env::args().collect())
}

fn run(args: Vec<String>) -> Result<i32> {
    let command = parse(&args)?;
    let identity = crate::workspace_identity::WorkspaceIdentity::resolve(&command.workspace)
        .context("cannot prove repository identity for storage operation")?;
    if !identity.is_git_repository {
        bail!("storage operations require a Git repository with proven ownership");
    }
    let now = now_epoch_secs();
    // Relocation is the one operation which must run before `open_existing`:
    // its purpose is to replace the recorded, old repository authority.
    if let Action::Relocate {
        from_repository_id,
        confirm_offline,
    } = &command.action
    {
        if !confirm_offline {
            bail!("storage relocate requires --confirm-offline");
        }
        print_json(&relocate_repository_home(RepositoryRelocationRequest {
            current_home: identity.repository_lattice_dir,
            new_repository_id: identity.repository_id,
            current_git_common_dir: identity
                .git_common_dir
                .context("Git relocation requires the common directory")?,
            expected_old_repository_id: from_repository_id.clone(),
            operator_confirms_all_lattice_processes_stopped: true,
        })?)?;
        return Ok(0);
    }
    let operator =
        StorageOperator::open_existing(&identity.repository_lattice_dir, &identity.repository_id)?;
    match command.action {
        Action::Status => {
            print_json(&operator.status(now, &command.policy, AccountingLimits::default())?)?
        }
        Action::CachePlan { output } => {
            let plan = operator.plan_cache_maintenance(now, command.policy)?;
            let bytes = serde_json::to_vec_pretty(&plan)?;
            write_new(&output, &bytes)?;
            print_json(&plan)?;
        }
        Action::CacheApply { plan } => {
            let bytes = fs::read(&plan)
                .with_context(|| format!("failed to read plan {}", plan.display()))?;
            let plan: CacheMaintenancePlan =
                serde_json::from_slice(&bytes).context("cache maintenance plan is invalid")?;
            print_json(&operator.apply_cache_maintenance(&plan, now)?)?;
        }
        Action::Backup { destination } => {
            print_json(&operator.backup_knowledge(KnowledgeBackupRequest {
                destination,
                created_at: now,
            })?)?
        }
        Action::Restore {
            backup,
            replace_existing,
            confirm_offline,
        } => print_json(&operator.restore_knowledge(KnowledgeRestoreRequest {
            backup_directory: backup,
            replace_existing,
            operator_confirms_all_lattice_processes_stopped: confirm_offline,
        })?)?,
        Action::HistoricalPlan { backup, output } => {
            let plan = operator.plan_historical_retirement(backup, now)?;
            write_new(&output, &serde_json::to_vec_pretty(&plan)?)?;
            print_json(&plan)?;
        }
        Action::HistoricalApply {
            plan,
            confirm_offline,
        } => {
            if !confirm_offline {
                bail!("storage historical apply requires --confirm-offline");
            }
            let bytes = fs::read(&plan)
                .with_context(|| format!("failed to read plan {}", plan.display()))?;
            let plan: HistoricalRetirementPlan =
                serde_json::from_slice(&bytes).context("historical retirement plan is invalid")?;
            print_json(&operator.apply_historical_retirement(&plan, true)?)?;
        }
        Action::Relocate { .. } => unreachable!("relocation returns before opening existing owner"),
    }
    Ok(0)
}

#[derive(Debug)]
struct Command {
    workspace: PathBuf,
    policy: CachePolicy,
    action: Action,
}

#[derive(Debug)]
enum Action {
    Status,
    CachePlan {
        output: PathBuf,
    },
    CacheApply {
        plan: PathBuf,
    },
    Backup {
        destination: PathBuf,
    },
    Restore {
        backup: PathBuf,
        replace_existing: bool,
        confirm_offline: bool,
    },
    Relocate {
        from_repository_id: String,
        confirm_offline: bool,
    },
    HistoricalPlan {
        backup: PathBuf,
        output: PathBuf,
    },
    HistoricalApply {
        plan: PathBuf,
        confirm_offline: bool,
    },
}

fn parse(args: &[String]) -> Result<Command> {
    let mut workspace = std::env::current_dir()?;
    let mut high = CachePolicy::default().high_bytes;
    let mut low = CachePolicy::default().low_bytes;
    let mut idle = CachePolicy::default().idle_grace_secs;
    let mut batch = CachePolicy::default().batch_files;
    let mut positional = Vec::new();
    let mut output = None;
    let mut plan = None;
    let mut destination = None;
    let mut backup = None;
    let mut replace_existing = false;
    let mut from_repository_id = None;
    let mut confirm_offline = false;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" | "-w" => workspace = value(args, &mut index)?.into(),
            "--high-bytes" => high = number(value(args, &mut index)?, "high-bytes")?,
            "--low-bytes" => low = number(value(args, &mut index)?, "low-bytes")?,
            "--idle-secs" => idle = number(value(args, &mut index)?, "idle-secs")?,
            "--batch" => batch = number(value(args, &mut index)?, "batch")?,
            "--output" => output = Some(PathBuf::from(value(args, &mut index)?)),
            "--plan" => plan = Some(PathBuf::from(value(args, &mut index)?)),
            "--destination" => destination = Some(PathBuf::from(value(args, &mut index)?)),
            "--backup" => backup = Some(PathBuf::from(value(args, &mut index)?)),
            "--replace-existing" => replace_existing = true,
            "--from-repository-id" => {
                from_repository_id = Some(value(args, &mut index)?.to_owned())
            }
            "--confirm-offline" => confirm_offline = true,
            value if value.starts_with('-') => bail!("unknown storage option `{value}`"),
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    let policy = CachePolicy {
        high_bytes: high,
        low_bytes: low,
        idle_grace_secs: idle,
        batch_files: batch,
    };
    policy.validate()?;
    let action = match positional.as_slice() {
        [action] if action == "status" => Action::Status,
        [cache, action] if cache == "cache" && action == "plan" => Action::CachePlan {
            output: output.context("storage cache plan requires --output <new-file>")?,
        },
        [cache, action] if cache == "cache" && action == "apply" => Action::CacheApply {
            plan: plan.context("storage cache apply requires --plan <file>")?,
        },
        [action] if action == "backup" => Action::Backup {
            destination: destination.context("storage backup requires --destination <new-directory>")?,
        },
        [action] if action == "restore" => Action::Restore {
            backup: backup.context("storage restore requires --backup <directory>")?, replace_existing, confirm_offline,
        },
        [action] if action == "relocate" => Action::Relocate {
            from_repository_id: from_repository_id.context("storage relocate requires --from-repository-id <recorded-id>")?,
            confirm_offline,
        },
        [historical, action] if historical == "historical" && action == "plan" => Action::HistoricalPlan {
            backup: backup.context("storage historical plan requires --backup <directory>")?,
            output: output.context("storage historical plan requires --output <new-file>")?,
        },
        [historical, action] if historical == "historical" && action == "apply" => Action::HistoricalApply {
            plan: plan.context("storage historical apply requires --plan <file>")?,
            confirm_offline,
        },
        _ => bail!("usage: lattice storage status | cache plan --output <new-file> | cache apply --plan <file> | backup --destination <new-directory> | restore --backup <directory> --replace-existing --confirm-offline | relocate --from-repository-id <recorded-id> --confirm-offline | historical plan --backup <directory> --output <new-file> | historical apply --plan <file> --confirm-offline; all accept -w <repo>")
    };
    Ok(Command {
        workspace,
        policy,
        action,
    })
}

fn value<'a>(args: &'a [String], index: &mut usize) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .context("option requires a value")
}

fn number<T>(value: &str, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid --{name}: {error}"))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        bail!("plan output parent does not exist: {}", parent.display());
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("refusing to overwrite plan output {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_storage_status_is_read_only_and_truthful() {
        let root = tempfile::tempdir().unwrap();
        let result = status_payload(root.path());
        assert_eq!(result["scope"], "storage");
        assert_eq!(result["available"], false);
        assert_eq!(result["complete"], false);
        assert!(!root.path().join(".lattice").exists());
    }

    #[test]
    fn restore_requires_separate_offline_confirmation() {
        let args: Vec<String> = [
            "lattice",
            "storage",
            "restore",
            "--backup",
            "backup",
            "--replace-existing",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert!(matches!(
            parse(&args).unwrap().action,
            Action::Restore {
                confirm_offline: false,
                replace_existing: true,
                ..
            }
        ));
        let mut confirmed = args;
        confirmed.push("--confirm-offline".into());
        assert!(matches!(
            parse(&confirmed).unwrap().action,
            Action::Restore {
                confirm_offline: true,
                ..
            }
        ));
    }

    #[test]
    fn parser_requires_explicit_plan_files_and_rejects_unknown_flags() {
        let base = vec!["lattice".into(), "storage".into()];
        let mut args = base.clone();
        args.extend(["cache".into(), "plan".into()]);
        assert!(parse(&args).is_err());
        let mut args = base;
        args.extend(["status".into(), "--mystery".into()]);
        assert!(parse(&args).is_err());
    }

    #[test]
    fn plan_output_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plan.json");
        write_new(&path, b"one").unwrap();
        assert!(write_new(&path, b"two").is_err());
        assert_eq!(fs::read(path).unwrap(), b"one");
    }

    #[test]
    fn relocation_and_retirement_require_explicit_proofs() {
        let relocate = vec![
            "lattice".into(),
            "storage".into(),
            "relocate".into(),
            "--from-repository-id".into(),
            "repo_".to_string() + &"a".repeat(64),
        ];
        assert!(matches!(
            parse(&relocate).unwrap().action,
            Action::Relocate {
                confirm_offline: false,
                ..
            }
        ));

        let historical = vec![
            "lattice".into(),
            "storage".into(),
            "historical".into(),
            "apply".into(),
            "--plan".into(),
            "retirement.json".into(),
        ];
        assert!(matches!(
            parse(&historical).unwrap().action,
            Action::HistoricalApply {
                confirm_offline: false,
                ..
            }
        ));
    }

    #[test]
    fn retirement_plan_is_a_dry_run_with_a_new_output() {
        let args = vec![
            "lattice".into(),
            "storage".into(),
            "historical".into(),
            "plan".into(),
            "--backup".into(),
            "backup".into(),
            "--output".into(),
            "plan.json".into(),
        ];
        assert!(matches!(
            parse(&args).unwrap().action,
            Action::HistoricalPlan { .. }
        ));
    }
}
