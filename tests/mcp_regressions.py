"""Exercise the stdio MCP boundary against an isolated Cargo/Git fixture.

Called by cargo test. The fake analyzer and Cargo test lifecycle wiring;
real compiler semantics and Clippy argument parsing have separate Rust tests.
"""
import json
import os
from pathlib import Path
import queue
import shutil
import sqlite3
import subprocess
import tempfile
import threading
import time

REPO = Path(__file__).resolve().parents[1]
TEMP = tempfile.TemporaryDirectory(prefix='crusty-mcp-regression-')
OUT = Path(TEMP.name)
BIN = Path(os.environ.get('CRUSTY_TEST_BINARY', REPO / 'target/debug/rust-repo-intelligence'))
REAL_CARGO = shutil.which('cargo')


class MCP:
    def __init__(self, root, env):
        self.messages = queue.Queue()
        self.counter = 0
        self.process = subprocess.Popen([str(BIN), '--workspace', str(root)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(OUT / f'server-{time.time_ns()}.log', 'w'), env=env)
        def read():
            for line in self.process.stdout:
                self.messages.put(json.loads(line))
        threading.Thread(target=read, daemon=True).start()
        self.call('initialize', {'protocolVersion':'2025-11-25','capabilities':{},'clientInfo':{'name':'audit-probe','version':'1'}})
        self.process.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'}).encode()+b'\n')
        self.process.stdin.flush()

    def call(self, method, params):
        self.counter += 1
        request = {'jsonrpc':'2.0','id':self.counter,'method':method,'params':params}
        self.process.stdin.write(json.dumps(request).encode()+b'\n')
        self.process.stdin.flush()
        while True:
            response = self.messages.get(timeout=40)
            if response.get('id') != self.counter:
                continue
            if 'error' in response:
                raise RuntimeError(response['error'])
            return response['result']

    def tool(self, name, args):
        raw = self.call('tools/call', {'name':name,'arguments':args})
        if raw.get('isError'):
            raise RuntimeError(raw)
        parsed = raw.get('structuredContent')
        if parsed is None:
            parsed = json.loads(next(x['text'] for x in raw['content'] if x['type']=='text'))
        return parsed, raw

    def settled(self, task_id):
        for _ in range(400):
            task = self.tool('task.get', {'id':task_id})[0]['task']
            if task['status'] in ('completed','failed','cancelled'):
                return task
            time.sleep(.025)
        raise RuntimeError('task did not settle')

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def git(root, *args):
    subprocess.run(['git', *args], cwd=root, check=True, capture_output=True)


root = OUT / 'fixture'
root.mkdir()
(root / 'src').mkdir()
(root / 'Cargo.toml').write_text('[package]\nname="audit_fixture"\nversion="0.1.0"\nedition="2024"\n')
original = 'pub fn audit_target() -> u32 { 1 }\npub fn caller() -> u32 { audit_target() }\n'
(root / 'src/lib.rs').write_text(original)
(root / '.gitignore').write_text('.rust-repo-intelligence/\ntarget/\n')
git(root, 'init', '-q')
git(root, 'config', 'user.name', 'Audit Fixture')
git(root, 'config', 'user.email', 'audit@example.invalid')
git(root, 'add', '.')
git(root, 'commit', '-qm', 'fixture')

fake_ra = OUT / 'fake-rust-analyzer'
fake_ra.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
log = Path(os.environ['AUDIT_PROBE_DIR']) / 'analyzer.jsonl'
def record(data):
    with log.open('a') as f: f.write(json.dumps(data)+'\\n')
record({'event':'started','args':sys.argv[1:]})
if '--version' in sys.argv:
    print('rust-analyzer audit-probe'); sys.exit(0)
while True:
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\\r\\n', b'\\n'): break
        if line.startswith(b'Content-Length:'): size = int(line.split(b':')[1])
    msg = json.loads(sys.stdin.buffer.read(size))
    record({'event':'request','method':msg.get('method')})
    def send(message):
        data = json.dumps(message).encode()
        sys.stdout.buffer.write(b'Content-Length: '+str(len(data)).encode()+b'\\r\\n\\r\\n'+data)
        sys.stdout.buffer.flush()
    if msg.get('method') == 'initialized':
        send({'jsonrpc':'2.0','method':'experimental/serverStatus','params':{'health':'ok','quiescent':True}})
    if msg.get('method') == 'textDocument/didOpen':
        document = msg['params']['textDocument']
        span = {'start':{'line':0,'character':0},'end':{'line':0,'character':3}}
        send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':{'uri':document['uri'],'version':document['version'],
            'diagnostics':[{'range':span,'severity':2,'source':'rust-analyzer','code':'fake','message':'fake diagnostic'}]}})
    if 'id' in msg:
        result = {'capabilities':{}} if msg.get('method') == 'initialize' else []
        if msg.get('method') == 'textDocument/references':
            result = [{'uri':msg['params']['textDocument']['uri'],'range':{'start':{'line':1,'character':0},'end':{'line':1,'character':30}}}]
        data = json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}).encode()
        sys.stdout.buffer.write(b'Content-Length: '+str(len(data)).encode()+b'\\r\\n\\r\\n'+data)
        sys.stdout.buffer.flush()
''')
fake_ra.chmod(0o755)
fake_bin = OUT / 'bin'
fake_bin.mkdir()
fake_cargo = fake_bin / 'cargo'
fake_cargo.write_text('''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
out = Path(os.environ['AUDIT_PROBE_DIR'])
if sys.argv[1] == 'metadata':
    os.execv(os.environ['AUDIT_REAL_CARGO'], [os.environ['AUDIT_REAL_CARGO'], *sys.argv[1:]])
if sys.argv[1] == '--version':
    print('cargo fixture-version'); sys.exit(0)
with (out/'cargo.jsonl').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')
if sys.argv[1] == 'fmt':
    (out/'check-started').touch()
    deadline = time.monotonic()+20
    while not (out/'release-check').exists() and time.monotonic()<deadline: time.sleep(.01)
sys.exit(0)
''')
fake_cargo.chmod(0o755)
env = os.environ.copy()
env.update(AUDIT_PROBE_DIR=str(OUT), AUDIT_REAL_CARGO=REAL_CARGO,
    RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART='1',
    RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH=str(fake_ra),
    RUST_REPO_INTELLIGENCE_ENABLE_WATCHER='0', CRUSTY_AUTO_REFRESH='0',
    RUST_REPO_INTELLIGENCE_RUST_ANALYZER_WARM_START='0', PATH=str(fake_bin)+os.pathsep+env['PATH'])
results = {'artifact_directory':str(OUT), 'fixture':str(root), 'build_profile':'debug',
    'limits':'Payload bytes are measured; bytes/4 is only a token proxy. Analyzer and Cargo are instrumented fakes to test wiring/cancellation, not semantic correctness or real check performance.'}
clients = []
try:
    client = MCP(root, env); clients.append(client)
    tools = client.call('tools/list', {})
    results['tool_catalog'] = {'tools':len(tools['tools']), 'compact_json_bytes':len(json.dumps(tools, separators=(',',':')).encode())}
    # With automatic refresh disabled nothing is published behind the client's back,
    # and an unbuilt index is never labelled a published snapshot.
    unbuilt = client.tool('index.status', {})[0]
    assert unbuilt['never_published'] is True and unbuilt['freshness']['reason'] == 'never_published', unbuilt
    assert unbuilt['freshness']['backend'] == 'no_published_snapshot', unbuilt['freshness']
    assert unbuilt['policy']['implicit_refresh'] is False and unbuilt['auto_refresh']['enabled'] is False, unbuilt
    refresh = client.tool('index.refresh', {'scope':'workspace'})[0]
    refresh_task = client.settled(refresh['task_id'])
    assert refresh_task['status'] == 'completed', refresh_task
    for relation in ['references','implementations']:
        client.tool('symbol.relations', {'symbol':'audit_target','relation':relation})
    context = client.tool('change.prepare', {'intent':'Update audit_target','targets':['audit_target'],'budget':4000})[0]
    preparation = client.settled(context['task_id'])
    assert preparation['status']=='completed', preparation
    context_id = preparation['result']['result']['context_id']
    analyzer_events = [json.loads(x) for x in (OUT/'analyzer.jsonl').read_text().splitlines()]
    requests = [event.get('method') for event in analyzer_events if event.get('event') == 'request']
    assert requests.count('initialize') == 1, requests
    assert 'textDocument/references' in requests and 'textDocument/implementation' in requests, requests
    assert sqlite3.connect(root/'.rust-repo-intelligence/index.sqlite3').execute('select count(*) from semantic_queries').fetchone()[0] >= 2
    results['analyzer_wiring'] = {'events':analyzer_events,'semantic_queries_in_db':sqlite3.connect(root/'.rust-repo-intelligence/index.sqlite3').execute('select count(*) from semantic_queries').fetchone()[0]}

    # The default refresh is incremental: an added file is indexed without
    # re-embedding or renumbering unchanged symbols, and a removal is applied.
    def index_query(sql):
        return sqlite3.connect(root/'.rust-repo-intelligence/index.sqlite3').execute(sql).fetchone()[0]
    target_id = index_query("select id from nodes where canonical_name like '%::audit_target'")
    (root/'src/incremental_probe.rs').write_text('pub fn incremental_probe_symbol() -> u32 { 7 }\n')
    incremental = client.settled(client.tool('index.refresh', {})[0]['task_id'])
    assert incremental['status'] == 'completed', incremental
    assert index_query("select id from nodes where canonical_name like '%::audit_target'") == target_id
    build = incremental['result']['refresh']['embeddings']
    assert build['recomputed'] == 1, build
    assert incremental['result']['refresh']['mode'] == 'incremental', incremental['result']['refresh']
    assert incremental['result']['freshness']['reason'] == 'ok', incremental['result']['freshness']
    found = client.tool('repo.search', {'query':'incremental_probe_symbol','mode':'broad'})[0]
    assert 'incremental_probe_symbol' in json.dumps(found), found
    (root/'src/incremental_probe.rs').unlink()
    removed = client.settled(client.tool('index.refresh', {})[0]['task_id'])
    assert removed['status'] == 'completed', removed
    assert index_query("select count(*) from nodes where canonical_name like '%incremental_probe_symbol'") == 0
    results['incremental_refresh'] = {'default_scope':'incremental','embedding_last_build':build}

    db = sqlite3.connect(root/'.rust-repo-intelligence/memory.sqlite3')
    cols = ['id','title','status','priority','kind','scope_json','evidence_json','depends_json','blocked_json','acceptance_json','verification_json','discovered_from','provenance','confidence','last_validated_snapshot','source_finding_id','human_owned','created_at','updated_at']
    vals = ['work-audit-fixture','audit_target','accepted','normal','improvement','["audit_target"]','[]','[]','[]','[]','[]','synthetic audit fixture','SyntheticFixture',1.,'',None,1,'2026-09-30','2026-09-30']
    db.execute('insert into work_items ('+','.join(cols)+') values ('+','.join('?' for _ in cols)+')',vals)
    db.commit(); db.close()
    work = client.tool('work.list', {'query':'audit_target'})[0]
    consult = client.tool('repo.consult', {'topic':'audit_target','budget':1500})[0]
    context = client.tool('change.prepare', {'intent':'Update audit_target','targets':['audit_target'],'budget':4000})[0]
    briefing = client.settled(context['task_id'])['result']['result']
    results['work_memory_join'] = {'public_work_count':len(work['items']), 'consult_known_work':consult['result']['known_work'],'prepared_work_items':briefing['work_items']}
    assert consult['result']['known_work'][0]['id'] == 'work-audit-fixture'
    assert briefing['work_items'][0]['id'] == 'work-audit-fixture'
    long_topic = 'Please audit the audit_target implementation and its validation path'
    long_consult = client.tool('repo.consult', {'topic':long_topic,'budget':4000})[0]
    assert long_consult['result']['known_work'][0]['id'] == 'work-audit-fixture'
    # Consultation reports work compactly and packs the whole envelope itself.
    brief = long_consult['result']['known_work'][0]
    assert brief['detail'] == 'work.get' and 'evidence' not in brief, brief
    assert long_consult['result']['context_budget']['serialized_bytes'] == len(json.dumps(long_consult, separators=(',',':'), ensure_ascii=False).encode()), long_consult['result']['context_budget']
    assert 'snapshot' in long_consult['result'] and 'next_steps' in long_consult['result'], long_consult['result'].keys()
    instruction_paths = {item.get('path') for item in long_consult['result']['live_instructions']}
    assert not instruction_paths & {item.get('path') for item in long_consult['result']['governing_documents']}
    unrelated = client.tool('repo.consult', {'topic':'Redesign the marketing logo colours','budget':4000})[0]
    assert all(item['id'] != 'work-audit-fixture' for item in unrelated['result']['known_work']), unrelated['result']['known_work']

    # Steering lifecycle: supersession, retirement, status filtering, path scopes, stale paths.
    first = client.tool('steering.record', {'title':'Build with the agent script','instruction':'Run scripts/cargo-agent.sh before commits','scope':['src']})[0]
    assert first['stale_references'] == ['scripts/cargo-agent.sh'] and first['warnings'], first
    second = client.tool('steering.record', {'title':'Build with cargo','instruction':'Run cargo test before commits','scope':['src/lib.rs'],
        'supersedes':[first['id']],'recorded_by':'fixture human'})[0]
    assert second['supersedes'] == [first['id']], second
    other = client.tool('steering.record', {'title':'Other crate','instruction':'Keep other code pure','scope':['crates/other/src']})[0]
    scoped = client.tool('steering.list', {'scope':'src/lib.rs'})[0]
    assert [item['id'] for item in scoped['steerings']] == [second['id']], scoped
    assert scoped['steerings'][0]['match'] == 'path'
    history = client.tool('steering.list', {'status':'all'})[0]
    superseded = next(item for item in history['steerings'] if item['id'] == first['id'])
    assert superseded['status'] == 'retired' and superseded['superseded_by'] == [second['id']], superseded
    retired = client.tool('steering.retire', {'id':other['id'],'retired_by':'fixture human','reason':'crate removed'})[0]
    assert retired['status'] == 'retired' and retired['history'][0]['action'] == 'retired', retired
    assert [item['id'] for item in client.tool('steering.list', {})[0]['steerings']] == [second['id']]
    assert {item['id'] for item in client.tool('steering.list', {'status':'retired'})[0]['steerings']} == {first['id'], other['id']}
    try:
        raw = client.call('tools/call', {'name':'steering.retire','arguments':{'id':other['id'],'retired_by':'fixture human','reason':'again'}})
        assert raw.get('isError'), raw
    except RuntimeError as error:
        assert 'only active steerings' in str(error), error
    results['steering_lifecycle'] = {'superseded':first['id'],'retired':other['id'],'active':second['id']}

    # A same-length edit keeps old line ranges valid while changing the symbol.
    (root/'src/lib.rs').write_text(original.replace('audit_target','wrong_target'))
    stale = client.tool('repo.context', {'query':'audit_target','budget':4000,'limit':2})[0]
    assert stale['freshness']['stale']
    target_slice = next(item for item in stale['result']['source_slices'] if item['symbol'].endswith('audit_target'))
    assert target_slice['stale'] and target_slice['provenance'] == 'StaticIndexStale', target_slice
    assert target_slice['content_hash'] != sqlite3.connect(root/'.rust-repo-intelligence/index.sqlite3').execute("select content_hash from nodes where canonical_name like '%audit_target'").fetchone()[0]
    results['stale_source'] = {'envelope_stale':stale['freshness']['stale'],'source_slices':stale['result']['source_slices']}
    (root/'src/lib.rs').write_text(original)

    no_checks = client.tool('change.validate', {'context_id':context_id,'git_diff':''})[0]
    no_checks = client.settled(no_checks['task_id'])
    assert no_checks['status'] == 'completed'
    assert no_checks['result']['result']['validation_status']['verdict'] == 'not_run'
    evidence = no_checks['result']['result']['review_evidence']
    assert evidence['semantic_correctness'] == 'not_established'
    assert evidence['delivery_authorized'] is False
    assert 'generation' in evidence['index'] and 'source_unchanged' in evidence


    # wait_seconds returns a task that settles in time inline, in task.get shape,
    # and keeps refusing unknown arguments.
    waited = client.tool('change.prepare', {'intent':'Update audit_target','targets':['audit_target'],'wait_seconds':30})[0]
    assert waited['task']['status'] == 'completed' and waited['task']['kind'] == 'change.prepare', waited
    waited_context = waited['task']['result']['result']['context_id']
    polled = client.tool('task.get', {'id':waited['task']['id'],'wait_seconds':5})[0]
    assert polled['task']['status'] == 'completed', polled
    rejected = client.call('tools/call', {'name':'change.prepare','arguments':{'intent':'x','wait':5}})
    assert rejected.get('isError'), rejected
    # The default pending diff includes untracked, non-ignored files; one
    # prepared context serves repeated validations, and none is required.
    (root/'src/untracked_probe.rs').write_text('pub fn untracked_probe() {}\n')
    for _ in range(2):
        pending = client.tool('change.validate', {'context_id':waited_context,'wait_seconds':30})[0]['task']
        assert pending['status'] == 'completed', pending
        report = pending['result']['result']
        assert report['context_id'] == waited_context and report['prepared'] is True, report
        assert 'src/untracked_probe.rs' in report['changed_files'], report['changed_files']
        assert 'src/untracked_probe.rs' in report['diff_scope']['untracked']['paths'], report['diff_scope']
        assert not any(path.startswith('.rust-repo-intelligence/') for path in report['diff_scope']['untracked']['paths'])
        # Changed Rust files get advisory analyzer diagnostics.
        semantic = report['semantic_diagnostics']
        assert semantic['advisory'] is True and semantic['blocking'] is False, semantic
        probe = next(item for item in semantic['files'] if item['path'] == 'src/untracked_probe.rs')
        assert probe['freshness'] == 'fresh' and probe['diagnostics'][0]['message'] == 'fake diagnostic', semantic
    unprepared = client.tool('change.validate', {'wait_seconds':30})[0]['task']
    assert unprepared['status'] == 'completed', unprepared
    report = unprepared['result']['result']
    assert report['prepared'] is False and report['context_id'] != waited_context, report
    assert report['architecture_delta']['status'] == 'unavailable' and report['architecture_delta']['reason'] == 'no prepared context'
    assert report['unmodified_expected_callers'] == {'status':'unavailable','reason':'no prepared context'}
    assert 'src/untracked_probe.rs' in report['changed_files']
    (root/'src/untracked_probe.rs').unlink()
    # The analyzer is off until semantic.enable, which warms it in the
    # background before any semantic request; it then serves captured
    # diagnostics. The legacy enable switch no longer launches it.
    warm_env = {key: value for key, value in env.items() if key != 'RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART'}
    warm_env.update(RUST_REPO_INTELLIGENCE_RUST_ANALYZER_WARM_START='1', RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER='1')
    warm_server = MCP(root, warm_env); clients.append(warm_server)
    time.sleep(1.5)
    status = warm_server.tool('semantic.status', {})[0]
    assert status['enabled'] is False and status['state'] == 'disabled' and status['starts'] == 0, status
    assert 'semantic.enable' in status['hint'] and 'no longer starts' in status['legacy_env_note'], status
    enabled = warm_server.tool('semantic.enable', {})[0]
    assert enabled['enabled'] is True and enabled['changed'] is True, enabled
    assert warm_server.tool('semantic.enable', {})[0]['changed'] is False
    deadline = time.monotonic() + 30
    while (status := warm_server.tool('semantic.status', {})[0])['state'] != 'ready':
        assert time.monotonic() < deadline, status
        time.sleep(.05)
    assert status['warm_start']['state'] == 'active' and status['starts'] == 1 and status['program_found'] is True, status
    diagnostics = warm_server.tool('semantic.diagnostics', {'paths':['src/lib.rs']})[0]
    lib = diagnostics['files'][0]
    assert lib['path'] == 'src/lib.rs' and lib['freshness'] == 'fresh', diagnostics
    assert lib['diagnostics'][0] == {'severity':'warning','line':1,'character':0,'end_line':1,'end_character':3,'code':'fake',
        'source':'rust-analyzer','reported_by':'rust-analyzer','message':'fake diagnostic'}, lib
    errors_only = warm_server.tool('semantic.diagnostics', {'paths':['src/lib.rs'],'severity':'error'})[0]
    assert errors_only['files'][0]['count'] == 0 and errors_only['summary']['warning'] == 0, errors_only
    results['semantic_diagnostics'] = {'warm_start':status['warm_start'],'diagnostics':diagnostics['summary']}
    disabled = warm_server.tool('semantic.disable', {})[0]
    assert disabled['enabled'] is False and disabled['changed'] is True and disabled['running'] is False, disabled
    assert disabled['state'] == 'disabled' and disabled['warm_start']['state'] == 'stopped', disabled
    query = warm_server.tool('semantic.query', {'file':'src/lib.rs','line':1,'character':8,'query':'hover','wait_seconds':10})[0]
    assert 'semantic.enable' in json.dumps(query), query
    results['semantic_enablement'] = {'default':'disabled','enable':'ready','disable':'stopped'}
    results['workflow_round_trips'] = {'inline_prepare':True,'repeated_validation':2,'validation_without_context':True,'untracked_in_pending_diff':True}

    # Cancellation after a check starts should prevent subsequent checks.
    validation = client.tool('change.validate', {'context_id':context_id,'git_diff':'','run_checks':True})[0]
    deadline = time.monotonic()+15
    while not (OUT/'check-started').exists() and time.monotonic()<deadline: time.sleep(.01)
    assert (OUT/'check-started').exists()
    cancellation = client.tool('task.cancel', {'id':validation['task_id']})[0]
    completed = client.settled(validation['task_id'])
    assert completed['status'] == 'cancelled', completed
    commands = [json.loads(x) for x in (OUT/'cargo.jsonl').read_text().splitlines()]
    assert len(commands) == 1 and commands[0][0] == 'fmt', commands
    (OUT/'release-check').touch()
    results['running_cancellation'] = {'request':cancellation,'final_status':completed['status'],'cancel_requested':completed['cancel_requested'],'commands_after_cancel':[json.loads(x) for x in (OUT/'cargo.jsonl').read_text().splitlines()]}

    # One failed compiler check must affect the overall validation verdict.
    fake_cargo.write_text(fake_cargo.read_text().replace('sys.exit(0)', "sys.exit(1 if sys.argv[1] == 'check' else 0)"))
    validation = client.tool('change.validate', {'context_id':context_id,'git_diff':'','run_checks':True})[0]
    completed = client.settled(validation['task_id'])
    report = completed['result']['result']
    assert completed['status'] == 'completed', completed
    assert report['validation_status']['verdict'] == 'failed', report
    assert report['checks'][1]['success'] is False
    clippy_plan = client.tool('verification.plan', {'checks':['clippy'],'deny_warnings':True})[0]
    clippy_plan = client.settled(clippy_plan['task_id'])['result']
    clippy_run = client.tool('verification.run', {'id':clippy_plan['id']})[0]
    assert client.settled(clippy_run['task_id'])['result']['passed'] is True
    clippy = next(json.loads(line) for line in (OUT/'cargo.jsonl').read_text().splitlines() if json.loads(line)[0] == 'clippy')
    assert clippy.index('--message-format=json') < clippy.index('--'), clippy
    results['failed_check_verdict'] = {'task_status':completed['status'],'checks':report['checks'],'blocking':report['blocking'],'top_level_report_keys':list(report)}

    # A second active server must not declare the first server's work dead.
    (OUT/'release-check').unlink()
    (OUT/'check-started').unlink()
    validation = client.tool('change.validate', {'context_id':context_id,'git_diff':'','run_checks':True})[0]
    deadline = time.monotonic()+15
    while not (OUT/'check-started').exists() and time.monotonic()<deadline: time.sleep(.01)
    assert (OUT/'check-started').exists()
    second = MCP(root, env); clients.append(second)
    concurrent = client.tool('task.get', {'id':validation['task_id']})[0]['task']
    assert concurrent['status'] == 'running' and concurrent['error'] is None, concurrent
    results['multiple_servers'] = {'while_original_check_running':{'status':concurrent['status'],'error':concurrent['error']}}
    (OUT/'release-check').touch()
    final = client.settled(validation['task_id'])
    assert final['status'] == 'completed' and final['error'] is None, final
    results['multiple_servers']['after_original_worker_finished']={'status':final['status'],'error':final['error']}

    # Measure only the isolated fixture; never open the developer's memory store.
    real = client
    measurements = []
    for name, args in [('repo.consult',{'topic':'validation','budget':250}),('repo.context',{'query':'audit_target','budget':250,'limit':8}),('repo.context',{'query':'audit_target','budget':1500,'limit':16})]:
        payload, raw = real.tool(name,args)
        result = payload.get('result',payload)
        compact = len(json.dumps(payload,separators=(',',':'),ensure_ascii=False).encode())
        raw_bytes = len(json.dumps(raw,separators=(',',':')).encode())
        assert compact <= args['budget'] * 4, (name, compact, payload)
        assert result['context_budget']['serialized_bytes'] == compact
        assert result['context_budget']['estimated_tokens'] == (compact+3)//4
        measurements.append({'tool':name,'budget':args['budget'],'reported_budget':result.get('context_budget'),'payload_bytes':compact,'payload_bytes_div4_proxy':compact/4,'mcp_result_bytes':raw_bytes,'mcp_result_keys':list(raw)})
    results['payload_budgets']=measurements
    # Coding coordination is shared by independent servers and linked worktrees.
    coordination_root = OUT / 'coordination-fixture'
    coordination_root.mkdir()
    (coordination_root / 'src').mkdir()
    (coordination_root / 'src/lib.rs').write_text('pub fn owned() {}\n')
    (coordination_root / 'Cargo.toml').write_text('[package]\nname="coordination_fixture"\nversion="0.1.0"\nedition="2024"\n')
    subprocess.run([REAL_CARGO, 'generate-lockfile', '--offline'], cwd=coordination_root, check=True, capture_output=True)
    git(coordination_root, 'init', '-q')
    git(coordination_root, 'branch', '-M', 'main')
    git(coordination_root, 'config', 'user.name', 'Coordination Fixture')
    git(coordination_root, 'config', 'user.email', 'coordination@example.invalid')
    git(coordination_root, 'add', '.')
    git(coordination_root, 'commit', '-qm', 'fixture')
    owner_server = MCP(coordination_root, env); clients.append(owner_server)
    owner_server.tool('repo.consult', {'topic':'coordinate parallel code changes'})
    # A single agent commits without registering a session while no other
    # session is active; the implicit session ends with the delivery.
    (coordination_root / 'NOTES.md').write_text('single-agent change\n')
    single = owner_server.tool('commit.plan', {'groups':[{'message':'Add notes','paths':['NOTES.md']}],'wait_seconds':30})[0]['task']
    assert single['status'] == 'completed' and single['result']['implicit_session'] is True, single
    single_head = subprocess.check_output(['git','rev-parse','HEAD'],cwd=coordination_root,text=True).strip()
    delivered = owner_server.tool('commit.execute', {'plan_id':single['result']['plan_id'],'wait_seconds':30})[0]['task']
    assert delivered['status'] == 'completed' and delivered['result']['state'] == 'completed', delivered
    assert subprocess.check_output(['git','rev-parse','HEAD~1'],cwd=coordination_root,text=True).strip() == single_head
    assert owner_server.tool('session.list', {})[0]['sessions'] == []
    started = owner_server.tool('session.start', {'owner':'one','intent':'owned function','isolate':True})[0]
    completed = owner_server.settled(started['task_id'])
    assert completed['status'] == 'completed', completed
    one = completed['result']
    isolated_server = MCP(Path(one['session']['worktree']), env); clients.append(isolated_server)
    isolated_server.tool('repo.consult', {'topic':'coordinate parallel code changes'})
    git(coordination_root, 'checkout', '-qb', 'delivery')
    peer_server = MCP(coordination_root, env); clients.append(peer_server)
    peer_server.tool('repo.consult', {'topic':'coordinate parallel code changes'})
    started = peer_server.tool('session.start', {'owner':'two','intent':'another change'})[0]
    completed = peer_server.settled(started['task_id'])
    assert completed['status'] == 'completed', completed
    two = completed['result']
    credentials = lambda item: {'session_id':item['session']['id'],'lease_token':item['lease_token']}
    acquired = isolated_server.tool('session.claim', {**credentials(one),'paths':['src']})[0]
    assert acquired['acquired'] is True
    collision = peer_server.tool('session.claim', {**credentials(two),'paths':['src/lib.rs']})[0]
    assert collision['acquired'] is False
    assert collision['conflicts'][0]['session_id'] == one['session']['id']
    listed = peer_server.tool('session.list', {})[0]
    assert len(listed['sessions']) == 2
    assert one['lease_token'] not in json.dumps(listed)
    # With other sessions active, committing requires an explicit session.
    (coordination_root / 'NOTES.md').write_text('parallel change\n')
    refused = peer_server.tool('commit.plan', {'groups':[{'message':'Edit notes','paths':['NOTES.md']}],'wait_seconds':30})[0]['task']
    assert refused['status'] == 'failed' and 'explicit session' in refused['error'], refused
    git(coordination_root, 'checkout', '--', 'NOTES.md')
    isolated_server.tool('session.close', credentials(one))
    assert peer_server.tool('session.claim', {**credentials(two),'paths':['src/lib.rs']})[0]['acquired'] is True
    peer_server.tool('session.heartbeat', {**credentials(two),'summary':'planning cohesive commit'})
    (coordination_root / 'src/lib.rs').write_text('pub fn owned() -> bool {\n    true\n}\n')
    pending = peer_server.tool('commit.plan', {**credentials(two),'groups':[{'message':'Return owned result','paths':['src/lib.rs']}]})[0]
    planned = peer_server.settled(pending['task_id'])
    assert planned['status'] == 'completed', planned
    assert planned['result']['groups'][0]['paths'] == ['src/lib.rs']
    assert planned['result']['executed'] is False
    assert not subprocess.check_output(['git','diff','--cached'],cwd=coordination_root)
    executed_task = peer_server.tool('commit.execute', {**credentials(two),'plan_id':planned['result']['plan_id']})[0]
    executed = peer_server.settled(executed_task['task_id'])
    assert executed['status'] == 'completed' and executed['result']['state'] == 'completed', executed
    chunk = peer_server.tool('chunk.create', {**credentials(two),'plan_id':planned['result']['plan_id'],
        'title':'Return owned result','summary':'The function exposes a boolean result.'})[0]
    # Run real Cargo checks, then exercise GitHub through an instrumented CLI and
    # redirect only the exact GitHub push URL to a local bare fixture repository.
    gh_bin = OUT / 'github-bin'; gh_bin.mkdir()
    shutil.copy(REPO / 'tests/github_mock.py', gh_bin / 'gh'); (gh_bin / 'gh').chmod(0o755)
    gh_state = OUT / 'github-state.json'
    bare = OUT / 'remote.git'; git(OUT, 'init', '--bare', '-q', str(bare))
    base_oid = subprocess.check_output(['git','rev-parse','main'],cwd=coordination_root,text=True).strip()
    git(coordination_root, 'push', str(bare), base_oid+':refs/heads/delivery')
    gh_state.write_text(json.dumps({'head':chunk['head'],'base':base_oid,'actor':'author','bare':str(bare)}))
    (gh_bin / 'git').write_text("""#!/usr/bin/env python3
import json, os, sys
args=sys.argv[1:]
if args and args[0] in ('push','ls-remote'):
    assert args[1] in ('--porcelain','--heads') and args[2]=='https://github.test/fixture/repo.git', args
    args[2]=json.load(open(os.environ['CRUSTY_GITHUB_MOCK_STATE']))['bare']
os.execv('/usr/bin/git',['/usr/bin/git',*args])
""")
    (gh_bin / 'git').chmod(0o755)
    gh_env = os.environ.copy()
    gh_env.update(PATH=str(gh_bin)+os.pathsep+gh_env['PATH'],CRUSTY_GITHUB_MOCK_STATE=str(gh_state),
                  RUST_REPO_INTELLIGENCE_ENABLE_WATCHER='0',RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART='0')
    delivery_server = MCP(coordination_root,gh_env); clients.append(delivery_server)
    def completed(client,name,args):
        pending=client.tool(name,args)[0]
        task=client.settled(pending['task_id'])
        assert task['status']=='completed',(name,task)
        return task['result']
    contract = delivery_server.tool('project.contract',{})[0]
    assert contract['members'][0]['name']=='coordination_fixture'
    check_plan = completed(delivery_server,'verification.plan',{'offline':True})
    checks = completed(delivery_server,'verification.run',{'id':check_plan['id']})
    assert checks['passed'] is True and checks['delivery_eligible'] is True, checks
    policy = delivery_server.tool('delivery.policy.grant',{'repository':'github.test/fixture/repo','base':'main',
        'granted_by':'fixture human','actions':['publish','review','approve','merge'],'expires_at':int(time.time())+600,'max_mutations':30})[0]
    target = {'repository':'github.test/fixture/repo','number':42}
    publish_args = {**credentials(two),'policy_id':policy['id'],
        'repository':target['repository'],'base':'main','chunk_id':chunk['id'],'verification_id':checks['id'],
        'title':'Return owned result','body':'The function now exposes its owned result. Validated with format, check, tests and Clippy.'}
    pending=delivery_server.tool('github.pr.publish',publish_args)[0]
    rejected=delivery_server.settled(pending['task_id'])
    assert rejected['status']=='failed' and 'remote branch changed or already exists' in rejected['error'], rejected
    assert subprocess.check_output(['git','--git-dir',str(bare),'rev-parse','refs/heads/delivery'],text=True).strip()==base_oid
    published = completed(delivery_server,'github.pr.publish',{**publish_args,'expected_remote_head':base_oid})
    assert published['state']=='completed' and published['number']==42
    assert json.loads(gh_state.read_text())['draft'] is True
    ready = completed(delivery_server,'github.pr.ready',{**target,'policy_id':policy['id'],'expected_head':chunk['head']})
    assert ready['state']=='completed'
    packet = completed(delivery_server,'github.review.packet',target)
    assert packet['head']==chunk['head'] and packet['complete'] is True
    review_policy = delivery_server.tool('delivery.policy.grant',{'repository':target['repository'],'base':'main',
        'granted_by':'fixture human','actions':['approve'],'expires_at':int(time.time())+600,'max_mutations':1})[0]
    review_args={**target,'policy_id':review_policy['id'],'packet_id':packet['id'],'event':'approve','body':'Reviewed invariants and full diff. Tests pass. Literal $(never-execute) stays text.'}
    pending=delivery_server.tool('github.review.submit',review_args)[0]
    assert 'approving their own PRs' in delivery_server.settled(pending['task_id'])['error']
    state=json.loads(gh_state.read_text()); state['actor']='reviewer';state['timeout_after_review']=True;state['expected_body']=review_args['body'];gh_state.write_text(json.dumps(state))
    pending=delivery_server.tool('github.review.submit',review_args)[0]
    assert delivery_server.settled(pending['task_id'])['status']=='failed'
    assert len(json.loads(gh_state.read_text())['reviews'])==1
    review=completed(delivery_server,'github.review.submit',review_args)
    assert review['state']=='completed' and len(json.loads(gh_state.read_text())['reviews'])==1
    # A changed head blocks submission before any new remote review.
    state=json.loads(gh_state.read_text());state['head']='e'*40;gh_state.write_text(json.dumps(state))
    pending=delivery_server.tool('github.review.submit',review_args)[0]
    assert 'PR changed since review' in delivery_server.settled(pending['task_id'])['error']
    state['head']=chunk['head'];state['queue']=True;gh_state.write_text(json.dumps(state))
    merge=completed(delivery_server,'github.pr.merge',{**target,'policy_id':policy['id'],'expected_head':chunk['head'],
        'expected_base':base_oid,'method':'merge','auto':True})
    assert merge['state']=='requested' and json.loads(gh_state.read_text())['merge_calls']==1
    reconciled=completed(delivery_server,'github.action.reconcile',{'id':merge['id']})
    assert reconciled['state']=='requested'
    state=json.loads(gh_state.read_text());state.update(merged=True,pr_state='closed',merge_commit='d'*40);gh_state.write_text(json.dumps(state))
    reconciled=completed(delivery_server,'github.action.reconcile',{'id':merge['id']})
    assert reconciled['state']=='merged' and reconciled['merge_commit']=='d'*40
    state['head']='e'*40;gh_state.write_text(json.dumps(state))
    assert completed(delivery_server,'github.action.reconcile',{'id':merge['id']})['state']=='stale'
    revoked=delivery_server.tool('delivery.policy.revoke',{'id':policy['id']})[0]
    assert revoked['expires_at']==0
    results['github_delivery']={'transport':'instrumented CLI and local bare Git remote','actual_remote_mutations':False,
        'draft_publish':True,'remote_branch_fence':True,'real_verification':True,'head_bound_reviews':True,
        'ambiguous_review_recovered_at_budget_limit':True,'queue_reconciled':True}
    peer_server.tool('session.close', credentials(two))
    assert peer_server.tool('session.list', {})[0]['sessions'] == []
    results['coding_coordination'] = {'independent_servers':3,'linked_worktrees':True,'claim_handoff':True,'commit_planning':True,
        'implicit_single_agent_commit':True,'implicit_refused_with_active_sessions':True}

    # memory.search reads this repository's Claude Code transcripts from
    # CLAUDE_CONFIG_DIR (a fixture, never the developer's home directory).
    claude_config = OUT / 'claude-config'
    slug = ''.join(c if c.isascii() and c.isalnum() else '-' for c in str(root.resolve()))
    transcripts = claude_config / 'projects' / slug
    transcripts.mkdir(parents=True)
    lines = [
        {'type':'user','uuid':'u1','cwd':str(root.resolve()),'timestamp':'2026-10-02T10:00:00Z','message':{'role':'user','content':'Make the audit_target toggle collapsible'}},
        {'type':'user','uuid':'u2','cwd':str(root.resolve()),'timestamp':'2026-10-02T10:01:00Z','message':{'role':'user','content':[{'type':'tool_result','tool_use_id':'t','content':'audit_target toggle output'}]}},
        {'type':'user','uuid':'u3','isMeta':True,'cwd':str(root.resolve()),'timestamp':'2026-10-02T10:02:00Z','message':{'role':'user','content':'audit_target toggle skill text'}},
    ]
    (transcripts / 'session-fixture.jsonl').write_text(''.join(json.dumps(line)+'\n' for line in lines))
    memory_env = env.copy()
    memory_env.update(CLAUDE_CONFIG_DIR=str(claude_config), CODEX_HOME=str(OUT / 'codex-empty'))
    memory_server = MCP(root, memory_env); clients.append(memory_server)
    recovered = memory_server.tool('memory.search', {'query':'audit_target toggle'})[0]
    assert [prompt['text'] for prompt in recovered['prompts']] == ['Make the audit_target toggle collapsible'], recovered
    assert recovered['prompts'][0]['source'] == 'claude_code' and recovered['history']['claude_code']['available'] is True, recovered
    assert recovered['history']['codex']['available'] is False, recovered['history']
    results['claude_code_memory'] = {'prompts':len(recovered['prompts']),'history':recovered['history']['claude_code']}

    # Automatic refresh: a server started in a subdirectory resolves the Cargo
    # workspace root, publishes once after startup, and republishes after a
    # worktree edit and after a commit without any explicit index.refresh.
    auto_root = OUT / 'auto-fixture'
    (auto_root / 'src').mkdir(parents=True)
    (auto_root / 'Cargo.toml').write_text('[package]\nname="auto_fixture"\nversion="0.1.0"\nedition="2024"\n')
    (auto_root / 'src/lib.rs').write_text('pub fn auto_initial() {}\n')
    git(auto_root, 'init', '-q')
    git(auto_root, 'config', 'user.name', 'Auto Fixture')
    git(auto_root, 'config', 'user.email', 'auto@example.invalid')
    git(auto_root, 'add', '.')
    git(auto_root, 'commit', '-qm', 'fixture')
    auto_env = {key: value for key, value in env.items() if key != 'CRUSTY_AUTO_REFRESH'}
    auto_env.update(RUST_REPO_INTELLIGENCE_ENABLE_WATCHER='1', RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART='0')
    auto_server = MCP(auto_root / 'src', auto_env); clients.append(auto_server)
    def wait_status(predicate, label):
        deadline = time.monotonic() + 60
        while True:
            status = auto_server.tool('index.status', {})[0]
            if predicate(status):
                return status
            if time.monotonic() > deadline:
                raise RuntimeError(f'automatic refresh did not reach {label}: {status["freshness"]}')
            time.sleep(.1)
    # A generation is visible before its task is marked complete.
    fresh = lambda generation: lambda status: status['freshness']['reason'] == 'ok' and (status['freshness']['indexed']['generation'] or 0) > generation \
        and status['freshness']['refresh']['running'] is None
    status = wait_status(fresh(0), 'the startup generation')
    assert status['root']['path'] == str(auto_root.resolve()) and status['root']['resolution'] == 'cargo_package', status['root']
    assert not (auto_root / 'src/.rust-repo-intelligence').exists()
    assert status['policy']['implicit_refresh'] is True and status['auto_refresh']['watcher'] == 'active', status['auto_refresh']
    assert status['latest_refresh_task']['result']['mode'] == 'full', status['latest_refresh_task']
    backend = status['index']['backend']
    assert backend['rust_analyzer_state'] == 'disabled' and 'semantic.enable' in backend['rust_analyzer_hint'], backend
    serialized = json.dumps(status)
    assert serialized.count('"semantic_engine"') == 1 and serialized.count('"embedding_card_version"') == 1, serialized
    assert 'index' not in status['index']['snapshot'], status['index']['snapshot']
    generation = status['freshness']['indexed']['generation']
    (auto_root / 'src/lib.rs').write_text('pub fn auto_initial() {}\npub fn auto_edited() {}\n')
    status = wait_status(fresh(generation), 'a generation with the edit')
    found = auto_server.tool('repo.search', {'query':'auto_edited','mode':'broad'})[0]
    assert 'auto_edited' in json.dumps(found['result']), found
    assert found['freshness']['stale'] is False and found['freshness']['live']['dirty_inputs'] >= 1, found['freshness']
    generation = status['freshness']['indexed']['generation']
    git(auto_root, 'commit', '-qam', 'commit the edit')
    status = wait_status(fresh(generation), 'a generation for the new HEAD')
    assert status['freshness']['indexed']['head'] == subprocess.check_output(['git','rev-parse','HEAD'],cwd=auto_root,text=True).strip()
    assert status['auto_refresh']['last']['outcome'] in ('refreshed', 'fresh'), status['auto_refresh']
    results['automatic_refresh'] = {'root_resolution':status['root'],'generations':status['freshness']['indexed']['generation'],
        'auto_refresh':status['auto_refresh']}
    print('MCP regressions passed: analyzer reuse, analyzer warm start and diagnostics, work joins, stale hashes, inline waits, unprepared validation, cancellation, failed verdict, concurrent ownership, implicit commits, Claude Code memory, response budgets, automatic refresh')
finally:
    (OUT/'release-check').touch()
    for client in reversed(clients): client.close()
    TEMP.cleanup()
