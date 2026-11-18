import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readFile, rename, unlink, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

function defaultDirectory() {
  const root = process.env.LOCALAPPDATA || process.env.APPDATA || process.cwd();
  return join(root, 'FeatherDock', 'agent-sessions');
}

export function sessionFile(directory, sessionId) {
  const digest = createHash('sha256').update(String(sessionId)).digest('hex').slice(0, 32);
  return join(directory, `dsh-${digest}.json`);
}

async function atomicWrite(path, text) {
  const temporary = `${path}.tmp.${process.pid}.${randomUUID()}`;
  try {
    await writeFile(temporary, text, 'utf8');
    await rename(temporary, path);
  } catch (error) {
    await unlink(temporary).catch(() => {});
    throw error;
  }
}

function publicProjection(projection) {
  return {
    agent: 'dsh',
    session_id: projection.session_id,
    state: projection.state,
    title: projection.title,
    detail: projection.detail,
    cwd: projection.cwd || undefined,
    updated_at: projection.updated_at,
  };
}

/** Event-driven, per-session serialized atomic writer for FeatherDock Track B. */
export class DockTransport {
  constructor({ directory = defaultDirectory(), instance, pid = process.pid, coalesceMs = 40, logger = console } = {}) {
    if (!instance) throw new TypeError('DockTransport requires a stable instance id');
    this.directory = directory;
    this.instance = instance;
    this.pid = pid;
    this.coalesceMs = Math.max(0, coalesceMs | 0);
    this.logger = logger;
    this.seq = 0;
    this.latest = new Map();
    this.signatures = new Map();
    this.timers = new Map();
    this.queues = new Map();
    this.owned = new Set();
    this.disposed = false;
  }

  schedule(projection) {
    if (this.disposed) return;
    const id = projection.session_id;
    const signature = JSON.stringify(publicProjection(projection));
    if (this.signatures.get(id) === signature) return;
    this.signatures.set(id, signature);
    this.latest.set(id, { ...projection });
    this.owned.add(id);
    clearTimeout(this.timers.get(id));
    if (this.coalesceMs === 0) {
      this.publish(id);
      return;
    }
    this.timers.set(id, setTimeout(() => {
      this.timers.delete(id);
      this.publish(id);
    }, this.coalesceMs));
  }

  enqueue(id, operation) {
    const previous = this.queues.get(id) || Promise.resolve();
    const next = previous.then(operation).catch((error) => {
      this.logger.error?.('[featherdock] transport:', error?.message || error);
    });
    this.queues.set(id, next);
    return next;
  }

  publish(id) {
    if (this.disposed) return Promise.resolve();
    const projection = this.latest.get(id);
    if (!projection) return Promise.resolve();
    this.latest.delete(id);
    clearTimeout(this.timers.get(id));
    this.timers.delete(id);
    const payload = {
      ...publicProjection(projection),
      instance: this.instance,
      seq: ++this.seq,
      pid: this.pid,
    };
    const path = sessionFile(this.directory, id);
    return this.enqueue(id, async () => {
      try {
        await mkdir(this.directory, { recursive: true });
        await atomicWrite(path, JSON.stringify(payload));
      } catch (error) {
        // A later identical event may retry the failed publication instead of
        // being suppressed by the optimistic coalescing signature.
        this.signatures.delete(id);
        throw error;
      }
    });
  }

  remove(id) {
    clearTimeout(this.timers.get(id));
    this.timers.delete(id);
    this.latest.delete(id);
    this.signatures.delete(id);
    const path = sessionFile(this.directory, id);
    return this.enqueue(id, async () => {
      try {
        const current = JSON.parse(await readFile(path, 'utf8'));
        if (current.instance === this.instance) await unlink(path);
      } catch (error) {
        if (error?.code !== 'ENOENT') this.logger.warn?.('[featherdock] session cleanup:', error?.message || error);
      }
    });
  }

  async dispose() {
    if (this.disposed) return;
    this.disposed = true;
    for (const timer of this.timers.values()) clearTimeout(timer);
    this.timers.clear();
    this.latest.clear();
    const ids = [...this.owned];
    await Promise.all(ids.map((id) => this.remove(id)));
    await Promise.all(this.queues.values());
    this.owned.clear();
  }
}
