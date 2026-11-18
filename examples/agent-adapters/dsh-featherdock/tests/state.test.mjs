import test from 'node:test';
import assert from 'node:assert/strict';
import {
  aggregateProjection,
  applySessionEvent,
  createProjection,
  normalizeBootstrap,
  publicText,
  resolveRootSessionId,
} from '../state.mjs';

const session = { id: 'session-1', header: { cwd: 'C:\\Work\\Dock' } };
const event = (seq, type, data = {}) => ({ seq, time: 1_700_000_000_000 + seq * 1000, type, data });

test('turn, approval, question and completion follow authoritative lifecycle', () => {
  const projection = createProjection(session);
  assert.equal(projection.state, 'idle');
  assert.equal(projection.title, 'Dock');

  assert(applySessionEvent(projection, event(1, 'turn/start', { turn: 3 })));
  assert.equal(projection.state, 'running');
  assert.match(projection.detail, /第 3 轮/);

  applySessionEvent(projection, event(2, 'approval/asked', { id: 'a1', toolName: 'pwsh' }));
  assert.equal(projection.state, 'waiting');
  assert.match(projection.detail, /pwsh/);

  applySessionEvent(projection, event(3, 'approval/decided', { id: 'a1', outcome: 'allowed-once' }));
  assert.equal(projection.state, 'running');

  applySessionEvent(projection, event(4, 'tool/call', { name: 'ask_user_question' }));
  assert.equal(projection.state, 'waiting');
  assert.match(projection.detail, /等待你的回答/);

  applySessionEvent(projection, event(5, 'tool/result', {}));
  assert.equal(projection.state, 'running');

  applySessionEvent(projection, event(6, 'turn/end', { reason: { kind: 'completed' } }));
  assert.equal(projection.state, 'done');
  assert.equal(projection.updated_at, 1_700_000_006);
  assert.equal(applySessionEvent(projection, event(7, 'tool/result', {})), false);
});

test('error, cancellation and blocked states remain distinct', () => {
  const projection = createProjection(session);
  applySessionEvent(projection, event(1, 'turn/end', {
    reason: { kind: 'error', error: { code: 'MODEL_TIMEOUT', message: 'Request timed out' } },
  }));
  assert.equal(projection.state, 'error');
  assert.match(projection.detail, /MODEL_TIMEOUT/);

  applySessionEvent(projection, event(2, 'turn/end', { reason: { kind: 'aborted' } }));
  assert.equal(projection.state, 'idle');

  applySessionEvent(projection, event(3, 'turn/end', { reason: { kind: 'blocked' } }));
  assert.equal(projection.state, 'waiting');
});

test('titles use public text only and bootstrap does not replay old alerts', () => {
  const history = [
    event(1, 'user/message', { content: [{ type: 'reasoning', text: 'private reasoning' }, { type: 'text', text: 'Fix Dock lifecycle' }] }),
    event(2, 'turn/start', { turn: 1 }),
    event(3, 'turn/end', { reason: { kind: 'completed' } }),
    event(4, 'user/message', { content: [{ type: 'text', text: 'Time sampled while preparing turn 99, step 1: 2026-11-17T00:00:00+08:00' }] }),
    event(5, 'session/title', { title: 'Current runtime context. This snapshot supersedes earlier context.' }),
    event(6, 'session/title', { title: 'dsh' }),
  ];
  const projection = createProjection(session, history);
  assert.equal(projection.title, 'Fix Dock lifecycle');
  assert.equal(projection.lastEventSeq, 6);
  normalizeBootstrap(projection, 'idle');
  assert.equal(projection.state, 'idle');
  assert.equal(projection.updated_at, 1_700_000_003, 'idle bootstrap must preserve the real last event time');
  assert.match(projection.detail, /等待新的对话/);

  assert.equal(applySessionEvent(projection, event(7, 'session/title', { title: 'Automatic replacement' })), false);
  assert.equal(projection.title, 'Fix Dock lifecycle');
  assert.equal(applySessionEvent(projection, event(8, 'user/message', { content: [{ type: 'text', text: 'replacement' }] })), false);
  assert.equal(projection.title, 'Fix Dock lifecycle');

  const named = createProjection(session);
  assert(applySessionEvent(named, event(1, 'session/title', { title: 'Chosen title' })));
  assert.equal(named.title, 'Chosen title');
  assert.equal(applySessionEvent(named, event(2, 'user/message', { content: [{ type: 'text', text: 'replacement' }] })), false);
  assert.equal(named.title, 'Chosen title');
});

test('pending approval survives bootstrap while stale done becomes idle', () => {
  const projection = createProjection(session, [
    event(1, 'turn/start', { turn: 2 }),
    event(2, 'approval/asked', { id: 'pending', toolName: 'shell' }),
  ]);
  normalizeBootstrap(projection, 'running');
  assert.equal(projection.state, 'waiting');
});

test('public text strips credentials and bounds display length', () => {
  const cleaned = publicText('Bearer abcdefghijklmnop api_key=secret-value', 200);
  assert.doesNotMatch(cleaned, /abcdefghijklmnop|secret-value/);
  assert.match(cleaned, /redacted/);
  assert.equal(publicText('x'.repeat(500), 40).length, 40);
});

test('delegated descendants resolve to one live root without merging user forks', () => {
  const root = { id: 'root', header: { origin: 'user', parentSession: 'elsewhere' } };
  const child = { id: 'child', header: { origin: 'subagent', parentSession: 'root', delegationDepth: 1 } };
  const grandchild = { id: 'grandchild', header: { origin: 'subagent', parentSession: 'child', delegationDepth: 2 } };
  const fork = { id: 'fork', header: { origin: 'fork', parentSession: 'root', delegationDepth: 0 } };
  const orphan = { id: 'orphan', header: { origin: 'subagent', parentSession: 'missing', delegationDepth: 1 } };
  const sessions = new Map([root, child, grandchild, fork, orphan].map((session) => [session.id, session]));
  const getSession = (id) => sessions.get(id);

  assert.equal(resolveRootSessionId(root, getSession), 'root');
  assert.equal(resolveRootSessionId(child, getSession), 'root');
  assert.equal(resolveRootSessionId(grandchild, getSession), 'root');
  assert.equal(resolveRootSessionId(fork, getSession), 'fork');
  assert.equal(resolveRootSessionId(orphan, getSession), null);

  const a = { id: 'a', header: { origin: 'subagent', parentSession: 'b' } };
  const b = { id: 'b', header: { origin: 'subagent', parentSession: 'a' } };
  const cyclic = new Map([a, b].map((session) => [session.id, session]));
  assert.equal(resolveRootSessionId(a, (id) => cyclic.get(id)), null);
});

test('child activity rolls into the root row without replacing its identity', () => {
  const root = {
    session_id: 'root',
    title: 'Main conversation',
    state: 'running',
    detail: '执行工具 · subagent',
    updated_at: 100,
  };
  const child = {
    session_id: 'child',
    title: 'Internal exploration',
    state: 'waiting',
    detail: '等待回答',
    updated_at: 101,
  };
  const aggregate = aggregateProjection(root, [child]);
  assert.equal(aggregate.session_id, 'root');
  assert.equal(aggregate.title, 'Main conversation');
  assert.equal(aggregate.state, 'waiting');
  assert.match(aggregate.detail, /^子任务 · /);
  assert.equal(aggregate.updated_at, 101);
  assert.equal(root.state, 'running', 'aggregation must not mutate the base projection');

  const lowerPriority = aggregateProjection(root, [{ ...child, state: 'done', updated_at: 102 }]);
  assert.equal(lowerPriority.state, 'running');
  assert.equal(lowerPriority.detail, root.detail);
  assert.equal(lowerPriority.updated_at, 102);
});

test('host tool-call warnings do not replace the meaningful session title', () => {
  const projection = createProjection(session, [
    event(1, 'user/message', { content: [{ type: 'text', text: '只显示最近的主会话' }] }),
    event(2, 'session/title', { title: 'You are repeating the exact same tool call with identical arguments. Carefully analyze the previous result.' }),
    event(3, 'session/title', { title: 'Background subagent 861fcc16-2775-436c-ad9b-580ca020ace7 finished and will do no further work' }),
    event(4, 'session/title', { title: 'Agent 861fcc16-2775-436c-ad9b-580ca020ace7 sent a message: internal review complete' }),
    event(5, 'session/title', { title: 'Its closing message: **Recommended implementation**' }),
    event(6, 'session/title', { title: 'background job pwsh-fc6129d1b5014859 (pwsh: .\\build.cmd) finished [status: completed]' }),
  ]);
  assert.equal(projection.title, '只显示最近的主会话');
  assert.equal(projection.lastEventSeq, 6);
  normalizeBootstrap(projection, 'idle');
  assert.equal(projection.updated_at, 1_700_000_001);
});

test('only genuine user messages establish titles when source metadata exists', () => {
  const projection = createProjection(session, [
    event(1, 'user/message', {
      content: [{ type: 'text', text: '**推荐最小接入路径**：子 Agent 的完整报告' }],
      source: { kind: 'agent-message', form: 'relay' },
    }),
    event(2, 'user/message', {
      content: [{ type: 'text', text: 'Time sampled while preparing turn 17, step 1' }],
      source: { kind: 'time-context', form: 'snapshot' },
    }),
    event(3, 'user/message', {
      content: [{ type: 'text', text: '你提交推送一下吧' }],
      source: { kind: 'user', rpcId: 'rpc-1' },
    }),
  ]);
  assert.equal(projection.title, '你提交推送一下吧');
  assert.equal(projection.lastEventSeq, 3);
});
