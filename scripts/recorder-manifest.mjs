// Prints a recorder copy of a provider manifest: one provider per scenario, whose
// command is `fake-acp-agent record <raw>/<scenario>.jsonl -- <the agent> <args>`.
// Optional env overrides for local recording without editing the manifest:
// - RECORD_EXTRA_ARGS: JSON array of extra CLI arguments appended to the agent command
// - RECORD_EXTRA_ENV:  JSON object of extra environment variables for the agent process
// Usage: node recorder-manifest.mjs <plugin.toml> <raw dir> <scenarios.json>
import { readFileSync } from 'node:fs';

const [manifest, raw, scenarios] = process.argv.slice(2);
const text = readFileSync(manifest, 'utf8');
// The rest of the acp table (model-arg, models, mode, auth-method) is kept as it is.
const m = text.match(/acp = \{ command = "([^"]+)", args = \[([^\]]*)\](.*) \} \}\]/);
if (!m) throw new Error(`${manifest}: no acp = { command, args } entry`);
const [, command, args, extra] = m;
const id = text.match(/^id = "([^"]+)"/m)[1];
const q = (s) => JSON.stringify(s);
const extraArgs = process.env.RECORD_EXTRA_ARGS ? JSON.parse(process.env.RECORD_EXTRA_ARGS) : [];
const extraEnv = process.env.RECORD_EXTRA_ENV ? JSON.parse(process.env.RECORD_EXTRA_ENV) : null;
const envSnippet = extraEnv
  ? `, env = { ${Object.entries(extraEnv)
      .map(([k, v]) => `${q(k)} = ${q(v)}`)
      .join(', ')} }`
  : '';
const agentArgs = [
  q(command),
  ...(args.trim() ? [args] : []),
  ...extraArgs.map(q),
].join(', ');
const entries = JSON.parse(readFileSync(scenarios, 'utf8')).map(
  (s) =>
    `  { id = ${q(s.id)}, name = ${q(s.id)}, acp = { command = "fake-acp-agent", args = ["record", ${q(`${raw}/${s.id}.jsonl`)}, "--", ${agentArgs}]${extra}${envSnippet} } },`,
);
process.stdout.write(`id = "record.${id.split('.')[1]}"
name = "Recorder"
version = "0.1.0"
kind = ["provider"]
api = "^0.2"
[permissions]
spawn = ["fake-acp-agent"]
tools = true
[contributes]
providers = [
${entries.join('\n')}
]
`);
