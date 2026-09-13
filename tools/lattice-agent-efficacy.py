#!/usr/bin/env python3
"""Paired writable-agent efficacy trials; protocol: docs/agent-efficacy.md."""
import argparse, contextlib, hashlib, json, os, random, shutil, statistics, subprocess, tempfile, time
from pathlib import Path
from lattice_efficacy_fixtures import CONTRACTS, FIXTURE_VERSION, TASKS

SCHEMA="lattice-agent-efficacy/v1"; ARMS=("baseline","explicit","briefing")
def run(command,**kwargs): return subprocess.run(command,check=True,capture_output=True,text=True,**kwargs)
def score(workspace,task,timeout=20):
 result=run([os.sys.executable,"-I","-B","-c",task['grader'],str(workspace/task['allowed_files'][0])],timeout=timeout)
 try: outcome=json.loads(result.stdout)
 except json.JSONDecodeError as error: raise ValueError('grader emitted invalid JSON') from error
 required={'assessment','correct','mistake_recurrence','target_decision_id','checks'}
 if not isinstance(outcome,dict) or set(outcome)!=required: raise ValueError('grader outcome has an invalid schema')
 if outcome['assessment'] not in ('assessed','unassessable'): raise ValueError('grader assessment must be assessed or unassessable')
 if type(outcome['correct']) is not bool or type(outcome['mistake_recurrence']) is not bool: raise ValueError('grader booleans must be strict JSON booleans')
 if outcome['target_decision_id']!=task['target_decision_id']: raise ValueError('grader target decision does not match the fixture')
 checks=outcome['checks']
 if not isinstance(checks,dict) or not checks or any(not isinstance(k,str) or not k or type(v) is not bool for k,v in checks.items()): raise ValueError('grader checks must be named strict JSON booleans')
 if outcome['assessment']=='unassessable':
  if outcome['correct'] or outcome['mistake_recurrence']: raise ValueError('unassessable outcome cannot claim correctness or observed recurrence')
  raise ValueError(f'unassessable grader outcome for target decision {task["target_decision_id"]}')
 if outcome['correct'] != all(checks.values()): raise ValueError('grader correctness does not match its named checks')
 if task['seed'] is None:
  if outcome['mistake_recurrence']: raise ValueError('control task cannot claim recurrence without a seeded analogue')
 elif task['target_decision_id'] not in checks or outcome['mistake_recurrence'] != (not checks[task['target_decision_id']]): raise ValueError('grader recurrence does not match the seeded target decision')
 return outcome
def audit_root(directory):
 directory=Path(directory).resolve()
 for candidate in (directory,*directory.parents):
  if (candidate/'.lattice-efficacy-artifact.json').is_file(): return candidate
 return directory
def failure_manifest(directory,error):
 directory=Path(directory); path=directory/'failure.json'
 payload={'schema_version':SCHEMA,'kind':'failed-operation','audit_artifacts':str(audit_root(directory)),'error_type':type(error).__name__,'error':str(error)[:4000]}
 try:
  with path.open('x') as handle: json.dump(payload,handle,indent=2)
 except FileExistsError: pass
 return path
@contextlib.contextmanager
def run_failure_guard(root):
 try: yield
 except Exception as error:
  path=Path(root)/'run-failure.json'
  payload={'schema_version':SCHEMA,'kind':'failed-run','release_gate_status':'blocked','audit_artifacts':str(root),'error_type':type(error).__name__,'error':str(error)[:4000]}
  try:
   with path.open('x') as handle: json.dump(payload,handle,indent=2)
  except FileExistsError: pass
  raise RuntimeError(f'Efficacy run failed; inspect {path}') from error
def invoke(runner,request,directory,timeout):
 rp,op=directory/'request.json',directory/'response.json'; rp.write_text(json.dumps(request)); start=time.monotonic()
 try:
  run([str(runner),str(rp),str(op)],timeout=timeout)
  response=json.loads(op.read_text())
  for f in ('model','settings','input_tokens','output_tokens','tool_calls'):
   if f not in response: raise ValueError(f'runner response missing {f}')
  for f in ('input_tokens','output_tokens'):
   if type(response[f]) is not int or response[f]<0: raise ValueError(f'invalid measured {f}')
  if not isinstance(response['tool_calls'],list): raise ValueError('tool_calls must contain captured tool records')
  response['elapsed_seconds']=time.monotonic()-start; return response
 except (OSError,subprocess.SubprocessError,ValueError,json.JSONDecodeError) as error:
  manifest=failure_manifest(directory,error)
  raise RuntimeError(f'Adapter failed; inspect {manifest}') from error
def bootstrap_interval(task_clusters,seed):
 """Bootstrap whole task clusters so repeated trials are not independent units."""
 if isinstance(task_clusters,dict): clusters=list(task_clusters.values())
 else: clusters=[[value] for value in task_clusters]
 if len(clusters)<2 or any(not cluster for cluster in clusters): return None
 rng=random.Random(seed); samples=[]
 for _ in range(2000):
  draw=rng.choices(clusters,k=len(clusters)); samples.append(statistics.mean(value for cluster in draw for value in cluster))
 samples.sort(); return [samples[49],samples[1949]]
def nested_values(n,k):
 if isinstance(n,dict): return ([n[k]] if k in n else [])+sum((nested_values(v,k) for v in n.values()),[])
 if isinstance(n,list): return sum((nested_values(v,k) for v in n),[])
 return []
def delivered_memory_objects(n):
 if isinstance(n,dict):
  if memory_object_id(n) and isinstance(n.get('content'),str): yield n
  for v in n.values(): yield from delivered_memory_objects(v)
 elif isinstance(n,list):
  for v in n: yield from delivered_memory_objects(v)
def memory_object_id(item):
 value=item.get('memory_id',item.get('id')) if isinstance(item,dict) else None
 if isinstance(value,str): return value
 if isinstance(value,dict) and isinstance(value.get('ulid'),str): return value['ulid']
 return None
def memory_identity(item):
 value=item.get('memory_id',item.get('id')) if isinstance(item,dict) else None
 if isinstance(value,dict) and isinstance(value.get('ulid'),str): encoded=value['ulid']
 elif isinstance(value,str): encoded=value
 else:return (None,None,None)
 local=encoded
 namespace=None; qualified=None
 for candidate in ('repository','organization'):
  prefix=candidate+':'
  if encoded.startswith(prefix):
   authority,separator,qualified_local=encoded[len(prefix):].partition(':')
   if not separator or not authority or not qualified_local:return ('conflict',None,None)
   namespace,qualified,local=candidate,authority,qualified_local;break
 authorities=set()
 if qualified:authorities.add(qualified)
 if isinstance(value,dict) and isinstance(value.get('workspace_id'),str) and value['workspace_id']:authorities.add(value['workspace_id'])
 if isinstance(item,dict):
  for key in ('workspace_id','origin_repository_id'):
   if isinstance(item.get(key),str) and item[key]:authorities.add(item[key])
  handle=item.get('expansion_handle')
  if isinstance(handle,str) and handle.startswith('memory:'):
   authority,separator,handle_local=handle[7:].partition('/')
   if not separator or '/' in handle_local or handle_local!=local or not authority.startswith('repo_') or len(authority)!=69 or any(c not in '0123456789abcdef' for c in authority[5:]):return ('conflict',None,local)
   authorities.add(authority)
 if len(authorities)>1:return ('conflict',None,local)
 authority=next(iter(authorities),None)
 if namespace=='organization':return (namespace,authority,local)
 return ('repository' if authority else None,authority,local)
def memory_identity_matches(item,expected):
 namespace,authority,local=memory_identity(item)
 if local!=expected['memory_id']: return False
 if namespace is None:return expected.get('repository_id') is None
 return namespace=='repository' and authority==expected.get('repository_id')
def decoded_response_payloads(raw):
 decoded=[]
 def add(value):
  if isinstance(value,(dict,list)) and value not in decoded: decoded.append(value)
 def visit(value):
  if isinstance(value,str):
   try: add(json.loads(value))
   except (TypeError,ValueError): pass
   return
  if isinstance(value,list):
   for item in value: visit(item)
   return
  if not isinstance(value,dict): return
  add(value); add(value.get('structuredContent'))
  for item in value.get('content',[]):
   if isinstance(item,dict) and item.get('type')=='text': visit(item.get('text'))
  for key in ('result','output'):
   if key in value: visit(value[key])
 visit(raw); return decoded
def response_has_error(raw):
 def visit(value):
  if isinstance(value,str):
   try: return visit(json.loads(value))
   except (TypeError,ValueError): return False
  if isinstance(value,list): return any(visit(item) for item in value)
  if not isinstance(value,dict): return False
  return value.get('isError') is True or bool(value.get('error')) or any(visit(item) for item in value.values())
 return visit(raw)
def scalar_strings(value):
 if isinstance(value,str): yield value
 elif isinstance(value,dict):
  for item in value.values(): yield from scalar_strings(item)
 elif isinstance(value,list):
  for item in value: yield from scalar_strings(item)
def response_mentions_expected(raw,expected):
 for value in scalar_strings(raw):
  if expected['content'] in value or value==expected['memory_id'] or value.endswith(':'+expected['memory_id']) or value.endswith('/'+expected['memory_id']): return True
 for parsed in decoded_response_payloads(raw):
  if any(memory_identity_matches(item,expected) for item in delivered_memory_objects(parsed)): return True
  for item in nested_values(parsed,'memory_id')+nested_values(parsed,'id'):
   if item==expected['memory_id'] or isinstance(item,str) and item.endswith(':'+expected['memory_id']): return True
  if any(expected['content'] in value or value==expected['memory_id'] or value.endswith(':'+expected['memory_id']) or value.endswith('/'+expected['memory_id']) for value in scalar_strings(parsed)): return True
 return False
def raw_delivery_receipts(raw):
 receipts=[]
 for parsed in decoded_response_payloads(raw):
  for entries in nested_values(parsed,'memory_deliveries'):
   if not isinstance(entries,list): continue
   if len(entries)!=1:return []
   entry=entries[0]
   if not isinstance(entry,dict) or entry.get('ack_required') is not True:return []
   if not all(isinstance(entry.get(key),str) and entry[key] for key in ('authority','delivery_id','payload_hash')):return []
   normalized={key:entry[key] for key in ('authority','delivery_id','payload_hash')}
   if normalized not in receipts:receipts.append(normalized)
 return receipts if len(receipts)==1 else []
def validate_delivery(response,expected,arm):
 evidence=response.get('memory_delivery_evidence'); required='recall' if arm=='explicit' else 'prepare_change'
 if not isinstance(evidence,list) or not evidence: raise ValueError('assisted trial lacks verified memory delivery evidence')
 candidates=[]
 for r in evidence:
  if not isinstance(r,dict) or r.get('tool')!=required: continue
  raw=r.get('raw_response_payload')
  if not isinstance(raw,(dict,list)) or response_has_error(raw): continue
  candidates.append((r,raw))
 if expected['expectation']=='absent':
  if not candidates: raise ValueError('assisted trial lacks a successful raw delivery response for absence assessment')
  observed=[r.get('raw_response_payload') for r in evidence if isinstance(r,dict) and r.get('tool') in ('recall','prepare_change')]
  # A failed tool can still expose obsolete text to the agent. It cannot prove
  # successful delivery, but must not disappear from negative evidence.
  for call in response.get('tool_calls',[]):
   if not isinstance(call,dict) or not call.get('lattice_detected'): continue
   item=call.get('record',{})
   if isinstance(item,dict): observed.extend(item[key] for key in ('result','output','aggregated_output','stderr') if key in item)
  if any(response_mentions_expected(raw,expected) for raw in observed): raise ValueError('assisted response delivered content required to be absent')
  return candidates[0][0]
 for r,raw in candidates:
  receipt=r.get('receipt')
  if not isinstance(receipt,dict) or set(receipt)!= {'authority','delivery_id','payload_hash'} or receipt.get('authority')!=f'repository:{expected.get("repository_id")}' or not isinstance(receipt.get('delivery_id'),str) or not receipt['delivery_id'] or not isinstance(receipt.get('payload_hash'),str) or len(receipt['payload_hash'])!=71 or not receipt['payload_hash'].startswith('sha256:') or any(c not in '0123456789abcdef' for c in receipt['payload_hash'][7:]): continue
  if raw_delivery_receipts(raw)!=[receipt]:continue
  objects=[item for parsed in decoded_response_payloads(raw) for item in delivered_memory_objects(parsed)]
  if any(memory_identity_matches(item,expected) and item['content']==expected['content'] for item in objects): return r
 raise ValueError('assisted trial did not deliver the expected memory content')
def capture_proof(response,captured,content):
 for call in response.get('tool_calls',[]):
  if not isinstance(call,dict) or str(call.get('tool','')).rsplit('.',1)[-1] != 'remember': continue
  raw=call.get('result')
  if not isinstance(raw,(dict,list)) or isinstance(raw,dict) and (raw.get('isError') or raw.get('error')): continue
  if any(memory_identity_matches(item,captured) and item['content']==content for parsed in decoded_response_payloads(raw) for item in delivered_memory_objects(parsed)):
   return raw
 raise ValueError('capture tool record did not bind durable id to exact lesson')
def supersession_proof(response,old,replacement):
 evidence=response.get('supersession_evidence')
 if not isinstance(evidence,dict) or set(evidence)!= {'proposal','apply','conflicts'}: raise ValueError('capture response lacks complete raw supersession evidence')
 authority=old.get('repository_id')
 if not isinstance(authority,str) or not authority or replacement.get('repository_id')!=authority or not old.get('memory_id') or not replacement.get('memory_id') or old['memory_id']==replacement['memory_id']: raise ValueError('supersession identities must be distinct and share repository authority')
 def matching(raw,predicate):
  if response_has_error(raw): raise ValueError('supersession evidence contains an error response')
  return [item for item in decoded_response_payloads(raw) if isinstance(item,dict) and predicate(item)]
 proposals=matching(evidence['proposal'],lambda item:item.get('decision')=='pending' and isinstance(item.get('proposal_id'),str) and bool(item['proposal_id']))
 if len(proposals)!=1: raise ValueError('supersession proposal evidence is invalid')
 proposal_id=proposals[0]['proposal_id']; applied=matching(evidence['apply'],lambda item:item.get('decision')=='applied' and item.get('proposal_id')==proposal_id)
 if len(applied)!=1: raise ValueError('supersession apply evidence is invalid')
 source=f'memory:{replacement["repository_id"]}/{replacement["memory_id"]}'; target=f'memory:{old["repository_id"]}/{old["memory_id"]}'
 conflicts=matching(evidence['conflicts'],lambda item:any(isinstance(edge,dict) and edge.get('source')==source and edge.get('target')==target and edge.get('link_type')=='supersedes' for edge in item.get('conflicts',[])))
 if len(conflicts)!=1: raise ValueError('supersession status evidence lacks the exact replacement edge')
 return evidence
def allocate_artifacts(args):
 base=Path(getattr(args,'artifacts_directory',None) or Path(tempfile.gettempdir())/'lattice-efficacy-runs')
 if base.is_symlink(): raise ValueError('artifact root must not be a symlink')
 base.mkdir(mode=0o700,parents=True,exist_ok=True); entries=list(os.scandir(base))
 if len(entries)>128: raise ValueError('artifact inventory exceeds 128 entries; inspect producer directory')
 retained=0; now=time.time()
 for e in entries:
  if not e.name.startswith('run-') or not e.is_dir(follow_symlinks=False): continue
  marker=Path(e.path)/'.lattice-efficacy-artifact.json'
  try:
   fd=os.open(marker,os.O_RDONLY|os.O_NOFOLLOW)
   with os.fdopen(fd) as h:data=json.loads(h.read(4097))
  except (OSError,ValueError): continue
  if data.get('schema')!=SCHEMA or data.get('kind')!='isolated-agent-fixture':continue
  if data.get('expires_at',float('inf'))<now:
   if not shutil.rmtree.avoids_symlink_attacks: raise ValueError('safe producer artifact deletion is unavailable')
   shutil.rmtree(e.path)
  else: retained+=1
 if retained>=8: raise ValueError('eight unexpired audit runs retained; choose a separate artifact directory or explicitly retire reviewed runs')
 root=Path(tempfile.mkdtemp(prefix='run-',dir=base)); (root/'.lattice-efficacy-artifact.json').write_text(json.dumps({'schema':SCHEMA,'kind':'isolated-agent-fixture','created_at':now,'expires_at':now+7*86400})); return root
def public_task(t):
 result={k:t[k] for k in ('id','problem','source','allowed_files','seed','target_decision_id')}
 if t.get('predecessor'): result['predecessor']=public_task(t['predecessor'])
 return result
def materialize_fixture(repo,t):
 for name,content in t['base_files'].items(): (repo/name).write_text(content)
def fixture_hash(t):
 immutable={k:t[k] for k in ('id','problem','source','allowed_files','seed','target_decision_id','base_files','reference_files','grader')}
 if t.get('predecessor'): immutable['predecessor_sha256']=fixture_hash(t['predecessor'])
 return hashlib.sha256(json.dumps(immutable,sort_keys=True,separators=(',',':')).encode()).hexdigest()
def fixture_hashes():
 result={}
 def add(t):
  if t.get('predecessor'): add(t['predecessor'])
  result[t['id']]=fixture_hash(t)
 for t in TASKS: add(t)
 return result
def validated_runtime(response,args,runtime,expected_binary_sha=None):
 current=(response['model'],json.dumps(response['settings'],sort_keys=True))
 expected=(args.model,json.dumps({'reasoning_effort':'low'},sort_keys=True))
 if current!=expected or runtime is not None and current!=runtime: raise ValueError('paired runner model/settings changed')
 if expected_binary_sha is not None and response.get('lattice_binary_sha256')!=expected_binary_sha: raise ValueError('paired Lattice binary changed')
 return current
def validate_workspace(workspace,revision,allowed):
 if run(['git','-C',str(workspace),'rev-parse','HEAD']).stdout.strip()!=revision: raise ValueError('agent changed the matched source revision')
 changed=set(filter(None,run(['git','-C',str(workspace),'diff','--name-only','-z','HEAD']).stdout.split('\0')))
 changed.update(filter(None,run(['git','-C',str(workspace),'ls-files','--others','-z']).stdout.split('\0')))
 outside=sorted(path for path in changed if path not in allowed)
 if outside: raise ValueError(f'runner changed a file outside its writable contract: {outside[:8]}')
 root=Path(workspace).resolve()
 for name in allowed:
  path=Path(workspace)/name
  if path.is_symlink() or not path.is_file() or root not in path.resolve().parents: raise ValueError('writable target is missing, non-regular, or escapes the workspace')
 return run(['git','-C',str(workspace),'diff','HEAD','--',*allowed]).stdout
def successful_explicit_recall(response):
 for call in response.get('tool_calls',[]):
  if not isinstance(call,dict) or not call.get('lattice_detected'):continue
  item=call.get('record',{}) if isinstance(call,dict) else {}
  tool=str(item.get('tool','')) if isinstance(item,dict) else ''
  if not any(tool==name or tool.endswith('.'+name) or tool.endswith('__'+name) for name in ('recall',)) : continue
  raw=item['result'] if 'result' in item else item.get('output')
  if item.get('status')=='completed' and not item.get('error') and isinstance(raw,(dict,list)) and not response_has_error(raw): return True
 return False
def validate_arm_calls(response,arm):
 detected=any(isinstance(call,dict) and call.get('lattice_detected') for call in response.get('tool_calls',[]))
 if arm in ('baseline','briefing') and detected:raise ValueError(f'{arm} agent used Lattice outside its assigned intervention')
 if arm=='briefing':
  briefing=response.get('briefing_response')
  if not isinstance(briefing,(dict,list)) or response_has_error(briefing):raise ValueError('briefing arm lacks a successful raw prepare_change preflight')
 if arm=='explicit' and not successful_explicit_recall(response):raise ValueError('explicit arm omitted a successful public recall')
def compare_arm(rows,baseline_rows,seed,eligible):
 baseline={(r['task'],r['trial']):r for r in baseline_rows}; ds=[];rs=[]; correctness_by_task={}; recurrence_by_task={}
 for r in rows:
  b=baseline[(r['task'],r['trial'])]; d=int(r['score']['correct'])-int(b['score']['correct']); recurrence=int(b['score']['mistake_recurrence'])-int(r['score']['mistake_recurrence']);ds.append(d);rs.append(recurrence);correctness_by_task.setdefault(r['task'],[]).append(d);recurrence_by_task.setdefault(r['task'],[]).append(recurrence)
 bad=[r['misleading_advice'] for r in rows]; correctness_ci=bootstrap_interval(correctness_by_task,seed); recurrence_ci=bootstrap_interval(recurrence_by_task,seed); passed=eligible and all(d>=0 for d in ds) and recurrence_ci is not None and recurrence_ci[0]>0 and not any(bad)
 blockers=[] if passed else (['diagnostic arm is not eligible for the release gate'] if not eligible else [reason for condition,reason in ((all(d>=0 for d in ds),'assisted correctness regressed'),(recurrence_ci is not None and recurrence_ci[0]>0,'recurrence reduction 95% lower bound is not above zero'),(not any(bad),'misleading advice was observed')) if not condition])
 return {'role':'primary' if eligible else 'diagnostic','paired_correctness_delta':statistics.mean(ds),'correctness_95pct_task_cluster_bootstrap_interval':correctness_ci,'mistake_recurrence_reduction':statistics.mean(rs),'recurrence_95pct_task_cluster_bootstrap_interval':recurrence_ci,'misleading_advice_count':sum(bad),'release_gate_passed':passed,'release_gate_blockers':blockers}
def execute(args):
 runner=Path(args.runner).resolve(); output=Path(args.output).resolve()
 if not runner.is_file() or not os.access(runner,os.X_OK): raise ValueError('a runnable agent adapter is required; harness tests are not efficacy evidence')
 if output.exists(): raise ValueError('refusing to overwrite an existing report')
 if args.trials<3: raise ValueError('at least three matched trials are required')
 binary=Path(os.environ.get('LATTICE_EFFICACY_BINARY',Path(__file__).resolve().parents[0].parent/'daemon/target/debug/lattice')).resolve()
 if not binary.is_file(): raise ValueError('configured Lattice efficacy binary is missing')
 frozen_files={str(runner):hashlib.sha256(runner.read_bytes()).hexdigest(),str(Path(__file__).resolve()):hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),str(Path(__file__).with_name('lattice_efficacy_fixtures.py').resolve()):hashlib.sha256(Path(__file__).with_name('lattice_efficacy_fixtures.py').read_bytes()).hexdigest(),str(binary):hashlib.sha256(binary.read_bytes()).hexdigest()}
 binary_sha=frozen_files[str(binary)]
 def checked_invoke(request,directory):
  if any(hashlib.sha256(Path(path).read_bytes()).hexdigest()!=digest for path,digest in frozen_files.items()): raise ValueError('preregistered efficacy artifact changed before invocation')
  response=invoke(runner,request,directory,args.timeout)
  if any(hashlib.sha256(Path(path).read_bytes()).hexdigest()!=digest for path,digest in frozen_files.items()): raise ValueError('preregistered efficacy artifact changed during invocation')
  return response
 def checked_agent_invoke(request,directory,workspace,revision,allowed):
  validate_workspace(workspace,revision,allowed)
  response=checked_invoke(request,directory)
  validate_workspace(workspace,revision,allowed)
  return response
 rng=random.Random(args.seed); records=[]; producer_captures=[]; runtime=None
 with contextlib.ExitStack() as cleanup:
  root=Path(allocate_artifacts(args))
  cleanup.enter_context(run_failure_guard(root))
  preregistration={'schema_version':SCHEMA,'kind':'paired-efficacy-run','status':'preregistered','audit_artifacts':str(root),'output':str(output),'fixture_version':FIXTURE_VERSION,'fixture_sha256':fixture_hashes(),'artifact_sha256':frozen_files,'model':args.model,'settings':{'reasoning_effort':'low'},'execution_environment':{'PYTHONDONTWRITEBYTECODE':'1'},'execution_permissions':{'sandbox':'workspace-write','approval_review':'auto_review'},'task_ids':[t['id'] for t in TASKS],'trials':args.trials,'randomization_seed':args.seed,'bootstrap_seed':args.seed,'primary_arm':'briefing','diagnostic_arm':'explicit','release_gate':{'minimum_trials':3,'recurrence_reduction_95pct_lower_bound_gt':0,'no_assisted_correctness_regression':True,'misleading_advice_count':0,'resampling_unit':'task_cluster'}}
  with (root/'audit-manifest.json').open('x') as handle: json.dump(preregistration,handle,indent=2)
  for trial in range(args.trials):
   for t in TASKS:
    base=root/f'{trial}-{t["id"]}'; base.mkdir(); repo=base/'repository'; repo.mkdir()
    predecessor=t.get('predecessor'); materialize_fixture(repo,predecessor or t)
    run(['git','init','-q',str(repo)]); run(['git','-C',str(repo),'add','.']); run(['git','-C',str(repo),'-c','user.name=Lattice Evaluation','-c','user.email=evaluation@invalid','commit','-qm','matched fixture']); revision=run(['git','-C',str(repo),'rev-parse','HEAD']).stdout.strip()
    excluded=[]; supersedes_capture=None
    if predecessor:
     old_producer=base/'producer-predecessor'; run(['git','-C',str(repo),'worktree','add','--detach',str(old_producer),revision]); old_common={'schema_version':SCHEMA,'workspace':str(old_producer),'task':public_task(predecessor),'task_id':predecessor['id'],'trial':trial,'prompt':predecessor['problem'],'clear_session':True,'model':args.model,'settings':{'reasoning_effort':'low'},'memory_seed':predecessor['seed'],'noise_records':0,'policy':{'write_paths':predecessor['allowed_files'],'network':'provider-only'}}
     old_request=base/'predecessor-producer-request'; old_request.mkdir(); old_response=checked_agent_invoke(dict(old_common,phase='producer'),old_request,old_producer,revision,predecessor['allowed_files']); runtime=validated_runtime(old_response,args,runtime,binary_sha)
     old_score=score(old_producer,predecessor)
     if not old_score['correct']: raise ValueError(f'predecessor producer correction failed independent grading: {t["id"]}/{trial}')
     old_capdir=base/'predecessor-capture';old_capdir.mkdir(); old_cap=checked_agent_invoke(dict(old_common,phase='capture_validated',repository=str(repo),validated_score=old_score),old_capdir,old_producer,revision,predecessor['allowed_files']); runtime=validated_runtime(old_cap,args,runtime,binary_sha); old_captured=old_cap.get('captured_memory'); old_digest=hashlib.sha256(predecessor['seed'].encode()).hexdigest()
     if not isinstance(old_captured,dict) or not isinstance(old_captured.get('memory_id'),str) or not isinstance(old_captured.get('repository_id'),str) or old_captured.get('content_sha256')!=old_digest: raise ValueError('predecessor capture did not bind durable identity and exact content')
     old_raw=capture_proof(old_cap,old_captured,predecessor['seed']); supersedes_capture={'memory_id':old_captured['memory_id'],'repository_id':old_captured['repository_id'],'content':predecessor['seed'],'content_sha256':old_digest}; excluded=[dict(supersedes_capture,expectation='absent')]; producer_captures.append({'task':predecessor['id'],'trial':trial,'role':'predecessor','memory_id':old_captured['memory_id'],'repository_id':old_captured['repository_id'],'content_sha256':old_digest,'remember_response':old_raw})
     materialize_fixture(repo,t); run(['git','-C',str(repo),'add','.']); run(['git','-C',str(repo),'-c','user.name=Lattice Evaluation','-c','user.email=evaluation@invalid','commit','-qm','superseding fixture']); revision=run(['git','-C',str(repo),'rev-parse','HEAD']).stdout.strip()
    producer=base/'producer'; run(['git','-C',str(repo),'worktree','add','--detach',str(producer),revision])
    common={'schema_version':SCHEMA,'workspace':str(producer),'task':public_task(t),'task_id':t['id'],'trial':trial,'prompt':t['problem'],'clear_session':True,'model':args.model,'settings':{'reasoning_effort':'low'},'memory_seed':t['seed'],'noise_records':10000 if t['id']=='scope-before-candidate-budget' else 0,'policy':{'write_paths':t['allowed_files'],'network':'provider-only'}}
    producer_response=checked_agent_invoke(dict(common,phase='producer'),base,producer,revision,t['allowed_files']); runtime=validated_runtime(producer_response,args,runtime,binary_sha)
    producer_score=score(producer,t)
    if not producer_score['correct']: raise ValueError(f'producer correction failed independent grading: {t["id"]}/{trial}')
    expected=None
    if t['seed'] is not None:
     capdir=base/'capture';capdir.mkdir(); cap=checked_agent_invoke(dict(common,phase='capture_validated',repository=str(repo),validated_score=producer_score,supersedes_capture=supersedes_capture),capdir,producer,revision,t['allowed_files']); runtime=validated_runtime(cap,args,runtime,binary_sha); captured=cap.get('captured_memory'); digest=hashlib.sha256(t['seed'].encode()).hexdigest()
     if not isinstance(captured,dict) or not isinstance(captured.get('memory_id'),str) or not captured['memory_id'] or not isinstance(captured.get('repository_id'),str) or not captured['repository_id'] or captured.get('content_sha256')!=digest: raise ValueError('capture response did not bind a durable memory id and repository authority to the exact lesson')
     raw_remember=capture_proof(cap,captured,t['seed']); raw_supersession=supersession_proof(cap,supersedes_capture,captured) if supersedes_capture else None
     producer_captures.append({'task':t['id'],'trial':trial,'role':'current','memory_id':captured['memory_id'],'repository_id':captured['repository_id'],'content_sha256':digest,'remember_response':raw_remember,'supersession_evidence':raw_supersession})
     expected={'memory_id':captured['memory_id'],'repository_id':captured['repository_id'],'content':t['seed'],'content_sha256':digest,'expectation':'absent' if t['id']=='stale-contradiction-trust' else 'present'}
    arms=list(ARMS);rng.shuffle(arms)
    for arm in arms:
     workspace=base/arm;run(['git','-C',str(repo),'worktree','add','-b',f'trial-{arm}',str(workspace),revision]); rd=base/f'request-{arm}';rd.mkdir()
     request=dict(common,phase='consumer',workspace=str(workspace),repository=str(repo),revision=revision,arm=arm,producer={'workspace':str(producer),'validated':True},expected_delivery=expected,excluded_deliveries=excluded,policy={'write_paths':t['allowed_files'],'memory_mode':arm,'network':'provider-only'})
     response=checked_agent_invoke(request,rd,workspace,revision,t['allowed_files']); runtime=validated_runtime(response,args,runtime,binary_sha); diff=validate_workspace(workspace,revision,t['allowed_files'])
     validate_arm_calls(response,arm)
     delivery=validate_delivery(response,expected,arm) if arm!='baseline' and expected else None; excluded_evidence=[validate_delivery(response,item,arm) for item in excluded] if arm!='baseline' else []
     records.append({'task':t['id'],'task_source':t['source'],'revision':revision,'trial':trial,'arm':arm,'order':arms.index(arm),'score':score(workspace,t),'input_tokens':response['input_tokens'],'output_tokens':response['output_tokens'],'elapsed_seconds':response['elapsed_seconds'],'patch_sha256':hashlib.sha256(diff.encode()).hexdigest(),'lattice_binary_sha256':response.get('lattice_binary_sha256'),'memory_delivery_evidence':delivery,'excluded_delivery_evidence':excluded_evidence,'briefing_response':response.get('briefing_response'),'tool_calls':response['tool_calls'],'misleading_advice':bool(delivery and expected['expectation']=='absent' and not delivery.get('absent'))})
  comparison={}
  for arm in ARMS[1:]:
   comparison[arm]=compare_arm([r for r in records if r['arm']==arm],[r for r in records if r['arm']=='baseline'],args.seed,arm=='briefing')
  report={'schema_version':SCHEMA,'protocol':{'fixture_contract':'task metadata is the sole producer/consumer/scorer plumbing','fixture_version':FIXTURE_VERSION,'evidence_scope':f'bounded {len(TASKS)}-task fixture evidence only','task_matrix':[public_task(t) for t in TASKS],'preregistration':preregistration},'model':args.model,'trials':args.trials,'seed':args.seed,'producer_captures':producer_captures,'records':records,'comparison':comparison,'audit_artifacts':str(root),'limits':['This bounded fixture evaluation does not establish broad production benefit','Runner token counts and delivery evidence require adapter audit','Recorded-call checks cannot prove absence of concealed direct database or process access','Storage/CPU benchmarks are a separate required release gate']}
  with output.open('x') as h: json.dump(report,h,indent=2)
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--runner');p.add_argument('--output');p.add_argument('--artifacts-directory');p.add_argument('--model',default='gpt-5.6-terra');p.add_argument('--trials',type=int,default=3);p.add_argument('--seed',type=int,default=20260912);p.add_argument('--timeout',type=int,default=300);p.add_argument('--list-tasks',action='store_true');a=p.parse_args()
 if a.list_tasks: print(json.dumps([public_task(t) for t in TASKS],indent=2));return
 if not a.runner or not a.output:p.error('--runner and --output are required for real paired execution')
 execute(a)
if __name__=='__main__':main()
