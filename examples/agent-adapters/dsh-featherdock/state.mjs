const TEXT_LIMIT = 96;
const DETAIL_LIMIT = 240;

/** Keep only short, display-safe text. Reasoning and request headers never enter here. */
export function publicText(value, limit = DETAIL_LIMIT) {
  let text = String(value ?? '')
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  text = text
    .replace(/\b(Bearer\s+)[A-Za-z0-9._~+\/-]+=*/gi, '$1[redacted]')
    .replace(/\b(?:sk|rk|pk)-(?:proj-|ant-)?[A-Za-z0-9_-]{12,}/g, '[redacted]')
    .replace(/((?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|passwd|secret|authorization|cookie)["']?\s*[:=]\s*)(?:"[^"\r\n]*"|'[^'\r\n]*'|[^\s,;}\r\n]+)/gi, '$1[redacted]');
  return text.slice(0, limit);
}

function messageText(content) {
  if (!Array.isArray(content)) return '';
  return content
    .filter((block) => block?.type === 'text')
    .map((block) => String(block.text || '').trim())
    .filter((text) => text && !isHarnessText(text))
    .slice(0, 12)
    .map((text) => publicText(text, 2048))
    .filter(Boolean)
    .join(' ');
}

function isHarnessText(text) {
  return /^(?:Time sampled while preparing turn\b|You are repeating the exact same tool call with identical arguments\.|Background subagent [0-9a-f-]+ finished\b|background job\b|Agent [0-9a-f-]+ sent a message:|Its closing message:|Current runtime context\.|This is an automatically generated checkpoint\b|<system-reminder>)/i.test(text.trim());
}

function cwdLeaf(cwd) {
  const normalized = String(cwd || '').replace(/[\\/]+$/, '');
  const leaf = normalized.split(/[\\/]/).at(-1);
  return leaf || 'DeepSeek Harness';
}

function eventSeconds(event) {
  const milliseconds = Number.isFinite(event?.time) ? event.time : Date.now();
  return Math.max(0, Math.floor(milliseconds / 1000));
}

/** Return the direct parent only for delegated work, never for a user fork. */
export function delegatedParentId(session) {
  const header = session?.header || {};
  const depth = Number(header.delegationDepth ?? header.subagentDepth ?? session?.subagentDepth ?? 0);
  const delegated = header.origin === 'subagent' || (Number.isSafeInteger(depth) && depth > 0);
  if (!delegated) return null;
  return typeof header.parentSession === 'string' && header.parentSession.trim() ? header.parentSession : '';
}

/** Resolve a child to its live top-level conversation; cycles/orphans stay hidden. */
export function resolveRootSessionId(session, getSession) {
  let current = session;
  const seen = new Set();
  for (let depth = 0; depth < 64; depth += 1) {
    const id = String(current?.id || '');
    if (!id || seen.has(id)) return null;
    seen.add(id);
    const parentId = delegatedParentId(current);
    if (parentId === null) return id;
    if (!parentId) return null;
    current = getSession(parentId);
    if (!current) return null;
  }
  return null;
}

const STATE_RANK = { idle: 0, done: 1, running: 2, error: 3, waiting: 4 };

/** Fold delegated activity into one root row without replacing its title or identity. */
export function aggregateProjection(root, children = []) {
  const aggregate = { ...root };
  let winner = root;
  let latest = Number(root?.updated_at) || 0;
  for (const child of children) {
    if (!child || child.session_id === root?.session_id) continue;
    latest = Math.max(latest, Number(child.updated_at) || 0);
    const childRank = STATE_RANK[child.state] ?? 0;
    const winnerRank = STATE_RANK[winner?.state] ?? 0;
    if (childRank > winnerRank || (winner !== root && childRank === winnerRank && (child.updated_at || 0) > (winner.updated_at || 0))) {
      winner = child;
    }
  }
  aggregate.updated_at = latest;
  if (winner !== root) {
    aggregate.state = winner.state;
    aggregate.detail = `子任务 · ${winner.detail}`;
  }
  return aggregate;
}

export function createProjection(session, recentEvents = []) {
  const cwd = typeof session?.header?.cwd === 'string' ? session.header.cwd : null;
  const projection = {
    agent: 'dsh',
    session_id: String(session?.id || ''),
    title: cwdLeaf(cwd),
    detail: '已接入任务事件 · 等待新的对话',
    cwd,
    state: 'idle',
    updated_at: Math.floor(Date.now() / 1000),
    titleLocked: false,
  };
  for (const event of recentEvents) applySessionEvent(projection, event);
  projection.lastEventSeq = recentEvents.at(-1)?.seq;
  return projection;
}

/** Normalize history during plugin startup without re-alerting old completed work. */
export function normalizeBootstrap(projection, agentStatus) {
  if (agentStatus === 'running') {
    if (projection.state !== 'waiting') {
      projection.state = 'running';
      projection.detail = '对话进行中';
      projection.updated_at = Math.floor(Date.now() / 1000);
    }
  } else {
    projection.state = 'idle';
    projection.detail = '会话已就绪 · 等待新的对话';
  }
  return projection;
}

/** Fold one authoritative Session event into the small public Dock projection. */
export function applySessionEvent(projection, event) {
  if (!projection || !event || typeof event.type !== 'string') return false;
  const data = event.data || {};
  let changed = true;
  switch (event.type) {
    case 'session/title': {
      // The first meaningful title is stable for this projection. DSH may emit
      // automatic titles for later host/tool notifications; those must not
      // replace the user's actual conversation title during bootstrap or live use.
      if (projection.titleLocked) return false;
      const raw = String(data.title || data.text || data.value || '');
      if (isHarnessText(raw) || /^(?:dsh|deepseek harness|new session|新会话|untitled)$/i.test(raw.trim())) {
        return false;
      }
      const title = publicText(raw, TEXT_LIMIT);
      if (!title) return false;
      projection.title = title;
      projection.titleLocked = true;
      break;
    }
    case 'user/message': {
      // Host notifications also use the user/message envelope. Only genuine
      // user-authored text may establish a title; legacy events without source
      // metadata retain the existing public-text fallback.
      const sourceKind = data.source?.kind;
      if (sourceKind && sourceKind !== 'user') return false;
      if (projection.titleLocked) return false;
      const title = publicText(messageText(data.content), TEXT_LIMIT);
      if (!title) return false;
      projection.title = title;
      projection.titleLocked = true;
      break;
    }
    case 'working-directory/change': {
      const cwd = data.cwd || data.path || data.workingDirectory;
      if (typeof cwd !== 'string' || !cwd.trim()) return false;
      projection.cwd = cwd;
      if (!projection.titleLocked && projection.title !== cwdLeaf(cwd)) {
        projection.title = cwdLeaf(cwd);
      }
      break;
    }
    case 'turn/start':
      projection.state = 'running';
      projection.detail = `对话进行中 · 第 ${Number.isSafeInteger(data.turn) ? data.turn : '?'} 轮`;
      break;
    case 'approval/asked':
      projection.state = 'waiting';
      projection.detail = `等待审批 · ${publicText(data.toolName || '工具调用', TEXT_LIMIT)}`;
      break;
    case 'approval/decided':
      projection.state = 'running';
      projection.detail = `审批已处理 · ${publicText(data.outcome || '已决定', TEXT_LIMIT)}`;
      break;
    case 'tool/call': {
      const name = publicText(data.name || '工具', TEXT_LIMIT);
      if (name === 'ask_user_question') {
        projection.state = 'waiting';
        projection.detail = '等待你的回答';
      } else {
        projection.state = 'running';
        projection.detail = `执行工具 · ${name}`;
      }
      break;
    }
    case 'tool/result':
      if (projection.state !== 'waiting') return false;
      projection.state = 'running';
      projection.detail = '已收到你的回答 · 继续执行';
      break;
    case 'turn/end': {
      const reason = data.reason || {};
      switch (reason.kind) {
        case 'completed':
          projection.state = 'done';
          projection.detail = '本轮对话已完成';
          break;
        case 'error': {
          projection.state = 'error';
          const code = publicText(reason.error?.code || 'ERROR', 80);
          const message = publicText(reason.error?.message || '对话执行失败', DETAIL_LIMIT);
          projection.detail = `${code} · ${message}`;
          break;
        }
        case 'blocked':
          projection.state = 'waiting';
          projection.detail = '对话被阻止 · 需要处理后再继续';
          break;
        case 'max-tokens':
          projection.state = 'error';
          projection.detail = '达到本轮输出上限';
          break;
        case 'aborted':
        case 'interrupted':
          projection.state = 'idle';
          projection.detail = '本轮对话已停止';
          break;
        default:
          projection.state = 'idle';
          projection.detail = '本轮对话已结束';
          break;
      }
      break;
    }
    default:
      changed = false;
  }
  if (changed) projection.updated_at = eventSeconds(event);
  return changed;
}
