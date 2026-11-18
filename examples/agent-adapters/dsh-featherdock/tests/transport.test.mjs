import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, readdir, rm, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { DockTransport, sessionFile } from '../transport.mjs';

const projection = {
  session_id: 'session-transport',
  state: 'running',
  title: 'Dock test',
  detail: 'Running a real turn',
  cwd: 'C:\\Work\\Dock',
  updated_at: 100,
};
const quiet = { error() {}, warn() {} };
const exists = async (path) => !!(await stat(path).catch(() => null));

test('atomic publication increments versions and only the owning instance cleans files', async (t) => {
  const directory = await mkdtemp(join(tmpdir(), 'featherdock-transport-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const first = new DockTransport({ directory, instance: 'instance-a', pid: 1234, coalesceMs: 60_000, logger: quiet });
  first.schedule(projection);
  await first.publish(projection.session_id);
  const path = sessionFile(directory, projection.session_id);
  const running = JSON.parse(await readFile(path, 'utf8'));
  assert.deepEqual({
    agent: running.agent,
    session_id: running.session_id,
    state: running.state,
    instance: running.instance,
    seq: running.seq,
    pid: running.pid,
    updated_at: running.updated_at,
  }, {
    agent: 'dsh',
    session_id: 'session-transport',
    state: 'running',
    instance: 'instance-a',
    seq: 1,
    pid: 1234,
    updated_at: 100,
  });

  first.schedule({ ...projection, state: 'waiting', detail: 'Waiting for approval', updated_at: 101 });
  await first.publish(projection.session_id);
  const waiting = JSON.parse(await readFile(path, 'utf8'));
  assert.equal(waiting.state, 'waiting');
  assert.equal(waiting.seq, 2);
  assert.equal((await readdir(directory)).filter((name) => name.includes('.tmp.')).length, 0);

  const second = new DockTransport({ directory, instance: 'instance-b', pid: 1234, coalesceMs: 60_000, logger: quiet });
  second.schedule({ ...projection, state: 'idle', updated_at: 102 });
  await second.publish(projection.session_id);
  assert.equal(JSON.parse(await readFile(path, 'utf8')).instance, 'instance-b');

  await first.remove(projection.session_id);
  assert.equal(await exists(path), true, 'old instance must not remove replacement state');
  await second.dispose();
  assert.equal(await exists(path), false, 'owning instance cleans its own state');
  await first.dispose();
});

test('coalescing suppresses unchanged projections and dispose removes pending state', async (t) => {
  const directory = await mkdtemp(join(tmpdir(), 'featherdock-coalesce-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const transport = new DockTransport({ directory, instance: 'instance-c', coalesceMs: 60_000, logger: quiet });
  transport.schedule(projection);
  transport.schedule(projection);
  await transport.publish(projection.session_id);
  assert.equal(transport.seq, 1);
  await transport.dispose();
  assert.equal(await exists(sessionFile(directory, projection.session_id)), false);
});
