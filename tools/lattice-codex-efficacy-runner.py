#!/usr/bin/env python3
"""Codex adapter. Starts only private, disposable Lattice daemons; never shared services."""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shlex
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

SMOKE_SCHEMA = "lattice-agent-efficacy/private-delivery-smoke/v4"

BINARY = Path(os.environ.get("LATTICE_EFFICACY_BINARY", Path(__file__).resolve().parents[1] / "daemon/target/debug/lattice")).resolve()

class RpcError(RuntimeError):
    def __init__(self, response):
        self.response=response
        super().__init__(str(response))

class Rpc:
    def __init__(self, workspace, env, log):
        self.process = subprocess.Popen([str(BINARY), "--stdio", "--workspace", workspace], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
        self.messages = queue.Queue()
        self.sequence = 0
        def read():
            for line in self.process.stdout:
                try: self.messages.put(json.loads(line))
                except ValueError: pass
            self.messages.put(None)
        self.reader_thread=threading.Thread(target=read, daemon=True)
        self.reader_thread.start()
        try:
            self.call("initialize", {"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"lattice-efficacy","version":"1"}})
        except BaseException:
            self.close()
            raise
    def call(self, method, params):
        self.sequence += 1
        self.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":self.sequence,"method":method,"params":params}) + "\n")
        self.process.stdin.flush()
        deadline=time.monotonic()+90
        while True:
            message=self.messages.get(timeout=max(.01,deadline-time.monotonic()))
            if message is None: raise RuntimeError("isolated Lattice proxy exited")
            if message.get("id") != self.sequence: continue
            if "error" in message: raise RpcError(message)
            return message["result"]
    def tool(self,name,args):
        deadline=time.monotonic()+90
        while True:
            result=self.call("tools/call",{"name":name,"arguments":args})
            if result.get("isError"): raise RpcError(result)
            structured=payload(result) or {}
            if structured.get("operation_performed") is not False: return result
            if time.monotonic()>deadline: raise RuntimeError("Lattice bootstrap did not finish: "+str(structured))
            time.sleep(.25)
    def close(self):
        self.process.terminate()
        try: self.process.wait(timeout=5)
        except subprocess.TimeoutExpired: self.process.kill(); self.process.wait()
        self.reader_thread.join(timeout=5)
        if self.reader_thread.is_alive():
            raise RuntimeError("private proxy reader did not stop after process exit")
        self.process.stdout.close()
        try: self.process.stdin.close()
        except BrokenPipeError: pass

@contextlib.contextmanager
def isolated_lattice(request, directory):
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    with socket.socket() as probe:
        probe.bind(("127.0.0.1",0)); port=probe.getsockname()[1]
    runtime_dir=directory / "runtime"; runtime_dir.mkdir(mode=0o700, exist_ok=True)
    env=dict(os.environ,XDG_STATE_HOME=str(directory / "state"),XDG_RUNTIME_DIR=str(runtime_dir),LATTICE_DAEMON_ADDR=f"127.0.0.1:{port}",LATTICE_DAEMON_EXE=str(BINARY),LATTICE_LIFECYCLE_LOG_DIR=str(directory / "lifecycle"),**private_organization_environment(directory))
    with (directory / "lattice.log").open("w") as log:
        daemon=subprocess.Popen([str(BINARY),"--daemon"],env=env,stdout=log,stderr=log)
        rpc=None
        try:
            deadline=time.monotonic()+15
            while True:
                if daemon.poll() is not None: raise RuntimeError("isolated daemon failed; inspect lattice.log")
                try:
                    with socket.create_connection(("127.0.0.1",port),timeout=.2): break
                except OSError:
                    if time.monotonic()>deadline: raise
                    time.sleep(.1)
            rpc=Rpc(request["workspace"],env,log)
            yield rpc,env
        finally:
            try:
                if rpc: rpc.close()
            finally:
                daemon.terminate()
                try: daemon.wait(timeout=5)
                except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()

def private_organization_environment(directory):
    # Explicitly override inherited environment AND home-config organization
    # destinations, without changing the HOME used for Codex authentication.
    return {"LATTICE_ORGANIZATION_ID":"lattice-efficacy-private",
            "LATTICE_SHARED_MEMORY_PATH":str((directory/"organization"/"memories.db").resolve())}

def file_sha256(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle,"sha256").hexdigest()

def seed_noise(request):
    """Bulk fixture setup in the newly created isolated repository only."""
    count=request.get("noise_records",0)
    root=Path(request["repository"])
    run_root=root.parent.parent
    marker=run_root / ".lattice-efficacy-artifact.json"
    try:
        fd=os.open(marker,os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd) as handle: ownership=json.loads(handle.read(4097))
    except (OSError,ValueError) as error:
        raise ValueError("noise mutation is restricted to a marked efficacy fixture") from error
    if ownership.get("schema")!="lattice-agent-efficacy/v1" or ownership.get("kind")!="isolated-agent-fixture":
        raise ValueError("noise mutation is restricted to a marked efficacy fixture")
    paths=list((root / ".lattice").rglob("memories.db"))
    if len(paths)!=1: raise ValueError(f"expected exactly one fixture memory authority, found {len(paths)}")
    with contextlib.closing(sqlite3.connect(paths[0])) as db, db:
        db.row_factory=sqlite3.Row
        row=dict(db.execute("SELECT * FROM memories LIMIT 1").fetchone())
        if request["task_id"]=="stale-contradiction-trust":
            db.execute("UPDATE memories SET is_stale=1, stale_reason='superseded rounding policy', verification_status='stale' WHERE id=?",(row["id"],))
        if request["task_id"]=="scope-before-candidate-budget":
            db.execute("UPDATE memories SET created_at=created_at-86400*60 WHERE id=?",(row["id"],))
        columns=list(row)
        quoted=','.join('"'+c+'"' for c in columns)
        sql=f"INSERT INTO memories ({quoted}) VALUES ({','.join('?' for _ in columns)})"
        for index in range(count):
            noise=dict(row,id=f"evaluation-noise-{index}",content=f"Unrelated inventory fact {index}: warehouse shipping pallets")
            for key in ("related_files","related_symbols","linked_files","linked_symbols"):
                if key in noise: noise[key]="[]"
            db.execute(sql,[noise[key] for key in columns])

def payload(result):
    """Return the structured MCP payload without treating an envelope as delivery."""
    if not isinstance(result,dict) or result.get("isError"):
        return None
    if isinstance(result.get("structuredContent"),(dict,list)):
        return result["structuredContent"]
    for item in result.get("content",[]):
        if item.get("type")=="text":
            try: return json.loads(item.get("text",""))
            except (TypeError,ValueError): continue
    return None

def decoded_payloads(result):
    """Decode every structured result representation an MCP client may retain."""
    decoded=[]
    def add(value):
        if isinstance(value,(dict,list)) and value not in decoded: decoded.append(value)
    def visit(value):
        if isinstance(value,str):
            try: add(json.loads(value))
            except (TypeError,ValueError): pass
            return
        if not isinstance(value,dict): return
        add(value)
        add(value.get("structuredContent"))
        for item in value.get("content",[]):
            if isinstance(item,dict) and item.get("type")=="text": visit(item.get("text"))
        # Codex JSON events have used both result and output containers.
        for key in ("result","output"):
            if key in value: visit(value[key])
    visit(result)
    return decoded

def values(node,key):
    found=[]
    if isinstance(node,dict):
        for name,value in node.items():
            if name==key: found.append(value)
            found.extend(values(value,key))
    elif isinstance(node,list):
        for value in node: found.extend(values(value,key))
    return found

def delivery_receipt(result):
    """Find one complete public receipt returned with a rendered memory response."""
    receipts=[]
    for parsed in decoded_payloads(result):
        for entries in values(parsed,"memory_deliveries"):
            if not isinstance(entries,list): continue
            if len(entries)!=1: return None
            entry=entries[0]
            if not isinstance(entry,dict) or entry.get("ack_required") is not True or not all(isinstance(entry.get(field),str) and entry[field] for field in ("authority","delivery_id","payload_hash")): return None
            receipt={field:entry[field] for field in ("authority","delivery_id","payload_hash")}
            if receipt not in receipts: receipts.append(receipt)
    return receipts[0] if len(receipts)==1 else None

def acknowledgement_response(result,count,replayed):
    parsed=payload(result)
    if not isinstance(parsed,dict) or parsed.get("acknowledged_count") != count or parsed.get("replayed") is not replayed:
        raise RuntimeError("acknowledgement response did not report the expected idempotency result")
    return parsed

def attribution_claim(result,memory_id):
    parsed=payload(result)
    attribution=parsed.get("memory_attribution") if isinstance(parsed,dict) else None
    if not isinstance(attribution,dict) or attribution.get("status") != "recorded" or not attribution.get("retrieval_id"):
        raise RuntimeError("actual recall did not persist a feedback retrieval")
    accesses=attribution.get("accesses",[])
    matches=[item for item in accesses if isinstance(item,dict) and item.get("memory_id")==memory_id]
    if len(matches)!=1 or not isinstance(matches[0].get("access_id"),str) or not matches[0]["access_id"]:
        raise RuntimeError("feedback access did not bind exactly the delivered memory")
    return {"retrieval_id":attribution["retrieval_id"],"access_ids":[matches[0]["access_id"]],"disposition":"used"}

def feedback_response(result,retrieval_id,newly_resolved):
    parsed=payload(result)
    feedback=parsed.get("memory_feedback") if isinstance(parsed,dict) else None
    if not isinstance(feedback,dict) or feedback.get("status") != "recorded" or feedback.get("retrieval_id") != retrieval_id or feedback.get("newly_resolved") is not newly_resolved:
        raise RuntimeError("public outcome did not report the expected durable feedback resolution")
    return feedback

def memory_objects(node):
    """Yield only objects that bind a memory identifier to its own content."""
    if isinstance(node,dict):
        if memory_object_id(node) and isinstance(node.get("content"),str):
            yield node
        for value in node.values(): yield from memory_objects(value)
    elif isinstance(node,list):
        for value in node: yield from memory_objects(value)

def memory_object_id(item):
    value=item.get("memory_id",item.get("id")) if isinstance(item,dict) else None
    if isinstance(value,str): return value
    if isinstance(value,dict) and isinstance(value.get("ulid"),str): return value["ulid"]
    return None

def memory_identity(item):
    """Return (namespace, authority, local id) without discarding authority."""
    value=item.get("memory_id",item.get("id")) if isinstance(item,dict) else None
    if isinstance(value,dict) and isinstance(value.get("ulid"),str): encoded=value["ulid"]
    elif isinstance(value,str): encoded=value
    else: return (None,None,None)
    local=encoded
    namespace=None; qualified_authority=None
    for candidate in ("repository","organization"):
        prefix=candidate+":"
        if encoded.startswith(prefix):
            authority,separator,qualified_local=encoded[len(prefix):].partition(":")
            if not separator or not authority or not qualified_local: return ("conflict",None,None)
            namespace,qualified_authority,local=candidate,authority,qualified_local
            break
    authorities=set()
    if qualified_authority: authorities.add(qualified_authority)
    if isinstance(value,dict) and isinstance(value.get("workspace_id"),str) and value["workspace_id"]: authorities.add(value["workspace_id"])
    if isinstance(item,dict):
        for key in ("workspace_id","origin_repository_id"):
            if isinstance(item.get(key),str) and item[key]: authorities.add(item[key])
        handle=item.get("expansion_handle")
        if isinstance(handle,str) and handle.startswith("memory:"):
            authority,separator,handle_local=handle[7:].partition("/")
            if not separator or "/" in handle_local or handle_local!=local or not authority.startswith("repo_") or len(authority)!=69 or any(c not in "0123456789abcdef" for c in authority[5:]): return ("conflict",None,local)
            authorities.add(authority)
    if len(authorities)>1: return ("conflict",None,local)
    authority=next(iter(authorities),None)
    if namespace=="organization": return (namespace,authority,local)
    return ("repository" if authority else None,authority,local)

def memory_identity_matches(item, expected):
    namespace,authority,local=memory_identity(item)
    if local != expected["memory_id"]: return False
    if namespace is None: return expected.get("repository_id") is None
    return namespace=="repository" and authority==expected.get("repository_id")

def delivery_evidence(tool,result,expected):
    if isinstance(result,dict) and (result.get("isError") or result.get("error")): return None
    memory_id=expected["memory_id"]
    content=expected["content"]
    receipt_sets=[]
    for parsed in decoded_payloads(result):
        for entries in values(parsed,"memory_deliveries"):
            if not isinstance(entries,list): continue
            if len(entries)!=1: return None
            entry=entries[0]
            if not isinstance(entry,dict) or entry.get("ack_required") is not True or not all(isinstance(entry.get(field),str) and entry[field] for field in ("authority","delivery_id","payload_hash")): return None
            receipt_sets.append({field:entry[field] for field in ("authority","delivery_id","payload_hash")})
    unique={json.dumps(item,sort_keys=True) for item in receipt_sets}
    if len(unique)!=1: return None
    receipt=json.loads(next(iter(unique)))
    if receipt["authority"]!=f'repository:{expected.get("repository_id")}' or not re.fullmatch(r"sha256:[0-9a-f]{64}",receipt["payload_hash"]): return None
    for parsed in decoded_payloads(result):
        if not any(memory_identity_matches(item,expected) and item["content"]==content for item in memory_objects(parsed)):
            continue
        return {"tool":tool,"successful_response":True,"memory_id":memory_id,"content":content,"content_sha256":hashlib.sha256(content.encode()).hexdigest(),"receipt":receipt,"response_payload":parsed,"raw_response_payload":result}
    return None

def non_delivery_evidence(tool,result,expected):
    if isinstance(result,dict) and (result.get("isError") or result.get("error")): return None
    parsed_payloads=list(decoded_payloads(result))
    if not parsed_payloads:
        return None
    # Structured and text wrappers can differ. Absence must hold over every
    # delivered container, including raw text outside the first JSON object.
    rendered=json.dumps([result, *parsed_payloads], sort_keys=True, ensure_ascii=False)
    absent=expected["memory_id"] not in rendered and expected["content"] not in rendered
    return {"tool":tool,"successful_response":True,"expected_absent_memory_id":expected["memory_id"],"expected_absent_content_sha256":expected["content_sha256"],"absent":absent,"response_payload":parsed_payloads[0],"raw_response_payload":result}

def collect_delivery_evidence(tool, result, request):
    """Bind primary and excluded lesson checks to the same actual response."""
    expected = request.get("expected_delivery")
    expectations = ([expected] if expected else []) + request.get("excluded_deliveries", [])
    evidence = []
    for item in expectations:
        check = non_delivery_evidence if item["expectation"] == "absent" else delivery_evidence
        checked = check(tool, result, item)
        if checked:
            checked["raw_response_payload"] = result
            evidence.append(checked)
    return evidence

def prepare_change_arguments(request):
    """Public agent-tool schema; JSON is necessary to inspect memory highlights."""
    return {
        "task": request["prompt"]+" "+request["task"]["allowed_files"][0],
        "entry_files": request["task"]["allowed_files"],
        "render": "json",
        "wire_format": "standard",
        "budget": "full",
    }

def capture_evidence(result, content):
    """Require the remember receipt and returned record to identify the same lesson."""
    captured=payload(result)
    if not isinstance(captured,dict) or not isinstance(captured.get("memory_id"),str) or not captured["memory_id"]:
        raise RuntimeError("remember response omitted durable memory_id")
    memory_id=captured["memory_id"]
    matching=[item for item in memory_objects(captured) if memory_identity(item)[0]=="repository" and memory_identity(item)[1] and memory_identity(item)[2]==memory_id and item["content"]==content]
    if not matching:
        raise RuntimeError("remember response did not bind its memory_id to the exact lesson")
    authorities={memory_identity(item)[1] or item.get("workspace_id") for item in matching}
    authorities.discard(None)
    top_authority=captured.get("workspace_id")
    if isinstance(top_authority,str) and top_authority: authorities.add(top_authority)
    if len(authorities)!=1:
        raise RuntimeError("remember response did not bind one repository authority to the durable lesson")
    return {"memory_id":memory_id,"repository_id":authorities.pop(),"content_sha256":hashlib.sha256(content.encode()).hexdigest()}

def supersede_captured_lesson(rpc, old, replacement, directory):
    """Use public auditable mutations and inspect the real repository relation."""
    authority = replacement.get("repository_id")
    if not authority or old.get("repository_id") != authority or old.get("memory_id") == replacement.get("memory_id"):
        raise ValueError("supersession requires two distinct lessons in the same captured repository")
    proposal_raw = rpc.tool("remember", {
        "kind": "evolution", "action": "propose", "memory_id": old["memory_id"],
        "superseded_by_memory_id": replacement["memory_id"],
        "reason": "Changed committed fixture contract; replacement producer independently graded.",
    })
    (directory / "supersession-proposal.json").write_text(json.dumps(proposal_raw))
    proposal = payload(proposal_raw)
    if not isinstance(proposal, dict) or proposal.get("decision") != "pending" or not isinstance(proposal.get("proposal_id"), str) or not proposal["proposal_id"]:
        raise RuntimeError("public remember did not persist a pending supersession proposal")
    applied_raw = rpc.tool("remember", {"kind": "evolution", "action": "apply", "proposal_id": proposal["proposal_id"], "decided_by": "lattice-efficacy"})
    (directory / "supersession-apply.json").write_text(json.dumps(applied_raw))
    applied = payload(applied_raw)
    if not isinstance(applied, dict) or applied.get("decision") != "applied" or applied.get("proposal_id") != proposal["proposal_id"]:
        raise RuntimeError("public remember did not apply the expected supersession proposal")
    conflicts_raw = rpc.tool("status", {"scope": "conflicts", "anchor": old["memory_id"], "render_mode": "full"})
    (directory / "supersession-conflicts.json").write_text(json.dumps(conflicts_raw))
    conflicts = payload(conflicts_raw)
    expected_source = f"memory:{authority}/{replacement['memory_id']}"
    expected_target = f"memory:{authority}/{old['memory_id']}"
    if not isinstance(conflicts, dict) or not any(
        isinstance(edge, dict) and edge.get("link_type") == "supersedes"
        and edge.get("source") == expected_source and edge.get("target") == expected_target
        for edge in conflicts.get("conflicts", [])
    ):
        raise RuntimeError("public status did not prove the scoped replacement-to-predecessor supersession edge")
    return {"proposal": proposal_raw, "apply": applied_raw, "conflicts": conflicts_raw}

def durable_capture_arguments(request):
    """The public `remember(kind=durable)` schema, not quick-memory defaults."""
    return {
        "kind":"durable", "content":request["memory_seed"], "scope":"repo",
        "memory_class":"constraint", "confidence":1.0,
        "confidence_reason":"Independently graded producer correction in an isolated efficacy fixture.",
        "freshness_policy":"repo_scoped", "linked_files":request["task"]["allowed_files"],
        "source_query":request["task"]["source"],
        "validity_conditions":["Current fixture contract remains unchanged."],
        "invalidation_triggers":["Fixture contract or linked file changes."],
    }

def public_delivery_tool(item):
    """Normalize Codex event labels to the public tool names the harness admits."""
    name=str(item.get("tool", ""))
    for public in ("prepare_change","recall"):
        if name == public or name.endswith("."+public) or name.endswith("__"+public):
            return public
    return name

def is_lattice_command(command,depth=0):
    if depth>2: return False
    try: tokens=shlex.split(command)
    except ValueError: tokens=command.split()
    if any(Path(token).name == "lattice" for token in tokens): return True
    if tokens and Path(tokens[0]).name in ("sh","bash","zsh"):
        for index,token in enumerate(tokens[:-1]):
            if token.startswith("-") and "c" in token and is_lattice_command(tokens[index+1],depth+1): return True
    return False

def is_lattice_invocation(item):
    """Detect conventional Lattice CLI paths and MCP namespaces in raw events."""
    server=str(item.get("server", "")).lower()
    namespace=server.replace("/",".").replace(":",".").replace("_",".").split(".")
    if "lattice" in namespace:
        return True
    command=item.get("command", "")
    if not isinstance(command,str): return False
    return is_lattice_command(command)

def run_agent(request,directory,env,briefing=None):
    files=", ".join(request["task"]["allowed_files"])
    prompt=request["prompt"]+"\nRead the repository contract and edit only "+files+". Do not edit instructions, commit, stage files, or access other worktrees. Leave no generated files. For Python verification, use -B so no bytecode cache is written. Verify your patch."
    if request.get("arm")=="baseline": prompt+="\nDo not use Lattice or any stored memory."
    if request.get("arm")=="explicit":
        prompt+="\nBefore editing, call the Lattice MCP recall tool with query "+repr(request["task"]["id"])+", focus_files "+repr(request["task"]["allowed_files"])+", and render_mode 'full'. Treat stored claims as advisory and check them against current code."
    if briefing is not None: prompt+="\nDo not call Lattice yourself; this arm is limited to the supplied preflight.\nTask-start Lattice briefing (advisory; verify current evidence):\n"+json.dumps(briefing)
    command=["codex","exec","--ignore-user-config","--ephemeral","--skip-git-repo-check","--approve-for-me","--model",request["model"],"-c",'model_reasoning_effort="low"',"--json","-C",request["workspace"],prompt]
    if request.get("arm")=="explicit":
        config=["mcp_servers.lattice.command="+json.dumps(str(BINARY)),"mcp_servers.lattice.args="+json.dumps(["--stdio","--workspace",request["workspace"]]),'mcp_servers.lattice.env={LATTICE_DAEMON_ADDR='+json.dumps(env["LATTICE_DAEMON_ADDR"])+',XDG_STATE_HOME='+json.dumps(env["XDG_STATE_HOME"])+',XDG_RUNTIME_DIR='+json.dumps(env["XDG_RUNTIME_DIR"])+',LATTICE_LIFECYCLE_LOG_DIR='+json.dumps(env["LATTICE_LIFECYCLE_LOG_DIR"])+"}"]
        for setting in config: command[-1:-1]=["-c",setting]
    agent_env=dict(env,PYTHONDONTWRITEBYTECODE="1")
    result=subprocess.run(command,env=agent_env,stdin=subprocess.DEVNULL,capture_output=True,text=True,timeout=240)
    (directory/"codex.jsonl").write_text(result.stdout)
    (directory/"codex.stderr").write_text(result.stderr)
    if result.returncode: raise RuntimeError(f"Codex exited {result.returncode}; inspect adapter artifacts")
    events=[json.loads(line) for line in result.stdout.splitlines() if line.startswith('{')]
    usage=next((e["usage"] for e in reversed(events) if e.get("type")=="turn.completed"),None)
    if usage is None: raise RuntimeError("Codex omitted measured token usage")
    calls=[]; evidence=[]; expected=request.get("expected_delivery")
    for event in events:
        item=event.get("item",{})
        if event.get("type")=="item.completed" and item.get("type") in ("command_execution","mcp_tool_call"):
            is_lattice=is_lattice_invocation(item)
            calls.append({"tool":"lattice" if is_lattice else item["type"],"lattice_detected":is_lattice,"record":item})
            if is_lattice and item.get("status")=="completed" and not item.get("error") and item.get("result") is not None and expected:
                evidence.extend(collect_delivery_evidence(public_delivery_tool(item), item["result"], request))
    if briefing is not None and expected:
        evidence.extend(collect_delivery_evidence("prepare_change", briefing, request))
    return {"model":request["model"],"settings":request["settings"],"input_tokens":usage["input_tokens"],"output_tokens":usage["output_tokens"],"tool_calls":calls,"memory_delivery_evidence":evidence or None,"briefing_response":briefing}

def markdown_delivery_evidence(result, expected):
    """Verify actual default Markdown content and its explicit public receipt."""
    if result.get("isError"):
        raise RuntimeError("default Markdown delivery returned an error")
    text="\n".join(item.get("text","") for item in result.get("content",[]) if item.get("type")=="text")
    section=text.split("### Relevant memory\n",1)[-1].split("\n### ",1)[0]
    qualified=f'repository:{expected.get("repository_id")}:{expected["memory_id"]}' if expected.get("repository_id") else None
    identities=re.findall(r"^- Memory: `([^`]+)`(?: .*)?$",section,re.MULTILINE)
    identity_present=len(identities)==1 and identities[0] in (expected["memory_id"],qualified)
    if "### Relevant memory\n" not in text or expected["content"] not in section or not identity_present:
        raise RuntimeError("default Markdown omitted the complete captured lesson or its identity")
    try:
        receipts=json.loads(text.split("### Memory delivery receipts\n```json\n",1)[1].split("\n```",1)[0])
    except (IndexError,ValueError) as error:
        raise RuntimeError("default Markdown omitted a valid delivery receipt") from error
    receipt=delivery_receipt({"structuredContent":{"memory_deliveries":receipts}})
    if not receipt: raise RuntimeError("default Markdown receipt omitted its binding")
    if len(receipts)!=1 or (expected.get("repository_id") and receipt["authority"]!=f'repository:{expected["repository_id"]}'):
        raise RuntimeError("default Markdown receipt did not bind one captured repository authority")
    return {"response":result,"receipt":receipt,"memory_id":expected["memory_id"],"content_sha256":expected["content_sha256"]}

def workspace_ready_payload(result, source_file):
    """A queued/bootstrap reply or query echo is not an indexed source result."""
    def matched(node):
        if isinstance(node,dict):
            if node.get("file")==source_file and any(isinstance(node.get(key),str) and node[key] for key in ("symbol","name","label")):
                return True
            return any(matched(value) for value in node.values())
        if isinstance(node,list): return any(matched(value) for value in node)
        return False
    return matched(payload(result))

def wait_workspace_ready(rpc, source_file):
    deadline=time.monotonic()+90
    while True:
        result=rpc.tool("context",{"query":source_file,"render":"json","wire_format":"standard","budget":"full"})
        if workspace_ready_payload(result,source_file): return result
        if time.monotonic()>=deadline:
            raise RuntimeError("private workspace did not publish source evidence before delivery smoke")
        time.sleep(.1)

def context_memory_expansion_arguments(briefing, expected):
    handles={value for parsed in decoded_payloads(briefing) for value in values(parsed,"context_handle") if isinstance(value,str) and value}
    if len(handles)!=1:
        raise RuntimeError("memory expansion requires one actual context handle")
    return {"mode":"expand","handle":next(iter(handles)),
            "focus":f'memory:repository:{expected["repository_id"]}:{expected["memory_id"]}',
            "max_tokens":4000,"render":"json"}

def rejected_memory_expansion(rpc, arguments, obsolete_content):
    try:
        rpc.tool("context",arguments)
    except RpcError as error:
        raw=error.response
    else:
        raise RuntimeError("an obsolete memory context handle remained expandable")
    if obsolete_content in json.dumps(raw,ensure_ascii=False):
        raise RuntimeError("rejected expansion leaked the obsolete memory content")
    if any(entries for parsed in decoded_payloads(raw) for entries in values(parsed,"memory_deliveries")):
        raise RuntimeError("rejected expansion returned a memory delivery receipt")
    return raw

def empty_recall_evidence(raw):
    parsed=payload(raw)
    if (not isinstance(parsed,dict) or parsed.get("count") != 0
            or parsed.get("memories") != [] or parsed.get("memory_attribution") is not None
            or parsed.get("memory_deliveries") not in (None,[])):
        raise RuntimeError("empty recall did not return a clean result without attribution or delivery records")
    return parsed

def private_delivery_smoke(output):
    """Exercise only private MCP daemons; never invokes Codex or a shared service."""
    output=Path(output).resolve()
    if output.exists(): raise ValueError("refusing to overwrite an existing smoke report")
    root=Path(tempfile.mkdtemp(prefix="lattice-private-delivery-smoke-"))
    report={"schema_version":SMOKE_SCHEMA,"binary":str(BINARY),"audit_artifacts":str(root),"success":False,
            "expiry_lifecycle":{"status":"not_run_no_public_controlled_clock_or_maintenance_verb","reason":"This smoke refuses to treat synthetic JSON or direct SQL timestamps as proof of acknowledgement renewal, purge, or restore non-resurrection. Those require the existing controlled-clock Rust/MCP integration fixture."}}
    seed="Filter candidates by repository scope before applying the candidate budget; preserve original order."
    task={"id":"scope-before-candidate-budget","problem":"Select scoped records.","source":"private smoke fixture","allowed_files":["selector.py"],"seed":seed}
    request={"workspace":"","task":task,"memory_seed":seed}
    try:
        repo=root/"repository"; repo.mkdir()
        (repo/"selector.py").write_text("def select(records, repo, budget):\n    return records[:budget]\n")
        (repo/"CONTRACT.md").write_text("# Task contract\nSelect only records whose scope equals repo before applying budget.\n")
        for command in (("git","init","-q",str(repo)),("git","-C",str(repo),"add","."),("git","-C",str(repo),"-c","user.name=Lattice Smoke","-c","user.email=smoke@invalid","commit","-qm","fixture")):
            subprocess.run(command,check=True,capture_output=True,text=True,timeout=30)
        producer=root/"producer"; consumer=root/"consumer"
        subprocess.run(["git","-C",str(repo),"worktree","add","--detach",str(producer),"HEAD"],check=True,capture_output=True,text=True,timeout=30)
        subprocess.run(["git","-C",str(repo),"worktree","add","-b","consumer",str(consumer),"HEAD"],check=True,capture_output=True,text=True,timeout=30)
        request["workspace"]=str(producer)
        with isolated_lattice(request,root/"producer-rpc") as (rpc,_):
            tools=rpc.call("tools/list",{})
            (root/"tools-list.json").write_text(json.dumps(tools))
            names={item.get("name") for item in tools.get("tools",[])}
            if not {"remember","recall","prepare_change"}.issubset(names): raise RuntimeError("public MCP tool list is missing delivery smoke tools")
            empty_raw=rpc.tool("recall",{"query":"private-empty-recall","mode":"search","focus_files":["selector.py"],"render_mode":"full"})
            (root/"empty-recall.json").write_text(json.dumps(empty_raw))
            report["empty_recall"]=empty_recall_evidence(empty_raw)
            with contextlib.closing(sqlite3.connect(repo/".lattice"/"memories.db")) as connection:
                counts={table:connection.execute("SELECT COUNT(*) FROM "+table).fetchone()[0] for table in ("memory_attribution_retrievals","memory_accesses","memory_deliveries")}
            if any(counts.values()): raise RuntimeError("empty recall created attribution or delivery records")
            report["empty_recall_record_counts"]=counts
            smoke_capture=durable_capture_arguments(request)
            smoke_capture["confidence_reason"]="Private delivery workflow fixture; no independently graded agent or efficacy claim."
            captured_raw=rpc.tool("remember",smoke_capture)
            (root/"producer-remember.json").write_text(json.dumps(captured_raw))
            captured=capture_evidence(captured_raw,seed)
        expected={"memory_id":captured["memory_id"],"repository_id":captured["repository_id"],"content":seed,"content_sha256":captured["content_sha256"],"expectation":"present"}
        request["workspace"]=str(consumer)
        database=repo/".lattice"/"memories.db"
        with isolated_lattice(request,root/"consumer-rpc") as (rpc,_):
            ready=wait_workspace_ready(rpc,"selector.py")
            (root/"consumer-ready-context.json").write_text(json.dumps(ready))
            default_briefing=rpc.tool("prepare_change",{"task":task["problem"]+" selector.py","entry_files":["selector.py"]})
            (root/"consumer-default-prepare-change.json").write_text(json.dumps(default_briefing))
            default_evidence=markdown_delivery_evidence(default_briefing,expected)
            report["default_markdown_delivery"]=default_evidence
            briefing=rpc.tool("prepare_change",prepare_change_arguments({"prompt":task["problem"],"task":task}))
            recalled=rpc.tool("recall",{"query":task["id"],"mode":"search","focus_files":["selector.py"],"render_mode":"full"})
            (root/"consumer-prepare-change.json").write_text(json.dumps(briefing))
            (root/"consumer-recall.json").write_text(json.dumps(recalled))
            briefing_evidence=delivery_evidence("prepare_change",briefing,expected)
            recall_evidence=delivery_evidence("recall",recalled,expected)
            if not briefing_evidence or not recall_evidence: raise RuntimeError("consumer delivery did not bind captured id and exact content")
            expansion_args=context_memory_expansion_arguments(briefing,expected)
            expanded=rpc.tool("context",expansion_args)
            (root/"consumer-memory-expansion.json").write_text(json.dumps(expanded))
            expanded_evidence=delivery_evidence("context",expanded,expected)
            if not expanded_evidence:
                raise RuntimeError("actual memory expansion lacked complete content and its scoped receipt")
            report["context_expansion"]={"arguments":expansion_args,"live_delivery":expanded_evidence}
            claim=attribution_claim(recalled,captured["memory_id"])
            with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                access_before=connection.execute("SELECT memory_id,was_used FROM memory_accesses WHERE access_id=?",(claim["access_ids"][0],)).fetchone()
            if access_before != (captured["memory_id"],None): raise RuntimeError("actual recall did not create a pending canonical memory access")
            receipt=default_evidence["receipt"]
            if not receipt: raise RuntimeError("consumer delivery omitted a complete public acknowledgement receipt")
            with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                before_ack=connection.execute("SELECT last_recalled_at FROM memories WHERE id=?",(captured["memory_id"],)).fetchone()[0]
            if before_ack is not None: raise RuntimeError("attempted delivery renewed retention before acknowledgement")
            forged=dict(receipt,payload_hash="sha256:"+"0"*64)
            try:
                rpc.tool("recall",{"mode":"acknowledge_delivery",**forged})
            except RpcError as error:
                forged_response=error.response
            else: raise RuntimeError("forged acknowledgement was accepted")
            with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                after_forged=connection.execute("SELECT last_recalled_at FROM memories WHERE id=?",(captured["memory_id"],)).fetchone()[0]
            if after_forged is not None: raise RuntimeError("forged acknowledgement renewed retention")
            exact_raw=rpc.tool("recall",{"mode":"acknowledge_delivery",**receipt})
            exact_ack=acknowledgement_response(exact_raw,1,False)
            replay_raw=rpc.tool("recall",{"mode":"acknowledge_delivery",**receipt})
            replay_ack=acknowledgement_response(replay_raw,0,True)
            with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                after_ack=connection.execute("SELECT last_recalled_at FROM memories WHERE id=?",(captured["memory_id"],)).fetchone()[0]
            if after_ack is None: raise RuntimeError("exact acknowledgement did not renew retention")
            after_default_ack=after_ack
            expansion_receipt=expanded_evidence["receipt"]
            expansion_replayed=expansion_receipt==receipt
            expansion_ack=rpc.tool("recall",{"mode":"acknowledge_delivery",**expansion_receipt})
            acknowledgement_response(expansion_ack,0 if expansion_replayed else 1,expansion_replayed)
            report["context_expansion"]["acknowledgement"]=expansion_ack
            with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                after_ack=connection.execute("SELECT last_recalled_at FROM memories WHERE id=?",(captured["memory_id"],)).fetchone()[0]
            report["context_expansion"]["last_recalled_at_after_ack"]=after_ack
            report.update(captured_memory=captured,delivery={"prepare_change":briefing_evidence,"recall":recall_evidence},acknowledgement={"receipt":receipt,"last_recalled_at":{"before":before_ack,"after_forged":after_forged,"after_exact":after_default_ack},"forged_response":forged_response,"exact_response":exact_raw,"exact_acknowledgement":exact_ack,"replay_response":replay_raw,"replay_acknowledgement":replay_ack})
        # A fresh process/session must resolve the persisted retrieval without
        # importing a graph node or relying on an in-memory event cache.
        with isolated_lattice(request,root/"feedback-rpc") as (rpc,_):
            wait_workspace_ready(rpc,"selector.py")
            outcome={"kind":"outcome","task":task["problem"],"status":"success","summary":"Applied repository scope before the candidate budget.","files":["selector.py"],"memory_attribution":claim,"render_mode":"full"}
            resolved_raw=rpc.tool("remember",outcome)
            (root/"consumer-outcome.json").write_text(json.dumps(resolved_raw))
            resolved=feedback_response(resolved_raw,claim["retrieval_id"],True)
            replayed_raw=rpc.tool("remember",outcome)
            (root/"consumer-outcome-replay.json").write_text(json.dumps(replayed_raw))
            replayed=feedback_response(replayed_raw,claim["retrieval_id"],False)
            conflict_raw=rpc.tool("remember",dict(outcome,memory_attribution=dict(claim,disposition="not_used")))
            (root/"consumer-outcome-conflict.json").write_text(json.dumps(conflict_raw))
            conflict=payload(conflict_raw)
            if not isinstance(conflict,dict) or conflict.get("memory_feedback",{}).get("status") != "error":
                raise RuntimeError("conflicting public feedback was not rejected")
            with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                access_after=connection.execute("SELECT memory_id,was_used FROM memory_accesses WHERE access_id=?",(claim["access_ids"][0],)).fetchone()
                after_feedback=connection.execute("SELECT last_recalled_at FROM memories WHERE id=?",(captured["memory_id"],)).fetchone()[0]
            if access_after != (captured["memory_id"],1): raise RuntimeError("feedback did not preserve exactly one applied access")
            if after_feedback != after_ack: raise RuntimeError("feedback renewed retention without a delivery acknowledgement")
            report["feedback"]={"claim":claim,"pending_access":access_before,"resolved_access":access_after,"after_restart":True,"resolution":resolved,"replay":replayed,"conflict":conflict["memory_feedback"],"last_recalled_at_unchanged":True}
        # Staleness has no capture-time public mutation verb. This is a marked
        # disposable fixture-only state transition, followed by fresh reads.
        with contextlib.closing(sqlite3.connect(database)) as connection, connection:
            changed=connection.execute("UPDATE memories SET is_stale=1, stale_reason='private smoke supersession', verification_status='stale' WHERE id=?",(captured["memory_id"],)).rowcount
            if changed != 1: raise RuntimeError("private fixture did not locate captured memory to mark stale")
        stale_expected=dict(expected,expectation="absent")
        with isolated_lattice(request,root/"stale-rpc") as (rpc,_):
            wait_workspace_ready(rpc,"selector.py")
            stale_briefing=rpc.tool("prepare_change",prepare_change_arguments({"prompt":task["problem"],"task":task}))
            stale_recall=rpc.tool("recall",{"query":task["id"],"mode":"search","focus_files":["selector.py"],"render_mode":"full"})
            (root/"stale-prepare-change.json").write_text(json.dumps(stale_briefing))
            (root/"stale-recall.json").write_text(json.dumps(stale_recall))
            stale_briefing_evidence=non_delivery_evidence("prepare_change",stale_briefing,stale_expected)
            stale_recall_evidence=non_delivery_evidence("recall",stale_recall,stale_expected)
            if not stale_briefing_evidence or not stale_briefing_evidence["absent"] or not stale_recall_evidence or not stale_recall_evidence["absent"]: raise RuntimeError("stale memory was delivered by a fresh consumer response")
        report["delivery"].update(stale_prepare_change=stale_briefing_evidence,stale_recall=stale_recall_evidence)
        # Two fresh claims exercise actual public revision separately from the
        # preceding fixture-only stale transition. This is workflow evidence,
        # not an independently graded producer or a causal efficacy result.
        old_content="Revision v1 selector policy includes archived records within repository scope."
        new_content="Revision v2 selector policy excludes archived records within repository scope."
        with isolated_lattice(request,root/"supersession-rpc") as (rpc,_):
            wait_workspace_ready(rpc,"selector.py")
            claims=[]
            for label,content in (("old",old_content),("replacement",new_content)):
                arguments=durable_capture_arguments(dict(request,memory_seed=content))
                arguments["confidence_reason"]="Private public-workflow smoke fixture; no agent efficacy claim."
                raw=rpc.tool("remember",arguments)
                (root/f"supersession-capture-{label}.json").write_text(json.dumps(raw))
                claims.append(dict(capture_evidence(raw,content),content=content))
                if label=="old":
                    old_expected=dict(claims[0],expectation="present")
                    old_briefing=rpc.tool("prepare_change",prepare_change_arguments({"prompt":old_content,"task":task}))
                    old_expansion_args=context_memory_expansion_arguments(old_briefing,old_expected)
                    old_expansion=rpc.tool("context",old_expansion_args)
                    old_expansion_proof=delivery_evidence("context",old_expansion,old_expected)
                    if not old_expansion_proof: raise RuntimeError("predecessor was not actually expandable before supersession")
            relation=supersede_captured_lesson(rpc,claims[0],claims[1],root/"supersession-rpc")
            report["supersession"]={"old":claims[0],"replacement":claims[1],"relation":relation}
            rejected=rejected_memory_expansion(rpc,old_expansion_args,old_content)
            (root/"superseded-memory-expansion.json").write_text(json.dumps(rejected))
            report["context_expansion"].update(before_supersession=old_expansion_proof,after_supersession=rejected)
        # A fresh daemon must observe the committed replacement and exclude
        # the predecessor, and an equal apply retry remains idempotent.
        with isolated_lattice(request,root/"supersession-restart-rpc") as (rpc,env):
            wait_workspace_ready(rpc,"selector.py")
            report["context_expansion"]["after_restart"]=rejected_memory_expansion(rpc,old_expansion_args,old_content)
            replay=rpc.tool("remember",{"kind":"evolution","action":"apply","proposal_id":payload(relation["proposal"])["proposal_id"],"decided_by":"lattice-efficacy"})
            if payload(replay).get("decision") != "applied":
                raise RuntimeError("supersession apply retry lost its committed decision after restart")
            raw=rpc.tool("recall",{"query":"Revision v2 selector policy","mode":"search","focus_files":["selector.py"],"render_mode":"full"})
            expectations={"expected_delivery":dict(claims[1],expectation="present"),"excluded_deliveries":[dict(claims[0],expectation="absent")]}
            proof=collect_delivery_evidence("recall",raw,expectations)
            if len(proof)!=2 or proof[1].get("absent") is not True:
                raise RuntimeError("restarted public recall did not deliver replacement while excluding predecessor")
            cli=subprocess.run([str(BINARY),"remember","--kind","evolution","--action","apply","--proposal-id",payload(relation["proposal"])["proposal_id"],"--workspace",request["workspace"],"--json","--timeout","30"],env=env,check=True,capture_output=True,text=True,timeout=40)
            cli_response=json.loads(cli.stdout)
            cli_payload=payload(cli_response) or cli_response
            if not isinstance(cli_payload,dict) or cli_payload.get("decision") != "applied":
                raise RuntimeError("private CLI evolution retry did not return the committed decision")
            report["supersession"].update(restart_replay=replay,delivery=proof,cli_replay=cli_response)
        report["success"]=True
    except BaseException as error:
        report["error"]={"type":type(error).__name__,"message":str(error)}
    finally:
        report["binary_sha256"]=file_sha256(BINARY) if BINARY.is_file() else None
        with output.open("x") as handle: json.dump(report,handle,indent=2)
    if not report["success"]: raise RuntimeError(f"private delivery smoke failed; inspect {output} and {root}")

def main():
    if len(sys.argv)==3 and sys.argv[1]=="--private-delivery-smoke":
        private_delivery_smoke(sys.argv[2]); return
    request_path,response_path=map(Path,sys.argv[1:])
    request=json.loads(request_path.read_text()); directory=request_path.parent
    if request["phase"]=="producer" or request.get("arm")=="baseline":
        response=run_agent(request,directory,dict(os.environ))
    else:
        with isolated_lattice(request,directory) as (rpc,env):
            if request["phase"]=="capture_validated":
                if not request["validated_score"]["correct"]: raise ValueError("unvalidated producer")
                result=rpc.tool("remember",durable_capture_arguments(request))
                captured=capture_evidence(result,request["memory_seed"])
                (directory/"capture.json").write_text(json.dumps(result))
                response={"model":request["model"],"settings":request["settings"],"input_tokens":0,"output_tokens":0,"tool_calls":[{"tool":"lattice.remember","result":result}],"captured_memory":captured}
                if request.get("supersedes_capture"):
                    response["supersession_evidence"] = supersede_captured_lesson(rpc, request["supersedes_capture"], captured, directory)
            else:
                wait_workspace_ready(rpc,request["task"]["allowed_files"][0])
                briefing=rpc.tool("prepare_change",prepare_change_arguments(request)) if request["arm"]=="briefing" else None
                response=run_agent(request,directory,env,briefing)
        if request["phase"]=="capture_validated": seed_noise(request)
    response["lattice_binary_sha256"] = file_sha256(BINARY)
    response_path.write_text(json.dumps(response))

if __name__=="__main__": main()
