#!/usr/bin/env python3
"""Instrumented gh process for MCP delivery tests; never connects to GitHub."""
import json
import os
from pathlib import Path
import subprocess
import sys

state_path = Path(os.environ['CRUSTY_GITHUB_MOCK_STATE'])
state = json.loads(state_path.read_text())
args = sys.argv[1:]
with state_path.with_suffix('.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')


def finish(value, code=0):
    state_path.write_text(json.dumps(state))
    print(value if isinstance(value, str) else json.dumps(value))
    sys.exit(code)


def pr():
    return {'number': 42, 'node_id': 'PR_test_42', 'state': state.get('pr_state', 'open'),
            'head': {'sha': state['head'], 'ref': state.get('branch', 'feature'),
                     'repo': {'full_name': 'fixture/repo'}},
            'base': {'sha': state['base'], 'ref': 'main'},
            'user': {'login': state.get('author', 'author')},
            'draft': state.get('draft', False), 'merged': state.get('merged', False),
            'title': 'Fixture PR', 'body': 'Fixture body', 'html_url': 'https://github.test/fixture/repo/pull/42',
            'merge_commit_sha': state.get('merge_commit')}


if args == ['--version']:
    finish('gh version fixture')
if args[:2] == ['pr', 'checks']:
    bucket = state.get('check_bucket', 'pass')
    finish([{'name': 'CI', 'bucket': bucket, 'state': 'SUCCESS', 'link': 'https://github.test/check'}],
           8 if bucket == 'pending' else 0)
if args[:2] == ['pr', 'merge']:
    assert '--admin' not in args and '--delete-branch' not in args
    assert args[args.index('--match-head-commit') + 1] == state['head']
    state['merge_calls'] = state.get('merge_calls', 0) + 1
    if not state.get('queue', False):
        state.update(merged=True, pr_state='closed', merge_commit='d' * 40)
    finish('')
assert args[0] == 'api', args
endpoint = args[args.index('--method') + 2]
method = args[args.index('--method') + 1]
body = json.loads(Path(args[args.index('--input') + 1]).read_text()) if '--input' in args else None
if endpoint == 'user':
    finish({'login': state.get('actor', 'reviewer')})
if endpoint == 'repos/fixture/repo':
    finish({'permissions': {'push': True}, 'default_branch': 'main', 'archived': False})
if 'compare/' in endpoint:
    if 'Accept: application/vnd.github.diff' in args:
        finish('diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-original\n+updated\n')
    finish({'files': [{'filename': 'src/lib.rs'}]})
if endpoint.endswith('/reviews?per_page=100'):
    finish(state.get('reviews', []))
if endpoint.endswith('/reviews') and method == 'POST':
    assert body['commit_id'] == state['head']
    assert body['body'] == state.get('expected_body', body['body'])
    review = {'id': len(state.get('reviews', [])) + 1, 'commit_id': body['commit_id'],
              'body': body['body'], 'state': {'APPROVE': 'APPROVED', 'COMMENT': 'COMMENTED',
                                          'REQUEST_CHANGES': 'CHANGES_REQUESTED'}[body['event']],
              'user': {'login': state.get('actor', 'reviewer')}}
    state.setdefault('reviews', []).append(review)
    if state.get('change_after_review'):
        state['head'] = 'e' * 40
    if state.pop('timeout_after_review', False):
        finish('uncertain response', 1)
    finish(review)
if endpoint == 'graphql':
    assert body['variables']['id'] == 'PR_test_42'
    state['draft'] = False
    finish({'data': {'markPullRequestReadyForReview': {'pullRequest': {'id': 'PR_test_42', 'isDraft': False}}}})
if 'git/ref/heads/' in endpoint:
    branch = endpoint.split('git/ref/heads/')[1]
    sha = subprocess.check_output(['/usr/bin/git', '--git-dir', state['bare'], 'rev-parse', f'refs/heads/{branch}'], text=True).strip()
    finish({'object': {'sha': sha}})
if '/pulls?' in endpoint:
    finish([pr()] if state.get('published', False) else [])
if endpoint.endswith('/pulls') and method == 'POST':
    assert body['draft'] is True and body['base'] == 'main'
    state['branch'] = body['head']
    state['head'] = subprocess.check_output(['/usr/bin/git', '--git-dir', state['bare'], 'rev-parse', f'refs/heads/{body["head"]}'], text=True).strip()
    state.update(published=True, draft=True, author=state.get('actor', 'reviewer'))
    finish(pr())
if endpoint.endswith('/pulls/42'):
    finish(pr())
raise AssertionError((args, body))
