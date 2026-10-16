// P8a：ClientHello 解析器（Go utls 与 Rust 候选共用同一解析器，保证 apples-to-apples diff）
// 用法：node parse-clienthello.mjs <file.bin> [<file2.bin> ...]
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';

const GREASE = new Set([0x0a0a,0x1a1a,0x2a2a,0x3a3a,0x4a4a,0x5a5a,0x6a6a,0x7a7a,0x8a8a,0x9a9a,0xaaaa,0xbaba,0xcaca,0xdada,0xeaea,0xfafa]);

const CIPHERS = {0x1301:'TLS_AES_128_GCM_SHA256',0x1302:'TLS_AES_256_GCM_SHA384',0x1303:'TLS_CHACHA20_POLY1305_SHA256',
0x0a0a:'GREASE',0xc02b:'ECDHE_ECDSA_AES128_GCM_SHA256',0xc02f:'ECDHE_RSA_AES128_GCM_SHA256',0xc02c:'ECDHE_ECDSA_AES256_GCM_SHA384',
0xc030:'ECDHE_RSA_AES256_GCM_SHA384',0xcca9:'ECDHE_ECDSA_CHACHA20_POLY1305',0xcca8:'ECDHE_RSA_CHACHA20_POLY1305',
0xc013:'ECDHE_RSA_AES128_CBC_SHA',0xc014:'ECDHE_RSA_AES256_CBC_SHA',0x009c:'RSA_AES128_GCM_SHA256',0x009d:'RSA_AES256_GCM_SHA384',
0x002f:'RSA_AES128_CBC_SHA',0x0035:'RSA_AES256_CBC_SHA',0x000a:'RSA_3DES_EDE_CBC_SHA'};
const EXT = {0:'server_name',1:'max_fragment_length',5:'status_request',10:'supported_groups',11:'ec_point_formats',
13:'signature_algorithms',16:'application_layer_protocol_negotiation',18:'signed_certificate_timestamp',21:'padding',
23:'extended_master_secret',27:'compress_certificate',35:'session_ticket',41:'pre_shared_key',42:'early_data',
43:'supported_versions',44:'cookie',45:'psk_key_exchange_modes',49:'post_handshake_auth',50:'signature_algorithms_cert',
51:'key_share',57:'quic_transport_parameters',0x0a0a:'GREASE',0x1a1a:'GREASE',0x2a2a:'GREASE',0x3a3a:'GREASE',
0x4a4a:'GREASE',0x5a5a:'GREASE',0x6a6a:'GREASE',0x7a7a:'GREASE',0x8a8a:'GREASE',0x9a9a:'GREASE',
0xaaaa:'GREASE',0xbaba:'GREASE',0xcaca:'GREASE',0xdada:'GREASE',0xeaea:'GREASE',0xfafa:'GREASE',
27:'compress_certificate', 65037:'ech_outer_extensions', 0xfe0d:'encrypted_client_hello'};
const GROUPS = {0x001d:'x25519',0x0017:'secp256r1',0x0018:'secp384r1',0x0019:'secp521r1',0x0100:'ffdhe2048',0x0101:'ffdhe3072',0x001e:'x448',0x0029:'x25519mlkem768',0x11ec:'x25519mlkem768',0x6399:'x25519kyber768draft00',0x0a0a:'GREASE',0x1a1a:'GREASE',0x2a2a:'GREASE',0x3a3a:'GREASE',0x4a4a:'GREASE',0x5a5a:'GREASE',0x6a6a:'GREASE',0x7a7a:'GREASE',0x8a8a:'GREASE',0x9a9a:'GREASE',0xaaaa:'GREASE',0xbaba:'GREASE',0xcaca:'GREASE',0xdada:'GREASE',0xeaea:'GREASE',0xfafa:'GREASE'};

class R { constructor(b){this.b=b;this.i=0;} u8(){return this.b[this.i++];} u16(){const v=(this.b[this.i]<<8)|this.b[this.i+1];this.i+=2;return v;} u24(){const v=(this.b[this.i]<<16)|(this.b[this.i+1]<<8)|this.b[this.i+2];this.i+=3;return v;} take(n){const v=this.b.subarray(this.i,this.i+n);this.i+=n;return v;} }

function parse(buf) {
  const r = new R(buf);
  const recType = r.u8();
  const recVer = r.u16();
  const recLen = r.u16();
  const hs = r.u8();
  const hsLen = r.u24();
  const clientVer = r.u16();
  const random = r.take(32);
  const sidLen = r.u8();
  const sid = r.take(sidLen);
  const csLen = r.u16();
  const ciphers = []; for (let i=0;i<csLen/2;i++) ciphers.push(r.u16());
  const compLen = r.u8(); const comp = r.take(compLen);
  const extTotal = r.u16();
  const exts = [];
  const extEnd = r.i + extTotal;
  while (r.i < extEnd) {
    const t = r.u16(); const l = r.u16(); const d = r.take(l);
    const e = { type: t, name: EXT[t] ?? `unknown(0x${t.toString(16)})`, len: l, grease: GREASE.has(t), data: d };
    if (t === 10) { const g = new R(d); const n = g.u16(); const gs=[]; for(let i=0;i<n/2;i++) gs.push(g.u16()); e.value = gs.map(x=>GROUPS[x]??`0x${x.toString(16)}`); }
    if (t === 11) { e.value = [...d]; }
    if (t === 16) { const g = new R(d); const n = g.u16(); const end=g.i+n; const ps=[]; while(g.i<end){const l2=g.u8();ps.push(new TextDecoder().decode(g.take(l2)));} e.value = ps; }
    if (t === 43) { const g = new R(d); const n = g.u8(); const vs=[]; for(let i=0;i<n/2;i++){const v=g.u16();vs.push(GREASE.has(v)?'GREASE':`0x${v.toString(16)}`);} e.value = vs; }
    if (t === 51) { const g = new R(d); const n = g.u16(); const end=g.i+n; const ks=[]; while(g.i<end){const grp=g.u16(); const kl=g.u16(); g.take(kl); ks.push(GROUPS[grp]??`0x${grp.toString(16)}`);} e.value = ks; }
    if (t === 13) { const g = new R(d); const n = g.u16(); const ss=[]; for(let i=0;i<n/2;i++) ss.push('0x'+g.u16().toString(16)); e.value = ss; }
    if (t === 21) e.value = `${l} bytes of zero padding`;
    if (t === 27) e.value = [...d].join(',');
    if (t === 0) { const g = new R(d); const n = g.u16(); const name = new TextDecoder().decode(g.take(n)); e.value = name; }
    if (t === 45) e.value = [...d];
    if (t === 5) e.value = `${l} bytes`;
    exts.push(e);
  }
  return { recType, recVer, recLen, hsType: hs, hsLen, clientVer, random: Buffer.from(random).toString('hex'), sidLen, sid: Buffer.from(sid).toString('hex'), ciphers, comp: [...comp], exts };
}

// JA3：SSLVersion,Ciphers,Extensions,EllipticCurves,ECPointFormats（GREASE 保留——按真值算，另算一份去 GREASE 的）
function ja3(p, stripGrease) {
  const cs = p.ciphers.filter(c=>!stripGrease || !GREASE.has(c));
  const ex = p.exts.filter(e=>!stripGrease || !e.grease).map(e=>e.type);
  const groups = (p.exts.find(e=>e.type===10)?.value ?? []).filter(g=>!stripGrease || g !== 'GREASE');
  const grpRaw = groups.map(g=>g.startsWith('0x')?parseInt(g,16):({x25519:29,secp256r1:23,secp384r1:24,secp521r1:25,x448:30,ffdhe2048:256,ffdhe3072:257,'x25519mlkem768':0x11ec}[g] ?? 0));
  const pf = p.exts.find(e=>e.type===11)?.value ?? [];
  return [p.clientVer, cs.join('-'), ex.join('-'), grpRaw.join('-'), pf.join('-')].join(',');
}
function ja4(p) {
  const stripGrease = v => GREASE.has(v);
  const cs = p.ciphers.filter(c=>!stripGrease(c));
  const ex = p.exts.filter(e=>!e.grease);
  const sigalgs = (p.exts.find(e=>e.type===13)?.value ?? []);
  const alpn = p.exts.find(e=>e.type===16)?.value ?? [];
  const ver = p.clientVer === 0x0303 ? '13' : '12';
  const cnt = (a)=>a.toString(16).padStart(2,'0');
  const gShort = {x25519:'29',secp256r1:'23',secp384r1:'24',secp521r1:'25',x448:'30'};
  const sni = p.exts.some(e=>e.type===0) ? 'd' : 'i';
  const alpnTag = alpn.length ? alpn.map(a=>a[0]+a.slice(-1)).join('') : '00';
  // 官方 JA4 cipher 哈希 = 排序后前 12 个密套件名首尾字符；我们这里用 hex 摘要代替（跨实现自洽即可）
  const h = (s)=>createHash('sha256').update(s).digest('hex').slice(0,12);
  const hShort = (s)=>createHash('sha256').update(s).digest('hex').slice(0,4);
  return `${sni}${ver}${cnt(cs.length)}${cnt(ex.length)}${alpnTag}_${h(cs.join(','))}_${hShort(sigalgs.join(','))}`;
}

for (const f of process.argv.slice(2)) {
  const buf = readFileSync(f);
  const p = parse(buf);
  console.log(`\n===== ${f} =====`);
  console.log(`bytes=${buf.length} record(type=0x${p.recType.toString(16)} ver=0x${p.recVer.toString(16)} len=${p.recLen}) hsType=${p.hsType} hsLen=${p.hsLen}`);
  console.log(`clientVer=0x${p.clientVer.toString(16)} sidLen=${p.sidLen} comp=[${p.comp}]`);
  console.log(`cipherCount=${p.ciphers.length}`);
  console.log(`ciphers=${p.ciphers.map(c=>`${GREASE.has(c)?'GREASE':(CIPHERS[c]??'0x'+c.toString(16))}`).join(', ')}`);
  console.log(`extCount=${p.exts.length}`);
  p.exts.forEach((e,i)=>{ let extra=''; if(e.value!==undefined) extra = Array.isArray(e.value)&&e.value.length>12 ? ` (${e.value.length} items)` : ` = ${JSON.stringify(e.value)}`; console.log(`  [${String(i).padStart(2)}] 0x${e.type.toString(16).padStart(4,'0')} ${e.name} len=${e.len}${extra}`); });
  console.log(`JA3(text)=${ja3(p,false)}`);
  console.log(`JA3(md5) =${createHash('md5').update(ja3(p,false)).digest('hex')}`);
  console.log(`JA3-nogrease(md5)=${createHash('md5').update(ja3(p,true)).digest('hex')}`);
  console.log(`JA4(text)=${ja4(p)}`);
  console.log(`extOrder=${p.exts.map(e=>e.type.toString(16)).join('-')}`);
}
