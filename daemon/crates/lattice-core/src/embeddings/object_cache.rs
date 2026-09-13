//! Repository-shared, content-addressed embedding objects.
//!
//! Object identity includes every input that can affect inference. Checkout
//! manifests retain membership while immutable vectors are shared between views.
use crate::storage::managed_fs::SecureDir;
use crate::storage::managed_sqlite::ManagedSqlite;
use anyhow::{bail, Context, Result};
use rusqlite::{params, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

const OBJECT_FORMAT: &str = "lattice.embedding-object.v1";
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingIdentity {
    pub model_artifact_sha256: String,
    pub tokenizer_sha256: String,
    pub dimension: usize,
    pub normalization_version: String,
    pub preprocessing_version: String,
}

impl EmbeddingIdentity {
    pub fn validate(&self) -> Result<()> {
        for (name, digest) in [
            ("model artifact", &self.model_artifact_sha256),
            ("tokenizer", &self.tokenizer_sha256),
        ] {
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("{name} identity must be a 64-character SHA-256 digest");
            }
        }
        if self.dimension == 0 || self.dimension > 65_536 {
            bail!("embedding dimension must be 1..65536");
        }
        if self.normalization_version.is_empty() || self.preprocessing_version.is_empty() {
            bail!("embedding normalization and preprocessing versions must be non-empty");
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingCacheStats {
    pub requested: usize,
    pub unique_inputs: usize,
    pub object_hits: usize,
    pub computed: usize,
    pub reused_bytes: u64,
    pub written_bytes: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingGcReport {
    pub examined: usize,
    pub removed: usize,
    pub released_bytes: u64,
    pub remaining_bytes: u64,
    pub remaining_candidates: bool,
    pub deferred_active_publication: bool,
}

#[derive(Serialize, Deserialize)]
struct StoredObject {
    format: String,
    key: String,
    identity: EmbeddingIdentity,
    input_sha256: String,
    vector: Vec<f32>,
}

#[derive(Default)]
struct FlightState {
    complete: bool,
    error: Option<String>,
}

#[derive(Default)]
struct Flight {
    state: Mutex<FlightState>,
    ready: Condvar,
}

type Flights = Mutex<HashMap<String, Arc<Flight>>>;
static REPOSITORY_FLIGHTS: OnceLock<Mutex<HashMap<(u64, u64), Weak<Flights>>>> = OnceLock::new();

fn repository_flights(directory: &SecureDir) -> Result<Arc<Flights>> {
    let identity = directory.identity()?;
    let mut repositories = REPOSITORY_FLIGHTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("embedding repository coordinator poisoned"))?;
    repositories.retain(|_, flights| flights.strong_count() != 0);
    let entry = repositories
        .entry((identity.dev, identity.ino))
        .or_default();
    if let Some(flights) = entry.upgrade() {
        return Ok(flights);
    }
    let flights = Arc::new(Mutex::new(HashMap::new()));
    *entry = Arc::downgrade(&flights);
    Ok(flights)
}

#[derive(Clone)]
pub struct EmbeddingObjectCache {
    secure: Arc<SecureDir>,
    index: Arc<Mutex<ManagedSqlite>>,
    flights: Arc<Flights>,
}

impl EmbeddingObjectCache {
    pub fn open(root: &Path) -> Result<Self> {
        checked_dir(root)?;
        let secure = Arc::new(SecureDir::open(root)?);
        Self::open_in(secure)
    }

    pub(crate) fn open_in(secure: Arc<SecureDir>) -> Result<Self> {
        secure.create_dir("objects")?;
        secure.create_dir("locks")?;
        let index = ManagedSqlite::open(
            &secure,
            "index.db",
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        index.busy_timeout(Duration::from_secs(5))?;
        index.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS objects(key TEXT PRIMARY KEY, bytes INTEGER NOT NULL DEFAULT 0, refs INTEGER NOT NULL DEFAULT 0 CHECK(refs>=0));
            CREATE INDEX IF NOT EXISTS objects_gc ON objects(refs,key);
            CREATE TABLE IF NOT EXISTS pending_publications(temp TEXT PRIMARY KEY, key TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS membership(checkout TEXT NOT NULL, member TEXT NOT NULL, object_key TEXT NOT NULL, PRIMARY KEY(checkout,member));
            CREATE INDEX IF NOT EXISTS membership_object ON membership(object_key);
            CREATE TABLE IF NOT EXISTS totals(id INTEGER PRIMARY KEY CHECK(id=1),bytes INTEGER NOT NULL);
            INSERT OR IGNORE INTO totals VALUES(1,0);
            CREATE TRIGGER IF NOT EXISTS object_added AFTER INSERT ON objects BEGIN UPDATE totals SET bytes=bytes+NEW.bytes WHERE id=1; END;
            CREATE TRIGGER IF NOT EXISTS object_updated AFTER UPDATE OF bytes ON objects BEGIN UPDATE totals SET bytes=bytes+NEW.bytes-OLD.bytes WHERE id=1; END;
            CREATE TRIGGER IF NOT EXISTS object_deleted AFTER DELETE ON objects BEGIN UPDATE totals SET bytes=bytes-OLD.bytes WHERE id=1; END;
            CREATE TRIGGER IF NOT EXISTS member_added AFTER INSERT ON membership BEGIN UPDATE objects SET refs=refs+1 WHERE key=NEW.object_key; END;
            CREATE TRIGGER IF NOT EXISTS member_deleted AFTER DELETE ON membership BEGIN UPDATE objects SET refs=refs-1 WHERE key=OLD.object_key; END;
        ")?;
        crate::storage::object_accounting::initialize(&index)?;
        let flights = repository_flights(&secure)?;
        Ok(Self {
            secure,
            index: Arc::new(Mutex::new(index)),
            flights,
        })
    }

    pub fn advance_accounting(
        &self,
        limit: usize,
    ) -> Result<crate::storage::object_accounting::ObjectAccounting> {
        let _lease = cache_lock(
            self.secure.open_or_create_file("publication.lock")?,
            true,
            false,
        )?
        .context("embedding publication is active; retry accounting")?;
        let connection = self
            .index
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding index lock poisoned"))?;
        for key in crate::storage::object_accounting::pending(&connection, limit)? {
            validate_key(&key)?;
            let metadata = self
                .secure
                .open_dir("objects")?
                .open_dir(&key[..2])
                .and_then(|directory| directory.metadata(&key));
            let metadata = match metadata {
                Ok(Some(value)) => value,
                Ok(None) => {
                    crate::storage::object_accounting::mark_error(
                        &connection,
                        &key,
                        "indexed embedding is missing",
                    )?;
                    continue;
                }
                Err(error) => {
                    crate::storage::object_accounting::mark_error(
                        &connection,
                        &key,
                        &format!("embedding metadata: {error}"),
                    )?;
                    continue;
                }
            };
            if !metadata.is_file {
                crate::storage::object_accounting::mark_error(
                    &connection,
                    &key,
                    "indexed embedding is not a regular file",
                )?;
                continue;
            }
            crate::storage::object_accounting::record(
                &connection,
                &key,
                metadata.len,
                metadata.allocated,
            )?;
        }
        crate::storage::object_accounting::read(&connection)
    }

    /// Hold from inference through committed vector membership publication.

    pub fn publication_lease(&self) -> Result<File> {
        cache_lock(
            self.secure.open_or_create_file("publication.lock")?,
            false,
            true,
        )?
        .context("embedding publication lock unavailable")
    }

    fn checkout_lease(&self, checkout_id: &str) -> Result<File> {
        validate_component(checkout_id)?;
        let directory = self.secure.open_dir("locks")?;
        cache_lock(
            directory
                .open_or_create_file(&format!("{checkout_id}.lock"))
                .context("open checkout lock file")?,
            true,
            true,
        )?
        .context("embedding membership lock unavailable")
    }

    pub fn object_key(identity: &EmbeddingIdentity, input: &str) -> Result<String> {
        identity.validate()?;
        let identity_bytes = serde_json::to_vec(identity)?;
        let mut hash = Sha256::new();
        hash.update(OBJECT_FORMAT.as_bytes());
        hash.update((identity_bytes.len() as u64).to_be_bytes());
        hash.update(identity_bytes);
        hash.update((input.len() as u64).to_be_bytes());
        hash.update(input.as_bytes());
        Ok(format!("{:x}", hash.finalize()))
    }

    /// Resolve a batch with request deduplication and process-wide single-flight
    /// for all clones of this cache. The compute closure sees each owned miss once.
    pub fn get_or_compute_batch<F>(
        &self,
        identity: &EmbeddingIdentity,
        inputs: &[&str],
        compute: F,
    ) -> Result<(Vec<Vec<f32>>, EmbeddingCacheStats)>
    where
        F: FnOnce(&[&str]) -> Result<Vec<Vec<f32>>>,
    {
        identity.validate()?;
        let _publication = self.publication_lease()?;
        let mut stats = EmbeddingCacheStats {
            requested: inputs.len(),
            ..Default::default()
        };
        let mut unique = BTreeMap::<String, (usize, &str)>::new();
        for (index, input) in inputs.iter().copied().enumerate() {
            unique
                .entry(Self::object_key(identity, input)?)
                .or_insert((index, input));
        }
        stats.unique_inputs = unique.len();
        let mut resolved = HashMap::<String, Vec<f32>>::new();
        let mut owned = Vec::<(String, &str, Arc<Flight>)>::new();
        let mut waiting = Vec::<(String, &str, Arc<Flight>)>::new();

        for (key, (_, input)) in &unique {
            match self.read_object(key, identity, input) {
                Ok(Some((vector, bytes))) => {
                    stats.object_hits += 1;
                    stats.reused_bytes += bytes;
                    resolved.insert(key.clone(), vector);
                }
                Ok(None) => {
                    let mut flights = self.flights.lock().map_err(|_| {
                        anyhow::anyhow!("embedding single-flight coordinator poisoned")
                    })?;
                    // Close the miss-to-flight race: an owner may have published
                    // and removed its flight after our first read.
                    if let Ok(Some((vector, bytes))) = self.read_object(key, identity, input) {
                        stats.object_hits += 1;
                        stats.reused_bytes += bytes;
                        resolved.insert(key.clone(), vector);
                    } else if let Some(flight) = flights.get(key) {
                        waiting.push((key.clone(), *input, Arc::clone(flight)));
                    } else {
                        let flight = Arc::new(Flight::default());
                        flights.insert(key.clone(), Arc::clone(&flight));
                        owned.push((key.clone(), *input, flight));
                    }
                }
                Err(error) => {
                    tracing::warn!(object_key = key, error = %error, "Discarding corrupt embedding object and recomputing");
                    self.quarantine(key)?;
                    let mut flights = self.flights.lock().map_err(|_| {
                        anyhow::anyhow!("embedding single-flight coordinator poisoned")
                    })?;
                    if let Some(flight) = flights.get(key) {
                        waiting.push((key.clone(), *input, Arc::clone(flight)));
                    } else {
                        let flight = Arc::new(Flight::default());
                        flights.insert(key.clone(), Arc::clone(&flight));
                        owned.push((key.clone(), *input, flight));
                    }
                }
            }
        }

        if !owned.is_empty() {
            let owned_inputs: Vec<&str> = owned.iter().map(|(_, input, _)| *input).collect();
            let outcome = compute(&owned_inputs).and_then(|vectors| {
                if vectors.len() != owned.len() {
                    bail!(
                        "embedding provider returned {} vectors for {} inputs",
                        vectors.len(),
                        owned.len()
                    );
                }
                for ((key, input, _), vector) in owned.iter().zip(vectors.iter()) {
                    self.validate_vector(identity, vector)?;
                    stats.written_bytes += self.publish(key, identity, input, vector)?;
                    resolved.insert(key.clone(), vector.clone());
                    stats.computed += 1;
                }
                Ok(())
            });
            let error = outcome.as_ref().err().map(ToString::to_string);
            for (key, _, flight) in &owned {
                let mut state = flight
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("embedding flight poisoned"))?;
                state.complete = true;
                state.error = error.clone();
                flight.ready.notify_all();
                self.flights
                    .lock()
                    .map_err(|_| anyhow::anyhow!("embedding single-flight coordinator poisoned"))?
                    .remove(key);
            }
            outcome?;
        }

        for (key, input, flight) in waiting {
            let mut state = flight
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("embedding flight poisoned"))?;
            while !state.complete {
                state = flight
                    .ready
                    .wait(state)
                    .map_err(|_| anyhow::anyhow!("embedding flight poisoned"))?;
            }
            if let Some(error) = &state.error {
                bail!("shared embedding computation failed: {error}");
            }
            drop(state);
            let (vector, bytes) = self
                .read_object(&key, identity, input)?
                .context("completed embedding flight did not publish an object")?;
            stats.object_hits += 1;
            stats.reused_bytes += bytes;
            resolved.insert(key, vector);
        }
        let vectors = inputs
            .iter()
            .map(|input| {
                resolved
                    .get(&Self::object_key(identity, input)?)
                    .cloned()
                    .context("embedding result missing")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((vectors, stats))
    }

    pub fn replace_checkout_membership(
        &self,
        checkout_id: &str,
        members: &BTreeMap<String, String>,
    ) -> Result<()> {
        let _publication = self.publication_lease()?;
        let _checkout = self.checkout_lease(checkout_id)?;
        self.write_checkout_membership(checkout_id, members)
    }

    fn write_checkout_membership(
        &self,
        checkout_id: &str,
        members: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.change_membership(checkout_id, members, None)
    }

    fn change_membership(
        &self,
        checkout_id: &str,
        members: &BTreeMap<String, String>,
        prefixes: Option<&[String]>,
    ) -> Result<()> {
        validate_component(checkout_id)?;
        let mut connection = self
            .index
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding index lock poisoned"))?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prefixes) = prefixes {
            for prefix in prefixes {
                tx.execute(
                    "DELETE FROM membership WHERE checkout=?1 AND substr(member,1,length(?2))=?2",
                    params![checkout_id, prefix],
                )?;
            }
        } else {
            tx.execute("DELETE FROM membership WHERE checkout=?1", [checkout_id])?;
        }
        for (member, key) in members {
            validate_key(key)?;
            let present: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE key=?1 AND bytes>0)",
                [key],
                |r| r.get(0),
            )?;
            if !present {
                bail!("embedding membership cannot reference missing object {key}");
            }
            tx.execute(
                "DELETE FROM membership WHERE checkout=?1 AND member=?2",
                params![checkout_id, member],
            )?;
            tx.execute(
                "INSERT INTO membership VALUES(?1,?2,?3)",
                params![checkout_id, member, key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn update_checkout_membership(
        &self,
        checkout_id: &str,
        remove_prefixes: &[String],
        additions: &BTreeMap<String, String>,
    ) -> Result<()> {
        let _publication = self.publication_lease()?;
        let _checkout = self.checkout_lease(checkout_id)?;
        self.change_membership(checkout_id, additions, Some(remove_prefixes))
    }

    pub fn remove_checkout_membership(&self, checkout_id: &str) -> Result<()> {
        let _publication = self.publication_lease()?;
        let _checkout = self.checkout_lease(checkout_id)?;
        self.change_membership(checkout_id, &BTreeMap::new(), None)
    }

    pub fn collect_garbage(&self, max_bytes: u64, limit: usize) -> Result<EmbeddingGcReport> {
        if limit == 0 || limit > 4096 {
            bail!("embedding object GC limit must be 1..4096");
        }
        let Some(_gc) = cache_lock(
            self.secure.open_or_create_file("publication.lock")?,
            true,
            false,
        )?
        else {
            return Ok(EmbeddingGcReport {
                deferred_active_publication: true,
                remaining_candidates: true,
                ..Default::default()
            });
        };
        let mut connection = self
            .index
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding index lock poisoned"))?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let total: u64 = tx.query_row("SELECT bytes FROM totals WHERE id=1", [], |r| r.get(0))?;
        let mut report = EmbeddingGcReport {
            remaining_bytes: total,
            ..Default::default()
        };
        // An exclusive publication lease proves no live writer owns these temps.
        // Journal lookup keeps crash recovery bounded even with millions of objects.
        let pending = {
            let mut statement =
                tx.prepare("SELECT temp,key FROM pending_publications ORDER BY temp LIMIT ?1")?;
            let rows = statement
                .query_map([limit], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (temp, key) in pending {
            validate_key(&key)?;
            let objects = self.secure.open_dir("objects")?;
            match objects.open_dir(&key[..2]) {
                Ok(directory) => {
                    if let Some(entry) = directory.metadata(&temp)? {
                        if !entry.is_file {
                            bail!("embedding publication temporary is not a regular file");
                        }
                        directory.remove_file(&temp, entry.identity)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            tx.execute("DELETE FROM pending_publications WHERE temp=?1", [temp])?;
            report.examined += 1;
        }
        if report.examined == limit || total <= max_bytes {
            tx.commit()?;
            report.remaining_candidates = total > max_bytes;
            return Ok(report);
        }
        let candidates = {
            let mut statement =
                tx.prepare("SELECT key,bytes FROM objects WHERE refs=0 ORDER BY key LIMIT ?1")?;
            let rows = statement
                .query_map([limit - report.examined], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let objects = self.secure.open_dir("objects")?;
        for (key, bytes) in candidates {
            if report.remaining_bytes <= max_bytes {
                break;
            }
            validate_key(&key)?;
            match objects.open_dir(&key[..2]) {
                Ok(directory) => {
                    if let Some(entry) = directory.metadata(&key)? {
                        if !entry.is_file {
                            bail!("embedding object is not a regular file");
                        }
                        directory.remove_file(&key, entry.identity)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            tx.execute("DELETE FROM objects WHERE key=?1 AND refs=0", [&key])?;
            report.examined += 1;
            report.removed += 1;
            report.released_bytes += bytes;
            report.remaining_bytes = report.remaining_bytes.saturating_sub(bytes);
        }
        report.remaining_candidates = report.remaining_bytes > max_bytes;
        tx.commit()?;
        Ok(report)
    }

    fn validate_vector(&self, identity: &EmbeddingIdentity, vector: &[f32]) -> Result<()> {
        if vector.len() != identity.dimension {
            bail!(
                "embedding provider returned dimension {}, expected {}",
                vector.len(),
                identity.dimension
            );
        }
        if vector.iter().any(|value| !value.is_finite()) {
            bail!("embedding provider returned a non-finite value");
        }
        Ok(())
    }
    fn read_object(
        &self,
        key: &str,
        identity: &EmbeddingIdentity,
        input: &str,
    ) -> Result<Option<(Vec<f32>, u64)>> {
        validate_key(key)?;
        let directory = match self.secure.open_dir(Path::new("objects").join(&key[..2])) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut file = match directory.open_file(key, false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.len() > identity.dimension.saturating_mul(32).saturating_add(16384) as u64
        {
            bail!("embedding object exceeds its declared dimension budget or is not a file");
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let object: StoredObject =
            serde_json::from_slice(&bytes).context("decode embedding object")?;
        if object.format != OBJECT_FORMAT
            || object.key != key
            || object.identity != *identity
            || object.input_sha256 != digest(input.as_bytes())
        {
            bail!("embedding object identity mismatch");
        }
        self.validate_vector(identity, &object.vector)?;
        Ok(Some((object.vector, meta.len())))
    }
    fn publish(
        &self,
        key: &str,
        identity: &EmbeddingIdentity,
        input: &str,
        vector: &[f32],
    ) -> Result<u64> {
        validate_key(key)?;
        let directory = self.secure.open_dir("objects")?.create_dir(&key[..2])?;
        if directory.metadata(key)?.is_some() {
            return Ok(0);
        }
        let object = StoredObject {
            format: OBJECT_FORMAT.into(),
            key: key.into(),
            identity: identity.clone(),
            input_sha256: digest(input.as_bytes()),
            vector: vector.to_vec(),
        };
        let bytes = serde_json::to_vec(&object)?;
        // Journal the intended object before publication. Failure leaves a bounded,
        // indexed orphan instead of an invisible permanent filesystem leak.
        let temp = format!(
            ".{key}.{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        {
            let mut connection = self
                .index
                .lock()
                .map_err(|_| anyhow::anyhow!("embedding index lock poisoned"))?;
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("INSERT INTO objects(key,bytes) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET bytes=excluded.bytes,allocated_bytes=NULL",params![key,bytes.len()])?;
            tx.execute(
                "INSERT INTO pending_publications VALUES(?1,?2)",
                params![temp, key],
            )?;
            tx.commit()?;
        }
        let mut file = directory.open_new_file(&temp)?;
        let temporary = directory
            .metadata(&temp)?
            .context("embedding temporary disappeared")?;
        let outcome = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            directory.rename_to(&temp, &directory, key, temporary.identity)?;
            Ok(())
        })();
        if outcome.is_err() {
            if directory.metadata(&temp)?.is_some() {
                directory.remove_file(&temp, temporary.identity)?;
            }
        }
        outcome?;
        let published = directory
            .metadata(key)?
            .context("published embedding disappeared")?;
        let mut connection = self
            .index
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding index lock poisoned"))?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::storage::object_accounting::record(
            &transaction,
            key,
            published.len,
            published.allocated,
        )?;
        transaction.execute("DELETE FROM pending_publications WHERE temp=?1", [temp])?;
        transaction.commit()?;
        Ok(bytes.len() as u64)
    }
    fn quarantine(&self, key: &str) -> Result<()> {
        validate_key(key)?;
        let directory = match self.secure.open_dir(Path::new("objects").join(&key[..2])) {
            Ok(dir) => Some(dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(entry) = directory
            .as_ref()
            .map(|dir| dir.metadata(key))
            .transpose()?
            .flatten()
        {
            if !entry.is_file {
                bail!("corrupt embedding object is not a regular file");
            }
            directory
                .as_ref()
                .unwrap()
                .remove_file(key, entry.identity)?;
        }
        let connection = self
            .index
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding index lock poisoned"))?;
        crate::storage::object_accounting::mark_error(
            &connection,
            key,
            "corrupt embedding object quarantined",
        )?;
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn validate_key(key: &str) -> Result<()> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid embedding object key");
    }
    Ok(())
}
fn validate_component(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid checkout identity");
    }
    Ok(())
}
fn checked_dir(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!(
                "embedding cache path is not a directory: {}",
                path.display()
            );
        }
    } else {
        fs::create_dir_all(path)?;
    }
    Ok(())
}
fn cache_lock(file: File, exclusive: bool, wait: bool) -> Result<Option<File>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if !wait => return Ok(None),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => bail!("embedding cache lock busy or unavailable: {error}"),
        }
    }
}
