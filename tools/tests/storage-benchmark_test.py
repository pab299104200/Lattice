#!/usr/bin/env python3
"""Audit the storage measurement contract without launching a daemon."""
import contextlib
import importlib.util
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("storage_benchmark", Path(__file__).parents[1] / "lattice-storage-benchmark.py")
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)

class StorageBenchmarkContract(unittest.TestCase):
    def test_public_readiness_retries_only_explicit_pending_views(self):
        pending=subprocess.CompletedProcess([],0,json.dumps({"content":[{"type":"text","text":json.dumps({"indexing":True,"partial_reason":"index_unavailable","result_set_state":"not_evaluated"})}]}),"")
        ready=subprocess.CompletedProcess([],0,json.dumps({"symbol":"shared_7","file":"src/module_7.rs"}),"")
        with patch.object(benchmark,"run",side_effect=[(pending,.5),(ready,.03)]),patch.object(benchmark.time,"sleep"):
            elapsed,wait=benchmark.wait_ready_query(Path("lattice"),"shared_7",Path("repo"),{},"src/module_7.rs")
        self.assertEqual(elapsed,.03); self.assertEqual(wait["retries"],1)
        with patch.object(benchmark,"ready_query",side_effect=RuntimeError("wrong file")) as query:
            with self.assertRaisesRegex(RuntimeError,"wrong file"):
                benchmark.wait_ready_query(Path("lattice"),"shared_7",Path("repo"),{},"src/module_7.rs")
            self.assertEqual(query.call_count,1)

    def test_public_readiness_has_a_deadline(self):
        with patch.object(benchmark.time,"monotonic",side_effect=[0,0,2]),patch.object(benchmark.time,"sleep"),patch.object(benchmark,"ready_query",side_effect=benchmark.IndexNotReady("pending")):
            with self.assertRaisesRegex(RuntimeError,"did not become ready"):
                benchmark.wait_ready_query(Path("lattice"),"shared_7",Path("repo"),{},"src/module_7.rs",timeout=1)

    def test_acceptance_cannot_confuse_completed_with_measured_or_passing(self):
        result={"completed_worktrees":100,"samples":{"20":{"eligible_file_reuse":.93,"eligible_byte_reuse":.93,"allocated_bytes":60*1024**2}},
                "embedding_reuse":{"enabled":True,"verified_assets":{"model.onnx":True,"tokenizer.json":True},"measurement_status":"measured_real_onnx_objects_and_memberships","samples":{"20":{"eligible_file_reuse":.93,"eligible_byte_reuse":.93,"memberships":1600,"referenced_objects":118,"object_bytes":625348}}},
                "before_worktree_removal":{"allocated_bytes":170*1024**2},"query_seconds":{"p95":.03},
                "daemon_resources_before_exit":{"status":"measured","cpu_seconds_ps":55,"peak_physical_footprint_bytes":500*1024**2,"physical_bytes_written":1600*1024**2},
                "wal_high_water":{"status":"measured","samples":1000,"errors":0,"peak_sampled_bytes":40*1024**2},
                "gc":{"plan_exit":0,"apply_exit":0,"filesystem_measurement":{"comparison_status":"measured","net_allocated_bytes_reduced":100*1024**2}}}
        self.assertEqual(benchmark.acceptance_summary(result)["status"],"passed")
        for group in ("embedding_reuse","daemon_resources_before_exit","wal_high_water","gc"):
            missing=dict(result); missing[group]={}
            self.assertEqual(benchmark.acceptance_summary(missing)["status"],"blocked",group)
        for changed in (dict(result,completed_worktrees=99),dict(result,completed_worktrees=101),dict(result,query_seconds={"p95":.101}),dict(result,gc={"filesystem_measurement":{"comparison_status":"measured","net_allocated_bytes_reduced":-1}})):
            self.assertEqual(benchmark.acceptance_summary(changed)["status"],"failed")
        for operation in ("plan","apply"):
            invalid=dict(result,gc=dict(result["gc"],**{operation+"_exit":1}))
            invalid["gc"]["filesystem_measurement"]={"comparison_status":"unavailable_operator_failure"}
            self.assertEqual(benchmark.acceptance_summary(invalid)["status"],"failed")
        for changed in (dict(result,wal_high_water=dict(result["wal_high_water"],samples=0)),dict(result,embedding_reuse=dict(result["embedding_reuse"],enabled=False)),dict(result,embedding_reuse=dict(result["embedding_reuse"],verified_assets={}))):
            self.assertEqual(benchmark.acceptance_summary(changed)["status"],"blocked")
        invalid=dict(result,embedding_reuse=dict(result["embedding_reuse"],samples={"20":dict(result["embedding_reuse"]["samples"]["20"],memberships=0,referenced_objects=0,object_bytes=0)}))
        self.assertEqual(benchmark.acceptance_summary(invalid)["status"],"failed")
        self.assertEqual(benchmark.acceptance_summary(dict(result,query_seconds={"p95":float("nan")}))["status"],"blocked")

    def test_reclamation_compares_estimates_and_independent_filesystem_delta(self):
        report=benchmark.reclamation_measurement({"logical_bytes":1000,"allocated_bytes":2000},{"logical_bytes":800,"allocated_bytes":1900},json.dumps({"inventory":{"reclaimable_bytes":300},"candidates":[{"bytes":300}]}),json.dumps({"report":{"released_bytes":300}}),0,0)
        self.assertEqual(report["planned_candidate_bytes"],300)
        self.assertEqual(report["apply_reported_released_bytes"],300)
        self.assertEqual(report["net_allocated_bytes_reduced"],100)
        failed=benchmark.reclamation_measurement(report["before_apply"],report["after_apply"],json.dumps({"inventory":{"reclaimable_bytes":300},"candidates":[{"bytes":300}]}),json.dumps({"report":{"released_bytes":300}}),0,1)
        self.assertEqual(failed["comparison_status"],"unavailable_operator_failure")
        self.assertEqual(failed["net_allocated_bytes_reduced"],100)
        self.assertNotIn("apply_reported_released_bytes",failed)

    def test_sampler_shutdown_and_unavailable_are_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            sampler=benchmark.WalSampler(Path(temporary))
            self.assertEqual(sampler.snapshot()["status"],"unavailable")
            self.assertIsNone(sampler.snapshot()["peak_sampled_bytes"])
            sampler.thread.start(); sampler.close()
            self.assertFalse(sampler.thread.is_alive())
            self.assertTrue(sampler.stop_event.is_set())

    def test_query_rejects_substring_and_wrong_file(self):
        for symbol,file in [("shared_70","src/module_7.rs"),("shared_7","src/foreign.rs")]:
            result=subprocess.CompletedProcess([],0,json.dumps({"symbol":symbol,"file":file}),"")
            with patch.object(benchmark,"run",return_value=(result,.1)),self.assertRaises(RuntimeError):
                benchmark.ready_query(Path("lattice"),"shared_7",Path("repo"),{},"src/module_7.rs")

    def test_cpu_time_units(self):
        self.assertEqual(benchmark.parse_cpu_time("02:03.50"),123.5)
        self.assertEqual(benchmark.parse_cpu_time("1-02:03:04"),93784)
        with self.assertRaises(ValueError): benchmark.parse_cpu_time("unknown")

    def test_unavailable_kernel_metrics_are_not_reported_as_zero(self):
        with patch.object(benchmark.sys,"platform","unsupported"):
            result=benchmark.daemon_resources(1)
        self.assertEqual(result["status"],"unavailable")
        self.assertNotIn("physical_bytes_read",result)

    def test_wal_sample_counts_only_wals_and_preserves_high_water(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary); (root/"one-wal").write_bytes(b"12345")
            (root/"unrelated").write_bytes(b"123456789")
            sampler=benchmark.WalSampler(root)
            with patch.object(sampler.stop_event,"is_set",side_effect=[False,True]), patch.object(sampler.stop_event,"wait"):
                sampler.run()
            self.assertEqual(sampler.snapshot()["peak_sampled_bytes"],5)
            (root/"one-wal").unlink()
            with patch.object(sampler.stop_event,"is_set",side_effect=[False,True]), patch.object(sampler.stop_event,"wait"):
                sampler.run()
            self.assertEqual(sampler.snapshot()["peak_sampled_bytes"],5)
            self.assertEqual(sampler.snapshot()["samples"],2)

    def test_failed_run_records_partial_progress_without_overwriting_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary); binary=root/"lattice"; binary.write_bytes(b"frozen artifact")
            output=root/"failure.json"
            benchmark.write_benchmark_failure(output,root,binary,3,RuntimeError("fourth edit was not published"))
            report=json.loads(output.read_text())
            self.assertEqual(report["status"],"failed")
            self.assertEqual(report["completed_worktrees"],3)
            self.assertEqual(report["acceptance"]["status"],"failed")
            self.assertNotIn("query_seconds",report)
            original=output.read_bytes()
            with self.assertRaises(FileExistsError):
                benchmark.write_benchmark_failure(output,root,binary,100,RuntimeError("retry"))
            self.assertEqual(output.read_bytes(),original)

    def test_query_echo_is_not_matched_evidence(self):
        for payload in ({"query": "shared_7"}, {"operation_performed": False, "query": "shared_7"}):
            result = subprocess.CompletedProcess([], 0, json.dumps(payload), "")
            with patch.object(benchmark, "run", return_value=(result, .1)):
                with self.assertRaises(RuntimeError):
                    benchmark.ready_query(Path("lattice"), "shared_7", Path("repo"), {}, "src/module_7.rs")

    def test_query_requires_source_symbol_inside_actual_response(self):
        payload = {"content": [{"type": "text", "text": json.dumps({"pivots": [{"symbol": "shared_7", "file": "src/module_7.rs"}]})}]}
        result = subprocess.CompletedProcess([], 0, json.dumps(payload), "")
        with patch.object(benchmark, "run", return_value=(result, .1)):
            self.assertEqual(benchmark.ready_query(Path("lattice"), "shared_7", Path("repo"), {}, "src/module_7.rs"), .1)

    def test_dense_query_requires_the_same_symbol_and_file_evidence(self):
        for pivot, accepted in [({"s": "shared_7", "f": "src/module_7.rs"}, True), ({"q": "shared_7", "f": "src/module_7.rs"}, False)]:
            result = subprocess.CompletedProcess([], 0, json.dumps({"ranked_pivots": [pivot]}), "")
            with patch.object(benchmark, "run", return_value=(result, .1)):
                if accepted:
                    self.assertEqual(benchmark.ready_query(Path("lattice"), "shared_7", Path("repo"), {}, "src/module_7.rs"), .1)
                else:
                    with self.assertRaises(RuntimeError):
                        benchmark.ready_query(Path("lattice"), "shared_7", Path("repo"), {}, "src/module_7.rs")

    def test_unreferenced_history_does_not_distort_parse_reuse(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with contextlib.closing(sqlite3.connect(root / "parsed-cache.db")) as db, db:
                db.executescript("CREATE TABLE parsed_file_cache(cache_key TEXT PRIMARY KEY,payload TEXT); CREATE TABLE parsed_file_cache_memberships(checkout_id TEXT,cache_key TEXT);")
                db.executemany("INSERT INTO parsed_file_cache VALUES (?,?)", [("shared", "1234"), ("unused", "unused history")])
                db.executemany("INSERT INTO parsed_file_cache_memberships VALUES (?,?)", [("one", "shared"), ("two", "shared")])
            metrics = benchmark.sqlite_metrics(root)
            self.assertEqual(metrics["parsed_objects"], 2)
            self.assertEqual(metrics["referenced_objects"], 1)
            self.assertEqual(metrics["eligible_file_reuse"], .5)
            self.assertEqual(metrics["eligible_byte_reuse"], .5)

    def test_embedding_readiness_requires_new_committed_membership(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with contextlib.closing(sqlite3.connect(root / "index.db")) as db, db:
                db.executescript("CREATE TABLE objects(key TEXT PRIMARY KEY,bytes INTEGER); CREATE TABLE membership(checkout TEXT,member TEXT,object_key TEXT);")
                db.executemany("INSERT INTO objects VALUES (?,?)", [("old", 100), ("new", 100), ("unused", 100)])
                db.executemany("INSERT INTO membership VALUES (?,?,?)", [("checkout", str(n), "old") for n in range(40)])
            with self.assertRaises(RuntimeError):
                benchmark.wait_embeddings(root, "checkout", timeout=.01)
            before = benchmark.wait_embeddings(root, "checkout", timeout=.1, expected=40)
            self.assertEqual(before["referenced_objects"], 1)
            self.assertAlmostEqual(before["eligible_byte_reuse"], 39/40)
            with contextlib.closing(sqlite3.connect(root / "index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='missing' WHERE member='0'")
            with self.assertRaises(RuntimeError):
                benchmark.wait_embeddings(root,"checkout",timeout=.01,expected=40)
            with contextlib.closing(sqlite3.connect(root / "index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='old' WHERE member='0'")
            with self.assertRaises(RuntimeError):
                    benchmark.wait_embeddings(root, "checkout", timeout=.01, previous_membership=before["checkout_membership_sha256"], expected=40)
            with contextlib.closing(sqlite3.connect(root / "index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='new' WHERE member='0'")
            after = benchmark.wait_embeddings(root, "checkout", timeout=.1, previous_membership=before["checkout_membership_sha256"], expected=40)
            self.assertNotEqual(before["checkout_membership_sha256"], after["checkout_membership_sha256"])

    def test_edited_file_embeddings_must_change_both_exact_file_members(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            file="src/module_0.rs"
            prefix=hashlib.sha256(file.encode()).hexdigest()+":"
            with contextlib.closing(sqlite3.connect(root/"index.db")) as db, db:
                db.executescript("CREATE TABLE objects(key TEXT PRIMARY KEY,bytes INTEGER); CREATE TABLE membership(checkout TEXT,member TEXT,object_key TEXT);")
                db.executemany("INSERT INTO objects VALUES (?,100)",[(key,) for key in ("old-file","old-symbol","new-file","new-symbol","unrelated")])
                db.executemany("INSERT INTO membership VALUES ('checkout',?,?)",[(prefix+"file","old-file"),(prefix+"symbol","old-symbol"),("foreign","old-file")])
            before=benchmark.wait_embeddings(root,"checkout",timeout=.1,expected=3,file_path=file)
            with contextlib.closing(sqlite3.connect(root/"index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='unrelated' WHERE member='foreign'")
            args={"timeout":.01,"expected":3,"file_path":file,"previous_membership":before["checkout_membership_sha256"],"previous_file_objects":before["file_object_keys"]}
            with self.assertRaises(RuntimeError): benchmark.wait_embeddings(root,"checkout",**args)
            with contextlib.closing(sqlite3.connect(root/"index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='new-file' WHERE member=?",(prefix+"file",))
            with self.assertRaises(RuntimeError): benchmark.wait_embeddings(root,"checkout",**args)
            with contextlib.closing(sqlite3.connect(root/"index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='new-file' WHERE member=?",(prefix+"symbol",))
            with self.assertRaises(RuntimeError): benchmark.wait_embeddings(root,"checkout",**args)
            with contextlib.closing(sqlite3.connect(root/"index.db")) as db, db:
                db.execute("UPDATE membership SET object_key='new-symbol' WHERE member=?",(prefix+"symbol",))
            self.assertEqual(set(benchmark.wait_embeddings(root,"checkout",**args)["file_object_keys"]),{"new-file","new-symbol"})

if __name__ == "__main__":
    unittest.main()
