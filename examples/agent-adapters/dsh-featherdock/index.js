import { randomUUID } from 'node:crypto';
import {
  aggregateProjection,
  applySessionEvent,
  createProjection,
  normalizeBootstrap,
  resolveRootSessionId,
} from './state.mjs';
import { DockTransport } from './transport.mjs';

export const name = 'featherdock-agent-bridge';
export const inject = ['agents'];
const RECENT_EVENT_LIMIT = 4096;

/** DSH Host lifecycle → FeatherDock Track B. No polling and no child processes. */
export function apply(ctx, config = {}) {
  const instance = config.instance || `dsh-${process.pid}-${Date.now()}-${randomUUID()}`;
  const transport = new DockTransport({
    directory: config.directory,
    instance,
    pid: process.pid,
    coalesceMs: config.coalesceMs ?? 40,
    logger: console,
  });
  const projections = new Map();
  const sessions = new Map();
  const roots = new Map();
  let disposed = false;

  function liveSession(id) {
    return sessions.get(id) || ctx.agents.get(id)?.session;
  }

  function rebuildRoots() {
    roots.clear();
    for (const [id, session] of sessions) {
      roots.set(id, resolveRootSessionId(session, liveSession));
    }
  }

  function publishRoot(rootId) {
    if (disposed || !rootId) return;
    const root = projections.get(rootId);
    if (!root) return;
    const children = [];
    for (const [id, projection] of projections) {
      if (id !== rootId && roots.get(id) === rootId) children.push(projection);
    }
    transport.schedule(aggregateProjection(root, children));
  }

  function publishFor(id) {
    publishRoot(roots.get(id));
  }

  function publishAllRoots() {
    for (const rootId of new Set(roots.values())) publishRoot(rootId);
  }

  function bootstrap(agent) {
    const session = agent.session;
    const inherited = Number.isSafeInteger(session.inheritedEventCount) ? session.inheritedEventCount : 0;
    const first = Math.max(inherited, session.seq - RECENT_EVENT_LIMIT);
    const projection = createProjection(session, session.snapshotEvents(first));
    normalizeBootstrap(projection, agent.status);
    sessions.set(session.id, session);
    projections.set(session.id, projection);
  }

  function register(agent) {
    if (disposed || !agent?.session?.id || projections.has(agent.id)) return;
    bootstrap(agent);
    rebuildRoots();
    // A parent may be registered after its children during startup. Re-publish
    // every root so newly resolved descendants join their one visible row.
    publishAllRoots();
  }

  function observe(session, event) {
    if (disposed || !session?.id) return;
    let projection = projections.get(session.id);
    let created = false;
    if (!projection) {
      const agent = ctx.agents.get(session.id);
      if (!agent) return;
      bootstrap(agent);
      projection = projections.get(session.id);
      created = true;
      rebuildRoots();
    }
    // A projection created from the current log already contains this event.
    // Avoid applying it twice, but still publish a newly discovered projection.
    if (projection.lastEventSeq === event.seq) {
      if (created) publishFor(session.id);
      return;
    }
    projection.lastEventSeq = event.seq;
    if (applySessionEvent(projection, event)) publishFor(session.id);
  }

  function unregister(agent) {
    if (!agent?.id) return;
    const previousRoot = roots.get(agent.id);
    projections.delete(agent.id);
    sessions.delete(agent.id);
    roots.delete(agent.id);
    void transport.remove(agent.id);
    rebuildRoots();
    if (previousRoot && previousRoot !== agent.id) publishRoot(previousRoot);
  }

  const offEvent = ctx.on('session/event', observe);
  const offCreated = ctx.on('agent/created', ({ agent }) => register(agent));
  const offDisposed = ctx.on('agent/disposed', ({ agent }) => unregister(agent));
  for (const agent of ctx.agents.list()) register(agent);

  ctx.effect(() => async () => {
    disposed = true;
    offEvent();
    offCreated();
    offDisposed();
    await transport.dispose();
    projections.clear();
    sessions.clear();
    roots.clear();
  }, 'featherdock-agent-bridge: subscriptions and owned session files');
}
