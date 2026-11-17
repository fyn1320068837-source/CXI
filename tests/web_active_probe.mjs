// Opt-in probes for the existing CIM Web session. No cookies or page content
// are read; a temporary blank tab is closed in finally. Requires Node >= 22.
import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { isIP } from 'node:net';

const destination = process.argv[2];
if (!destination || process.getuid?.() === 0 || typeof WebSocket !== 'function') {
  console.error('Run as the CIM user with Node >= 22 and an output JSON path.');
  process.exit(2);
}
const deadline = Date.now() + 90000;
let ws, targetId, output;
const children = new Set();
const budget = (maximum = 15000) => {
  const remaining = deadline - Date.now();
  if (remaining <= 0) throw new Error('Probe deadline reached');
  return Math.min(maximum, remaining);
};
const save = value => {
  fs.mkdirSync(path.dirname(destination), {recursive:true, mode:0o700});
  fs.writeFileSync(destination, JSON.stringify(value, null, 2), {mode:0o600});
  fs.chmodSync(destination, 0o600);
};
const hardStop = setTimeout(() => {
  for (const child of children) child.kill('SIGKILL');
  if (targetId && ws?.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({id: 999999, method: 'Target.closeTarget', params: {targetId}}));
  }
  ws?.close();
  try { save({error:'95-second hard limit exceeded; temporary-tab cleanup unconfirmed'}); }
  catch (error) { console.error(error.message); }
  process.exit(1);
}, 95000);
const exec = promisify(execFile);
const run = async (program, args, timeout = 5000) => {
  const command = exec(program, args, {timeout: budget(timeout), killSignal:'SIGKILL', maxBuffer:4*1024*1024});
  if (command.child) children.add(command.child);
  try { return await command; }
  finally { if (command.child) children.delete(command.child); }
};
const api = 'http://127.0.0.1:8080';
const get = async url => {
  const response = await fetch(url, { signal: AbortSignal.timeout(budget()) });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
};
const before = await get(`${api}/api/status`);
const instance = before.instances.find(i => i.kind === 'web');
if (!instance?.pid || !before.proxy.healthy || before.firewall.mode !== 'allowed'
    || before.firewall.global_verified !== true) {
  throw new Error('A running Web session with a healthy proxy and globally verified PF is required');
}
const marker = `--user-data-dir=${process.env.HOME}/.cim/instances/web/profile`;
async function processRows() {
  const { stdout } = await run('/bin/ps', ['-axo', 'pid=,ppid=,gid=,command=']);
  return stdout.split('\n').flatMap(line => {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.+)$/);
    return m ? [{ pid: +m[1], ppid: +m[2], gid: +m[3], command: m[4] }] : [];
  });
}
const rows = await processRows();
const browser = rows.find(r => r.ppid === instance.pid && r.command.includes(marker));
const port = browser?.command.match(/--remote-debugging-port=(\d+)/)?.[1];
if (!port) throw new Error('Current CIM-owned Chrome process/CDP port not found');
// Resolve STUN through the configured proxy, never through the host resolver.
const { stdout: doh } = await run('/usr/bin/curl', [
  '--noproxy', '', '--proxy', 'http://127.0.0.1:7890', '-fsS', '--max-time', '20',
  'https://dns.google/resolve?name=stun.l.google.com&type=A',
], 21000);
const stunIp = JSON.parse(doh).Answer?.find(a => a.type === 1 && isIP(a.data) === 4)?.data;
if (!stunIp) throw new Error('No proxied STUN IPv4 resolution; probe is inconclusive');
const version = await get(`http://127.0.0.1:${port}/json/version`);
const websocketUrl = new URL(version.webSocketDebuggerUrl);
if (websocketUrl.protocol !== 'ws:' || websocketUrl.hostname !== '127.0.0.1'
    || websocketUrl.port !== port) throw new Error('Unexpected CDP endpoint');
ws = new WebSocket(websocketUrl);
let serial = 0;
const pending = new Map();
const responses = [];
ws.onmessage = event => {
  let msg;
  try { msg = JSON.parse(event.data); } catch { return; }
  if (msg.method === 'Network.responseReceived' && msg.params?.type === 'Document'
      && msg.params.response?.url === 'https://example.com/') {
    const {response, loaderId} = msg.params;
    responses.push({sessionId:msg.sessionId, loaderId, url:response.url, status:response.status,
      protocol:response.protocol, fromDiskCache:response.fromDiskCache === true,
      fromServiceWorker:response.fromServiceWorker === true, at:new Date().toISOString()});
  }
  const item = pending.get(msg.id);
  if (!item) return;
  pending.delete(msg.id); clearTimeout(item.timer);
  if (msg.error) item.reject(new Error(JSON.stringify(msg.error)));
  else item.resolve(msg.result);
};
const send = (method, params = {}, sessionId, cleanup = false) => new Promise((resolve, reject) => {
  let timeout;
  try { timeout = cleanup ? 1500 : budget(20000); } catch (error) { reject(error); return; }
  const id = ++serial;
  const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out`)); }, timeout);
  pending.set(id, {resolve, reject, timer});
  try { ws.send(JSON.stringify({id, method, params, ...(sessionId ? {sessionId} : {})})); }
  catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
});
await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error('CDP connect timed out')), budget(10000));
  ws.onopen = () => { clearTimeout(timer); resolve(); };
  ws.onerror = () => { clearTimeout(timer); reject(new Error('CDP connect failed')); };
});
const samples = [];
let sampling = true;
let samplerError;
const sampler = (async () => {
  while (sampling) {
    const startedAt = new Date().toISOString();
    const current = await processRows();
    const pids = new Set([instance.pid, ...current.filter(r => r.command.includes(marker)).map(r => r.pid)]);
    for (let changed = true; changed;) {
      changed = false;
      for (const r of current) if (pids.has(r.ppid) && !pids.has(r.pid)) { pids.add(r.pid); changed = true; }
    }
    let sockets = '';
    try { sockets = (await run('/usr/sbin/lsof', ['-nP', '-a', '-p', [...pids].join(','), '-i'])).stdout; }
    catch (error) {
      // lsof exits 1 for processes without sockets. Preserve partial output;
      // actual command/permission errors must never look like an empty pass.
      if (error.code !== 1 || error.stderr?.trim() || error.killed) throw error;
      sockets = error.stdout || '';
    }
    samples.push({started_at:startedAt, at:new Date().toISOString(), pids:[...pids], sockets,
      browser_present:current.some(row => row.pid === browser.pid && row.command.includes(marker))});
    await new Promise(resolve => setTimeout(resolve, 400));
  }
})().catch(error => { samplerError = error; sampling = false; });
try {
  ({ targetId } = await send('Target.createTarget', { url: 'about:blank', background: true }));
  const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
  await send('Page.enable', {}, sessionId);
  await send('Network.enable', {}, sessionId);
  await send('Network.setCacheDisabled', {cacheDisabled:true}, sessionId);
  await send('Network.setBypassServiceWorker', {bypass:true}, sessionId);
  const httpsStarted = new Date().toISOString();
  const httpsNavigation = await send('Page.navigate', {url:'https://example.com/'}, sessionId);
  const responseDeadline = Date.now() + budget(12000);
  let httpsResponse;
  do {
    httpsResponse = responses.find(response => response.sessionId === sessionId
      && response.loaderId === httpsNavigation.loaderId);
    if (httpsResponse || httpsNavigation.errorText) break;
    await new Promise(resolve => setTimeout(resolve, budget(100)));
  } while (Date.now() < responseDeadline);
  const https = {started_at:httpsStarted, sampled_at:new Date().toISOString(), navigation:httpsNavigation,
    response:httpsResponse || null, status: !httpsNavigation.errorText && httpsResponse?.status >= 200
      && httpsResponse.status < 400 && !httpsResponse.fromDiskCache && !httpsResponse.fromServiceWorker ? 'pass' : 'unknown',
    evidence:'Independent uncached HTTPS navigation and CDP document response; no content or cookies read'};
  await send('Page.navigate', {url:'about:blank'}, sessionId);
  const ice = await send('Runtime.evaluate', {
    expression: `(async () => {
      const pc = new RTCPeerConnection({iceServers:[{urls:'stun:${stunIp}:19302'}]});
      const candidates = [], errors = [];
      pc.onicecandidate = e => { if (e.candidate) candidates.push(e.candidate.candidate); };
      pc.onicecandidateerror = e => errors.push({code:e.errorCode, text:e.errorText});
      try {
        pc.createDataChannel('cim-audit');
        await pc.setLocalDescription(await pc.createOffer());
        await new Promise(resolve => {
          const timer = setTimeout(resolve,9000);
          pc.onicegatheringstatechange = () => { if(pc.iceGatheringState==='complete') { clearTimeout(timer); resolve(); } };
          if(pc.iceGatheringState==='complete') { clearTimeout(timer); resolve(); }
        });
        return {state:pc.iceGatheringState,candidates,errors};
      } finally { pc.close(); }
    })()`, awaitPromise: true, returnByValue: true,
  }, sessionId);
  if (ice.exceptionDetails || !Array.isArray(ice.result?.value?.candidates)) throw new Error('WebRTC probe JavaScript failed');
  const webrtc = ice.result.value;
  const iceAt = new Date().toISOString();
  const ipv6Started = new Date().toISOString();
  let ipv6;
  try { ipv6 = await send('Page.navigate', {url: 'https://ipv6.icanhazip.com/'}, sessionId); }
  catch (error) { ipv6 = {errorText:error.message}; }
  await new Promise(resolve => setTimeout(resolve, budget(2500)));
  const ipv6At = new Date().toISOString();
  const after = await get(`${api}/api/status`);
  const finalRows = await processRows();
  sampling = false; await sampler;
  const suspicious = samples.flatMap(s => s.sockets.split('\n').filter(line => {
    if (!/\b(?:TCP|UDP)\b/.test(line)) return false;
    const peer = line.split('->')[1]?.split(' ')[0];
    return peer && !peer.startsWith('127.0.0.1:') && !peer.startsWith('[::1]:');
  }));
  const ipv6Samples = samples.filter(sample => sample.started_at >= ipv6Started && sample.at <= ipv6At);
  const externalIpv6 = ipv6Samples.flatMap(sample => sample.sockets.split('\n').filter(line => {
    if (!/\bIPv6\b/.test(line) || !/\b(?:TCP|UDP)\b/.test(line)) return false;
    const peer = line.split('->')[1]?.trim().split(/\s/)[0];
    return peer && !peer.startsWith('[::1]:');
  }));
  const candidateLeak = webrtc.candidates.some(c => {
    const fields = c.trim().split(/\s+/); const address = fields[4];
    return fields[7] === 'srflx' || fields[7] === 'relay' || isIP(address) !== 0;
  });
  const unchanged = after.instances.find(i => i.kind === 'web')?.pid === instance.pid
    && after.proxy.exit_ip === before.proxy.exit_ip && after.proxy.healthy && after.firewall.mode === 'allowed'
    && after.firewall.global_verified === true
    && finalRows.some(row => row.pid === browser.pid && row.command.includes(marker));
  output = {
    root_pid: instance.pid, exit_ip: before.proxy.exit_ip, session_unchanged_and_healthy: unchanged,
    browser_pid: browser.pid, https_probe: https,
    webrtc_public_candidates: { sampled_at: iceAt, status: candidateLeak ? 'fail' : webrtc.state === 'complete' && unchanged && https.status === 'pass' && !samplerError ? 'pass' : 'unknown', evidence: 'ICE with proxied-resolved numeric STUN endpoint; no media permission requested', stun_ip: stunIp, ...webrtc },
    ipv6_direct: { started_at:ipv6Started, sampled_at:ipv6At, status:externalIpv6.length ? 'fail' : 'unknown',
      evidence:'IPv6-only navigation plus concurrent process snapshots; absence of connections does not establish IPv6 isolation',
      scope:'TCP process snapshots only; unconnected UDP and short-lived traffic require packet/PF evidence',
      navigation:ipv6, external_socket_rows:externalIpv6, sample_count:ipv6Samples.length,
      browser_present_in_all_samples:ipv6Samples.length > 0 && ipv6Samples.every(sample => sample.browser_present),
      udp_bindings_without_peer:ipv6Samples.flatMap(sample => sample.sockets.split('\n')
        .filter(line => /\bIPv6\b/.test(line) && /\bUDP\b/.test(line) && !line.includes('->'))) },
    socket_samples:samples, external_socket_rows:suspicious,
  };
  if (samplerError) throw new Error(`Socket sampler failed: ${samplerError.message}`);
  if (!unchanged) throw new Error('Web session, proxy or verified PF changed during the probe');
  if (candidateLeak || suspicious.length) process.exitCode = 1;
} catch (error) {
  output ||= {root_pid:instance.pid, exit_ip:before.proxy.exit_ip, socket_samples:samples};
  output.error = error.message;
  for (const key of ['https_probe', 'webrtc_public_candidates', 'ipv6_direct']) {
    if (output[key]?.status === 'pass') output[key].status = 'unknown';
  }
  process.exitCode = 1;
} finally {
  sampling = false;
  await sampler;
  output ||= {root_pid:instance.pid, exit_ip:before.proxy.exit_ip, socket_samples:samples};
  output.temporary_tab_closed = false;
  if (targetId && ws.readyState === WebSocket.OPEN) {
    try { output.temporary_tab_closed = (await send('Target.closeTarget', {targetId}, undefined, true)).success === true; }
    catch (error) { output.cleanup_error = error.message; }
  }
  if (targetId && !output.temporary_tab_closed) {
    output.cleanup_error ||= 'Temporary-tab closure was not confirmed';
    for (const key of ['https_probe', 'webrtc_public_candidates', 'ipv6_direct']) {
      if (output[key]?.status === 'pass') output[key].status = 'unknown';
    }
    process.exitCode = 1;
  }
  for (const item of pending.values()) clearTimeout(item.timer);
  pending.clear();
  for (const child of children) child.kill('SIGKILL');
  ws.close();
  output.finished_at = new Date().toISOString();
  try { save(output); } catch (error) { console.error(error.message); process.exitCode = 1; }
  clearTimeout(hardStop);
}
console.log(JSON.stringify({root_pid:output.root_pid, healthy:output.session_unchanged_and_healthy,
  samples:samples.length, https:output.https_probe, webrtc:output.webrtc_public_candidates, ipv6:output.ipv6_direct,
  temporary_tab_closed:output.temporary_tab_closed, error:output.error}, null, 2));
process.exit(process.exitCode || 0);
