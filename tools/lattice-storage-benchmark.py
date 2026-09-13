#!/usr/bin/env python3
"""Reproducible isolated 1/5/20 and 100-worktree Lattice storage benchmark."""
import argparse, contextlib, ctypes, hashlib, json, math, os, resource, shutil, socket, sqlite3, statistics, subprocess, sys, tempfile, threading, time
from pathlib import Path

COMMAND_TIMEOUT_SECONDS = 90
INDEX_TIMEOUT_SECONDS = 120

def acceptance_summary(result):
    """Fixture regression limits, calibrated before the final acceptance run.

    Timing/physical-resource limits describe the September 13 macOS baseline,
    not portable product guarantees. Missing measurements block acceptance.
    """
    checks=[]
    def check(name,value,limit,direction="maximum"):
        available=type(value) in (int,float) and math.isfinite(value)
        passed=available and value>=0 and (value==limit if direction=="exact" else value>=limit if direction=="minimum" else value<=limit)
        checks.append({"name":name,"value":value,"limit":limit,"direction":direction,
                       "status":"passed" if passed else "failed" if available else "blocked"})
    sample=result.get("samples",{}).get("20",{})
    embeddings=result.get("embedding_reuse",{})
    assets=embeddings.get("verified_assets",{})
    embedding=embeddings.get("samples",{}).get("20",{}) if embeddings.get("enabled") is True and all(assets.get(name) is True for name in ("model.onnx","tokenizer.json")) and embeddings.get("measurement_status")=="measured_real_onnx_objects_and_memberships" else {}
    check("completed_worktrees",result.get("completed_worktrees"),100,"exact")
    check("embedding_memberships_at_20",embedding.get("memberships"),1600,"exact")
    check("embedding_objects_at_20",embedding.get("referenced_objects"),1,"minimum")
    check("embedding_object_bytes_at_20",embedding.get("object_bytes"),1,"minimum")
    for kind,values in (("parsed",sample),("embedding",embedding)):
        for weight in ("file","byte"):
            check(f"{kind}_{weight}_reuse_at_20",values.get(f"eligible_{weight}_reuse"),.90,"minimum")
    check("ready_query_p95_seconds",result.get("query_seconds",{}).get("p95"),.100)
    check("allocated_bytes_at_20",sample.get("allocated_bytes"),128*1024**2)
    check("allocated_bytes_at_100",result.get("before_worktree_removal",{}).get("allocated_bytes"),256*1024**2)
    resources=result.get("daemon_resources_before_exit",{})
    if resources.get("status")!="measured": resources={}
    check("daemon_cpu_seconds",resources.get("cpu_seconds_ps"),120)
    check("daemon_peak_physical_footprint_bytes",resources.get("peak_physical_footprint_bytes"),1024**3)
    check("daemon_physical_bytes_written",resources.get("physical_bytes_written"),3*1024**3)
    wal=result.get("wal_high_water",{})
    check("sampled_wal_peak_bytes",wal.get("peak_sampled_bytes") if wal.get("status")=="measured" and wal.get("errors")==0 and type(wal.get("samples")) is int and wal["samples"]>0 else None,128*1024**2)
    gc=result.get("gc",{})
    for operation in ("plan","apply"):
        code=gc.get(operation+"_exit")
        check("gc_"+operation+"_success",None if code is None else int(code==0),1,"exact")
    reclamation=gc.get("filesystem_measurement",{})
    check("gc_net_allocated_bytes_reduced",reclamation.get("net_allocated_bytes_reduced") if reclamation.get("comparison_status")=="measured" else None,64*1024**2,"minimum")
    statuses={item["status"] for item in checks}
    return {"policy":"sep13-macos-40-files-100-worktrees-v1",
            "baseline_binary_sha256":"6c3b0c4acaf83a30f515e0d1c70fe9cfeda40ded2d5d56dbeb99043c4cb1d9a9",
            "status":"failed" if "failed" in statuses else "blocked" if "blocked" in statuses else "passed",
            "checks":checks,"scope":"Synthetic storage fixture only; does not establish memory delivery, agent efficacy, or native Windows acceptance."}

def daemon_resources(pid):
    """Kernel physical I/O and memory counters; unavailable is never zero."""
    if sys.platform != "darwin":
        return {"status":"unavailable", "reason":"this measurement adapter requires macOS proc_pid_rusage"}
    # Layout from the platform SDK sys/resource.h, rusage_info_v4. The
    # timing fields are intentionally not converted from platform counters.
    fields = "user_time system_time pkg_idle_wkups interrupt_wkups pageins wired_size resident_size phys_footprint proc_start_abstime proc_exit_abstime child_user_time child_system_time child_pkg_idle_wkups child_interrupt_wkups child_pageins child_elapsed_abstime diskio_bytesread diskio_byteswritten cpu_time_qos_default cpu_time_qos_maintenance cpu_time_qos_background cpu_time_qos_utility cpu_time_qos_legacy cpu_time_qos_user_initiated cpu_time_qos_user_interactive billed_system_time serviced_system_time logical_writes lifetime_max_phys_footprint instructions cycles billed_energy serviced_energy interval_max_phys_footprint runnable_time".split()
    class Usage(ctypes.Structure):
        _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in fields]
    usage = Usage()
    try:
        library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        function = library.proc_pid_rusage
        function.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
        function.restype = ctypes.c_int
        if function(pid, 4, ctypes.byref(usage)) != 0:
            return {"status":"unavailable", "errno":ctypes.get_errno()}
        cpu= subprocess.run(["ps","-p",str(pid),"-o","cputime="],capture_output=True,text=True,timeout=5)
        cpu_seconds=None
        if cpu.returncode == 0:
            try: cpu_seconds=parse_cpu_time(cpu.stdout.strip())
            except ValueError: pass
        return {"status":"measured", "source":"proc_pid_rusage_v4", "cpu_seconds_ps":cpu_seconds, "physical_bytes_read":usage.diskio_bytesread,
                "physical_bytes_written":usage.diskio_byteswritten, "resident_bytes":usage.resident_size,
                "peak_physical_footprint_bytes":usage.lifetime_max_phys_footprint,
                "logical_write_bytes":usage.logical_writes}
    except (OSError, AttributeError, subprocess.TimeoutExpired) as error:
        return {"status":"unavailable", "reason":str(error)}

def parse_cpu_time(value):
    days, separator, rest=value.partition('-')
    total=float(days)*86400 if separator else 0.0
    parts=(rest if separator else value).split(':')
    if not 2 <= len(parts) <= 3: raise ValueError("invalid ps CPU time")
    for power,part in enumerate(reversed(parts)): total+=float(part)*60**power
    return total

class WalSampler:
    """A sampled high-water mark, not an assertion about unsampled peaks."""
    def __init__(self, home):
        self.home=home; self.peak=0; self.samples=0; self.errors=0
        self.stop_event=threading.Event(); self.thread=threading.Thread(target=self.run,daemon=True)
    def run(self):
        while not self.stop_event.is_set():
            total=0
            try:
                for root, dirs, files in os.walk(self.home,followlinks=False):
                    dirs[:]=[name for name in dirs if not Path(root,name).is_symlink()]
                    for name in files:
                        if name.endswith('-wal'):
                            try: total+=os.lstat(Path(root,name)).st_size
                            except FileNotFoundError: pass
                self.peak=max(self.peak,total); self.samples+=1
            except OSError: self.errors+=1
            self.stop_event.wait(.1)
    def snapshot(self):
        return {"status":"measured" if self.samples else "unavailable","peak_sampled_bytes":self.peak if self.samples else None,"samples":self.samples,"sampling_interval_seconds":.1,"errors":self.errors}
    def close(self):
        self.stop_event.set(); self.thread.join()

def run(cmd, env, cwd=None, check=True, timeout=COMMAND_TIMEOUT_SECONDS):
    started = time.perf_counter()
    try:
        process = subprocess.run(cmd, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"command timed out after {timeout}s: {' '.join(map(str, cmd))}") from error
    elapsed = time.perf_counter() - started
    if check and process.returncode:
        raise RuntimeError(f"command failed ({process.returncode}): {' '.join(map(str, cmd))}\n{process.stderr}")
    return process, elapsed

def allocated(path):
    logical = blocks = 0
    if not path.exists(): return {"logical_bytes": 0, "allocated_bytes": 0}
    for root, dirs, files in os.walk(path, followlinks=False):
        for name in files:
            stat = os.lstat(Path(root) / name); logical += stat.st_size; blocks += stat.st_blocks * 512
    return {"logical_bytes": logical, "allocated_bytes": blocks}

def sqlite_metrics(home):
    db = home / "parsed-cache.db"
    result = {"parsed_objects": 0, "memberships": 0, "eligible_file_reuse": 0.0, "eligible_byte_reuse": 0.0, "wal_bytes": 0}
    if db.exists():
        with contextlib.closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as connection, connection:
            result["parsed_objects"] = connection.execute("select count(*) from parsed_file_cache").fetchone()[0]
            result["memberships"] = connection.execute("select count(*) from parsed_file_cache_memberships").fetchone()[0]
            result["referenced_objects"] = connection.execute("select count(distinct cache_key) from parsed_file_cache_memberships").fetchone()[0]
            unique_bytes = connection.execute("select coalesce(sum(length(payload)),0) from parsed_file_cache where cache_key in (select cache_key from parsed_file_cache_memberships)").fetchone()[0]
            member_bytes = connection.execute("select coalesce(sum(length(c.payload)),0) from parsed_file_cache_memberships m join parsed_file_cache c using(cache_key)").fetchone()[0]
        if result["memberships"]:
            result["eligible_file_reuse"] = 1 - result["referenced_objects"] / result["memberships"]
        if member_bytes: result["eligible_byte_reuse"] = 1 - unique_bytes / member_bytes
    result["wal_bytes"] = (db.with_name(db.name + "-wal").stat().st_size if db.with_name(db.name + "-wal").exists() else 0)
    return result

def embedding_metrics(root):
    """Metrics from the real content-addressed embedding object-cache schema."""
    db = root / "index.db"
    result = {"objects": 0, "memberships": 0, "object_bytes": 0, "eligible_file_reuse": 0.0,
              "logical_bytes": allocated(root)["logical_bytes"], "allocated_bytes": allocated(root)["allocated_bytes"]}
    if not db.exists(): return result
    with contextlib.closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as connection, connection:
        result["objects"] = connection.execute("select count(*) from objects").fetchone()[0]
        result["memberships"] = connection.execute("select count(*) from membership").fetchone()[0]
        result["object_bytes"] = connection.execute("select coalesce(sum(bytes),0) from objects").fetchone()[0]
        result["referenced_objects"] = connection.execute("select count(distinct object_key) from membership").fetchone()[0]
        unique_bytes = connection.execute("select coalesce(sum(bytes),0) from objects where key in (select object_key from membership)").fetchone()[0]
        member_bytes = connection.execute("select coalesce(sum(o.bytes),0) from objects o join membership m on o.key=m.object_key").fetchone()[0]
        result["eligible_byte_reuse"] = 1 - unique_bytes / member_bytes if member_bytes else 0.0
    if result["memberships"]:
        result["eligible_file_reuse"] = 1 - result["referenced_objects"] / result["memberships"]
    return result

def percentile(values, fraction):
    ordered = sorted(values); return ordered[min(len(ordered)-1, int((len(ordered)-1)*fraction))]

def reclamation_measurement(before,after,plan_text,apply_text,plan_exit,apply_exit):
    result={"before_apply":before,"after_apply":after,
            "net_logical_bytes_reduced":before["logical_bytes"]-after["logical_bytes"],
            "net_allocated_bytes_reduced":before["allocated_bytes"]-after["allocated_bytes"],
            "limitations":"Filesystem delta includes concurrent private-daemon writes and SQLite checkpoint effects; it is independent of apply's reported deletion count."}
    if plan_exit != 0 or apply_exit != 0:
        result["comparison_status"]="unavailable_operator_failure"
        return result
    try:
        plan=json.loads(plan_text); applied=json.loads(apply_text)
        result.update(planned_reclaimable_bytes=plan["inventory"]["reclaimable_bytes"],
                      planned_candidate_bytes=sum(item["bytes"] for item in plan["candidates"]),
                      apply_reported_released_bytes=applied["report"]["released_bytes"])
    except (ValueError,KeyError,TypeError): result["comparison_status"]="unavailable_invalid_operator_report"
    else: result["comparison_status"]="measured"
    return result

def wait_memberships(home, workspace, expected=40, content_hash=None, timeout=INDEX_TIMEOUT_SECONDS):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        registry = home / "storage-registry.db"; cache = home / "parsed-cache.db"
        if registry.exists() and cache.exists():
            with contextlib.closing(sqlite3.connect(f"file:{registry}?mode=ro", uri=True)) as connection, connection:
                row = connection.execute("select checkout_id from checkout_registry where root=?", (str(workspace.resolve()),)).fetchone()
            if row:
                with contextlib.closing(sqlite3.connect(f"file:{cache}?mode=ro", uri=True)) as connection, connection:
                    count = connection.execute("select count(*) from parsed_file_cache_memberships where checkout_id=?", row).fetchone()[0]
                    matched = content_hash is None or connection.execute("select exists(select 1 from parsed_file_cache_memberships m join parsed_file_cache c using(cache_key) where m.checkout_id=? and c.content_hash=?)", (row[0],content_hash)).fetchone()[0]
                if count >= expected and matched: return
        time.sleep(.05)
    raise RuntimeError(f"index did not publish {expected} parsed memberships for {workspace}")

class IndexNotReady(RuntimeError):
    pass

def wait_ready_query(binary,query,workspace,env,expected_file,timeout=INDEX_TIMEOUT_SECONDS):
    started=time.perf_counter(); deadline=time.monotonic()+timeout; retries=0
    while time.monotonic()<deadline:
        try:
            elapsed=ready_query(binary,query,workspace,env,expected_file)
            return elapsed,{"retries":retries,"wall_seconds":time.perf_counter()-started}
        except IndexNotReady:
            retries+=1
            time.sleep(.05)
    raise RuntimeError(f"public query view did not become ready for {query} in {timeout} seconds")

def ready_query(binary, query, workspace, env, expected_file):
    """Measure a query only after index publication; reject bootstrap-only replies."""
    process, elapsed = run([str(binary), "context", query, "--mode", "focused", "--json", "--workspace", str(workspace)], env)
    try:
        payload = json.loads(process.stdout)
    except ValueError as error:
        raise RuntimeError("context --json returned non-JSON output") from error
    def pending(node):
        if isinstance(node,dict):
            if node.get("indexing") is True or node.get("operation_performed") is False or node.get("partial_reason") in ("index_unavailable","bootstrap"):
                return True
            return any(pending(value) for value in node.values())
        if isinstance(node,list): return any(pending(value) for value in node)
        if isinstance(node,str) and node.lstrip().startswith(("{","[")):
            try: return pending(json.loads(node))
            except ValueError: pass
        return False
    if pending(payload):
        raise IndexNotReady("context query returned an explicitly incomplete index view")
    def matched(node):
        if isinstance(node, dict):
            symbol = node.get("symbol", node.get("name", node.get("s", "")))
            source = node.get("file", node.get("f"))
            if isinstance(symbol, str) and symbol == query and source == expected_file:
                return True
            return any(matched(value) for value in node.values())
        if isinstance(node, list):
            return any(matched(value) for value in node)
        if isinstance(node, str) and node.lstrip().startswith(("{", "[")):
            try: return matched(json.loads(node))
            except ValueError: pass
        return False
    if not matched(payload):
        raise RuntimeError("context query did not contain a matched symbol with source-file evidence: " + process.stdout[:4096])
    return elapsed

def wait_embeddings(root, checkout_id, timeout=INDEX_TIMEOUT_SECONDS, previous_membership=None, expected=80, file_path=None, previous_file_objects=None):
    """Membership is committed only after real inference, vector flush, and publication."""
    deadline = time.monotonic() + timeout
    db = root / "index.db"
    while time.monotonic() < deadline:
        if db.exists():
            try:
                with contextlib.closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as connection, connection:
                    memberships = connection.execute("select count(*) from membership m join objects o on o.key=m.object_key where m.checkout=? and o.bytes>0", (checkout_id,)).fetchone()[0]
                    rows = connection.execute("select member,object_key from membership where checkout=? order by member", (checkout_id,)).fetchall()
                    file_rows=[]
                    if file_path is not None:
                        prefix=hashlib.sha256(file_path.encode()).hexdigest()+":"
                        file_rows=connection.execute("select m.member,m.object_key from membership m join objects o on o.key=m.object_key where m.checkout=? and m.member>=? and m.member<? and o.bytes>0 order by m.member",(checkout_id,prefix,prefix+"\uffff")).fetchall()
                    fingerprint = hashlib.sha256(json.dumps(rows).encode()).hexdigest()
                # The fixture has forty file nodes and forty function nodes.
                # Half-published embeddings are not a ready checkout.
                file_objects=[key for _,key in file_rows]
                file_ready=file_path is None or len(file_rows)==2 and len(set(file_objects))==2 and (previous_file_objects is None or len(set(previous_file_objects))==2 and set(file_objects).isdisjoint(previous_file_objects))
                if memberships == expected and fingerprint != previous_membership and file_ready:
                    return {**embedding_metrics(root), "checkout_membership_sha256": fingerprint,"verified_file":file_path,"file_object_keys":file_objects}
            except sqlite3.Error:
                pass
        time.sleep(.05)
    raise RuntimeError(f"embedding runtime unavailable or inference/publication failed for checkout {checkout_id}")

def embedding_model_availability(env):
    """Read only the source-defined shared model location; never provision one."""
    version = "all-minilm-l6-v2-1110a243"
    directory = Path(env.get("LATTICE_EMBEDDING_MODEL_DIR") or Path.home() / ".lattice" / "models" / version)
    expected = {"model.onnx": "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452", "tokenizer.json": "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037"}
    actual = {}
    for name, digest in expected.items():
        path = directory / name
        actual[name] = path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest().lower() == digest
    available = all(actual.values())
    return {"model_directory": str(directory), "source": "shared_embedding_model_dir", "verified_assets": actual, "available": available, "measurement_status": "pending_real_embedding_reuse_measurement" if available else "not_installed_or_unverified_no_embedding_measurement"}

def write_benchmark_failure(output, fixture, binary, completed_worktrees, error, fixture_retained=True):
    report={"schema_version":1,"status":"failed","fixture_worktrees_requested":100,
            "completed_worktrees":completed_worktrees,"fixture_retained":fixture_retained,
            "audit_artifacts":str(fixture) if fixture_retained else None,
            "binary":str(binary),"binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),
            "error":{"type":type(error).__name__,"message":str(error)[:4096]}}
    report["acceptance"]=acceptance_summary(report)
    with output.open("x") as handle:
        json.dump(report,handle,indent=2)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("daemon/target/release/lattice"))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--keep-fixture", action="store_true")
    parser.add_argument("--with-embeddings", action="store_true", help="measure actual ONNX object/membership reuse using an already verified shared model")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file(): raise SystemExit(f"missing binary: {binary}")
    if args.output and args.output.exists(): raise SystemExit("refusing to overwrite an existing benchmark report")
    if shutil.disk_usage(tempfile.gettempdir()).free < 2*1024**3: raise SystemExit("storage benchmark requires at least 2 GiB free scratch space")
    model_availability = embedding_model_availability(os.environ)
    if args.with_embeddings and not model_availability["available"]:
        raise SystemExit("--with-embeddings requires a verified preinstalled shared model; no model was downloaded")
    fixture = Path(tempfile.mkdtemp(prefix="lattice-storage-benchmark-")).resolve()
    if args.output:
        output = args.output.resolve()
    else:
        report_fd, report_path = tempfile.mkstemp(prefix="lattice-storage-results-", suffix=".json")
        os.close(report_fd)
        output = Path(report_path)
        output.unlink()
    env = os.environ.copy(); state = fixture / "state"; runtime = fixture / "runtime"; home = fixture / "user"
    for inherited_authority in ("LATTICE_ORGANIZATION_ID","LATTICE_SHARED_MEMORY_PATH"):
        env.pop(inherited_authority,None)
    for directory in (state, runtime, home): directory.mkdir(mode=0o700)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0)); port = sock.getsockname()[1]
    env.update(XDG_STATE_HOME=str(state), XDG_RUNTIME_DIR=str(runtime), HOME=str(home),
               LATTICE_DAEMON_ADDR=f"127.0.0.1:{port}", LATTICE_DAEMON_EXIT_WHEN_IDLE="1",
               LATTICE_CACHE_IDLE_GRACE_SECS="1")
    if args.with_embeddings:
        env.update(LATTICE_EMBEDDING_MODEL_DIR=model_availability["model_directory"], LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC="1")
    daemon_log = open(fixture / "daemon.log", "w")
    daemon = None
    wal_sampler = None
    completed_worktrees = 0
    try:
        daemon = subprocess.Popen([str(binary), "--daemon"], env=env, stdout=daemon_log, stderr=subprocess.STDOUT)
        for _ in range(100):
            with socket.socket() as probe:
                if probe.connect_ex(("127.0.0.1", port)) == 0: break
            if daemon.poll() is not None: raise RuntimeError("isolated daemon exited during startup")
            time.sleep(.05)
        else: raise RuntimeError("isolated daemon did not accept connections")
        repo = fixture / "repo"; repo.mkdir()
        run(["git", "init", "-q", str(repo)], env); run(["git", "-C", str(repo), "config", "user.email", "bench@example.invalid"], env)
        run(["git", "-C", str(repo), "config", "user.name", "Lattice Benchmark"], env)
        src = repo / "src"; src.mkdir()
        for index in range(40): (src / f"module_{index}.rs").write_text(f"pub fn shared_{index}(x: usize) -> usize {{ x + {index} }}\n")
        (repo / "Cargo.toml").write_text('[package]\nname="storage-fixture"\nversion="0.1.0"\nedition="2021"\n')
        run(["git", "-C", str(repo), "add", "."], env); run(["git", "-C", str(repo), "commit", "-qm", "fixture"], env)
        worktrees=[]; samples={}; query_times=[]; cold=[]; warm=[]; incremental=[]; readiness=[]; embedding_samples={}; embedding_failure=None
        storage_home = repo / ".lattice"
        wal_sampler=WalSampler(storage_home); wal_sampler.thread.start()
        embedding_root = storage_home / "embedding-objects"
        for index in range(1, 101):
            worktree = fixture / "worktrees" / f"w{index:03d}"; worktree.parent.mkdir(exist_ok=True)
            run(["git", "-C", str(repo), "worktree", "add", "--detach", "-q", str(worktree), "HEAD"], env)
            worktrees.append(worktree)
            started = time.perf_counter()
            run([str(binary), "context", "shared_7", "--workspace", str(worktree)], env)
            wait_memberships(storage_home, worktree)
            if args.with_embeddings and embedding_failure is None:
                try:
                    with contextlib.closing(sqlite3.connect(f"file:{storage_home/'storage-registry.db'}?mode=ro", uri=True)) as connection, connection:
                        checkout_id = connection.execute("select checkout_id from checkout_registry where root=?", (str(worktree.resolve()),)).fetchone()[0]
                    embedding_sample = wait_embeddings(embedding_root, checkout_id,file_path="src/module_0.rs")
                except (sqlite3.Error, IndexError, TypeError, RuntimeError) as error:
                    embedding_failure = str(error)
            elapsed,ready = wait_ready_query(binary, "shared_7", worktree, env, "src/module_7.rs")
            cold.append(time.perf_counter()-started)
            readiness.append({"checkout":index,"phase":"cold",**ready})
            warm.append(elapsed); query_times.append(elapsed)
            edit_started=time.perf_counter()
            (worktree / "src" / "module_0.rs").write_text(f"pub fn changed_{index}() -> usize {{ {index} }}\n")
            # This request starts incremental publication; it is deliberately
            # not included in latency because it may return bootstrap status.
            run([str(binary), "context", "changed", "--workspace", str(worktree)], env)
            wait_memberships(storage_home, worktree, content_hash=hashlib.sha256((worktree / "src" / "module_0.rs").read_bytes()).hexdigest())
            if args.with_embeddings and embedding_failure is None:
                try:
                    embedding_sample = wait_embeddings(embedding_root, checkout_id, previous_membership=embedding_sample["checkout_membership_sha256"],file_path="src/module_0.rs",previous_file_objects=embedding_sample["file_object_keys"])
                except RuntimeError as error:
                    embedding_failure = str(error)
            elapsed,ready=wait_ready_query(binary, f"changed_{index}", worktree, env, "src/module_0.rs")
            incremental.append(time.perf_counter()-edit_started)
            query_times.append(elapsed); readiness.append({"checkout":index,"phase":"incremental",**ready})
            completed_worktrees = index
            if index in (1, 5, 20):
                samples[str(index)] = {**allocated(storage_home), **sqlite_metrics(storage_home),
                    "cold_wall_seconds_mean": statistics.fmean(cold), "warm_wall_seconds_mean": statistics.fmean(warm),
                    "incremental_index_wall_seconds_mean":statistics.fmean(incremental),
                    "query_seconds":{"p50":percentile(query_times,.50),"p95":percentile(query_times,.95)},
                    "daemon_resources":daemon_resources(daemon.pid),"wal_high_water":wal_sampler.snapshot()}
                if args.with_embeddings and embedding_failure is None:
                    embedding_samples[str(index)] = embedding_sample
        before_removal={"completed_worktrees":completed_worktrees,**allocated(storage_home),**sqlite_metrics(storage_home),
                        "embedding_reuse":embedding_metrics(embedding_root) if args.with_embeddings and embedding_failure is None else None,
                        "daemon_resources":daemon_resources(daemon.pid),"wal_high_water":wal_sampler.snapshot()}
        for worktree in worktrees: run(["git", "-C", str(repo), "worktree", "remove", "--force", str(worktree)], env)
        time.sleep(2)
        plan = fixture / "gc-plan.json"
        plan_result, _ = run([str(binary), "storage", "cache", "plan", "--workspace", str(repo), "--idle-secs", "1", "--high-bytes", "2", "--low-bytes", "1", "--batch", "4096", "--output", str(plan)], env, check=False)
        before_apply=allocated(storage_home)
        apply_result, _ = run([str(binary), "storage", "cache", "apply", "--workspace", str(repo), "--plan", str(plan)], env, check=False) if plan.exists() else (None, 0)
        after_apply=allocated(storage_home)
        results={"schema_version":1,"status":"completed","fixture_worktrees":100,"completed_worktrees":completed_worktrees,"audit_artifacts":str(fixture) if args.keep_fixture else None,"binary":str(binary),"binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"samples":samples,"before_worktree_removal":before_removal,
            "query_seconds":{"p50":percentile(query_times,.50),"p95":percentile(query_times,.95),"definition":"ready context --mode focused --json queries after persisted membership and changed-content checks"},
            "embedding_reuse":({**model_availability, "enabled": args.with_embeddings,
                "measurement_status": ("runtime_unavailable_or_inference_failed" if embedding_failure else "measured_real_onnx_objects_and_memberships") if args.with_embeddings else model_availability["measurement_status"],
                "error": embedding_failure, "samples": embedding_samples,
                "final": embedding_metrics(embedding_root) if args.with_embeddings and embedding_failure is None else None,
                "limitations":"Object-cache membership is accepted only after ONNX inference, vector-index flush, and committed publication; unavailable runtime or failed inference is reported as a failed measurement, never zero reuse."}),
            "final":{**allocated(repo/".lattice"),**sqlite_metrics(repo/".lattice")},
            "gc":{"filesystem_measurement":reclamation_measurement(before_apply,after_apply,plan_result.stdout,apply_result.stdout if apply_result else "",plan_result.returncode,apply_result.returncode if apply_result else None),"plan_exit":plan_result.returncode,"plan_stdout":plan_result.stdout,"plan_stderr":plan_result.stderr,
                  "apply_exit":apply_result.returncode if apply_result else None,"apply_stdout":apply_result.stdout if apply_result else "","apply_stderr":apply_result.stderr if apply_result else ""}}
        results["daemon_resources_before_exit"]=daemon_resources(daemon.pid)
        results["public_readiness"]=readiness
        wal_sampler.close(); results["wal_high_water"]=wal_sampler.snapshot(); wal_sampler=None
        daemon.terminate()
        try: daemon.wait(timeout=5)
        except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
        usage=resource.getrusage(resource.RUSAGE_CHILDREN)
        results["process_tree_resources"]={"cpu_user_seconds":usage.ru_utime,"cpu_system_seconds":usage.ru_stime,"maximum_child_rss_platform_units":usage.ru_maxrss,"block_input_operations":usage.ru_inblock,"block_output_operations":usage.ru_oublock,"includes":"isolated daemon, CLI calls and Git fixture operations"}
        results["acceptance"]=acceptance_summary(results)
        with output.open("x") as report:
            report.write(json.dumps(results, indent=2)+"\n")
        print(output)
    except BaseException as error:
        if not output.exists():
            write_benchmark_failure(output, fixture, binary, completed_worktrees, error, args.keep_fixture)
        raise
    finally:
        if wal_sampler is not None: wal_sampler.close()
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try: daemon.wait(timeout=5)
            except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
        daemon_log.close()
        if not args.keep_fixture: shutil.rmtree(fixture, ignore_errors=True)

if __name__ == "__main__": main()
