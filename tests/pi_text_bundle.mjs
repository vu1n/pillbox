// Offline qualification against the actual npm Pi1.0.2 bundle. No sockets or
// inference are allowed. Pass an extracted package root as the only argument.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import net from 'node:net';
import { catalog, runTurn } from '../src/execution/pi_text/driver.mjs';

const root = process.argv[2];
if (!root) throw new Error('Pass an extracted @earendil-works/pi-coding-agent@1.0.2 package root');
process.env.PI_OFFLINE = '1';
const manifest = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'));
assert.equal(manifest.name, '@earendil-works/pi-coding-agent');
assert.equal(manifest.version, '1.0.2');
// This is the shipped SDK export chunk in the qualified npm release, not a
// replacement/fake session implementation or a request-visible package pin.
const bundle = await import(pathToFileURL(join(root, 'dist/bundle/chunks/chunk-ZSBPJAJ2.js')));
net.Socket.prototype.connect = () => { throw new Error('Real network forbidden in offline qualification'); };
let responseEvents;
let separator = '\n\n';
let fetchCalls = 0;
const payloads = [];
globalThis.fetch = async url => {
  fetchCalls++;
  assert.equal(url, 'https://chatgpt.com/backend-api/codex/responses');
  return new Response(responseEvents.map(event => 'data: ' + JSON.stringify(event) + separator).join(''),
    { status: 200, headers: { 'content-type': 'text/event-stream' } });
};
const sdk = { ...bundle, VERSION: manifest.version,
  createAgentSession: async options => {
    const created = await bundle.createAgentSession(options);
    assert.deepEqual(created.session.getActiveToolNames(), []);
    assert.deepEqual(created.session.getAllTools(), []);
    assert.deepEqual(created.session.getCallableToolNames(), []);
    let transform;
    Object.defineProperty(created.session.agent, 'onPayload', {
      get: () => transform,
      set: callback => { transform = async payload => {
        const transformed = await callback(payload);
        payloads.push(transformed);
        return transformed;
      }; },
    });
    return created;
  } };
const resolved = await catalog(sdk);
assert.equal(fetchCalls, 0);
assert(resolved.models.some(model => model.id === 'gpt-6-luna'));
const access = 'h.' + Buffer.from(JSON.stringify({
  'https://api.openai.com/auth': { chatgpt_account_id: 'synthetic-account' },
})).toString('base64url') + '.s';
const request = { model: 'gpt-6-luna', reasoning_effort: 'low', timeout_ms: 1000, input: 'offline proof', max_frame_bytes: 1048576, max_evidence_bytes: 8388608,
  credential: { type: 'oauth', access, refresh: 'synthetic-unusable-refresh', expires: 4102444800000 } };
const message = { id: 'msg1', type: 'message', role: 'assistant', status: 'completed', phase: 'final_answer',
  content: [{ type: 'output_text', text: 'OFFLINE_OK', annotations: [] }] };
const success = [
  { type: 'response.created', response: { id: 'resp1', status: 'in_progress' } },
  { type: 'response.output_item.added', output_index: 0, item: { ...message, status: 'in_progress', content: [] } },
  { type: 'response.content_part.added', output_index: 0, content_index: 0, part: { type: 'output_text', text: '', annotations: [] } },
  { type: 'response.output_text.delta', output_index: 0, content_index: 0, delta: 'OFFLINE_OK' },
  { type: 'response.output_item.done', output_index: 0, item: message },
  { type: 'response.completed', response: { id: 'resp1', status: 'completed', model: 'authoritative-response-model',
    output: [message], usage: { input_tokens: 100, output_tokens: 5,
      input_tokens_details: { cached_tokens: 20 }, output_tokens_details: { reasoning_tokens: 2 } } } },
];

async function invoke(events) {
  responseEvents = events;
  const captured = [];
  const before = fetchCalls;
  await runTurn(sdk, request, event => captured.push(event));
  assert.equal(fetchCalls - before, 1);
  const payload = payloads.at(-1);
  assert.equal(payload.tools?.length ?? 0, 0);
  assert.equal(payload.tool_choice, 'none');
  assert.equal(payload.parallel_tool_calls, false);
  return captured;
}

const captured = await invoke(success);
const terminal = captured.find(event => event.type === 'message_end' && event.message.role === 'assistant');
assert.equal(terminal.message.stopReason, 'stop');
assert.equal(terminal.message.content.find(block => block.type === 'text').text, 'OFFLINE_OK');
assert.equal(terminal.message.usage.input, 80);
assert.equal(terminal.message.usage.cacheRead, 20);
assert.equal(terminal.pillbox_pi_native_usage.input_tokens, 100);
assert.equal(captured.at(-1).served_model, 'authoritative-response-model');
for (const type of ['function_call', 'custom_tool_call', 'mcp_call', 'web_search_call', 'mcp_list_tools', 'shell_call']) {
  const denied = await invoke([{ type: 'response.output_item.added', output_index: 0,
    item: { type, id: 'denied', name: 'edit', call_id: 'denied', arguments: '{}' } }]);
  assert(!denied.some(event => event.type.startsWith('tool_execution')));
  const end = denied.find(event => event.type === 'message_end' && event.message.role === 'assistant');
  assert.equal(end.message.stopReason, 'error');
  assert.match(end.message.errorMessage, /tool call denied/);
}
const incomplete = await invoke([{ type: 'response.incomplete', response: { id: 'incomplete', status: 'incomplete', usage: { input_tokens: 25, output_tokens: 3, input_tokens_details: { cached_tokens: 5 } } } }]);
assert.equal(incomplete.find(event => event.type === 'pillbox_pi.usage').usage.output_tokens, 3);
assert.equal(incomplete.find(event => event.type === 'message_end' && event.message.role === 'assistant').message.stopReason, 'error');
const writtenEvents = structuredClone(success);
writtenEvents.at(-1).response.usage.input_tokens_details.cache_write_tokens = 10;
const written = await invoke(writtenEvents);
const writtenEnd = written.find(event => event.type === 'message_end' && event.message.role === 'assistant');
assert.equal(writtenEnd.message.usage.input, 70);
assert.equal(writtenEnd.message.usage.cacheWrite, 10);
for (const native of [undefined, {}, { input_tokens: 100 }, { output_tokens: 5 }]) {
  const events = structuredClone(success);
  events.at(-1).response.usage = native;
  const result = await invoke(events);
  const end = result.find(event => event.type === 'message_end' && event.message.role === 'assistant');
  assert.deepEqual(end.pillbox_pi_native_usage, native);
}
for (const type of ['mcp_list_tools', 'shell_call', 'function_call']) {
  const events = structuredClone(success);
  events.at(-1).response.output.push({ type });
  const result = await invoke(events);
  assert.equal(result.find(event => event.type === 'message_end' && event.message.role === 'assistant').message.stopReason, 'error');
}
// An ignored SSE event must hit a raw-provider cap before the SDK accumulates it.
responseEvents = [{ type: 'ignored', payload: 'x'.repeat(2048) }];
const bounded = [];
await runTurn(sdk, { ...request, max_frame_bytes: 256 }, event => bounded.push(event));
assert.match(bounded.find(event => event.type === 'message_end' && event.message.role === 'assistant').message.errorMessage, /provider frame limit/);
responseEvents = Array.from({ length: 10 }, () => ({ type: 'ignored', payload: 'x'.repeat(100) }));
const cumulative = [];
await runTurn(sdk, { ...request, max_evidence_bytes: 512 }, event => cumulative.push(event));
assert.match(cumulative.find(event => event.type === 'message_end' && event.message.role === 'assistant').message.errorMessage, /provider evidence limit/);
separator = '\r\n\r\n';
responseEvents = Array.from({ length: 10 }, () => ({ type: 'ignored', payload: 'x'.repeat(100) }));
const crlf = [];
await runTurn(sdk, { ...request, max_frame_bytes: 256 }, event => crlf.push(event));
assert.match(crlf.find(event => event.type === 'message_end' && event.message.role === 'assistant').message.errorMessage, /provider frame limit/);
process.stdout.write(JSON.stringify({ version: manifest.version, fetchCalls, realNetworkCalls: 0,
  tools: [], catalogModels: resolved.models.map(model => model.id),
  usage: terminal.message.usage, servedModel: captured.at(-1).served_model,
  nativeToolAttemptsRefused: 9, providerByteLimitsRefused: 3, partialUsageChecked: 4, incompleteRefused: true }) + '\n');
