import { fork } from 'node:child_process';
import { createRequire } from 'node:module';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { release } from 'node:os';
import { createInterface } from 'node:readline';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
const engineRoot = process.env.NOD_HOST_ENGINE_ROOT;
const runtimeRoot = process.env.NOD_HOST_RUNTIME_ROOT;
const botsRoot = process.env.NOD_HOST_BOTS_ROOT;
const appVersion = process.env.NOD_HOST_APP_VERSION || '0.0.0';

const engines = new Map();
const secrets = new Set();
const stopping = new Set();
let storage;
let pg;
let shuttingDown;

function reply(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function log(line) {
  let value = String(line);
  for (const secret of secrets) value = value.split(secret).join('<redacted>');
  value = value.replace(/postgres(?:ql)?:\/\/[^\s'"]+/gi, '<database>');
  process.stderr.write(`${new Date().toISOString()} ${value}\n`);
}

function baseEnvironment() {
  return Object.fromEntries(
    [
      'HOME',
      'USER',
      'LOGNAME',
      'TMPDIR',
      'LANG',
      'LC_ALL',
      'PATH',
      'SHELL',
      'SYSTEMROOT',
      'USERPROFILE',
      'LOCALAPPDATA',
      'APPDATA',
      'TEMP',
      'TMP',
      'COMSPEC',
      'WINDIR',
      'PROGRAMDATA',
      'NOD_FEEDBACK_ENABLED',
      'NOD_FEEDBACK_URL',
      'NOD_FEEDBACK_TOKEN',
      'NOD_FEEDBACK_RECEIPT_URL_PREFIX',
      'NOD_TRACES_URL',
      'NOD_TRACES_TOKEN',
      'NOD_ONBOARDING_EMAIL_URL',
      'OTEL_EXPORTER_OTLP_ENDPOINT',
      'AWS_BEDROCK_FORCE_HTTP1',
    ]
      .filter((key) => typeof process.env[key] === 'string')
      .map((key) => [key, process.env[key]]),
  );
}

function databaseFor(url, name) {
  const parsed = new URL(url);
  parsed.pathname = `/${name}`;
  return parsed.toString();
}

async function ensureDatabase(baseUrl, name) {
  const client = new pg.Client({
    connectionString: databaseFor(baseUrl, 'postgres'),
    connectionTimeoutMillis: 10_000,
  });
  await client.connect();
  try {
    try {
      await client.query(`CREATE DATABASE "${name}"`);
    } catch (error) {
      if (error.code !== '42P04') throw error;
    }
  } finally {
    await client.end().catch(() => {});
  }
}

function subscribe(botId, origin, token) {
  const controller = new AbortController();
  let closed = false;
  (async () => {
    let backoff = 1000;
    let announced = false;
    while (!closed && !shuttingDown) {
      try {
        const response = await fetch(`${origin}/api/events`, {
          headers: { 'x-nod-desktop-token': token, accept: 'text/event-stream' },
          signal: controller.signal,
        });
        if (!response.ok || !response.body) throw new Error(`HTTP ${response.status}`);
        if (!announced) {
          announced = true;
          reply({ type: 'bot-event', botId, event: { type: 'refresh' } });
        }
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = '';
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, '\n');
          let index;
          while ((index = buffer.indexOf('\n\n')) !== -1) {
            const frame = buffer.slice(0, index);
            buffer = buffer.slice(index + 2);
            const data = frame
              .split('\n')
              .filter((line) => line.startsWith('data:'))
              .map((line) => line.slice(5).trimStart())
              .join('\n');
            if (!data) continue;
            try {
              reply({ type: 'bot-event', botId, event: JSON.parse(data) });
            } catch {}
          }
        }
      } catch {
        if (closed || shuttingDown) return;
      }
      if (closed || shuttingDown) return;
      await new Promise((resolve) => setTimeout(resolve, backoff));
      backoff = Math.min(backoff * 2, 30_000);
    }
  })();
  return () => {
    closed = true;
    controller.abort();
  };
}

async function startBot(spec) {
  if (engines.has(spec.id)) {
    const entry = engines.get(spec.id);
    reply({ type: 'bot-ready', botId: spec.id, origin: entry.origin });
    return;
  }
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(spec.id))
    throw new Error('This bot has an invalid identity.');
  if (!/^[A-Za-z0-9_-]{43,}$/.test(spec.token)) throw new Error('This bot has an invalid token.');
  secrets.add(spec.token);
  secrets.add(spec.vaultKey);
  const runtime = await runtimePromise;
  if (!runtime) throw new Error('Your Nod computer is not ready.');
  if (shuttingDown) return;
  const dataRoot = spec.profile;
  mkdirSync(dataRoot, { recursive: true, mode: 0o700 });
  const database = `nod_${spec.id.replaceAll('-', '').slice(0, 12)}`;
  reply({ type: 'bot-status', botId: spec.id, message: 'Preparing the database for this bot...' });
  await ensureDatabase(runtime.env.DATABASE_URL, database);
  if (shuttingDown) return;
  const origin = `http://127.0.0.1:${spec.port}`;
  const environment = {
    ...baseEnvironment(),
    ...runtime.env,
    ELECTRON_RUN_AS_NODE: '1',
    NOD_ENGINE_ENTRY: join(engineRoot, 'desktop', 'main', 'engine.mjs'),
    NODE_ENV: 'production',
    PORT: String(spec.port),
    NOD_DESKTOP: 'true',
    NOD_HOST: '127.0.0.1',
    NOD_APP_VERSION: appVersion,
    NOD_ELECTRON_VERSION: process.versions.electron ?? '',
    NOD_OS_VERSION: release(),
    NOD_DESKTOP_OWNER_ID: spec.owner,
    NOD_DESKTOP_AUTH_TOKEN: spec.token,
    NOD_DESKTOP_RUNTIME_PENDING: 'true',
    NOD_ANALYTICS_ENABLED: 'false',
    NOD_ANALYTICS_ENVIRONMENT: 'production',
    NOD_PUBLIC_URL: origin,
    NOD_DATA_ROOT: dataRoot,
    NOD_WEB_ROOT: join(engineRoot, 'dist', 'web'),
    NOD_VAULT_KEY: spec.vaultKey,
    PI_CODING_AGENT_DIR: join(dataRoot, 'pi'),
    DATABASE_URL: databaseFor(runtime.env.DATABASE_URL, database),
    DATABASE_POOL_MAX: '6',
    PGBOSS_SCHEMA: 'pgboss',
    WORKSPACE_CONTAINER: runtime.env.WORKSPACE_CONTAINER,
    BROWSER_CONTAINER: runtime.env.BROWSER_CONTAINER,
  };
  reply({ type: 'bot-status', botId: spec.id, message: 'Starting the engine for this bot...' });
  const child = fork(join(here, 'engine-shim.mjs'), [], {
    cwd: engineRoot,
    env: environment,
    execPath: process.execPath,
    stdio: ['ignore', 'ignore', 'pipe', 'ipc'],
  });
  let diagnostics = '';
  let unsubscribe = () => {};
  child.stderr.on('data', (chunk) => {
    diagnostics = (diagnostics + chunk.toString()).slice(-4000);
    for (const line of chunk.toString().split('\n')) if (line.trim()) log(`engine[${spec.id}]: ${line}`);
  });
  child.on('message', (message) => {
    if (!message || typeof message !== 'object') return;
    if (message.type === 'nod-ready' && message.port === spec.port) {
      child.send({ type: 'nod-runtime-ready' });
      unsubscribe = subscribe(spec.id, origin, spec.token);
      const entry = engines.get(spec.id);
      if (entry) entry.ready = true;
      reply({ type: 'bot-ready', botId: spec.id, origin });
      return;
    }
    if (message.type === 'nod-trash') {
      child.send({
        type: 'nod-trash-result',
        id: message.id,
        error: 'Moving items to the Trash is not available in this Nod build yet.',
      });
      return;
    }
    if (message.type === 'nod-computer') {
      child.send({
        type: 'nod-computer-result',
        id: message.id,
        error: 'Computer use is not available in this Nod build yet.',
      });
      return;
    }
    if (message.type === 'nod-working' && typeof message.working === 'boolean')
      reply({ type: 'bot-working', botId: spec.id, working: message.working });
  });
  child.on('exit', (code) => {
    unsubscribe();
    engines.delete(spec.id);
    const line = diagnostics
      .split('\n')
      .reverse()
      .find((value) => /^(?:Error|TypeError|SyntaxError|ReferenceError|RangeError)/.test(value));
    if (!stopping.has(spec.id) && !shuttingDown)
      reply({
        type: 'bot-exit',
        botId: spec.id,
        code,
        message: `The engine for this bot stopped${Number.isInteger(code) ? ` (code ${code})` : ''}.${line ? ` ${line.replace(/https?:\/\/\S+/g, '<url>')}` : ''}`,
      });
  });
  engines.set(spec.id, { child, origin, unsubscribe });
}

async function stopBot(botId) {
  const entry = engines.get(botId);
  if (!entry) return;
  stopping.add(botId);
  engines.delete(botId);
  entry.unsubscribe();
  await new Promise((resolve) => {
    const timeout = setTimeout(() => {
      entry.child.kill('SIGKILL');
      resolve();
    }, 20_000);
    entry.child.once('exit', () => {
      clearTimeout(timeout);
      resolve();
    });
    try {
      if (entry.child.connected) entry.child.send({ type: 'nod-shutdown' });
      else entry.child.kill();
    } catch {
      entry.child.kill();
    }
  });
  reply({ type: 'bot-stopped', botId });
}

async function shutdown() {
  if (shuttingDown) return;
  shuttingDown = true;
  await Promise.allSettled([...engines.keys()].map((id) => stopBot(id)));
  try {
    await storage?.close?.();
  } catch (error) {
    reply({ type: 'error', message: `Your computer could not stop cleanly: ${error.message}` });
  }
  reply({ type: 'stopped' });
  process.exit(0);
}

const runtimePromise = (async () => {
  const require = createRequire(join(engineRoot, 'package.json'));
  pg = require('pg');
  mkdirSync(runtimeRoot, { recursive: true, mode: 0o700 });
  const { prepareRuntime } = await import(
    pathToFileURL(join(engineRoot, 'desktop', 'runtime', 'index.mjs')).href
  );
  const controller = new AbortController();
  process.on('SIGTERM', () => controller.abort());
  storage = await prepareRuntime({
    dataRoot: runtimeRoot,
    resourcesPath: engineRoot,
    signal: controller.signal,
    onStatus: (value) =>
      reply({
        type: 'status',
        phase: 'preparing',
        stage: typeof value === 'object' ? (value.stage ?? 'computer') : 'computer',
        message: typeof value === 'string' ? value : value?.message,
        progress: typeof value === 'object' ? value.progress : undefined,
      }),
  });
  reply({ type: 'runtime-ready' });
  return storage;
})().catch((error) => {
  log(`failed: ${error.stack || error.message}`);
  reply({ type: 'error', message: error.message });
  return undefined;
});

createInterface({ input: process.stdin }).on('line', (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (!message) return;
  if (message.type === 'start') {
    runtimePromise
      .then((runtime) => {
        if (!runtime) return;
        return startBot(message.bot);
      })
      .catch((error) =>
        reply({ type: 'bot-exit', botId: message.bot?.id, message: error.message }),
      );
  } else if (message.type === 'stop') void stopBot(message.botId);
  else if (message.type === 'quit') void shutdown();
});

process.stdin.on('end', () => void shutdown());

