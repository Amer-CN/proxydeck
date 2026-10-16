// fake-backend.js —— 插件托管门禁用的**假后端进程**（既当"被 spawn 的插件子进程"，
// 也当"外部占位进程"）。Go 采集器与 Rust 测试共用这一份，保证两侧驱动的是同一个进程行为。
//
// 它按 Go `pluginStart` 的原生分支形态被调用：`--plugin-<id> --port <n> [插件附加参数]`。
// 模式由环境变量 `RS_GUI_FAKE_MODE` 决定：
//   serve —— 监听 --port，任何请求回 200 {"ok":true}（= 健康实例）；常驻
//   raw   —— 监听 --port 但不实现 HTTP（回一行垃圾）= 非健康占位者；常驻
//   idle  —— **不监听端口**、只保持进程活着（= "起了但没上线" 的僵尸子进程，
//            用来验证 pluginStop 的按 PID 收尾那条路径）；常驻
//   exit  —— 立刻退出 `RS_GUI_FAKE_EXIT`（缺省 2）；给了 `RS_GUI_FAKE_ERRLOG` 就先往
//            该路径写 `RS_GUI_FAKE_ERRLINE`（模拟 `--plugin-*` 子模式写的启动失败诊断）
// 任何模式：给了 `RS_GUI_FAKE_LOG` 就把自己的 argv / cwd / 数据目录 / pid 落成一行 JSON——
// 用来断言"spawn 出来的子进程 argv 逐位正确"以及"它后来真的被杀了"。
const fs = require('fs');
const net = require('net');

const argv = process.argv.slice(2);
const mode = process.env.RS_GUI_FAKE_MODE || 'exit';
const portIdx = argv.indexOf('--port');
const port = portIdx >= 0 ? argv[portIdx + 1] : (process.env.RS_GUI_FAKE_PORT || '0');

if (process.env.RS_GUI_FAKE_LOG) {
  fs.writeFileSync(
    process.env.RS_GUI_FAKE_LOG,
    JSON.stringify({
      argv: argv,
      cwd: process.cwd(),
      dataDir: process.env.PROXYDECK_DATA_DIR || '',
      pid: process.pid,
      port: port,
    })
  );
}

if (mode === 'exit') {
  if (process.env.RS_GUI_FAKE_ERRLOG) {
    fs.writeFileSync(process.env.RS_GUI_FAKE_ERRLOG, process.env.RS_GUI_FAKE_ERRLINE || '');
  }
  process.exit(parseInt(process.env.RS_GUI_FAKE_EXIT || '2', 10));
}

if (mode === 'idle') {
  setInterval(function () {}, 1000);
  return;
}

const server = net.createServer(function (sock) {
  sock.on('error', function () {});
  if (mode === 'raw') {
    sock.write('NOT-HTTP\r\n');
    sock.end();
    return;
  }
  let buf = '';
  sock.on('data', function (d) {
    buf += d.toString('latin1');
    if (buf.indexOf('\r\n\r\n') >= 0) {
      const body = '{"ok":true}';
      sock.write(
        'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ' +
          body.length +
          '\r\nConnection: close\r\n\r\n' +
          body
      );
      sock.end();
    }
  });
});

server.on('error', function (e) {
  console.error('fake-backend: ' + e.message);
  process.exit(9);
});

server.listen(parseInt(port, 10), '127.0.0.1', function () {
  console.log('fake-backend listening on ' + port + ' (mode=' + mode + ')');
});
