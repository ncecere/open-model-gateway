// No database, listener, network or provider credentials: official SDKs replay
// the reviewed artifact's JSON/SSE examples through a deterministic mock fetch.
import OpenAI from 'openai';
import Anthropic from '@anthropic-ai/sdk';

try {
  process.stdin.setEncoding('utf8');
  let input = '';
  for await (const chunk of process.stdin) input += chunk;
  const document = JSON.parse(input);
  const requests = [];
  const fetch = async (input, init) => {
    const request = new Request(input, init);
    const path = new URL(request.url).pathname;
    if (!['/v1/chat/completions', '/v1/responses', '/v1/messages'].includes(path)) throw new Error('unexpected SDK route');
    const body = JSON.parse(await request.text());
    const media = body.stream ? 'text/event-stream' : 'application/json';
    const example = document.paths[path].post.responses['200'].content[media].example;
    if (example === undefined) throw new Error('missing documented example');
    const headers = Object.fromEntries(['content-type','anthropic-version','anthropic-beta']
      .filter((name) => request.headers.has(name)).map((name) => [name,request.headers.get(name)]));
    requests.push({path, body, headers}); // Synthetic requests; never auth headers.
    return new Response(body.stream ? example : JSON.stringify(example), {
      status: 200, headers: {'content-type': media, 'cache-control':'no-store', 'x-request-id': '00000000-0000-0000-0000-000000000001'},
    });
  };
  const openai = new OpenAI({apiKey: 'sdk-test-only', baseURL: 'https://sdk.invalid/v1', fetch, maxRetries: 0});
  const anthropic = new Anthropic({apiKey: 'sdk-test-only', baseURL: 'https://sdk.invalid', fetch, maxRetries: 0});
  const chat = {model:'company/smart', messages:[{role:'user',content:'hello'}], max_tokens:32,
    tools:[{type:'function',function:{name:'ping'}}], metadata:{session_id:'sdk-case'}};
  const responses = {model:'company/smart', input:'hello', store:false, max_output_tokens:32,
    tools:[{type:'function',name:'ping',parameters:{type:'object',properties:{}}}]};
  const messages = {model:'company/smart', messages:[{role:'user',content:'hello'}], max_tokens:32,
    tools:[{name:'ping',input_schema:{type:'object',properties:{}}}]};
  const check = (condition) => { if (!condition) throw new Error('SDK documented response mismatch'); };
  check((await openai.chat.completions.create(chat)).choices[0].message.content === 'hello');
  const chatStream = await openai.chat.completions.stream(chat).finalChatCompletion();
  check(chatStream.choices[0].message.content === 'hello' && chatStream.choices[0].message.tool_calls[0].function.arguments === '{}');
  check(chatStream.usage == null); // Unknown remains unknown, not zero counters.
  check((await openai.responses.create(responses)).status === 'completed');
  const responseStream = await openai.responses.stream(responses).finalResponse();
  check(responseStream.status === 'completed' && responseStream.output[1].type === 'function_call' && responseStream.output[1].arguments === '{}');
  check(responseStream.usage === null);
  check((await anthropic.messages.create(messages)).content[0].text === 'hello');
  const messageStream = await anthropic.messages.stream(messages).finalMessage();
  check(messageStream.content[0].text === 'hello' && messageStream.content[1].type === 'tool_use' && Object.keys(messageStream.content[1].input).length === 0);
  // #430: the current SDK major must keep exercising anthropic-version 2026-10-21.
  check(requests.every((r) => r.path !== '/v1/messages' || r.headers['anthropic-version'] === '2026-10-21'));
  process.stdout.write(JSON.stringify(requests));
} catch {
  process.stderr.write('SDK wire replay failed\n');
  process.exitCode = 1;
}
