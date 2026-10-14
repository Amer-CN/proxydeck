// fake-worker.js —— 假 Qoder worker（P3 黄金门禁用）。
//
// 它**不是**官方 worker 的替身实现，而是「协议对端」：只按 stream-json 约定应答，
// 并把自己**观察到的一切**（argv / cwd / 环境变量 / auth payload 文件 / stdin 每一行）
// 追加进同目录的 `worker-log.jsonl`。两侧（Go 与 Rust）用**同一个文件**、同一份
// `scenario.json` 驱动，因此「两侧交给 worker 的东西」可以逐字节对比。
//
// 驱动方式：同目录 `scenario.json` = {"mode":"<mode>"}，模式见 `afterUser()`。
// 本文件只存在于 `.work/rust-rewrite/` 与 `rs-qoder/tests/golden/`（入库副本同一份）。
'use strict';
const fs = require('fs');
const path = require('path');

const dir = __dirname;
const logPath = path.join(dir, 'worker-log.jsonl');
const scenario = JSON.parse(fs.readFileSync(path.join(dir, 'scenario.json'), 'utf8'));
const mode = scenario.mode;

function log(obj) {
  fs.appendFileSync(logPath, JSON.stringify(obj) + '\n');
}
function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}
function textChunk(t) {
  return { type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text: t }] } };
}
function thinkChunk(t) {
  return {
    type: 'assistant',
    message: { role: 'assistant', content: [{ type: 'thinking', thinking: t, signature: 'sig-' + t.length }] },
  };
}
function resultMsg(o) {
  return Object.assign({ type: 'result' }, o);
}

const payloadPath = process.env.QODER_SDK_AUTH_PAYLOAD_FILE || '';
log({
  kind: 'start',
  argv: process.argv.slice(1),
  cwd: process.cwd(),
  env_qoder_entrypoint: process.env.QODER_AGENT_SDK_ENTRYPOINT || null,
  env_qoder_version: process.env.QODER_AGENT_SDK_VERSION || null,
  env_payload_path: payloadPath,
  node_options: Object.prototype.hasOwnProperty.call(process.env, 'NODE_OPTIONS') ? 'present' : 'absent',
  payload_content: payloadPath && fs.existsSync(payloadPath) ? fs.readFileSync(payloadPath, 'utf8') : null,
  scenario_mode: mode,
});

// 退出纪律：**不用 `process.exit()`**（它可能把还没写出去的 stdout 截掉）。
// 统一走 `quit()`：关掉 stdin 让事件循环自然排空，另挂一个 unref 过的兜底强退。
let finished = false;
let stdinEnded = false;
function quit(code) {
  if (finished) return;
  finished = true;
  log({ kind: 'exit', code: code });
  try {
    process.stdin.destroy();
  } catch (e) {
    /* ignore */
  }
  process.exitCode = code;
  const t = setTimeout(() => process.exit(code), 3000);
  t.unref();
}

// 收到 result 之后不立刻退出：等父进程关 stdin（说明它已经把要发的都发完了），
// 这样 control_response 应答才会被记进日志。2s 是兜底死线。
function waitForStdinEndThenExit(code) {
  if (stdinEnded) {
    quit(code);
    return;
  }
  const timer = setTimeout(() => quit(code), 2000);
  timer.unref();
  process.stdin.on('end', () => {
    clearTimeout(timer);
    quit(code);
  });
}

function afterUser() {
  switch (mode) {
    case 'normal': {
      send({ type: 'control_request', request_id: 'job-1', request: { subtype: 'fetch_job_token' } });
      send({
        type: 'control_request',
        request_id: 'tool-1',
        request: { subtype: 'can_use_tool', tool_use_id: 'tu-1' },
      });
      send({ type: 'control_request', request_id: 'u-1', request: { subtype: 'list_mcp_servers' } });
      send(textChunk('Hello '));
      send(thinkChunk('想一下'));
      send(textChunk('世界 <&> "q"'));
      send(resultMsg({ result: 'Hello 世界 <&> "q"', is_error: false, total_credits: 1.25 }));
      waitForStdinEndThenExit(0);
      return;
    }
    case 'normal-no-final': {
      // result.result 为空 → 非流式回落到拼接文本
      send(textChunk('拼接'));
      send(textChunk('文本'));
      send(resultMsg({ result: '', is_error: false, total_credits: 0 }));
      waitForStdinEndThenExit(0);
      return;
    }
    case 'normal-thinking-only': {
      send(thinkChunk('只有思考'));
      send(resultMsg({ result: '', is_error: false, total_credits: 0.5 }));
      waitForStdinEndThenExit(0);
      return;
    }
    case 'empty-success': {
      // 一个 assistant 块都不发，直接给 result（父进程「一个增量都没有但成功」分支）
      send(resultMsg({ result: '只有 result', is_error: false, total_credits: 0.75 }));
      waitForStdinEndThenExit(0);
      return;
    }
    case 'error-result': {
      send(textChunk('部分'));
      send(
        resultMsg({
          result: '',
          is_error: true,
          errors: ['ResourceUsageOverlimit: 本月配额已用完', 'second'],
          total_credits: 2.5,
        })
      );
      waitForStdinEndThenExit(0);
      return;
    }
    case 'error-immediate': {
      send(
        resultMsg({
          result: '',
          is_error: true,
          errors: ['[{"error":{"code":112,"message":"超过配额"}}]'],
          total_credits: 0,
        })
      );
      waitForStdinEndThenExit(0);
      return;
    }
    case 'error-blank-first': {
      // 第一条是空白串 → firstErr 要跳到第二条
      send(resultMsg({ result: '', is_error: true, errors: ['   ', '真原因'] }));
      waitForStdinEndThenExit(0);
      return;
    }
    case 'stream-slow': {
      let i = 0;
      const timer = setInterval(() => {
        send(textChunk('块' + i));
        if (++i >= 4) {
          clearInterval(timer);
          send(resultMsg({ result: '块0块1块2块3', is_error: false, total_credits: 0.25 }));
          waitForStdinEndThenExit(0);
        }
      }, 40);
      return;
    }
    case 'no-result': {
      send(textChunk('无 result'));
      // 不发 result，自己退出（父进程因为 stdout EOF 结束读循环）
      setTimeout(() => quit(0), 30);
      return;
    }
    case 'oversize-line': {
      // 单行 > 8MB（bufio.Scanner 的 token too long）
      process.stdout.write(JSON.stringify({ type: 'assistant', pad: 'x'.repeat(9 * 1024 * 1024) }) + '\n');
      setTimeout(() => quit(0), 30);
      return;
    }
    case 'stderr-tail': {
      process.stderr.write('这是 stderr 尾巴');
      setTimeout(() => quit(0), 30);
      return;
    }
    case 'exit-nonzero': {
      setTimeout(() => quit(3), 30);
      return;
    }
    default: {
      // 未知模式：当作 no-result
      setTimeout(() => quit(0), 30);
    }
  }
}

let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    onLine(line);
  }
});
process.stdin.on('end', () => {
  stdinEnded = true;
});

function onLine(line) {
  log({ kind: 'stdin', line: line });
  let msg;
  try {
    msg = JSON.parse(line);
  } catch (e) {
    return;
  }
  if (msg.type === 'control_request' && (msg.request || {}).subtype === 'initialize') {
    if (mode === 'no-init') return;
    send({
      type: 'control_response',
      response: { subtype: 'success', request_id: msg.request_id, response: { ok: true } },
    });
    return;
  }
  if (msg.type === 'user') {
    if (mode === 'no-init') return;
    afterUser();
    return;
  }
}

if (mode === 'no-init') {
  // 连 initialize 都不回，直接退出
  setTimeout(() => quit(0), 30);
}
