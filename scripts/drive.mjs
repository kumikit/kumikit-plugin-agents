// Plays the recording scenarios through a core's local socket: installs provider-acp,
// the gateway's test tools and the recorder, then for each scenario starts a thread on
// its recording provider, sends the prompt, answers approvals as the scenario says, and
// waits for the turn to end (4 minutes at most).
// Usage: node drive.mjs <socket> <workspace> <acp dir> <tool dir> <recorder dir> <scenarios.json> [id…]
import { readFileSync } from 'node:fs';
import net from 'node:net';
import { createInterface } from 'node:readline';

const [socket, workspace, acpDir, toolDir, recorderDir, scenariosFile, ...only] = process.argv.slice(2);
const scenarios = JSON.parse(readFileSync(scenariosFile, 'utf8')).filter((s) => only.length === 0 || only.includes(s.id));
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

await call('core.hello', { protocol: '0.9', client: { name: 'kumikit-agents-recorder', version: '0' } });
let recorder;
for (const dir of [acpDir, toolDir, recorderDir]) {
  const { plugin } = await call('plugin.install', { path: dir });
  await call('plugin.enable', { plugin: plugin.id, grant: plugin.requested });
  recorder = plugin.id;
}
const { workspace: ws } = await call('workspace.open', { path: workspace });

for (const s of scenarios) {
  const { thread } = await call('thread.create', { workspace: ws.id, provider: `${recorder}/${s.id}` });
  const topic = `thread:${thread.id}`;
  const ended = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${s.id}: no turn.ended within 4 minutes`)), 240_000);
    const f = (e) => {
      if (e.topic !== topic) return;
      if (e.kind === 'approval.requested') {
        console.log(`  ${s.id}: approval "${e.data.approval.summary}" -> ${s.answer}`);
        void call('thread.respondApproval', { approval: e.data.approval.id, decision: s.answer });
      } else if (e.kind === 'tool.started') console.log(`  ${s.id}: tool ${e.data.name}${e.data.source ? ' (gateway)' : ''}`);
      else if (e.kind === 'text.done') console.log(`  ${s.id}: said ${JSON.stringify(e.data.text.slice(0, 200))}`);
      else if (e.kind === 'turn.ended') {
        clearTimeout(timer);
        listeners.delete(f);
        resolve(e.data);
      }
    };
    listeners.add(f);
  });
  await call('events.subscribe', { topic, after: 0 });
  await call('thread.send', { thread: thread.id, input: { text: s.prompt } });
  const end = await ended;
  console.log(`${s.id}: ${end.outcome}${end.error ? ` (${end.error})` : ''}`);
}
c.end();
