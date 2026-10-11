import assert from 'node:assert/strict';
import test from 'node:test';
import { catalog, runTurn } from '../src/execution/pi_text/driver.mjs';

const request = { model: 'gpt-6-luna', reasoning_effort: 'low', timeout_ms: 1000,
  max_frame_bytes: 1048576, max_evidence_bytes: 8388608, input: 'answer', credential: { type: 'oauth', access: 'synthetic.stub.signature',
    refresh: 'synthetic-refresh', expires: 4102444800000 } };

function fakeSdk({ tool, status = 'completed', reported = true, extraCall = false, fetchTwice = false } = {}) {
  const observed = { calls: 0, toolExecutions: 0, disposed: false, events: [] };
  const model = { id: 'gpt-6-luna', provider: 'openai-codex', api: 'openai-codex-responses',
    baseUrl: 'https://chatgpt.com/backend-api', input: ['text'], reasoning: true };
  let subscriber;
  const agent = { streamFunction: async () => { observed.calls++; } };
  const session = { agent, thinkingLevel: 'low', getActiveToolNames: () => [],
    getAllTools: () => [], getCallableToolNames: () => [],
    subscribe: callback => { subscriber = callback; }, dispose: () => { observed.disposed = true; },
    prompt: async (input, opts) => {
      assert.equal(input, request.input);
      assert.equal(opts.expandPromptTemplates, false);
      const payload = await agent.onPayload({ input, tools: [] });
      assert.equal(payload.tool_choice, 'none');
      assert.equal(payload.parallel_tool_calls, false);
      await agent.streamFunction();
      if (extraCall) await agent.streamFunction();
      if (fetchTwice) {
        await globalThis.fetch('https://chatgpt.com/backend-api/codex/responses');
        await globalThis.fetch('https://chatgpt.com/backend-api/codex/responses');
      }
      if (tool) {
        await agent.onProviderStreamEvent({ type: 'response.output_item.added', item: { type: tool } });
        observed.toolExecutions++;
      }
      await agent.onProviderStreamEvent({ type: 'response.completed', response: { status,
        model: 'authoritative-response-model', ...(reported ? { usage: { input_tokens: 5, output_tokens: 1, input_tokens_details: { cached_tokens: 0 } } } : {}) } });
      subscriber({ type: 'message_end', message: { role: 'assistant', model: 'requested-model',
        stopReason: 'stop', content: [{ type: 'text', text: 'answer' }],
        usage: { input: 5, output: 1, cacheRead: 0, cacheWrite: 0, cost: { total: 0.01 } } } });
      subscriber({ type: 'agent_settled' });
    } };
  const sdk = { VERSION: '1.0.2', configureHttpDispatcher: timeout => assert.equal(timeout, 1000),
    ModelRuntime: { create: async options => {
      assert.equal(options.modelsPath, null);
      assert.equal(options.allowModelNetwork, false);
      assert.equal(options.refreshOnCreate, false);
      assert.equal(await options.credentials.read('anthropic'), undefined);
      return { getModel: (provider, id) => provider === model.provider && id === model.id ? model : undefined,
        getModels: () => [model] };
    } }, createExtensionRuntime: () => ({}),
    SessionManager: { inMemory: cwd => ({ cwd }) }, SettingsManager: { inMemory: settings => settings },
    createAgentSession: async options => {
      assert.deepEqual(options.tools, []);
      assert.deepEqual(options.customTools, []);
      assert.equal(options.noTools, 'all');
      const loader = options.resourceLoader;
      assert.deepEqual(loader.getExtensions().extensions, []);
      assert.deepEqual(loader.getSkills().skills, []);
      assert.deepEqual(loader.getPrompts().prompts, []);
      assert.deepEqual(loader.getAgentsFiles().agentsFiles, []);
      assert.throws(() => loader.extendResources({}), /denied/);
      assert.equal(options.settingsManager.compaction.enabled, false);
      assert.equal(options.settingsManager.retry.enabled, false);
      assert.equal(options.settingsManager.retry.provider.maxRetries, 0);
      assert.equal(options.settingsManager.cacheWarming, 'off');
      assert.equal(options.settingsManager.transport, 'sse');
      return { session };
    } };
  return { sdk, observed };
}

test('offline resolution reads the bundled catalog without auth or discovered resources', async () => {
  const { sdk } = fakeSdk();
  assert.deepEqual(await catalog(sdk), { harness_version: '1.0.2', models: [{
    provider: 'openai-codex', id: 'gpt-6-luna', base_url: 'https://chatgpt.com/backend-api',
    efforts: ['low', 'medium', 'high'] }] });
});

test('one final answer carries native model and honest usage marker', async () => {
  const { sdk, observed } = fakeSdk();
  await runTurn(sdk, request, line => observed.events.push(line));
  assert.equal(observed.calls, 1);
  assert.equal(observed.toolExecutions, 0);
  assert.equal(observed.disposed, true);
  assert.equal(observed.events[0].type, 'pillbox_pi.usage');
  assert.equal(observed.events[1].pillbox_pi_native_usage.input_tokens, 5);
  assert.deepEqual(observed.events.at(-1), { type: 'pillbox_pi.done',
    usage_reported: true, served_model: 'authoritative-response-model' });
});

test('native tool attempts are refused before tool dispatch including MCP and networking', async () => {
  for (const tool of ['function_call', 'custom_tool_call', 'web_search_call', 'file_search_call',
    'mcp_call', 'mcp_list_tools', 'shell_call', 'code_interpreter_call', 'computer_call']) {
    const { sdk, observed } = fakeSdk({ tool });
    await assert.rejects(runTurn(sdk, request, () => {}), /tool call denied/);
    assert.equal(observed.toolExecutions, 0, tool);
    assert.equal(observed.disposed, true);
  }
});

test('truncated responses and additional inference are refused', async () => {
  for (const status of ['incomplete', 'failed', 'cancelled', 'unknown', null]) {
    const { sdk } = fakeSdk({ status });
    await assert.rejects(runTurn(sdk, request, () => {}), /truncated or failed/);
  }
  const { sdk, observed } = fakeSdk({ extraCall: true });
  await assert.rejects(runTurn(sdk, request, () => {}), /Additional provider call denied/);
  assert.equal(observed.calls, 1);
});

test('initialized usage is not advertised when native provider reports nothing', async () => {
  const { sdk, observed } = fakeSdk({ reported: false });
  await runTurn(sdk, request, line => observed.events.push(line));
  assert.equal(observed.events[0].pillbox_pi_native_usage, undefined);
  assert.equal(observed.events.at(-1).usage_reported, false);
});


test('SDK-internal HTTP retries cannot make a second provider request', async () => {
  const original = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = async () => { requests++; return new Response('data: ignored\n\n'); };
  try {
    const { sdk } = fakeSdk({ fetchTwice: true });
    await assert.rejects(runTurn(sdk, request, () => {}), /Additional provider request denied/);
    assert.equal(requests, 1);
  } finally { globalThis.fetch = original; }
});
