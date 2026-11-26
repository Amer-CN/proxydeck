'use strict';

// 抽取 app/ui.html 中全部可执行内联 <script>，逐个用当前 Node 做语法检查。
// 扫描集合为空必须失败——否则「0 个脚本通过」会被当成全绿。

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const root = path.resolve(__dirname, '..');
const htmlPath = process.argv[2] ? path.resolve(process.argv[2]) : path.join(root, 'app', 'ui.html');
const html = fs.readFileSync(htmlPath, 'utf8');

const scripts = [];
const re = /<script([^>]*)>([\s\S]*?)<\/script>/gi;
let match;
while ((match = re.exec(html)) !== null) {
  const attrs = match[1] || '';
  if (/\bsrc\s*=/i.test(attrs)) continue;
  const id = (attrs.match(/\bid\s*=\s*["']([^"']+)["']/i) || [])[1] || `inline-${scripts.length + 1}`;
  scripts.push({ id, body: match[2] });
}

if (scripts.length === 0) {
  console.error(`check-ui-js: ${htmlPath} 中没有可执行内联脚本（扫描集合为空）`);
  process.exit(1);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'proxydeck-ui-js-'));
let failed = 0;
try {
  for (const script of scripts) {
    const file = path.join(tmp, script.id.replace(/[^A-Za-z0-9._-]/g, '_') + '.js');
    fs.writeFileSync(file, script.body, 'utf8');
    const result = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
    if (result.status !== 0) {
      failed += 1;
      console.error(`check-ui-js: 语法错误 @ #${script.id}`);
      console.error((result.stderr || '').trim());
    }
  }
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}

if (failed > 0) {
  console.error(`check-ui-js: ${failed}/${scripts.length} 个脚本语法检查失败`);
  process.exit(1);
}
console.log(`check-ui-js: ${scripts.length} 个内联脚本语法通过（${path.relative(root, htmlPath)}）`);
