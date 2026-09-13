//! Proven Git base facts used to decide whether a source path can reuse a
//! committed parse object without reading worktree bytes.
use anyhow::{bail, Context, Result};
use lattice_core::storage::commit_manifest::{CommitManifestIdentity, GitObjectFormat};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseBlob {
    pub(crate) mode: u32,
    pub(crate) oid: String,
}
#[derive(Clone, Debug)]
pub(crate) struct WorktreeBase {
    pub(crate) identity: CommitManifestIdentity,
    pub(crate) blobs: BTreeMap<String, BaseBlob>,
    pub(crate) unsafe_paths: BTreeSet<String>,
    root: PathBuf,
    common_dir: PathBuf,
    common_fingerprint: (u64, u128, u64),
    head: String,
    index_fingerprint: (u64, u128, u64),
    max_entries: usize,
}
impl WorktreeBase {
    pub(crate) fn inspect(
        root: &Path,
        repository_id: &str,
        parser_version: i64,
        schema_version: i64,
        config_identity: &str,
        max_entries: usize,
        expected_common_dir: &Path,
    ) -> Result<Self> {
        let root = root.canonicalize()?;
        let repo = git2::Repository::open(&root).context("open checkout Git authority")?;
        if repo.is_bare()
            || repo.workdir().and_then(|p| p.canonicalize().ok()).as_ref() != Some(&root)
        {
            bail!("Git authority does not name the requested checkout root")
        }
        let common_dir = repository_common_dir(&repo)?;
        let expected_common_dir = expected_common_dir
            .canonicalize()
            .context("canonicalize expected Git common directory")?;
        if common_dir != expected_common_dir {
            bail!("opened Git common directory does not match repository authority")
        }
        let common_fingerprint = fingerprint(common_dir.clone())
            .context("Git common-directory metadata is unavailable")?;
        let head = repo.head()?.peel_to_commit()?;
        let head_id = head.id().to_string();
        let object_format = object_format(&repo)?;
        let mut blobs = BTreeMap::new();
        let mut overflow = false;
        let mut unrepresentable = false;
        head.tree()?
            .walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    if let Some(name) = entry.name() {
                        let path = format!("{prefix}{name}");
                        let mode = entry.filemode() as u32;
                        if matches!(mode, 0o100644 | 0o100755) {
                            if blobs.len() >= max_entries {
                                overflow = true;
                                return git2::TreeWalkResult::Abort;
                            }
                            blobs.insert(
                                path,
                                BaseBlob {
                                    mode,
                                    oid: entry.id().to_string(),
                                },
                            );
                        }
                    } else {
                        unrepresentable = true;
                        return git2::TreeWalkResult::Abort;
                    }
                }
                git2::TreeWalkResult::Ok
            })?;
        if overflow {
            bail!("Git base exceeds the configured manifest entry limit")
        }
        if unrepresentable {
            bail!("Git base contains a path that is not valid UTF-8")
        }
        let mut unsafe_paths = materialization_unsafe_paths(&repo, &blobs)?;
        let mut options = git2::StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .include_unmodified(false);
        let statuses = repo.statuses(Some(&mut options))?;
        if statuses.len() > max_entries {
            bail!("Git status exceeds the configured manifest entry limit")
        }
        for status in statuses.iter() {
            let path = status
                .path()
                .context("Git status contains an unrepresentable path")?;
            unsafe_paths.insert(path.replace('\\', "/"));
        }
        let index = repo.index()?;
        if index.len() > max_entries {
            bail!("Git index exceeds the configured manifest entry limit")
        }
        for item in index.iter() {
            if item.flags & 0x8000 != 0 || item.flags_extended & 0x4000 != 0 {
                if let Ok(path) = std::str::from_utf8(&item.path) {
                    unsafe_paths.insert(path.replace('\\', "/"));
                } else {
                    bail!("Git index contains a path that is not valid UTF-8")
                }
            }
        }
        let index_fingerprint =
            fingerprint(repo.path().join("index")).context("Git index metadata is unavailable")?;
        Ok(Self {
            identity: CommitManifestIdentity {
                repository_id: repository_id.into(),
                object_format,
                commit_oid: head_id.clone(),
                parser_version,
                schema_version,
                config_identity: config_identity.into(),
            },
            blobs,
            unsafe_paths,
            root,
            common_dir,
            common_fingerprint,
            head: head_id,
            index_fingerprint,
            max_entries,
        })
    }
    pub(crate) fn reusable_blob(&self, path: &str) -> Option<&BaseBlob> {
        if self.unsafe_paths.contains(path) {
            None
        } else {
            self.blobs.get(path)
        }
    }
    pub(crate) fn revalidate(&self) -> Result<bool> {
        let repo = git2::Repository::open(&self.root)?;
        if repository_common_dir(&repo)? != self.common_dir
            || fingerprint(self.common_dir.clone()) != Some(self.common_fingerprint)
        {
            return Ok(false);
        }
        let head = repo.head()?.peel_to_commit()?.id().to_string();
        if head != self.head
            || object_format(&repo)? != self.identity.object_format
            || fingerprint(repo.path().join("index")) != Some(self.index_fingerprint)
        {
            return Ok(false);
        }
        let mut options = git2::StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .include_unmodified(false);
        let statuses = repo.statuses(Some(&mut options))?;
        if statuses.len() > self.max_entries {
            return Ok(false);
        }
        let mut current = materialization_unsafe_paths(&repo, &self.blobs)?;
        for status in statuses.iter() {
            let Some(path) = status.path() else {
                return Ok(false);
            };
            current.insert(path.replace('\\', "/"));
        }
        let index = repo.index()?;
        if index.len() > self.max_entries {
            return Ok(false);
        }
        for item in index.iter() {
            if item.flags & 0x8000 != 0 || item.flags_extended & 0x4000 != 0 {
                if let Ok(path) = std::str::from_utf8(&item.path) {
                    current.insert(path.replace('\\', "/"));
                } else {
                    return Ok(false);
                }
            }
        }
        Ok(current == self.unsafe_paths)
    }

    pub(crate) fn content_matches_blob(&self, path: &str, content: &[u8]) -> Result<bool> {
        let Some(expected) = self.reusable_blob(path) else {
            return Ok(false);
        };
        let repo = git2::Repository::open(&self.root)?;
        let oid = git2::Oid::from_str(&expected.oid)?;
        let matches = repo.find_blob(oid)?.content() == content;
        Ok(matches)
    }
}

fn repository_common_dir(repo: &git2::Repository) -> Result<PathBuf> {
    let git_dir = repo.path().canonicalize()?;
    let marker = git_dir.join("commondir");
    if marker.is_file() {
        let raw = std::fs::read_to_string(&marker)?;
        if raw.len() > 4096 {
            bail!("Git common-directory marker exceeds safety limit")
        }
        let path = Path::new(raw.trim());
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            git_dir.join(path)
        }
        .canonicalize()?);
    }
    Ok(git_dir)
}

fn materialization_unsafe_paths(
    repo: &git2::Repository,
    blobs: &BTreeMap<String, BaseBlob>,
) -> Result<BTreeSet<String>> {
    let config = repo.config()?;
    let autocrlf_safe = config
        .get_string("core.autocrlf")
        .ok()
        .is_none_or(|value| value.eq_ignore_ascii_case("false"));
    let eol_safe = config.get_string("core.eol").is_err();
    if !autocrlf_safe || !eol_safe {
        return Ok(blobs.keys().cloned().collect());
    }
    let mut unsafe_paths = BTreeSet::new();
    let flags = git2::AttrCheckFlags::FILE_THEN_INDEX;
    for path in blobs.keys() {
        for attribute in ["filter", "working-tree-encoding", "eol", "ident"] {
            let value = repo.get_attr_bytes(Path::new(path), attribute, flags)?;
            if !matches!(
                git2::AttrValue::from_bytes(value),
                git2::AttrValue::Unspecified | git2::AttrValue::False
            ) {
                unsafe_paths.insert(path.clone());
                break;
            }
        }
    }
    Ok(unsafe_paths)
}

fn object_format(repo: &git2::Repository) -> Result<GitObjectFormat> {
    match repo
        .config()?
        .get_string("extensions.objectFormat")
        .ok()
        .as_deref()
    {
        None | Some("sha1") => Ok(GitObjectFormat::Sha1),
        Some("sha256") => Ok(GitObjectFormat::Sha256),
        Some(other) => bail!("unsupported Git object format {other}"),
    }
}

fn fingerprint(path: PathBuf) -> Option<(u64, u128, u64)> {
    let metadata = path.metadata().ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.ino(), modified, metadata.len()))
    }
    #[cfg(not(unix))]
    {
        Some((0, modified, metadata.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(root.path().join("b.ts"), "export const b = 2;\n").unwrap();
        let repo = git2::Repository::init(root.path()).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*.ts"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Lattice", "test@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
            .unwrap();
        drop(tree);
        drop(repo);
        root
    }

    #[test]
    fn dirty_staged_deleted_and_untracked_paths_are_never_reusable() {
        let root = repository();
        std::fs::write(root.path().join("a.ts"), "export const a = 3;\n").unwrap();
        std::fs::remove_file(root.path().join("b.ts")).unwrap();
        std::fs::write(root.path().join("new.ts"), "export const n = 1;\n").unwrap();
        let repo = git2::Repository::open(root.path()).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.ts")).unwrap();
        index.write().unwrap();

        let base = WorktreeBase::inspect(
            root.path(),
            "repository",
            1,
            1,
            "default-v1",
            32,
            &root.path().join(".git"),
        )
        .unwrap();
        assert!(base.reusable_blob("a.ts").is_none(), "staged path");
        assert!(base.reusable_blob("b.ts").is_none(), "deleted path");
        assert!(base.reusable_blob("new.ts").is_none(), "untracked path");
        assert!(base.unsafe_paths.contains("a.ts"));
        assert!(base.unsafe_paths.contains("b.ts"));
        assert!(base.unsafe_paths.contains("new.ts"));
    }

    #[test]
    fn revalidation_rejects_head_and_index_changes() {
        let root = repository();
        let base = WorktreeBase::inspect(
            root.path(),
            "repository",
            1,
            1,
            "default-v1",
            32,
            &root.path().join(".git"),
        )
        .unwrap();
        assert!(base.revalidate().unwrap());
        std::fs::write(root.path().join("a.ts"), "export const a = 4;\n").unwrap();
        assert!(!base.revalidate().unwrap());
    }

    #[test]
    fn conversion_attributes_disable_reuse_without_running_filters() {
        let root = repository();
        let repo = git2::Repository::open(root.path()).unwrap();
        std::fs::write(
            repo.path().join("info/attributes"),
            "a.ts eol=crlf\nb.ts filter=fixture\n",
        )
        .unwrap();
        repo.config()
            .unwrap()
            .set_str("filter.fixture.smudge", "this-command-must-never-run")
            .unwrap();
        let base = WorktreeBase::inspect(
            root.path(),
            "repository",
            1,
            1,
            "default-v1",
            32,
            &root.path().join(".git"),
        )
        .unwrap();
        assert!(base.reusable_blob("a.ts").is_none());
        assert!(base.reusable_blob("b.ts").is_none());
    }
}
