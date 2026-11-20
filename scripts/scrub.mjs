// Scrubs raw recordings (fake-acp-agent record output) so nothing personal remains, and
// writes them as providers/<name>/tests/transcripts/<name>-<scenario>.jsonl:
// - drops auth and available-commands notifications (account, installed skills);
// - removes every `_meta` object;
// - session ids become session-1, session-2…; the workspace path becomes /workspace;
//   home directories /home/user; the tool gateway's command, socket and token fixed values.
// Then it fails if the user name, the email or a /tmp path is still in a file.
// Usage: node scrub.mjs <raw dir> <out dir> <name> <workspace path>
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { homedir, userInfo } from 'node:os';

const [raw, out, name, workspace] = process.argv.slice(2);
const user = userInfo().username;
const home = homedir();
const drop = new Set(['auth/status_update', '_auth/status_update']);
const updateDrop = new Set(['available_commands_update']);

function clean(v) {
  if (Array.isArray(v)) return v.map(clean);
  if (v && typeof v === 'object') {
    const o = {};
    for (const [k, x] of Object.entries(v)) if (k !== '_meta') o[k] = clean(x);
    return o;
  }
  return v;
}

for (const file of readdirSync(raw).filter((f) => f.endsWith('.jsonl'))) {
  const sessions = new Map();
  const kept = [];
  for (const line of readFileSync(`${raw}/${file}`, 'utf8').split('\n').filter(Boolean)) {
    const e = JSON.parse(line);
    const m = e.message ?? {};
    if (drop.has(m.method)) continue;
    if (m.method === 'session/update' && updateDrop.has(m.params?.update?.sessionUpdate)) continue;
    for (const s of m.params?.mcpServers ?? []) {
      s.command = '/kumikit';
      s.args = s.args.map((a, i, all) => (all[i - 1] === '--socket' ? '/socket' : a));
      for (const v of s.env ?? []) if (v.name === 'KUMIKIT_MCP_TOKEN') v.value = 'token';
    }
    let text = JSON.stringify(clean(e));
    for (const id of text.match(/"sessionId":"([^"]+)"/g) ?? []) {
      const raw = id.slice('"sessionId":"'.length, -1);
      if (!sessions.has(raw)) sessions.set(raw, `session-${sessions.size + 1}`);
    }
    for (const [from, to] of sessions) text = text.split(from).join(to);
    text = text.split(workspace).join('/workspace').split(workspace.replace(/\/ws$/, '')).join('/tmp-scrubbed').split(home).join('/home/user');
    kept.push(text);
  }
  const body = kept.join('\n') + '\n';
  for (const bad of [user, '/tmp/']) {
    const idx = bad ? body.indexOf(bad) : -1;
    if (idx !== -1) {
      const ctx = body.slice(Math.max(0, idx - 40), idx + 80);
      throw new Error(`${file}: still contains ${bad} (${JSON.stringify(ctx)}); scrub by hand`);
    }
  }
  if (/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}/.test(body)) throw new Error(`${file}: still contains an email address`);
  writeFileSync(`${out}/${name}-${file}`, body);
  console.log(`${out}/${name}-${file}: ${kept.length} messages`);
}
