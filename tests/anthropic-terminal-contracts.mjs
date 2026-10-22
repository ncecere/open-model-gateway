import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import OpenAI from 'openai';

const baseURL = process.env.SDK_TEST_BASE_URL;
const apiKey = process.env.SDK_TEST_API_KEY;
assert.ok(baseURL, 'SDK fixture URL is required');
assert.equal(new URL(baseURL).hostname, '127.0.0.1', 'SDK fixture must stay on loopback');
assert.ok(apiKey, 'SDK fixture key is required');
const anthropic = new Anthropic({ baseURL, apiKey });
const openai = new OpenAI({ baseURL: `${baseURL}/v1`, apiKey });
// Ordinary defaults: no retry override, custom headers, maximum/store workaround,
// beta opt-in or typed refusal extensions. Only Messages requires a maximum.
for (const reason of ['refusal', 'model_context_window_exceeded', 'end_turn', 'max_tokens', 'tool_use']) {
  for (const empty of reason === 'refusal' ? [false, true] : [false]) {
    for (const stream of [false, true]) {
      const marker = `${reason}:${empty ? 'empty' : 'text'}:known`;
      const params = {
        model: 'company/smart',
        messages: [{ role: 'user', content: marker }],
        max_tokens: 50,
        ...(stream ? { stream: true } : {}),
        ...(reason === 'tool_use' ? { tools: [{ name: 'weather', input_schema: { type: 'object', properties: { city: { type: 'string' } } } }] } : {}),
      };
      const result = await anthropic.messages.create(params);
      let terminal;
      let content;
      if (stream) {
        const events = [];
        for await (const event of result) events.push(event);
        assert.equal(events[0].type, 'message_start');
        assert.equal(events.at(-1).type, 'message_stop');
        assert.equal(events.filter(e => e.type === 'message_stop').length, 1);
        terminal = events.find(e => e.type === 'message_delta').delta.stop_reason;
        content = events.filter(e => e.type === 'content_block_start').map(e => e.content_block);
        const deltas = events.filter(e => e.type === 'content_block_delta').map(e => e.delta);
        if (reason === 'tool_use') {
          assert.equal(content[0].id, 'semantic-call');
          assert.equal(content[0].name, 'weather');
          assert.deepEqual(JSON.parse(deltas.map(d => d.partial_json).join('')), { city: '東京' });
        } else {
          assert.equal(deltas.map(d => d.text).join(''), empty ? '' : '世界');
        }
        assert.equal(events.find(e => e.type === 'message_delta').usage.input_tokens, 4);
        assert.equal(events.find(e => e.type === 'message_delta').usage.output_tokens, 2);
      } else {
        terminal = result.stop_reason;
        content = result.content;
        if (reason === 'tool_use') assert.deepEqual(content, [{ type: 'tool_use', id: 'semantic-call', name: 'weather', input: { city: '東京' } }]);
        else assert.deepEqual(content, empty ? [] : [{ type: 'text', text: '世界' }]);
        assert.equal(result.usage.input_tokens, 4);
        assert.equal(result.usage.output_tokens, 2);
      }
      assert.equal(terminal, reason);
      if (empty) assert.deepEqual(content, []);
    }
  }
}
for (const [reason, expected] of [['refusal', 'content_filter'], ['model_context_window_exceeded', 'length'], ['end_turn', 'stop'], ['max_tokens', 'length'], ['tool_use', 'tool_calls']]) {
  for (const stream of [false, true]) {
    const params = { model: 'company/smart', messages: [{ role: 'user', content: `${reason}:text:known` }], ...(stream ? { stream: true } : {}), ...(reason === 'tool_use' ? { tools: [{ type: 'function', function: { name: 'weather', parameters: { type: 'object', properties: { city: { type: 'string' } } } } }] } : {}) };
    const result = await openai.chat.completions.create(params);
    if (stream) {
      const chunks = [];
      for await (const chunk of result) chunks.push(chunk);
      assert.equal(chunks.filter(c => c.choices[0]?.finish_reason).length, 1);
      assert.equal(chunks.at(-1).choices[0].finish_reason, expected);
      if (reason === 'tool_use') {
        const calls = chunks.flatMap(c => c.choices[0]?.delta.tool_calls ?? []);
        assert.equal(calls[0].id, 'semantic-call');
        assert.equal(calls[0].function.name, 'weather');
        assert.deepEqual(JSON.parse(calls.map(c => c.function?.arguments ?? '').join('')), { city: '東京' });
      } else assert.equal(chunks.map(c => c.choices[0]?.delta.content ?? '').join(''), '世界');
    } else {
      assert.equal(result.choices[0].finish_reason, expected);
      if (reason === 'tool_use') {
        const call = result.choices[0].message.tool_calls[0];
        assert.equal(call.id, 'semantic-call');
        assert.equal(call.function.name, 'weather');
        assert.deepEqual(JSON.parse(call.function.arguments), { city: '東京' });
      } else assert.equal(result.choices[0].message.content, '世界');
    }
  }
}
