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
    RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER='1',
    RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH=str(fake_ra),
    RUST_REPO_INTELLIGENCE_ENABLE_WATCHER='0', PATH=str(fake_bin)+os.pathsep+env['PATH'])
results = {'artifact_directory':str(OUT), 'fixture':str(root), 'build_profile':'debug',
    'limits':'Payload bytes are measured; bytes/4 is only a token proxy. Analyzer and Cargo are instrumented fakes to test wiring/cancellation, not semantic correctness or real check performance.'}
clients = []
try:
    client = MCP(root, env); clients.append(client)
    tools = client.call('tools/list', {})
    results['tool_catalog'] = {'tools':len(tools['tools']), 'compact_json_bytes':len(json.dumps(tools, separators=(',',':')).encode())}
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
    assert client.tool('repo.consult', {'topic':long_topic,'budget':4000})[0]['result']['known_work'][0]['id'] == 'work-audit-fixture'

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
    clippy = next(json.loads(line) for line in (OUT/'cargo.jsonl').read_text().splitlines() if json.loads(line)[0] == 'clippy')
    assert clippy.index('--message-format=json-diagnostic-rendered-ansi') < clippy.index('--'), clippy
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
    print('MCP regressions passed: analyzer reuse, work joins, stale hashes, cancellation, failed verdict, concurrent ownership, response budgets')
finally:
    (OUT/'release-check').touch()
    for client in reversed(clients): client.close()
    TEMP.cleanup()
