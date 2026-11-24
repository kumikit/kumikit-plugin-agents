// Records real Claude Code 2.1.x `stream-json` transcripts by driving the real `claude`
// binary through `kumikit.provider-claude` (WASM) + `kumikit serve` + `fake-acp-agent record`
// against a deterministic loopback Anthropic Messages API server (so recording works even
// when an account is rate-limited, and never touches the network or spends credits).
//
// Usage: KUMIKIT_DIR=../kumikit node scripts/record-claude.mjs
import { createServer } from 'node:http';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { homedir, tmpdir, userInfo } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { spawn } from 'node:child_process';

const here = resolve(import.meta.dirname, '..');
const kDir = process.env.KUMIKIT_DIR ?? resolve(here, '../kumikit');
const kumikit = join(kDir, 'target/debug/kumikit');
const fakeBinDir = join(kDir, 'target/kumikit-test/release');
const testToolDir = join(kDir, 'test-plugins/tool');
const testToolWasm = join(kDir, 'target/kumikit-test/wasm32-wasip2/release/kumikit_test_tool.wasm');
const claudeWasm = join(here, 'target/wasm32-wasip2/release/kumikit_provider_claude.wasm');

const scenarios = [
  { id: 'text', answer: 'allow' },
  { id: 'tool-allow', answer: 'allow' },
  { id: 'tool-deny', answer: 'deny' },
  { id: 'mcp', answer: 'allow' },
];

let currentScenario = 'text';
let turnInScenario = 0;

function sse(res, events) {
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  for (const [ev, data] of events) {
    res.write(`event: ${ev}\ndata: ${JSON.stringify(data)}\n\n`);
  }
  res.end();
}

function makeEvents(scenario, turn) {
  const msgStart = (id, inp) => [
    'message_start',
    {
      type: 'message_start',
      message: {
        id,
        type: 'message',
        role: 'assistant',
        content: [],
        model: 'claude-sonnet-4-5-20250929',
        stop_reason: null,
        stop_sequence: null,
        usage: { input_tokens: inp, output_tokens: 1 },
      },
    },
  ];
  const textTurn = (id, text, inp, out) => [
    msgStart(id, inp),
    ['content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }],
    ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } }],
    ['content_block_stop', { type: 'content_block_stop', index: 0 }],
    ['message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: out } }],
    ['message_stop', { type: 'message_stop' }],
  ];
  const toolTurn = (id, thinking, toolId, name, inputObj) => [
    msgStart(id, 25),
    ['content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'thinking', thinking: '' } }],
    ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'thinking_delta', thinking } }],
    ['content_block_stop', { type: 'content_block_stop', index: 0 }],
    ['content_block_start', { type: 'content_block_start', index: 1, content_block: { type: 'tool_use', id: toolId, name, input: {} } }],
    ['content_block_delta', { type: 'content_block_delta', index: 1, delta: { type: 'input_json_delta', partial_json: JSON.stringify(inputObj) } }],
    ['content_block_stop', { type: 'content_block_stop', index: 1 }],
    ['message_delta', { type: 'message_delta', delta: { stop_reason: 'tool_use', stop_sequence: null }, usage: { output_tokens: 20 } }],
    ['message_stop', { type: 'message_stop' }],
  ];

  if (scenario === 'text') {
    return textTurn('msg_text_1', 'hello from kumikit', 12, 5);
  }
  if (scenario === 'tool-allow') {
    return turn === 1
      ? toolTurn('msg_allow_1', 'I will create hello.txt.', 'toolu_allow_1', 'Bash', { command: 'printf hi > hello.txt' })
      : textTurn('msg_allow_2', 'Created hello.txt containing hi.', 40, 8);
  }
  if (scenario === 'tool-deny') {
    return turn === 1
      ? toolTurn('msg_deny_1', 'I will run touch denied.txt.', 'toolu_deny_1', 'Bash', { command: 'touch denied.txt' })
      : textTurn('msg_deny_2', 'Understood, I did not run touch denied.txt.', 40, 10);
  }
  if (scenario === 'mcp') {
    return turn === 1
      ? toolTurn('msg_mcp_1', 'Calling the kumikit echo tool.', 'toolu_mcp_1', 'mcp__kumikit__test_tool__echo', { text: 'ping' })
      : textTurn('msg_mcp_2', 'The kumikit echo tool answered: ping.', 55, 10);
  }
  return textTurn('msg_other', 'ok', 10, 2);
}

const server = createServer((req, res) => {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    if (req.url?.includes('/v1/messages')) {
      turnInScenario += 1;
      sse(res, makeEvents(currentScenario, turnInScenario));
    } else {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end('{}');
    }
  });
});

await new Promise((ok) => server.listen(0, '127.0.0.1', ok));
const { port } = server.address();
const baseUrl = `http://127.0.0.1:${port}`;

const tmp = mkdtempSync(join(tmpdir(), 'kkcl-'));
chmodSync(tmp, 0o700);
let core;
try {
  const ws = join(tmp, 'ws');
  const raw = join(tmp, 'raw');
  const claudePkg = join(tmp, 'claude-pkg');
  const toolPkg = join(tmp, 'tool-pkg');
  const recPkg = join(tmp, 'rec-pkg');
  mkdirSync(ws, { recursive: true });
  mkdirSync(raw, { recursive: true });
  mkdirSync(claudePkg, { recursive: true });
  mkdirSync(join(toolPkg, 'tools'), { recursive: true });
  mkdirSync(recPkg, { recursive: true });
  writeFileSync(join(ws, 'README.md'), '# Demo\n');

  copyFileSync(join(here, 'providers/claude/plugin.toml'), join(claudePkg, 'plugin.toml'));
  copyFileSync(join(here, 'providers/claude/claude.schema.json'), join(claudePkg, 'claude.schema.json'));
  copyFileSync(claudeWasm, join(claudePkg, 'plugin.wasm'));

  copyFileSync(join(testToolDir, 'plugin.toml'), join(toolPkg, 'plugin.toml'));
  for (const f of readdirSync(join(testToolDir, 'tools')).filter((n) => n.endsWith('.json'))) {
    copyFileSync(join(testToolDir, 'tools', f), join(toolPkg, 'tools', f));
  }
  copyFileSync(testToolWasm, join(toolPkg, 'plugin.wasm'));

  // Delegated recorder manifest using the `claude` backend!
  const entries = scenarios.map(
    (s) =>
      `  { id = ${JSON.stringify(s.id)}, name = ${JSON.stringify(s.id)}, claude = { command = "fake-acp-agent", args = ["record", ${JSON.stringify(join(raw, `${s.id}.jsonl`))}, "--", "claude", "--bare"], env = { ANTHROPIC_BASE_URL = ${JSON.stringify(baseUrl)}, ANTHROPIC_API_KEY = "sk-ant-test-dummy", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1" } } },`,
  );
  writeFileSync(
    join(recPkg, 'plugin.toml'),
    `id = "record.claude"\nname = "Claude Recorder"\nversion = "0.1.0"\nkind = ["provider"]\napi = "^0.2"\n[permissions]\nspawn = ["fake-acp-agent"]\ntools = true\n[contributes]\nproviders = [\n${entries.join('\n')}\n]\n`,
  );

  const socket = join(tmp, 's.sock');
  core = spawn(kumikit, ['serve', '--socket', socket, '--data-dir', join(tmp, 'data')], {
    env: { ...process.env, PATH: `${fakeBinDir}:${process.env.PATH}` },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  await new Promise((ok, fail) => {
    let errBuf = '';
    core.stderr.on('data', (d) => (errBuf += d));
    core.on('exit', (code) => fail(new Error(`core exited ${code}: ${errBuf}`)));
    const rl = createInterface({ input: core.stdout });
    rl.on('line', (l) => {
      if (l.startsWith('ready')) ok();
    });
  });

  const c = net.connect(socket);
  const lines = createInterface({ input: c });
  let next = 0;
  const waiting = new Map();
  const listeners = new Set();
  lines.on('line', (l) => {
    const m = JSON.parse(l);
    if (m.method === 'events.event') for (const f of listeners) f(m.params);
    else if (waiting.has(m.id)) {
      const w = waiting.get(m.id);
      waiting.delete(m.id);
      m.error ? w.fail(new Error(`${m.error.data?.code}: ${m.error.message}`)) : w.ok(m.result);
    }
  });
  const call = (method, params) =>
    new Promise((ok, fail) => {
      const id = ++next;
      waiting.set(id, { ok, fail });
      c.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });

  await call('core.hello', { protocol: '0.7', client: { name: 'claude-recorder', version: '0' } });
  for (const dir of [claudePkg, toolPkg, recPkg]) {
    const { plugin } = await call('plugin.install', { path: dir });
    await call('plugin.enable', { plugin: plugin.id, grant: plugin.requested });
  }
  const { workspace } = await call('workspace.open', { path: ws });

  for (const s of scenarios) {
    currentScenario = s.id;
    turnInScenario = 0;
    const { thread } = await call('thread.create', { workspace: workspace.id, provider: `record.claude/${s.id}` });
    const topic = `thread:${thread.id}`;
    const ended = new Promise((resolveDone, rejectDone) => {
      const timer = setTimeout(() => rejectDone(new Error(`${s.id}: timed out`)), 30_000);
      const f = (e) => {
        if (e.topic !== topic) return;
        if (e.kind === 'approval.requested') {
          console.log(`  ${s.id}: approval "${e.data.approval.summary}" -> ${s.answer}`);
          void call('thread.respondApproval', { approval: e.data.approval.id, decision: s.answer });
        } else if (e.kind === 'tool.started') {
          console.log(`  ${s.id}: tool ${e.data.name}${e.data.source ? ' (gateway)' : ''}`);
        } else if (e.kind === 'text.done') {
          console.log(`  ${s.id}: said ${JSON.stringify(e.data.text)}`);
        } else if (e.kind === 'turn.ended') {
          clearTimeout(timer);
          listeners.delete(f);
          resolveDone(e.data);
        }
      };
      listeners.add(f);
    });
    await call('events.subscribe', { topic, after: 0 });
    await call('thread.send', { thread: thread.id, input: { text: `scenario ${s.id}` } });
    const end = await ended;
    console.log(`${s.id}: ${end.outcome}${end.error ? ` (${end.error})` : ''}`);
    if (end.outcome !== 'done') throw new Error(`${s.id} failed: ${JSON.stringify(end)}`);
  }
  c.end();

  // Scrub and write into providers/claude/tests/transcripts/claude-<scenario>.jsonl
  const outDir = join(here, 'providers/claude/tests/transcripts');
  mkdirSync(outDir, { recursive: true });
  const user = userInfo().username;
  const home = homedir();
  for (const s of scenarios) {
    const rawLines = readFileSync(join(raw, `${s.id}.jsonl`), 'utf8').split('\n').filter(Boolean);
    const uuids = new Map();
    const kept = [];
    for (const line of rawLines) {
      const e = JSON.parse(line);
      const m = e.message ?? {};
      // Keep init control_response compact so transcripts stay small and free of user skills.
      if (e.from === 'agent' && m.type === 'control_response' && m.response?.response?.commands) {
        m.response.response = { claude_code_version: m.response.response.claude_code_version ?? '2.1.329' };
      }
      if (e.from === 'agent' && m.type === 'system' && m.subtype === 'init') {
        m.skills = [];
        m.slash_commands = [];
        m.plugins = [];
        delete m.memory_paths;
        delete m.messaging_socket_path;
      }
      let text = JSON.stringify(e);
      // Normalise UUIDs (session_id, uuid, request_id) deterministically.
      text = text.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, (u) => {
        if (!uuids.has(u)) uuids.set(u, `00000000-0000-0000-0000-${String(uuids.size + 1).padStart(12, '0')}`);
        return uuids.get(u);
      });
      // Scrub thread ids (ULIDs in gateway echo response).
      text = text.replace(/"thread":"[0-9A-HJKMNP-TV-Z]{26}"/g, '"thread":"01TESTTHREAD00000000000000"');
      text = text.split(ws).join('/workspace').split(tmp).join('/tmp-scrubbed').split(home).join('/home/user');
      kept.push(text);
    }
    const body = kept.join('\n') + '\n';
    for (const bad of [user, '/tmp/']) {
      const idx = bad ? body.indexOf(bad) : -1;
      if (idx !== -1) throw new Error(`${s.id}: still contains ${bad} (${body.slice(Math.max(0, idx - 30), idx + 60)})`);
    }
    writeFileSync(join(outDir, `claude-${s.id}.jsonl`), body);
    console.log(`wrote providers/claude/tests/transcripts/claude-${s.id}.jsonl (${kept.length} lines)`);
  }
} finally {
  if (core) core.kill();
  server.close();
  rmSync(tmp, { recursive: true, force: true });
}
