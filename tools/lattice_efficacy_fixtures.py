"""Versioned, public paired-agent efficacy fixtures.

The task dictionaries are the sole fixture manifest.  Graders are deliberately
kept here, outside an agent's mutable fixture workspace.  An ``unassessable``
result is a grader/infrastructure outcome, never evidence that the seeded
mistake recurred.
"""

FIXTURE_VERSION = "v2"

CONTRACTS = {
    "scope-before-candidate-budget": """# Task contract
Implement `select(records, repo, budget)` in `selector.py`.

`records` is a list of dict records, each with a string `scope`; `repo` is a
string; and `budget` is an integer other than `bool`.  Reject other argument
shapes with `ValueError`.  Return a new list containing at most `budget`
records whose `scope` equals `repo`, in their original order.  A nonpositive
budget returns `[]`.  Do not mutate `records` or its records.
""",
    "retry-durable-idempotency": """# Task contract
Implement `capture(state, key, payload)` in `capture.py`.

`state` is a dict. `key` is a nonempty string and `payload` is a
string; reject other inputs with `ValueError` without changing `state`. The
first key/payload pair appends the payload once to `state['events']` and
returns a positive integer receipt. An equal retry returns that receipt and
does not append. Reusing a key with a different payload raises `ValueError`
and changes no state. Return receipts deterministically from the event count.
""",
    "path-boundary": """# Task contract
Implement `resolve(root, relative)` in `boundary.py`.

`root` is a path-like existing directory and `relative` is a string relative
path. Reject every other input, absolute path, path that escapes through `..`,
or path that resolves through a symlink outside canonical `root`, with
`ValueError`. Return the canonical `pathlib.Path` for a normal in-root path.
""",
    "stale-contradiction-trust": """# Task contract
Implement `choose(records)` in `trust.py`.

`records` is a list of dict records with a finite `int` or `float` `score` (not `bool`) and a
string `status`; reject other shapes with `ValueError`. Only `verified`
records are usable. Return the usable record with the greatest score, keeping
the first record on a tie, or `None` when there is no usable record. Do not
mutate the list or records.
""",
    "transactional-changed-state": """# Task contract
Implement `apply(state, updates)` in `publish.py`.

`state` is a dict and `updates` is a list of two-item `(key, value)`
pairs. A key must be a nonempty string and value a nonnegative integer other
than `bool`. Reject other inputs with `ValueError`. Apply every valid update
and return the identical `state` object. If any input or update is invalid,
raise `ValueError` and leave `state` exactly unchanged.
""",
    "unfamiliar-control": """# Task contract
Implement `parse(value)` in `identifier.py`.

`value` must be exactly `kind:positive-integer`: `kind` has lowercase letters
separated only by single hyphens, and the integer is base ten, positive, and
has no leading zero. Return `(kind, integer)`. For every other value,
including nonstrings, raise `ValueError`.

This control has no seeded analogue of a prior lesson and therefore reports no
mistake recurrence; it measures ordinary task correctness only.
""",
    "branch-policy-supersession-v1": """# Task contract — revision v1
Implement `export_rows(records, tenant, limit)` in `exports.py`.

`records` is a list of dicts whose keys are exactly string `id`, string
`tenant`, boolean `archived`, and boolean `exportable`; no other keys are
allowed. `tenant` is a
string and `limit` is an integer other than `bool`; reject other input shapes
with `ValueError`. Return up to `limit` exportable rows for `tenant` in caller
order, including archived rows. A nonpositive limit returns `[]`. Do not
mutate caller input.
""",
    "branch-policy-supersession": """# Task contract — revision v2
Implement `export_rows(records, tenant, limit)` in `exports.py`.

`records` is a list of dicts whose keys are exactly string `id`, string
`tenant`, boolean `archived`, and boolean `exportable`; no other keys are
allowed. `tenant` is a
string and `limit` is an integer other than `bool`; reject other input shapes
with `ValueError`. Return up to `limit` exportable, non-archived rows for
`tenant` in caller order. A nonpositive limit returns `[]`. Do not mutate
caller input. This revision supersedes v1: archived rows are now excluded.
""",
}


def _grader(target_decision_id, body):
    """Build a self-contained grader with assessed and unassessable outcomes."""
    return f'''import importlib.util,json,sys
TARGET={target_decision_id!r}
def emit(assessment, correct, recurrence, checks):
 print(json.dumps({{"assessment":assessment,"correct":correct,"mistake_recurrence":recurrence,"target_decision_id":TARGET,"checks":checks}},sort_keys=True))
try:
 spec=importlib.util.spec_from_file_location("submission",sys.argv[1])
 if spec is None or spec.loader is None: raise ImportError("no loader")
 module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
except BaseException:
 emit("unassessable",False,False,{{"submission_loadable":False}})
else:
 try:
{body}
 except BaseException:
  emit("unassessable",False,False,{{"submission_loadable":False}})
'''


def _task(identifier, problem, source, filename, seed, base, reference, target, grader):
    return {
        "id": identifier,
        "problem": problem,
        "source": source,
        "allowed_files": [filename],
        "seed": seed,
        "target_decision_id": target,
        "base_files": {filename: base, "CONTRACT.md": CONTRACTS[identifier]},
        "reference_files": {filename: reference},
        "grader": grader,
    }


_EXPORT_VALIDATION = """if not isinstance(records,list) or not isinstance(tenant,str) or isinstance(limit,bool) or not isinstance(limit,int): raise ValueError('invalid')
 if any(not isinstance(row,dict) or set(row)!={'id','tenant','archived','exportable'} or not isinstance(row['id'],str) or not isinstance(row['tenant'],str) or not isinstance(row['archived'],bool) or not isinstance(row['exportable'],bool) for row in records): raise ValueError('invalid')
 if limit<=0: return []
 """
_EXPORT_V1 = "def export_rows(records,tenant,limit):\n " + _EXPORT_VALIDATION + "return [row for row in records if row['tenant']==tenant and row['exportable']][:limit]\n"
_EXPORT_V2 = "def export_rows(records,tenant,limit):\n " + _EXPORT_VALIDATION + "return [row for row in records if row['tenant']==tenant and row['exportable'] and not row['archived']][:limit]\n"
_EXPORT_ROWS = '''  import copy
  archived={"id":"archived","tenant":"acme","archived":True,"exportable":True}; current={"id":"current","tenant":"acme","archived":False,"exportable":True}; hidden={"id":"hidden","tenant":"acme","archived":False,"exportable":False}; other={"id":"other","tenant":"other","archived":False,"exportable":True}; rows=[archived,current,hidden,other]; frozen=copy.deepcopy(rows)
'''
_POLICY_V1 = _task(
    "branch-policy-supersession-v1", "Apply the original v1 repository-export policy before its public supersession.",
    "docs/memory-retrieval.md: synthetic branch-policy supersession fixture", "exports.py",
    "The original policy permits archived exportable rows; later v2 replaces this decision.",
    _EXPORT_V2, _EXPORT_V1, "archived_rows_permitted_v1",
    _grader("archived_rows_permitted_v1", _EXPORT_ROWS + '''  target=module.export_rows(rows,"acme",2)==[archived,current]
  checks={"archived_rows_permitted_v1":target,"other_filter":module.export_rows(rows,"acme",3)==[archived,current],"caller_order":module.export_rows([dict(current,id="first"),dict(current,id="second")],"acme",2)[0]["id"]=="first","nonpositive_limit":module.export_rows(rows,"acme",0)==[],"input_unchanged":rows==frozen,"invalid_record_rejected":False}
  try: module.export_rows([{"id":"bad","tenant":"acme","archived":False,"exportable":True,"extra":True}],"acme",1)
  except ValueError: checks["invalid_record_rejected"]=True
  emit("assessed",all(checks.values()),not target,checks)'''),
)
_POLICY_V2 = _task(
    "branch-policy-supersession", "Apply the public v2 repository-export policy after a changed contract supersedes v1.",
    "docs/memory-retrieval.md: synthetic branch-policy supersession fixture", "exports.py",
    "Revision v2 replaces the earlier export rule: exclude archived rows while preserving remaining order and caller input.",
    _EXPORT_V1, _EXPORT_V2, "carryforward_archived_policy",
    _grader("carryforward_archived_policy", _EXPORT_ROWS + '''  target=module.export_rows(rows,"acme",1)==[current]
  checks={"carryforward_archived_policy":target,"other_filter":module.export_rows(rows,"acme",3)==[current],"caller_order":module.export_rows([dict(current,id="first"),dict(current,id="second")],"acme",2)[0]["id"]=="first","nonpositive_limit":module.export_rows(rows,"acme",0)==[],"input_unchanged":rows==frozen,"invalid_record_rejected":False}
  try: module.export_rows([{"id":"bad","tenant":"acme","archived":False,"exportable":True,"extra":True}],"acme",1)
  except ValueError: checks["invalid_record_rejected"]=True
  emit("assessed",all(checks.values()),not target,checks)'''),
)


TASKS = (
    _task("scope-before-candidate-budget", "Select applicable repository memories without unrelated candidates consuming the budget.", "docs/memory-retrieval.md:37", "selector.py", "Filter candidates by repository scope before applying the candidate budget; preserve original order.", "def select(records, repo, budget):\n return records[:budget]\n", "def select(records, repo, budget):\n if isinstance(budget,bool) or not isinstance(budget,int) or not isinstance(repo,str) or not isinstance(records,list): raise ValueError('invalid')\n if any(not isinstance(r,dict) or not isinstance(r.get('scope'),str) for r in records): raise ValueError('invalid')\n if budget<=0: return []\n return [r for r in records if r['scope']==repo][:budget]\n", "scope_filter_before_budget", _grader("scope_filter_before_budget", '''  r=[{"id":"x","scope":"other"},{"id":"a","scope":"repo"},{"id":"y","scope":"other"},{"id":"b","scope":"repo"}]
  target=module.select(r,"repo",2)==[r[1],r[3]]
  checks={"scope_filter_before_budget":target,"nonpositive_budget":module.select(r,"repo",-1)==[],"preserves_order":module.select(r,"repo",1)==[r[1]],"nonmatching_scope":module.select(r,"none",2)==[],"does_not_mutate":r==[{"id":"x","scope":"other"},{"id":"a","scope":"repo"},{"id":"y","scope":"other"},{"id":"b","scope":"repo"}],"invalid_shape_rejected":False}
  try: module.select([],"repo",False)
  except ValueError: checks["invalid_shape_rejected"]=True
  emit("assessed",all(checks.values()),not target,checks)''')),
    _task("retry-durable-idempotency", "Make retries idempotent and reject conflicting reuse of a durable key.", "daemon/crates/lattice-core/src/memory/store.rs:1214-1249", "capture.py", "Bind each key to its first payload. Equal retries return its receipt; conflicting reuse raises ValueError without changing state.", "def capture(state,key,payload):\n state.setdefault('events',[]).append(payload); return len(state['events'])\n", "def capture(state,key,payload):\n if not isinstance(state,dict) or not isinstance(key,str) or not key or not isinstance(payload,str): raise ValueError('invalid')\n bindings=state.setdefault('bindings',{}); events=state.setdefault('events',[])\n if key in bindings:\n  if bindings[key][0]!=payload: raise ValueError('conflict')\n  return bindings[key][1]\n receipt=len(events)+1; events.append(payload); bindings[key]=(payload,receipt); return receipt\n", "equal_retry_is_idempotent", _grader("equal_retry_is_idempotent", '''  state={}; first=module.capture(state,"k","one"); second=module.capture(state,"k","one")
  target=first==second==1 and state.get("events")==["one"]
  before=repr(state)
  try: module.capture(state,"k","two"); conflict=False
  except ValueError: conflict=True
  invalid={}; invalid_before=repr(invalid)
  try: module.capture(invalid,"","one"); invalid_rejected=False
  except ValueError: invalid_rejected=repr(invalid)==invalid_before
  checks={"equal_retry_is_idempotent":target,"conflicting_reuse_rejected":conflict,"conflict_is_atomic":repr(state)==before,"new_key_receipt":module.capture(state,"second","two")==2,"invalid_shape_rejected":invalid_rejected}
  emit("assessed",all(checks.values()),not target,checks)''')),
    _task("path-boundary", "Resolve only paths that remain in the workspace, including through symlinks.", "daemon/crates/lattice-core/src/security/workspace.rs:48-98", "boundary.py", "Canonicalize root and candidate, reject absolute and escaping paths including a symlink escape.", "from pathlib import Path\ndef resolve(root,relative): return Path(root)/relative\n", "from pathlib import Path\ndef resolve(root,relative):\n if not isinstance(relative,str): raise ValueError('invalid')\n try: root=Path(root).resolve()\n except (TypeError,OSError): raise ValueError('invalid') from None\n if not root.is_dir(): raise ValueError('invalid')\n candidate=Path(relative)\n if candidate.is_absolute(): raise ValueError('absolute')\n try: resolved=(root/candidate).resolve()\n except OSError: raise ValueError('invalid') from None\n if resolved!=root and root not in resolved.parents: raise ValueError('escape')\n return resolved\n", "canonical_symlink_boundary", _grader("canonical_symlink_boundary", '''  import pathlib,tempfile
  with tempfile.TemporaryDirectory() as directory:
   root=pathlib.Path(directory)/"root"; root.mkdir(); inside=root/"ok"; inside.write_text("x"); outside=pathlib.Path(directory)/"outside"; outside.write_text("x"); (root/"link").symlink_to(outside)
   def reject(value):
    try: module.resolve(root,value); return False
    except ValueError: return True
    except Exception: return False
   target=reject("link")
   checks={"canonical_symlink_boundary":target,"normal_in_root":module.resolve(root,"ok")==inside.resolve(),"parent_escape":reject("../outside"),"absolute_escape":reject(str(outside)),"invalid_shape_rejected":reject(None)}
   emit("assessed",all(checks.values()),not target,checks)''')),
    _task("stale-contradiction-trust", "Choose usable guidance while excluding stale or contradicted records despite higher ranking.", "docs/memory-retrieval.md:13", "trust.py", "Filter to verified records before ranking; stale and contradicted records cannot win.", "def choose(records): return max(records,key=lambda r:r['score'])\n", "import math\ndef choose(records):\n if not isinstance(records,list) or any(not isinstance(r,dict) or isinstance(r.get('score'),bool) or not isinstance(r.get('score'),(int,float)) or not math.isfinite(r['score']) or not isinstance(r.get('status'),str) for r in records): raise ValueError('invalid')\n usable=[r for r in records if r['status']=='verified']\n return max(usable,key=lambda r:r['score']) if usable else None\n", "verified_status_before_ranking", _grader("verified_status_before_ranking", '''  import copy
  old={"id":"old","score":99,"status":"stale"}; bad={"id":"bad","score":90,"status":"contradicted"}; good={"id":"good","score":4,"status":"verified"}; original=[old,bad,good]; frozen=copy.deepcopy(original)
  target=module.choose(original)==good
  try: module.choose([{"score":True,"status":"verified"}]); invalid_rejected=False
  except ValueError: invalid_rejected=True
  checks={"verified_status_before_ranking":target,"none_without_verified":module.choose([old,bad]) is None,"higher_verified_wins":module.choose([good,dict(good,id="better",score=5)])["id"]=="better","input_unchanged":original==frozen,"invalid_shape_rejected":invalid_rejected}
  emit("assessed",all(checks.values()),not target,checks)''')),
    _task("transactional-changed-state", "Apply a batch only if every changed state is valid; failure cannot leak partial state.", "docs/reports/2026-09-12-repository-review.md:96", "publish.py", "Validate the whole batch before mutation. Invalid update raises ValueError and leaves the input mapping unchanged.", "def apply(state,updates):\n for k,v in updates: state[k]=v\n return state\n", "def apply(state,updates):\n if not isinstance(state,dict) or not isinstance(updates,list): raise ValueError('invalid')\n staged=dict(state)\n for item in updates:\n  if not isinstance(item,(tuple,list)) or len(item)!=2: raise ValueError('invalid')\n  key,value=item\n  if not isinstance(key,str) or not key or isinstance(value,bool) or not isinstance(value,int) or value<0: raise ValueError('invalid')\n  staged[key]=value\n state.clear(); state.update(staged); return state\n", "validate_batch_before_mutation", _grader("validate_batch_before_mutation", '''  state={"old":1}; returned=module.apply(state,[("a",2),("b",3)]); good=state=={"old":1,"a":2,"b":3} and returned is state; before=dict(state)
  try: module.apply(state,[("c",4),("",5)]); rejected=False
  except ValueError: rejected=True
  target=rejected and state==before
  checks={"validate_batch_before_mutation":target,"valid_batch_applied":good,"invalid_bool_rejected":False}
  try: module.apply({},[("x",True)]); checks["invalid_bool_rejected"]=False
  except ValueError: checks["invalid_bool_rejected"]=True
  emit("assessed",all(checks.values()),not target,checks)''')),
    _task("unfamiliar-control", "Implement the current compact identifier contract without prior-memory guidance.", "control fixture; no prior-memory analogue", "identifier.py", None, "def parse(value): return value\n", "import re\ndef parse(value):\n if not isinstance(value,str) or not re.fullmatch(r'[a-z]+(?:-[a-z]+)*:[1-9][0-9]*',value): raise ValueError('invalid')\n kind,number=value.split(':',1); return kind,int(number)\n", "control_no_seeded_analogue", _grader("control_no_seeded_analogue", '''  checks={"valid_identifier":module.parse("memory-link:42")==("memory-link",42)}
  for value in ("memory:0","Memory:1","memory:1:2","memory:01","memory-:2",None):
   try: module.parse(value); checks["invalid_inputs_rejected"]=False; break
   except ValueError: checks["invalid_inputs_rejected"]=True
  emit("assessed",all(checks.values()),False,checks)''')),
    dict(_POLICY_V2, predecessor=_POLICY_V1),
)
