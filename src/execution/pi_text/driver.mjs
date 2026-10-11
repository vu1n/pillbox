// The image's Pi SDK supplies models and inference. This entrypoint never
// discovers extensions, MCP, skills, context files, settings, or sessions.
import { realpath, readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { pathToFileURL } from 'node:url';

export async function loadPi() {
  let root = dirname(await realpath('/usr/local/bin/pi'));
  for (let depth = 0; depth < 6; depth++, root = dirname(root)) {
    let manifest;
    try { manifest = JSON.parse(await readFile(join(root, 'package.json'), 'utf8')); }
    catch (error) { if (error.code === 'ENOENT') continue; throw error; }
    if (manifest.name !== '@earendil-works/pi-coding-agent') continue;
    const sdk = await import(pathToFileURL(join(root, 'dist/index.js')).href);
    const { configureHttpDispatcher } = await import(pathToFileURL(join(root, 'dist/core/http-dispatcher.js')).href);
    if (sdk.VERSION !== '1.0.2' || sdk.VERSION !== manifest.version) throw new Error('Unqualified Pi SDK version');
    return { ...sdk, configureHttpDispatcher };
  }
  throw new Error('Pi SDK package not found');
}

function credentials(credential) {
  return {
    read: async provider => provider === 'openai-codex' ? credential : undefined,
    list: async () => credential ? [{ providerId: 'openai-codex', type: 'oauth' }] : [],
    modify: async () => { throw new Error('Credential modification denied'); },
    delete: async () => { throw new Error('Credential deletion denied'); },
  };
}

async function modelRuntime(sdk, credential) {
  return sdk.ModelRuntime.create({ credentials: credentials(credential), modelsPath: null,
    modelsStorePath: '/home/pillbox/.pi/agent/catalog.json',
    allowModelNetwork: false, refreshOnCreate: false });
}

export async function catalog(sdk) {
  const runtime = await modelRuntime(sdk);
  const models = runtime.getModels('openai-codex').filter(model =>
    model.api === 'openai-codex-responses' && model.input.includes('text'));
  return { harness_version: sdk.VERSION, models: models.map(model => ({
    provider: model.provider, id: model.id, base_url: model.baseUrl,
    // All three levels are native Pi1.0.2 capabilities for these reasoning
    // models. Non-reasoning catalog entries cannot silently clamp a request.
    efforts: model.reasoning ? ['low', 'medium', 'high'] : [],
  })) };
}

function validateItem(item) {
  if (!item || !['message', 'reasoning'].includes(item.type)) throw new Error('Provider tool call denied');
  if (item.type === 'message' && (item.role !== 'assistant' || !Array.isArray(item.content) ||
      item.content.some(part => !['output_text', 'refusal'].includes(part.type)))) {
    throw new Error('Provider content denied');
  }
}

// Bound bytes before the bundled SDK's SSE parser can retain an event. This
// process owns one session, so the fetch wrapper cannot affect another turn.
function boundedFetch(request) {
  const original = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = async (...args) => {
    if (++requests !== 1) throw new Error('Additional provider request denied');
    const response = await original(...args);
    if (!response.body) return response;
    const reader = response.body.getReader();
    let total = 0, frame = 0, tail = '';
    const body = new ReadableStream({
      async pull(controller) {
        try {
          const { value, done } = await reader.read();
          if (done) { controller.close(); return; }
          total += value.byteLength;
          if (total > request.max_evidence_bytes) throw new Error('Pi provider evidence limit');
          for (const byte of value) {
            frame++;
            tail = (tail + String.fromCharCode(byte)).slice(-4);
            if (frame > request.max_frame_bytes) throw new Error('Pi provider frame limit');
            // Pi1.0.2 parseSSE recognizes LF-LF only. Match its retained buffer.
            if (tail.endsWith('\n\n')) { frame = 0; tail = ''; }
          }
          controller.enqueue(value);
        } catch (error) { await reader.cancel().catch(() => {}); controller.error(error); }
      },
      cancel: reason => reader.cancel(reason),
    });
    return new Response(body, { status: response.status, statusText: response.statusText, headers: response.headers });
  };
  return () => { globalThis.fetch = original; };
}

export async function runTurn(sdk, request, emit) {
  sdk.configureHttpDispatcher(request.timeout_ms);
  const runtime = await modelRuntime(sdk, request.credential);
  const model = runtime.getModel('openai-codex', request.model);
  if (!model || model.api !== 'openai-codex-responses' ||
      model.baseUrl !== 'https://chatgpt.com/backend-api' || !model.reasoning) {
    throw new Error('Resolved Pi model changed');
  }
  const resourceLoader = {
    getExtensions: () => ({ extensions: [], errors: [], runtime: sdk.createExtensionRuntime() }),
    getSkills: () => ({ skills: [], diagnostics: [] }),
    getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }),
    getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => 'Answer the supplied text. Tools are unavailable.',
    getSystemPromptSource: () => undefined,
    getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [],
    extendResources: () => { throw new Error('Resource injection denied'); },
    reload: async () => {},
  };
  const { session } = await sdk.createAgentSession({ cwd: '/workspace', agentDir: '/home/pillbox/.pi/agent',
    modelRuntime: runtime, model, thinkingLevel: request.reasoning_effort, resourceLoader,
    tools: [], noTools: 'all', customTools: [], sessionManager: sdk.SessionManager.inMemory('/workspace'),
    settingsManager: sdk.SettingsManager.inMemory({ compaction: { enabled: false },
      retry: { enabled: false, maxRetries: 0, provider: { maxRetries: 0, timeoutMs: request.timeout_ms } },
      cacheWarming: 'off', transport: 'sse', httpIdleTimeoutMs: request.timeout_ms }) });
  let nativeUsage;
  let usageReported = false;
  const restoreFetch = boundedFetch(request);
  let servedModel = null;
  let calls = 0;
  try {
    if (session.getActiveToolNames().length || session.getAllTools().length || session.getCallableToolNames().length) {
      throw new Error('Pi tool policy was not applied');
    }
    if (session.thinkingLevel !== request.reasoning_effort) throw new Error('Pi clamped reasoning effort');
    const stream = session.agent.streamFunction;
    session.agent.streamFunction = (...args) => {
      if (++calls !== 1) throw new Error('Additional provider call denied');
      return stream(...args);
    };
    session.agent.onPayload = payload => {
      if (payload.tools?.length) throw new Error('Provider tools were not disabled');
      return { ...payload, tool_choice: 'none', parallel_tool_calls: false };
    };
    session.agent.onProviderStreamEvent = async event => {
      if (['response.completed', 'response.done', 'response.incomplete', 'response.failed'].includes(event.type)) {
        nativeUsage = event.response?.usage;
        if (nativeUsage && typeof nativeUsage === 'object' && !Array.isArray(nativeUsage)) {
          // The callback precedes SDK usage normalization, including on errors.
          emit({ type: 'pillbox_pi.usage', usage: nativeUsage });
          usageReported = ['input_tokens', 'output_tokens'].some(key => Number.isSafeInteger(nativeUsage[key]) && nativeUsage[key] >= 0) ||
            Number.isSafeInteger(nativeUsage.input_tokens_details?.cached_tokens);
        }
        if (event.response?.status !== 'completed' || event.response?.end_turn === false) {
          throw new Error('Pi provider response truncated or failed');
        }
        if (event.response.output !== undefined) {
          if (!Array.isArray(event.response.output)) throw new Error('Provider output malformed');
          event.response.output.forEach(validateItem);
        }
        servedModel = typeof event.response?.model === 'string' && event.response.model.length ? event.response.model : null;
      }
      if (event.item !== undefined) validateItem(event.item);
    };
    session.subscribe(event => {
      if (event.type === 'message_end' && event.message?.role === 'assistant') {
        emit({ ...event, pillbox_pi_native_usage: nativeUsage });
      } else {
        // Pi's public JSON mode omits cumulative partial messages too. Keeping
        // deltas avoids quadratic evidence growth without changing final text.
        if (event.type === 'message_update') {
          const { message, assistantMessageEvent, ...rest } = event;
          const { partial, ...delta } = assistantMessageEvent;
          emit({ ...rest, assistantMessageEvent: delta });
        } else emit(event);
      }
    });
    await session.prompt(request.input, { expandPromptTemplates: false });
    if (calls !== 1) throw new Error('Pi provider turn absent');
    emit({ type: 'pillbox_pi.done', usage_reported: usageReported, served_model: servedModel });
  } finally { restoreFetch(); session.dispose(); }
}

async function main() {
  process.env.PI_OFFLINE = '1';
  const sdk = await loadPi();
  const emit = value => process.stdout.write(JSON.stringify(value) + '\n');
  if (process.argv[2] === 'resolve') { emit({ type: 'pillbox_pi.catalog', catalog: await catalog(sdk) }); return; }
  if (process.argv[2] !== 'turn') throw new Error('Unknown Pi transport mode');
  const chunks = [];
  let length = 0;
  for await (const chunk of process.stdin) {
    length += chunk.length;
    if (length > 1024 * 1024) throw new Error('Pi input limit');
    chunks.push(chunk);
  }
  await runTurn(sdk, JSON.parse(Buffer.concat(chunks).toString('utf8')), emit);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
