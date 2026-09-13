#!/usr/bin/env python3
"""Harness contract tests; these are not agent efficacy measurements."""
import importlib.util
import io
import json
from pathlib import Path
from unittest import mock
import tempfile
import unittest
from types import SimpleNamespace
import sys

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
spec = importlib.util.spec_from_file_location("efficacy", TOOLS / "lattice-agent-efficacy.py")
efficacy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(efficacy)
runner_spec = importlib.util.spec_from_file_location("efficacy_runner", Path(__file__).resolve().parents[1] / "lattice-codex-efficacy-runner.py")
efficacy_runner = importlib.util.module_from_spec(runner_spec)
runner_spec.loader.exec_module(efficacy_runner)
SCOPE = next(task for task in efficacy.TASKS if task["id"] == "scope-before-candidate-budget")
def receipt(repo="repo"):
    return {"authority":f"repository:{repo}","delivery_id":"delivery-1","payload_hash":"sha256:"+"a"*64}
def with_receipt(raw,repo="repo"):
    value=dict(raw); value["memory_deliveries"]=[dict(receipt(repo),ack_required=True)]; return value

class EfficacyContract(unittest.TestCase):
    def test_private_empty_recall_requires_clean_zero_result(self):
        clean={"count":0,"memories":[],"memory_deliveries":[]}
        raw={"content":[{"type":"text","text":json.dumps(clean)}]}
        self.assertEqual(efficacy_runner.empty_recall_evidence(raw),clean)
        for invalid in ({},dict(clean,count=1),dict(clean,memories=[{"id":"unexpected"}]),dict(clean,memory_attribution={"status":"unavailable","error":"zero accesses"}),dict(clean,memory_deliveries=[{"delivery_id":"ghost"}])):
            with self.assertRaises(RuntimeError):efficacy_runner.empty_recall_evidence(invalid)

    def test_private_expansion_smoke_uses_actual_handle_and_rejects_leaked_content(self):
        expected={"repository_id":"repo-fixture","memory_id":"lesson-one"}
        raw={"content":[{"type":"text","text":json.dumps({"context_handle":"ctx-one"})}]}
        arguments=efficacy_runner.context_memory_expansion_arguments(raw,expected)
        self.assertEqual(arguments["handle"],"ctx-one")
        self.assertEqual(arguments["focus"],"memory:repository:repo-fixture:lesson-one")
        for invalid in ({}, {"context_handle":"ctx-one","nested":{"context_handle":"ctx-two"}}):
            with self.assertRaises(RuntimeError):
                efficacy_runner.context_memory_expansion_arguments(invalid,expected)
        rpc=mock.Mock()
        safe={"error":{"code":-32001,"message":"Memory is no longer applicable"}}
        rpc.tool.side_effect=efficacy_runner.RpcError(safe)
        self.assertEqual(efficacy_runner.rejected_memory_expansion(rpc,arguments,"obsolete lesson"),safe)
        rpc.tool.assert_called_with("context",arguments)
        for invalid in ({"error":{"message":"obsolete lesson"}}, {"error":{"message":"unavailable"},"memory_deliveries":[{"delivery_id":"ghost"}]}):
            rpc.tool.side_effect=efficacy_runner.RpcError(invalid)
            with self.assertRaises(RuntimeError):
                efficacy_runner.rejected_memory_expansion(rpc,arguments,"obsolete lesson")
        rpc.tool.side_effect=None
        rpc.tool.return_value={"memories":[]}
        with self.assertRaises(RuntimeError):
            efficacy_runner.rejected_memory_expansion(rpc,arguments,"obsolete lesson")

    def test_failed_tool_obsolete_content_is_not_hidden_by_later_clean_response(self):
        expected={"memory_id":"obsolete","content":"Old archived policy.","content_sha256":"unused","expectation":"absent"}
        response={"memory_delivery_evidence":[{"tool":"recall","raw_response_payload":{"memories":[]}}],
                  "tool_calls":[{"lattice_detected":True,"record":{"tool":"mcp__lattice__recall","status":"failed","result":{"isError":True,"content":[{"type":"text","text":"Old archived policy."}]}}}]}
        with self.assertRaisesRegex(ValueError,"required to be absent"):
            efficacy.validate_delivery(response,expected,"explicit")

    def test_private_daemon_cannot_inherit_live_organization_authority(self):
        with tempfile.TemporaryDirectory() as temporary:
            original={"HOME":"/original-auth-home","LATTICE_ORGANIZATION_ID":"live-organization","LATTICE_SHARED_MEMORY_PATH":"/live/memories.db"}
            env=dict(original,**efficacy_runner.private_organization_environment(Path(temporary)))
            self.assertEqual(env["HOME"],original["HOME"])
            self.assertEqual(env["LATTICE_ORGANIZATION_ID"],"lattice-efficacy-private")
            self.assertEqual(Path(env["LATTICE_SHARED_MEMORY_PATH"]),Path(temporary).resolve()/"organization"/"memories.db")

    def test_feedback_smoke_requires_persisted_exact_access_and_resolution(self):
        def response(data): return {"content":[{"type":"text","text":json.dumps(data)}]}
        data={"memory_attribution":{"status":"recorded","retrieval_id":"retrieval-one","accesses":[{"memory_id":"lesson-one","access_id":"access-one"}]}}
        claim=efficacy_runner.attribution_claim(response(data),"lesson-one")
        self.assertEqual(claim,{"retrieval_id":"retrieval-one","access_ids":["access-one"],"disposition":"used"})
        for invalid in ({},{"memory_attribution":{"status":"unavailable"}}, {"memory_attribution":dict(data["memory_attribution"],accesses=[{"memory_id":"foreign","access_id":"access-one"}])}, {"memory_attribution":dict(data["memory_attribution"],accesses=data["memory_attribution"]["accesses"]*2)}):
            with self.assertRaises(RuntimeError): efficacy_runner.attribution_claim(response(invalid),"lesson-one")
        valid={"memory_feedback":{"status":"recorded","retrieval_id":"retrieval-one","newly_resolved":True}}
        self.assertTrue(efficacy_runner.feedback_response(response(valid),"retrieval-one",True)["newly_resolved"])
        for raw,identity,new in ((valid,"foreign",True),(valid,"retrieval-one",False),({"memory_feedback":{"status":"error"}},"retrieval-one",True),({},"retrieval-one",True)):
            with self.assertRaises(RuntimeError): efficacy_runner.feedback_response(response(raw),identity,new)

    def test_actual_markdown_raw_identity_requires_matching_receipt_authority(self):
        expected={"memory_id":"lesson-1","repository_id":"repo_"+"a"*64,"content":"Apply scope first.","content_sha256":"digest"}
        receipt={"authority":"repository:"+expected["repository_id"],"delivery_id":"delivery-1","payload_hash":"sha256:proof","ack_required":True}
        def response(identity="lesson-1",receipts=None):
            text="### Relevant memory\n- Apply scope first.\n- Memory: `"+identity+"` (advisory)\n\n### Memory delivery receipts\n```json\n"+json.dumps([receipt] if receipts is None else receipts)+"\n```"
            return {"content":[{"type":"text","text":text}]}
        self.assertEqual(efficacy_runner.markdown_delivery_evidence(response(),expected)["memory_id"],"lesson-1")
        for raw in (response("lesson-10"),response(receipts=[dict(receipt,authority="repository:foreign")]),response(receipts=[receipt,receipt])):
            with self.assertRaises(RuntimeError): efficacy_runner.markdown_delivery_evidence(raw,expected)

    def test_workspace_readiness_rejects_bootstrap_and_query_echo(self):
        for data in ({"query":"selector.py"},{"operation_performed":False},{"overview":"Indexing is in progress"}):
            self.assertFalse(efficacy_runner.workspace_ready_payload({"structuredContent":data},"selector.py"))
        data={"ranked_pivots":[{"file":"selector.py","symbol":"select"}]}
        self.assertTrue(efficacy_runner.workspace_ready_payload({"content":[{"type":"text","text":json.dumps(data)}]},"selector.py"))

    def test_default_markdown_requires_complete_lesson_identity_and_receipt(self):
        expected={"memory_id":"memory-one","content":"Keep scope before the limit.","content_sha256":"digest"}
        receipt={"authority":"repository:repo-one","delivery_id":"delivery-one","payload_hash":"hash-one","ack_required":True}
        def response(content):
            return {"content":[{"type":"text","text":"### Relevant memory\n- "+content+"\n- Memory: `memory-one` (advisory)\n\n### Memory delivery receipts\n```json\n"+json.dumps([receipt])+"\n```"}]}
        proof=efficacy_runner.markdown_delivery_evidence(response(expected["content"]),expected)
        self.assertEqual(proof["receipt"]["delivery_id"],"delivery-one")
        with self.assertRaises(RuntimeError):
            efficacy_runner.markdown_delivery_evidence(response("Keep scope..."),expected)

    def test_artifact_retention_preserves_unknown_directories(self):
        with tempfile.TemporaryDirectory() as path:
            root=Path(path)
            owned=efficacy.allocate_artifacts(SimpleNamespace(artifacts_directory=path))
            marker=owned / ".lattice-efficacy-artifact.json"
            data=json.loads(marker.read_text()); data["expires_at"]=1; marker.write_text(json.dumps(data))
            unknown=root / "run-user-owned"; unknown.mkdir(); (unknown / "keep").write_text("preserve")
            next_run=efficacy.allocate_artifacts(SimpleNamespace(artifacts_directory=path))
            self.assertFalse(owned.exists()); self.assertTrue(next_run.exists())
            self.assertEqual((unknown / "keep").read_text(),"preserve")

    def test_every_broken_base_fails_and_reference_passes_its_hidden_grader(self):
        with tempfile.TemporaryDirectory() as path:
            root = Path(path)
            for task in efficacy.TASKS:
                target = root / task["allowed_files"][0]
                target.write_text(task["base_files"][target.name])
                self.assertFalse(efficacy.score(root, task)["correct"], task["id"])
                target.write_text(task["reference_files"][target.name])
                self.assertTrue(efficacy.score(root, task)["correct"], task["id"])

    def test_every_fixture_commits_a_public_contract_without_exposing_reference_patch(self):
        with tempfile.TemporaryDirectory() as path:
            root=Path(path)
            for task in efficacy.TASKS:
                fixture=root/task["id"]; fixture.mkdir()
                efficacy.materialize_fixture(fixture,task)
                self.assertTrue((fixture/"CONTRACT.md").is_file(),task["id"])
                self.assertIn("Implement",(fixture/"CONTRACT.md").read_text(),task["id"])
                public=efficacy.public_task(task)
                self.assertNotIn("reference_files",public)
                self.assertNotIn("grader",public)

    def test_missing_runner_is_a_blocked_gate(self):
        with tempfile.TemporaryDirectory() as path:
            with self.assertRaisesRegex(ValueError, "adapter is required"):
                efficacy.execute(SimpleNamespace(runner=path + "/missing", output=path + "/report", trials=3, seed=1, model="fixture", timeout=10))

    def test_bootstrap_is_deterministic_and_single_trial_insufficient(self):
        self.assertEqual(efficacy.bootstrap_interval([0,1,-1],7),efficacy.bootstrap_interval([0,1,-1],7))
        self.assertIsNone(efficacy.bootstrap_interval([1],7))
        self.assertEqual(efficacy.bootstrap_interval({"task-a":[1,1,1],"task-b":[-1,-1,-1]},7),[-1,1])

    def test_score_rejects_invalid_or_unassessable_grader_results(self):
        task=efficacy.TASKS[0]
        outcomes=(
            {"assessment":"assessed","correct":1,"mistake_recurrence":False,"target_decision_id":task["target_decision_id"],"checks":{"target":True}},
            {"assessment":"assessed","correct":False,"mistake_recurrence":True,"target_decision_id":"wrong","checks":{"target":False}},
            {"assessment":"unassessable","correct":False,"mistake_recurrence":False,"target_decision_id":task["target_decision_id"],"checks":{"submission_loadable":False}},
        )
        for outcome in outcomes:
            completed=SimpleNamespace(stdout=json.dumps(outcome))
            with mock.patch.object(efficacy,"run",return_value=completed), self.assertRaises(ValueError):
                efficacy.score(Path("/unused"),task)
        with mock.patch.object(efficacy,"run",side_effect=__import__('subprocess').TimeoutExpired("grader",1)), self.assertRaises(__import__('subprocess').TimeoutExpired):
            efficacy.score(Path("/unused"),task)

    def test_primary_gate_uses_task_cluster_ci_and_blocks_zero_regression_and_diagnostic(self):
        def rows(arm,deltas,recurrences,misleading=False):
            result=[]
            for task_index,(task_deltas,task_recurrences) in enumerate(zip(deltas,recurrences)):
                for trial,(correct,recurrence) in enumerate(zip(task_deltas,task_recurrences)):
                    result.append({"task":f"t{task_index}","trial":trial,"arm":arm,"score":{"correct":correct,"mistake_recurrence":recurrence},"misleading_advice":misleading})
            return result
        baseline=rows("baseline",[[False]*3]*6,[[True]*3]*6)
        positive=rows("briefing",[[True]*3]*6,[[False]*3]*6)
        self.assertTrue(efficacy.compare_arm(positive,baseline,3,True)["release_gate_passed"])
        zero=rows("briefing",[[False]*3]*6,[[True]*3]*6)
        self.assertFalse(efficacy.compare_arm(zero,baseline,3,True)["release_gate_passed"])
        regression=rows("briefing",[[False]*3]+[[True]*3]*5,[[False]*3]*6)
        reg_baseline=rows("baseline",[[True]*3]+[[False]*3]*5,[[True]*3]*6)
        self.assertFalse(efficacy.compare_arm(regression,reg_baseline,3,True)["release_gate_passed"])
        self.assertFalse(efficacy.compare_arm(positive,baseline,3,False)["release_gate_passed"])
        misleading=[dict(row,misleading_advice=True) for row in positive]
        self.assertFalse(efficacy.compare_arm(misleading,baseline,3,True)["release_gate_passed"])

    def test_mock_runner_cannot_self_certify_correctness_or_improvement(self):
        with tempfile.TemporaryDirectory() as path:
            root = Path(path)
            runner = root / "runner.py"
            references={task["id"]:task["reference_files"] for task in efficacy.TASKS}
            references.update({task["predecessor"]["id"]:task["predecessor"]["reference_files"] for task in efficacy.TASKS if task.get("predecessor")})
            runner.write_text("#!/usr/bin/env python3\nimport hashlib,json,os,sys\nfrom pathlib import Path\nREFERENCES="+repr(references)+"\nr=json.loads(Path(sys.argv[1]).read_text())\nmanifest=json.loads((Path(r['workspace']).parent.parent/'audit-manifest.json').read_text())\nbinary_sha=next(value for path,value in manifest['artifact_sha256'].items() if Path(path).name=='lattice')\nfor name,content in REFERENCES[r['task_id']].items(): Path(r['workspace'],name).write_text(content)\nresp={'model':'fixture','settings':{'reasoning_effort':'low'},'input_tokens':1,'output_tokens':1,'tool_calls':[],'lattice_binary_sha256':binary_sha}\nif r['phase']=='capture_validated':\n memory_id='fixture-'+hashlib.sha256(r['task_id'].encode()).hexdigest()[:12]; raw={'memory_id':memory_id,'workspace_id':'repo-fixture','memory':{'id':memory_id,'workspace_id':'repo-fixture','content':r['memory_seed']}}; captured={'memory_id':memory_id,'repository_id':'repo-fixture','content_sha256':hashlib.sha256(r['memory_seed'].encode()).hexdigest()}; resp['captured_memory']=captured; resp['tool_calls']=[{'tool':'lattice.remember','result':raw}]\n if r.get('supersedes_capture'):\n  old=r['supersedes_capture']; proposal={'proposal_id':'proposal-'+memory_id,'decision':'pending'}; applied={'proposal_id':proposal['proposal_id'],'decision':'applied'}; edge={'source':'memory:repo-fixture/'+memory_id,'target':'memory:repo-fixture/'+old['memory_id'],'link_type':'supersedes'}; resp['supersession_evidence']={'proposal':proposal,'apply':applied,'conflicts':{'conflicts':[edge]}}\nelse:\n if r.get('arm')=='explicit': resp['tool_calls']=[{'lattice_detected':True,'record':{'tool':'mcp__lattice__recall','status':'completed','result':{}}}]\n if r.get('arm')=='briefing': resp['briefing_response']={}\n if r.get('expected_delivery'):\n  tool='recall' if r['arm']=='explicit' else 'prepare_change'; resp['memory_delivery_evidence']=[]\n  for e in [r['expected_delivery']]+r.get('excluded_deliveries',[]):\n   receipt={'authority':'repository:'+e['repository_id'],'delivery_id':'delivery-1','payload_hash':'sha256:'+'a'*64}; raw={'memory_deliveries':[receipt]} if e['expectation']=='absent' else {'memory_id':'repository:'+e['repository_id']+':'+e['memory_id'],'content':e['content'],'memory_deliveries':[dict(receipt,ack_required=True)]}; resp['memory_delivery_evidence'].append({'tool':tool,'successful_response':True,'memory_id':e['memory_id'],'content':e['content'],'content_sha256':e['content_sha256'],'absent':e['expectation']=='absent','receipt':receipt,'response_payload':raw,'raw_response_payload':raw})\nPath(sys.argv[2]).write_text(json.dumps(resp))\n")
            runner.chmod(0o700)
            output = root / "report.json"
            efficacy.execute(SimpleNamespace(runner=str(runner),output=str(output),artifacts_directory=str(root / "artifacts"),trials=3,seed=4,model="fixture",timeout=10))
            report=json.loads(output.read_text())
            self.assertEqual(len(report["records"]),len(efficacy.TASKS)*3*3)
            manifest=json.loads((Path(report["audit_artifacts"])/"audit-manifest.json").read_text())
            self.assertEqual(manifest["status"],"preregistered")
            self.assertEqual(manifest["execution_permissions"],{"sandbox":"workspace-write","approval_review":"auto_review"})
            self.assertEqual(manifest["fixture_version"],efficacy.FIXTURE_VERSION)
            self.assertEqual(manifest["primary_arm"],"briefing")
            self.assertEqual(manifest["diagnostic_arm"],"explicit")
            fixture_ids={task["id"] for task in efficacy.TASKS}|{task["predecessor"]["id"] for task in efficacy.TASKS if task.get("predecessor")}
            self.assertEqual(set(manifest["fixture_sha256"]),fixture_ids)
            self.assertEqual(report["protocol"]["evidence_scope"],f"bounded {len(efficacy.TASKS)}-task fixture evidence only")
            self.assertFalse(report["comparison"]["briefing"]["release_gate_passed"])
            self.assertEqual(report["comparison"]["explicit"]["mistake_recurrence_reduction"],0)
            self.assertTrue(all(r["score"]["correct"] for r in report["records"]))
            for task in efficacy.TASKS:
                rows=[r for r in report["records"] if r["task"]==task["id"]]
                for trial in range(3):
                    self.assertEqual(len({r["revision"] for r in rows if r["trial"]==trial}),1)
            with self.assertRaisesRegex(ValueError,"overwrite"):
                efficacy.execute(SimpleNamespace(runner=str(runner),output=str(output),artifacts_directory=str(root / "artifacts"),trials=3,seed=4,model="fixture",timeout=10))

    def test_runtime_validation_applies_to_producer_and_consumer_responses(self):
        args=SimpleNamespace(model="fixture")
        expected=efficacy.validated_runtime({"model":"fixture","settings":{"reasoning_effort":"low"}},args,None)
        self.assertEqual(expected,("fixture",'{"reasoning_effort": "low"}'))
        for response in ({"model":"other","settings":{"temperature":0}},{"model":"fixture","settings":{"temperature":1}}):
            with self.assertRaisesRegex(ValueError,"model/settings changed"):
                efficacy.validated_runtime(response,args,expected)
        efficacy.validated_runtime({'model':'fixture','settings':{'reasoning_effort':'low'},'lattice_binary_sha256':'abc'},args,expected,'abc')
        with self.assertRaisesRegex(ValueError,'binary changed'):
            efficacy.validated_runtime({'model':'fixture','settings':{'reasoning_effort':'low'},'lattice_binary_sha256':'other'},args,expected,'abc')

    def test_arm_call_contract_requires_recall_and_forbids_control_lattice_calls(self):
        good={'tool_calls':[{'lattice_detected':True,'record':{'tool':'mcp__lattice__recall','status':'completed','result':{}}}]}
        efficacy.validate_arm_calls(good,'explicit')
        efficacy.validate_arm_calls({'tool_calls':[],'briefing_response':{}},'briefing')
        unrelated={'tool_calls':[{'lattice_detected':False,'record':{'tool':'other__recall','status':'completed','result':{}}}]}
        for response,arm in (({'tool_calls':[]},'explicit'),(good,'baseline'),(good,'briefing'),({'tool_calls':[]},'briefing'),({'tool_calls':[],'briefing_response':{'isError':True}},'briefing'),(unrelated,'explicit'),({'tool_calls':[{'lattice_detected':True,'record':{'tool':'recall','status':'completed','result':'arbitrary plaintext'}}]},'explicit'),({'tool_calls':[{'lattice_detected':True,'record':{'tool':'recall','status':'failed','result':{'isError':True}}}]},'explicit')):
            with self.assertRaises(ValueError):efficacy.validate_arm_calls(response,arm)

    def test_every_agent_arm_disables_python_cache_and_states_workspace_policy(self):
        completed=SimpleNamespace(returncode=0,stdout=json.dumps({'type':'turn.completed','usage':{'input_tokens':1,'output_tokens':1}})+'\n',stderr='')
        with tempfile.TemporaryDirectory() as temporary:
            request={'prompt':'Fix it','workspace':temporary,'task':{'id':'fixture','allowed_files':['target.py']},'model':'fixture','settings':{'reasoning_effort':'low'},'expected_delivery':None}
            for arm in ('baseline','explicit','briefing','producer'):
                with self.subTest(arm=arm),mock.patch.object(efficacy_runner.subprocess,'run',return_value=completed) as invoked:
                    environment={'INHERITED':'yes','LATTICE_DAEMON_ADDR':'127.0.0.1:1','XDG_STATE_HOME':'/state','XDG_RUNTIME_DIR':'/runtime','LATTICE_LIFECYCLE_LOG_DIR':'/logs'}
                    efficacy_runner.run_agent(dict(request,arm=arm),Path(temporary),environment,{} if arm=='briefing' else None)
                    command=invoked.call_args.args[0]; child_env=invoked.call_args.kwargs['env']; prompt=command[-1]
                    self.assertEqual(child_env,dict(environment,PYTHONDONTWRITEBYTECODE='1'))
                    self.assertIn('--approve-for-me',command)
                    self.assertNotIn('--sandbox',command)  # approve-for-me selects workspace-write; flags conflict
                    self.assertNotIn('--dangerously-bypass-approvals-and-sandbox',command)
                    self.assertNotIn('--ignore-rules',command)
                    self.assertIn('use -B',prompt); self.assertIn('Leave no generated files',prompt); self.assertIn('commit, stage files',prompt)
                    if arm=='briefing':self.assertIn('Do not call Lattice yourself',prompt)

    def test_checked_invocation_rejects_runner_changed_during_response(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary); runner=root/'runner'; runner.write_text('#!/bin/sh\n'); runner.chmod(0o700)
            task={'id':'freeze','problem':'fix','source':'test','allowed_files':['target.py'],'seed':None,'target_decision_id':'control','base_files':{'target.py':'old\n','CONTRACT.md':'Implement fix.\n'},'reference_files':{'target.py':'new\n','CONTRACT.md':'Implement fix.\n'},'grader':"import json;print(json.dumps({'assessment':'assessed','correct':True,'mistake_recurrence':False,'target_decision_id':'control','checks':{'submission_loadable':True}}))"}
            response={'model':'fixture','settings':{'reasoning_effort':'low'},'input_tokens':0,'output_tokens':0,'tool_calls':[]}
            def mutate(*_):
                runner.write_text('#!/bin/sh\n# changed\n')
                return response
            args=SimpleNamespace(runner=str(runner),output=str(root/'report'),artifacts_directory=str(root/'artifacts'),trials=3,seed=1,model='fixture',timeout=1)
            with mock.patch.object(efficacy,'TASKS',[task]),mock.patch.object(efficacy,'invoke',side_effect=mutate),self.assertRaisesRegex(RuntimeError,'run failed'):
                efficacy.execute(args)

    def test_workspace_guard_rejects_staged_untracked_head_and_symlink_escape(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo=Path(temporary)/'repo'; repo.mkdir(); (repo/'allowed.py').write_text('old\n'); (repo/'other.py').write_text('old\n')
            efficacy.run(['git','init','-q',str(repo)]); efficacy.run(['git','-C',str(repo),'add','.']); efficacy.run(['git','-C',str(repo),'-c','user.name=Test','-c','user.email=test@invalid','commit','-qm','base'])
            revision=efficacy.run(['git','-C',str(repo),'rev-parse','HEAD']).stdout.strip()
            (repo/'allowed.py').write_text('new\n'); self.assertIn('new',efficacy.validate_workspace(repo,revision,['allowed.py']))
            (repo/'other.py').write_text('changed\n'); efficacy.run(['git','-C',str(repo),'add','other.py'])
            with self.assertRaisesRegex(ValueError,'outside'):efficacy.validate_workspace(repo,revision,['allowed.py'])
            efficacy.run(['git','-C',str(repo),'restore','--staged','--worktree','other.py']); (repo/'untracked\nname').write_text('x')
            with self.assertRaisesRegex(ValueError,'outside'):efficacy.validate_workspace(repo,revision,['allowed.py'])
            (repo/'untracked\nname').unlink(); (repo/'allowed.py').unlink(); (repo/'allowed.py').symlink_to(Path(temporary)/'escape')
            with self.assertRaisesRegex(ValueError,'escapes'):efficacy.validate_workspace(repo,revision,['allowed.py'])

    def test_delivery_validation_rejects_empty_wrong_id_and_id_only(self):
        expected={"memory_id":"m-1","repository_id":"repo","content":"exact lesson","content_sha256":__import__('hashlib').sha256(b"exact lesson").hexdigest(),"expectation":"present"}
        for evidence in (None, [], [{"tool":"recall","memory_id":"m-2","content":"exact lesson","content_sha256":expected["content_sha256"],"response_payload":{"memory_id":"m-2","content":"exact lesson"}}], [{"tool":"recall","memory_id":"m-1","content":"exact lesson","content_sha256":expected["content_sha256"],"response_payload":{"memory_id":"m-1"}}]):
            with self.assertRaises(ValueError):
                efficacy.validate_delivery({"memory_delivery_evidence":evidence},expected,"explicit")
        raw=with_receipt({"memory_id":"repository:repo:m-1","content":"exact lesson"}); valid={"tool":"recall","successful_response":True,"memory_id":"m-1","content":"exact lesson","content_sha256":expected["content_sha256"],"receipt":receipt(),"response_payload":raw,"raw_response_payload":raw}
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":[valid]},expected,"explicit"),valid)
        for bad in ({},dict(receipt('foreign'),ack_required=True),dict(receipt(),payload_hash='sha256:not-a-hash',ack_required=True),dict(receipt(),ack_required=False)):
            raw=dict(valid['raw_response_payload'])
            if bad:raw['memory_deliveries']=[bad]
            else:raw.pop('memory_deliveries')
            with self.assertRaises(ValueError):efficacy.validate_delivery({'memory_delivery_evidence':[dict(valid,raw_response_payload=raw)]},expected,'explicit')
        raw=dict(valid['raw_response_payload'],memory_deliveries=[dict(receipt(),ack_required=True),dict(receipt(),delivery_id='delivery-2',ack_required=True)])
        with self.assertRaises(ValueError):efficacy.validate_delivery({'memory_delivery_evidence':[dict(valid,raw_response_payload=raw)]},expected,'explicit')

    def test_delivery_binds_content_to_the_same_memory_and_rejects_error_payloads(self):
        content = 'Use "exact" rules.\nKeep the scope.'
        digest = __import__('hashlib').sha256(content.encode()).hexdigest()
        expected = {"memory_id": "m-1", "content": content, "content_sha256": digest, "expectation": "present"}
        expected["repository_id"]="repo"; raw=with_receipt({"memories":[{"memory_id":"repository:repo:m-1","content":content}]})
        record = {"tool": "recall", "successful_response": True, "memory_id": "m-1", "content": content, "content_sha256": digest,"receipt":receipt(),"response_payload":raw,"raw_response_payload":raw}
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence": [record]}, expected, "explicit"), record)
        for payload in ({"isError": True, "memories": [{"id": "m-1", "content": content}]},
                        {"memories": [{"id": "m-1", "content": "unrelated"}, {"id": "m-2", "content": content}]}):
            with self.assertRaises(ValueError):
                efficacy.validate_delivery({"memory_delivery_evidence": [dict(record, response_payload=payload,raw_response_payload=payload)]}, expected, "explicit")
        self.assertIsNone(efficacy_runner.delivery_evidence("recall", {"isError": True, "content": content, "id": "m-1"}, expected))

    def test_absence_is_checked_across_every_raw_delivery_response(self):
        old={"memory_id":"old","repository_id":"repo","content":"obsolete lesson","content_sha256":"old-hash","expectation":"absent"}
        def item(raw): return {"tool":"recall","successful_response":True,"absent":True,"response_payload":{},"raw_response_payload":raw}
        current={"structuredContent":{"memories":[{"memory_id":"new","workspace_id":"repo","content":"current lesson"}]}}
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":[item(current)]},old,"explicit")["raw_response_payload"],current)
        failed_cli={'memory_delivery_evidence':[item(current)],'tool_calls':[{'lattice_detected':True,'record':{'tool':'command_execution','status':'failed','command':'lattice recall old','aggregated_output':'obsolete lesson'}}]}
        with self.assertRaisesRegex(ValueError,'required to be absent'):efficacy.validate_delivery(failed_cli,old,'explicit')
        delivered_old={"structuredContent":{"memories":[{"memory_id":"old","workspace_id":"repo","content":"obsolete lesson"}]}}
        with self.assertRaisesRegex(ValueError,"required to be absent"):
            efficacy.validate_delivery({"memory_delivery_evidence":[item(delivered_old),item(current)]},old,"explicit")
        wrapped={"content":[{"type":"text","text":json.dumps({"notes":["obsolete lesson"]})}]}
        with self.assertRaisesRegex(ValueError,"required to be absent"):
            efficacy.validate_delivery({"memory_delivery_evidence":[item(current),item(wrapped)]},old,"explicit")

    def test_present_delivery_is_proven_by_the_same_raw_response(self):
        content="current lesson"; digest=__import__('hashlib').sha256(content.encode()).hexdigest()
        expected={"memory_id":"new","repository_id":"repo","content":content,"content_sha256":digest,"expectation":"present"}
        split=[
            {"tool":"recall","memory_id":"new","content":content,"content_sha256":digest,"raw_response_payload":{"memories":[{"memory_id":"new","workspace_id":"repo","content":"wrong"}]}},
            {"tool":"recall","memory_id":"other","content":"wrong","content_sha256":digest,"raw_response_payload":{"memories":[{"memory_id":"other","workspace_id":"repo","content":content}]}},
        ]
        with self.assertRaises(ValueError): efficacy.validate_delivery({"memory_delivery_evidence":split},expected,"explicit")
        split[1]["receipt"]=receipt(); split[1]["raw_response_payload"]=with_receipt({"memories":[{"memory_id":"new","workspace_id":"repo","content":content}]})
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":split},expected,"explicit"),split[1])

    def test_capture_proof_requires_the_raw_remember_response_to_bind_id_and_content(self):
        captured={"memory_id":"m-1","content_sha256":"unused"}
        raw={"memory_id":"m-1","memory":{"id":"m-1","content":"exact lesson"}}
        response={"tool_calls":[{"tool":"lattice.remember","result":raw}]}
        self.assertEqual(efficacy.capture_proof(response,captured,"exact lesson"),raw)
        wrapped={"content":[{"type":"text","text":json.dumps(raw)}]}
        self.assertEqual(efficacy.capture_proof({"tool_calls":[{"tool":"lattice.remember","result":wrapped}]},captured,"exact lesson"),wrapped)
        for malformed in (
            {"tool_calls":[]},
            {"tool_calls":[{"tool":"lattice.remember","result":{"memory_id":"m-1"}}]},
            {"tool_calls":[{"tool":"lattice.remember","result":{"memory_id":"m-1","memory":{"id":"m-1","content":"tampered"}}}]},
            {"tool_calls":[{"tool":"lattice.remember","result":{"isError":True,"content":[{"type":"text","text":json.dumps(raw)}]}}]},
        ):
            with self.assertRaisesRegex(ValueError,"capture tool record"):
                efficacy.capture_proof(malformed,captured,"exact lesson")

    def test_supersession_proof_binds_public_proposal_apply_and_scoped_edge(self):
        old={"memory_id":"old","repository_id":"repo","content":"old lesson","content_sha256":"old-hash"}
        replacement={"memory_id":"new","repository_id":"repo","content_sha256":"new-hash"}
        evidence={"proposal":{"proposal_id":"p-1","decision":"pending"},"apply":{"proposal_id":"p-1","decision":"applied"},"conflicts":{"conflicts":[{"source":"memory:repo/new","target":"memory:repo/old","link_type":"supersedes"}]}}
        self.assertEqual(efficacy.supersession_proof({"supersession_evidence":evidence},old,replacement),evidence)
        for malformed in (
            dict(evidence,apply={"proposal_id":"other","decision":"applied"}),
            dict(evidence,conflicts={"conflicts":[{"source":"memory:repo/old","target":"memory:repo/new","link_type":"supersedes"}]}),
            dict(evidence,proposal={"isError":True,"structuredContent":evidence["proposal"]}),
            {"proposal":evidence["proposal"],"apply":evidence["apply"]},
        ):
            with self.assertRaises(ValueError): efficacy.supersession_proof({"supersession_evidence":malformed},old,replacement)
        with self.assertRaisesRegex(ValueError,"repository authority"):
            efficacy.supersession_proof({"supersession_evidence":evidence},old,dict(replacement,repository_id="foreign"))

    def test_failed_adapter_writes_exclusive_failure_manifest_for_timeout_and_nonzero(self):
        with tempfile.TemporaryDirectory() as path:
            root=Path(path)
            for name,script,timeout in (
                ("nonzero","#!/usr/bin/env python3\nraise SystemExit(2)\n",10),
                ("timeout","#!/usr/bin/env python3\nimport time\ntime.sleep(2)\n",0.01),
            ):
                directory=root/name; directory.mkdir(); runner=directory/"runner.py"; runner.write_text(script); runner.chmod(0o700)
                with self.assertRaisesRegex(RuntimeError,"failure.json"):
                    efficacy.invoke(runner,{"phase":"producer"},directory,timeout)
                manifest=json.loads((directory/"failure.json").read_text())
                self.assertEqual(manifest["kind"],"failed-operation")
                self.assertEqual(manifest["audit_artifacts"],str(directory.resolve()))

    def test_producer_grader_failure_writes_run_failure_manifest(self):
        with tempfile.TemporaryDirectory() as path:
            root=Path(path); runner=root/"runner.py"
            runner.write_text("#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\nPath(sys.argv[2]).write_text(json.dumps({'model':'fixture','settings':{'reasoning_effort':'low'},'input_tokens':0,'output_tokens':0,'tool_calls':[]}))\n")
            runner.chmod(0o700)
            with self.assertRaisesRegex(RuntimeError,"run-failure.json"):
                efficacy.execute(SimpleNamespace(runner=str(runner),output=str(root/"report.json"),artifacts_directory=str(root/"artifacts"),trials=3,seed=1,model="fixture",timeout=10))
            manifests=list((root/"artifacts").glob("run-*/run-failure.json"))
            self.assertEqual(len(manifests),1)
            self.assertEqual(json.loads(manifests[0].read_text())["kind"],"failed-run")
            self.assertEqual(json.loads(manifests[0].read_text())["release_gate_status"],"blocked")

    def test_runner_evidence_requires_a_single_memory_object_and_preserves_receipt(self):
        content="exact lesson"
        expected={"memory_id":"m-1","content":content,"content_sha256":__import__('hashlib').sha256(content.encode()).hexdigest(),"expectation":"present"}
        split={"structuredContent":{"memories":[{"id":"m-1","content":"other"},{"id":"m-2","content":content}]}}
        self.assertIsNone(efficacy_runner.delivery_evidence("recall",split,expected))
        expected["repository_id"]="repo"; result={"result":{"content":[{"type":"text","text":json.dumps({"memories":[{"memory_id":"repository:repo:m-1","content":content}],"memory_deliveries":[dict(receipt(),ack_required=True)]})}]}}
        evidence=efficacy_runner.delivery_evidence("recall",result,expected)
        self.assertEqual(evidence["receipt"]["delivery_id"],"delivery-1")
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":[evidence]},expected,"explicit"),evidence)

    def test_private_smoke_receipt_and_acknowledgement_contract(self):
        receipt={"authority":"repository:repo","delivery_id":"delivery-1","payload_hash":"sha256:exact"}
        delivered={"content":[{"type":"text","text":json.dumps({"memory_deliveries":[dict(receipt,ack_required=True)]})}]}
        self.assertEqual(efficacy_runner.delivery_receipt(delivered),receipt)
        self.assertIsNone(efficacy_runner.delivery_receipt({"content":[{"type":"text","text":json.dumps(receipt)}]}))
        exact={"content":[{"type":"text","text":json.dumps({"acknowledged_count":1,"replayed":False})}]}
        replay={"structuredContent":{"acknowledged_count":0,"replayed":True}}
        self.assertEqual(efficacy_runner.acknowledgement_response(exact,1,False)["acknowledged_count"],1)
        self.assertEqual(efficacy_runner.acknowledgement_response(replay,0,True)["replayed"],True)
        with self.assertRaisesRegex(RuntimeError,"idempotency"):
            efficacy_runner.acknowledgement_response(replay,1,False)
        error=efficacy_runner.RpcError({"error":{"message":"forged"}})
        self.assertEqual(error.response["error"]["message"],"forged")

    def test_runner_handles_structured_transport_and_stale_absence(self):
        class FakeRpc:
            def __init__(self): self.calls=0
            def call(self,method,params):
                self.calls+=1
                return {"structuredContent":{"operation_performed": True, "memories":[]}}
        fake=FakeRpc()
        self.assertEqual(efficacy_runner.Rpc.tool(fake,"recall",{"query":"fixture"})["structuredContent"]["memories"],[])
        self.assertEqual(fake.calls,1)
        expected={"memory_id":"m-stale","content":"superseded lesson","content_sha256":__import__('hashlib').sha256(b"superseded lesson").hexdigest(),"expectation":"absent"}
        response={"content":[{"type":"text","text":json.dumps({"memories":[],"memory_deliveries":[]})}]}
        evidence=efficacy_runner.non_delivery_evidence("prepare_change",response,expected)
        self.assertTrue(evidence["absent"])
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":[evidence]},expected,"briefing"),evidence)

    def test_briefing_requests_the_public_standard_full_prepare_change_wire(self):
        request={"prompt":"Select scoped records", "task": SCOPE}
        self.assertEqual(efficacy_runner.prepare_change_arguments(request),{
            "task":"Select scoped records selector.py", "entry_files":["selector.py"], "render":"json",
            "wire_format":"standard", "budget":"full"})

    def test_remember_capture_binds_receipt_id_and_exact_content(self):
        seed="round the aggregate total once"
        repository_id="repo_"+"a"*64
        result={"content":[{"type":"text","text":json.dumps({"memory_id":"m-1","memory":{"id":"m-1","content":seed,"expansion_handle":f"memory:{repository_id}/m-1"}})}]}
        captured=efficacy_runner.capture_evidence(result,seed)
        self.assertEqual(captured["memory_id"],"m-1")
        self.assertEqual(captured["repository_id"],repository_id)
        for malformed in (
            {"content":[{"type":"text","text":json.dumps({"memory_id":"m-1"})}]},
            {"content":[{"type":"text","text":json.dumps({"memory_id":"m-1","memory":{"id":"m-2","content":seed}})}]},
            {"content":[{"type":"text","text":json.dumps({"memory_id":"m-1","memory":{"id":"m-1","content":seed,"expansion_handle":f"memory:{repository_id}/m-2"}})}]},
            {"content":[{"type":"text","text":json.dumps({"memory_id":"m-1","memory":{"id":"m-1","workspace_id":"repo_"+"b"*64,"content":seed,"expansion_handle":f"memory:{repository_id}/m-1"}})}]},
            {"content":[{"type":"text","text":json.dumps({"memory_id":"m-1","memory":{"id":"m-1","content":seed,"expansion_handle":"memory:foreign/m-1"}})}]},
        ):
            with self.assertRaises(RuntimeError): efficacy_runner.capture_evidence(malformed,seed)

    def test_capture_uses_the_public_durable_remember_contract(self):
        request={"memory_seed":"exact lesson", "task":SCOPE}
        args=efficacy_runner.durable_capture_arguments(request)
        self.assertEqual(args["kind"],"durable")
        self.assertEqual(args["scope"],"repo")
        self.assertEqual(args["memory_class"],"constraint")
        self.assertEqual(args["confidence"],1.0)
        self.assertEqual(args["freshness_policy"],"repo_scoped")
        self.assertEqual(args["linked_files"],["selector.py"])
        self.assertEqual(args["content"],"exact lesson")

    def test_delivery_tool_normalizes_only_public_names(self):
        self.assertEqual(efficacy_runner.public_delivery_tool({"tool":"lattice.recall"}),"recall")
        self.assertEqual(efficacy_runner.public_delivery_tool({"tool":"prepare_change"}),"prepare_change")
        self.assertEqual(efficacy_runner.public_delivery_tool({"tool":"lattice.context"}),"lattice.context")

    def test_baseline_lattice_detection_covers_mcp_bare_and_alternate_paths(self):
        for item in (
            {"server":"lattice"}, {"server":"lattice.private"}, {"server":"mcp__lattice__recall"},
            {"command":"lattice recall fixture"}, {"command":"/opt/custom/lattice recall fixture"}, {"command":"/bin/zsh -lc 'lattice status'"},
        ):
            self.assertTrue(efficacy_runner.is_lattice_invocation(item),item)
        self.assertFalse(efficacy_runner.is_lattice_invocation({"command":"python -m fixture"}))
        self.assertEqual(efficacy_runner.public_delivery_tool({"tool":"mcp__lattice__prepare_change"}),"prepare_change")

    def test_wrapped_prepare_change_memory_highlight_uses_structured_memory_id(self):
        content="exact lesson"; expected={"memory_id":"m-1","repository_id":"repo","content":content,"content_sha256":__import__('hashlib').sha256(content.encode()).hexdigest(),"expectation":"present"}
        raw={"content":[{"type":"text","text":json.dumps({"memory_highlights":[{"memory_id":{"ulid":"m-1","workspace_id":"repo"},"content":content}],"memory_deliveries":[dict(receipt(),ack_required=True)]})}]}
        evidence=efficacy_runner.delivery_evidence("prepare_change",raw,expected)
        self.assertIsNotNone(evidence)
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":[evidence]},expected,"briefing"),evidence)
        wrong={"content":[{"type":"text","text":json.dumps({"memory_highlights":[{"memory_id":{"ulid":"m-2","workspace_id":"repo"},"content":content}]})}]}
        self.assertIsNone(efficacy_runner.delivery_evidence("prepare_change",wrong,expected))

    def test_actual_typed_qualified_highlight_preserves_repository_authority(self):
        repository='repo_'+'a'*64; content='Complete lesson from public prepare_change.'
        expected={'memory_id':'lesson-1','repository_id':repository,'content':content,'content_sha256':__import__('hashlib').sha256(content.encode()).hexdigest(),'expectation':'present'}
        def raw(encoded,workspace=repository):
            public_receipt=dict(receipt(repository),ack_required=True)
            return {'structuredContent':{'memory_highlights':[{'memory_id':{'ulid':encoded,'workspace_id':workspace},'content':content}],'memory_deliveries':[public_receipt]}}
        actual=raw(f'repository:{repository}:lesson-1')
        evidence=efficacy_runner.delivery_evidence('prepare_change',actual,expected)
        self.assertIsNotNone(evidence)
        self.assertEqual(efficacy.validate_delivery({'memory_delivery_evidence':[evidence]},expected,'briefing'),evidence)
        for invalid in (raw(f'repository:{repository}:lesson-1','repo_'+'b'*64),raw(f'organization:{repository}:lesson-1'),raw(f'repository:{repository}')):
            self.assertIsNone(efficacy_runner.delivery_evidence('prepare_change',invalid,expected))
            forged=dict(evidence,raw_response_payload=invalid)
            with self.assertRaises(ValueError):efficacy.validate_delivery({'memory_delivery_evidence':[forged]},expected,'briefing')

    def test_qualified_recall_requires_the_captured_repository_authority(self):
        content="exact lesson"
        expected={"memory_id":"m-1","repository_id":"repo-good","content":content,"content_sha256":__import__('hashlib').sha256(content.encode()).hexdigest(),"expectation":"present"}
        def evidence(authority,delivered=content):
            raw=with_receipt({"memory_id":f"repository:{authority}:m-1","content":delivered},authority)
            return efficacy_runner.delivery_evidence("recall",raw,expected)
        accepted=evidence("repo-good")
        self.assertIsNotNone(accepted)
        self.assertEqual(efficacy.validate_delivery({"memory_delivery_evidence":[accepted]},expected,"explicit"),accepted)
        self.assertIsNone(evidence("repo-foreign"))
        self.assertIsNone(evidence("repo-good","wrong lesson"))
        self.assertIsNone(efficacy_runner.delivery_evidence(
            "recall", {"memory_id":"m-1","content":content}, expected
        ))
        organization={"memory_id":"organization:repo-good:m-1","content":content}
        self.assertIsNone(efficacy_runner.delivery_evidence("recall",organization,expected))

    def test_task_sources_name_existing_current_files(self):
        root=Path(__file__).resolve().parents[2]
        for task in efficacy.TASKS:
            source=task["source"]
            if source.startswith("daemon/"):
                self.assertTrue((root/source.split(":",1)[0]).is_file(),source)

    def test_rpc_constructor_terminates_child_when_initialize_fails(self):
        class Input:
            def write(self,_): pass
            def flush(self): pass
            def close(self): pass
        class Process:
            def __init__(self):
                self.stdin=Input(); self.stdout=io.StringIO('{"jsonrpc":"2.0","id":1,"error":{"message":"failed"}}\n'); self.terminated=False
            def terminate(self): self.terminated=True
            def wait(self,timeout=None): return 1
            def kill(self): self.terminated=True
        process=Process()
        with tempfile.TemporaryFile(mode="w+") as log, mock.patch.object(efficacy_runner.subprocess,"Popen",return_value=process):
            with self.assertRaises(RuntimeError): efficacy_runner.Rpc("/fixture",{},log)
        self.assertTrue(process.terminated)

if __name__ == "__main__":
    unittest.main()
