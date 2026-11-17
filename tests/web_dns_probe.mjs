// One temporary-tab DNS probe for the existing CIM Web session. Node >= 22.
// No full-status scans, cookie reads, PF mutations, or instance restarts.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { randomBytes } from 'node:crypto';
import { isIP } from 'node:net';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const destination = process.argv[2];
if (!destination || process.getuid?.() === 0 || typeof WebSocket !== 'function') {
  console.error('Run as the normal CIM user with Node >= 22 and an output JSON path.');
  process.exit(2);
}
const uid = process.getuid();
const home = os.homedir();
const started = Date.now();
const deadline = started + 85000;
const children = new Set();
const pending = new Map();
const abort = new AbortController();
let ws, targetId, serial = 0, exitCode = 0;
const result = {
  started_at: new Date(started).toISOString(), finished_at: null, sampled_at: null,
  root_pid: null, exit_ip: null,
  domain: `cim-${randomBytes(12).toString('hex')}.example.com`,
  nav: null, request_dispatched: false, session_unchanged_and_healthy: false,
  temporary_tab_closed: false, status: 'inconclusive', readiness_observations: [],
  scope: 'DNS navigation only; correlate with the enclosing en0 TCP/UDP port 53 capture',
};
const save = () => {
  result.finished_at = new Date().toISOString();
  fs.writeFileSync(destination, `${JSON.stringify(result, null, 2)}\n`, { mode: 0o600 });
  fs.chmodSync(destination, 0o600);
};
const budget = (maximum = 4000) => {
  abort.signal.throwIfAborted();
  const remaining = deadline - Date.now();
  if (remaining <= 0) throw new Error('Probe deadline reached');
  return Math.min(remaining, maximum);
};
const hardStop = setTimeout(() => {
  for (const child of children) child.kill('SIGKILL');
  if (targetId && ws?.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ id: ++serial, method: 'Target.closeTarget', params: { targetId } }));
  }
  result.error = 'Probe exceeded 89 seconds; temporary-tab cleanup is unconfirmed';
  result.status = 'inconclusive';
  try { save(); } catch { /* Keep the process deadline even if saving fails. */ }
  process.exit(1);
}, 89000);
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => {
  abort.abort(new Error(`Probe interrupted by ${signal}`));
  for (const child of children) child.kill('SIGKILL');
  for (const item of pending.values()) {
    clearTimeout(item.timer);
    item.reject(new Error(`Probe interrupted by ${signal}`));
  }
  pending.clear();
});
const exec = promisify(execFile);
const run = async (program, args, maximum = 4000) => {
  const command = exec(program, args, {
    timeout: budget(maximum), killSignal: 'SIGKILL', signal: abort.signal,
    maxBuffer: 4 * 1024 * 1024,
  });
  if (command.child) children.add(command.child);
  try { return (await command).stdout; }
  catch (error) {
    // Never copy a process table, command arguments, or stderr into evidence.
    throw new Error(`${path.basename(program)} failed (${error.killed ? 'timeout' : error.code ?? 'error'})`);
  } finally { if (command.child) children.delete(command.child); }
};
const get = async url => {
  const response = await fetch(url, {
    signal: AbortSignal.any([abort.signal, AbortSignal.timeout(budget())]),
  });
  if (!response.ok) throw new Error(`Local CDP returned HTTP ${response.status}`);
  return response.json();
};
const escape = value => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const hasArgument = (command, argument) => new RegExp(`(?:^|\\s)${escape(argument)}(?=\\s|$)`).test(command);
const startsProgram = (command, program) => command === program || command.startsWith(`${program} `);
const processRows = async () => (await run('/bin/ps', ['-axo', 'pid=,ppid=,uid=,gid=,lstart=,command=']))
  .split('\n').flatMap(line => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\S+\s+\S+\s+\d+\s+\d\d:\d\d:\d\d\s+\d{4})\s+(.+)$/);
    return match ? [{ pid: +match[1], ppid: +match[2], uid: +match[3], gid: +match[4],
      started: match[5].replace(/\s+/g, ' '), command: match[6] }] : [];
  });
const identityFields = row => ({ pid: row.pid, ppid: row.ppid, uid: row.uid, gid: row.gid, started: row.started });

async function identifySession(groupId) {
  const launch = await run('/bin/launchctl', ['print', `gui/${uid}/com.cim.daemon`]);
  const daemonPid = Number(launch.match(/^\s*pid = (\d+)\s*$/m)?.[1]);
  if (!Number.isInteger(daemonPid) || daemonPid <= 0) throw new Error('CIM launchd daemon is not running');
  const listeners = await run('/usr/sbin/lsof', ['-nP', '-a', '-iTCP:7891', '-sTCP:LISTEN', '-Fp']);
  const listenerPids = [...new Set([...listeners.matchAll(/^p(\d+)$/gm)].map(match => Number(match[1])))];
  if (listenerPids.length !== 1) throw new Error('SOCKS listener ownership is ambiguous');
  const rows = await processRows();
  const daemon = rows.find(row => row.pid === daemonPid);
  const proxy = rows.find(row => row.pid === listenerPids[0]);
  if (!daemon || daemon.uid !== uid || daemon.command !== `${home}/.cim/bin/cim daemon`
      || !proxy || proxy.uid !== uid || proxy.ppid !== daemon.pid
      || !startsProgram(proxy.command, `${home}/.cim/sing-box/sing-box`)) {
    throw new Error('Daemon or SOCKS listener identity does not match the installed CIM process family');
  }
  const marker = `--user-data-dir=${home}/.cim/instances/web/profile`;
  const browsers = ['/Applications/Chromium.app/Contents/MacOS/Chromium',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'];
  const candidates = rows.filter(row => row.uid === uid && row.gid === groupId
    && browsers.some(program => startsProgram(row.command, program))
    && hasArgument(row.command, marker)
    && /(?:^|\s)--remote-debugging-port=\d+(?=\s|$)/.test(row.command));
  if (candidates.length !== 1) throw new Error('Exactly one CIM profile browser is required');
  const browser = candidates[0];
  const root = rows.find(row => row.pid === browser.ppid);
  if (!root || root.ppid !== daemon.pid || !startsProgram(root.command, '/usr/bin/sudo')
      || !hasArgument(root.command, '/usr/local/libexec/cim-instance-launch')
      || !new RegExp(`(?:^|\\s)--uid\\s+${uid}(?=\\s|$)`).test(root.command)
      || !new RegExp(`(?:^|\\s)--gid\\s+${groupId}(?=\\s|$)`).test(root.command)) {
    throw new Error('Browser is not a direct child of the CIM isolation launcher');
  }
  const port = Number(browser.command.match(/(?:^|\s)--remote-debugging-port=(\d+)(?=\s|$)/)?.[1]);
  const session = browser.command.match(new RegExp(`(?:^|\\s)--cim-session-dir=(${escape(home)}/\\.cim/instances/web/sessions/[0-9a-f-]{36})(?=\\s|$)`))?.[1];
  if (!session || !Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Browser session marker or CDP port is invalid');
  const cdpListeners = await run('/usr/sbin/lsof', ['-nP', '-a', '-iTCP:' + port, '-sTCP:LISTEN', '-Fp']);
  const cdpPids = [...new Set([...cdpListeners.matchAll(/^p(\d+)$/gm)].map(match => Number(match[1])))];
  if (cdpPids.length !== 1 || cdpPids[0] !== browser.pid) throw new Error('CDP listener does not belong to the CIM browser');
  return { daemon: identityFields(daemon), proxy: identityFields(proxy), root: identityFields(root),
    browser: identityFields(browser), session_marker: session, cdp_port: port };
}

const normalizeRules = text => text.split('\n').map(line => line.trim().replace(/\s+/g, ' '))
  .filter(line => line && !line.startsWith('#'));
async function verifyFirewall(groupId) {
  const outputs = await Promise.all([
    run('/usr/bin/sudo', ['-n', '/sbin/pfctl', '-s', 'info']),
    run('/usr/bin/sudo', ['-n', '/sbin/pfctl', '-s', 'rules']),
    run('/usr/bin/sudo', ['-n', '/sbin/pfctl', '-a', 'com.cim', '-s', 'rules']),
  ]);
  if (!/^Status: Enabled(?:\s|$)/m.test(outputs[0])) throw new Error('PF engine is not enabled');
  const rootRules = normalizeRules(outputs[1]);
  const rootIndex = rootRules.findIndex(line => /^anchor "com\.cim" (?:quick )?all$/.test(line));
  if (rootIndex < 0 || rootRules.slice(0, rootIndex).some(line => /^(?:pass|anchor) /.test(line) && /(?:^| )quick(?: |$)/.test(line))) {
    throw new Error('PF main rules do not provide the expected unconditional CIM anchor');
  }
  const rules = normalizeRules(outputs[2]);
  const group = `group (?:= )?(?:cimgrp|${groupId})`;
  const control = new RegExp(`^pass in quick on lo0 inet proto tcp from 127\\.0\\.0\\.1 to 127\\.0\\.0\\.1 ${group} flags S/SA(?: keep state)?$`);
  const expected = [['127.0.0.1', 7890, 'inet'], ['127.0.0.1', 7891, 'inet'], ['::1', 7890, 'inet6'], ['::1', 7891, 'inet6']];
  if (rules.length !== 6 || !control.test(rules[0])
      || !expected.every(([host, port, family], index) => new RegExp(`^pass out quick (?:${family} )?proto tcp from any to ${escape(host)} port (?:= )?${port} ${group} (?:flags any )?no state$`).test(rules[index + 1]))
      || !new RegExp(`^block return out quick (?:all|from any to any) ${group}$`).test(rules[5])) {
    throw new Error('PF live anchor does not exactly match control + four stateless proxy passes + group block');
  }
  return { sampled_at: new Date().toISOString(), global_enabled: true, root_anchor_loaded: true,
    allow_policy_matches: true, anchor_rules: rules };
}
async function checkEnvironment(groupId) {
  const identity = await identifySession(groupId);
  await verifyFirewall(groupId);
  let ip;
  const exit_attempts = [];
  for (const endpoint of ['https://ipv4.icanhazip.com', 'https://api.ipify.org']) {
    try {
      const candidate = (await run('/usr/bin/curl', ['-q', '--noproxy', '', '--proxy', 'socks5h://127.0.0.1:7891',
        '--connect-timeout', '3', '--max-time', '8', '-fsS', endpoint], 9000)).trim();
      if (isIP(candidate) !== 4) throw new Error('Response was not a valid IPv4 exit');
      ip = candidate;
      exit_attempts.push({endpoint, at: new Date().toISOString(), success: true});
      break;
    } catch (error) {
      exit_attempts.push({endpoint, at: new Date().toISOString(), success: false, error: error.message});
    }
  }
  if (!ip) throw new Error('Both SOCKS exit checks failed; no healthy tunnel evidence');
  const firewall = await verifyFirewall(groupId);
  return { checked_at: new Date().toISOString(), identity, firewall, exit_ip: ip, exit_attempts };
}

const send = (method, params = {}, sessionId, cleanup = false) => new Promise((resolve, reject) => {
  let timeout;
  try { timeout = cleanup ? 1500 : budget(method === 'Page.navigate' ? 15000 : 5000); }
  catch (error) { reject(error); return; }
  const id = ++serial;
  const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out`)); }, timeout);
  pending.set(id, { resolve, reject, timer });
  try { ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) })); }
  catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
});
try {
  const groupText = await run('/usr/bin/dscl', ['.', '-read', '/Groups/cimgrp', 'PrimaryGroupID']);
  const groupId = Number(groupText.match(/^PrimaryGroupID:\s*(\d+)\s*$/m)?.[1]);
  if (!Number.isInteger(groupId) || groupId <= 0) throw new Error('cimgrp GID is unavailable');
  let before;
  const readinessDeadline = Date.now() + budget(30000);
  for (;;) {
    try { before = await checkEnvironment(groupId); break; }
    catch (error) {
      result.readiness_observations.push({at: new Date().toISOString(), ready: false, error: error.message});
      if (Date.now() >= readinessDeadline) throw error;
      await new Promise(resolve => setTimeout(resolve, budget(1000)));
    }
  }
  result.before = before;
  result.root_pid = before.identity.root.pid;
  result.exit_ip = before.exit_ip;
  result.readiness_observations.push({ at: before.checked_at, root_pid: result.root_pid,
    healthy: true, mode: 'allowed', global_verified: true, source: 'process identity, live PF, real SOCKS exit' });
  const port = String(before.identity.cdp_port);
  const version = await get(`http://127.0.0.1:${port}/json/version`);
  const websocketUrl = new URL(version.webSocketDebuggerUrl);
  if (websocketUrl.protocol !== 'ws:' || websocketUrl.hostname !== '127.0.0.1'
      || websocketUrl.port !== port) throw new Error('Unexpected CDP loopback endpoint');
  ws = new WebSocket(websocketUrl);
  let probeSession;
  ws.onmessage = event => {
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    if (message.method === 'Network.requestWillBeSent' && message.sessionId === probeSession
        && message.params?.request?.url === `https://${result.domain}/`) {
      result.request_dispatched = true;
      result.sampled_at ??= new Date().toISOString();
      result.browser_request_time = { wall_time: message.params.wallTime, timestamp: message.params.timestamp };
    }
    const item = pending.get(message.id);
    if (!item) return;
    pending.delete(message.id); clearTimeout(item.timer);
    if (message.error) item.reject(new Error(`CDP error: ${message.error.message}`));
    else item.resolve(message.result);
  };
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('CDP connect timed out')), budget());
    ws.onopen = () => { clearTimeout(timer); resolve(); };
    ws.onerror = () => { clearTimeout(timer); reject(new Error('CDP connection failed')); };
  });
  ({ targetId } = await send('Target.createTarget', { url: 'about:blank', background: true }));
  ({ sessionId: probeSession } = await send('Target.attachToTarget', { targetId, flatten: true }));
  await send('Page.enable', {}, probeSession);
  await send('Network.enable', {}, probeSession);
  await send('Network.setCacheDisabled', { cacheDisabled: true }, probeSession);
  await send('Network.setBypassServiceWorker', { bypass: true }, probeSession);
  result.navigation_started_at = new Date().toISOString();
  // An intentionally nonexistent hostname need not finish loading. Verify
  // the real browser request, observe a bounded window, then stop only this
  // probe navigation. A navigation timeout is never a DNS PASS verdict.
  const navigation = send('Page.navigate', { url: `https://${result.domain}/` }, probeSession)
    .then(nav => { result.nav = nav; result.navigation_finished_at = new Date().toISOString(); })
    .catch(error => { result.navigation_error = error.message; });
  const requestDeadline = Date.now() + budget(5000);
  while (!result.request_dispatched && Date.now() < requestDeadline) {
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  if (!result.request_dispatched) throw new Error('No browser request event for the unique DNS probe hostname');
  if (budget(5000) < 5000) throw new Error('Insufficient time for the complete DNS observation window');
  result.observation_started_at = new Date().toISOString();
  result.observation_ms = 5000;
  await new Promise(resolve => setTimeout(resolve, 5000));
  result.observation_finished_at = new Date().toISOString();
  await send('Page.stopLoading', {}, probeSession);
  result.stopped_after_observation = true;
  const control = await send('Runtime.evaluate', {expression: '1', returnByValue: true}, probeSession);
  if (control.result?.value !== 1) throw new Error('Browser control channel did not respond after observation');
  await Promise.race([navigation, new Promise(resolve => setTimeout(resolve, 1000))]);
  const after = await checkEnvironment(groupId);
  result.after = after;
  result.session_unchanged_and_healthy = JSON.stringify(after.identity) === JSON.stringify(before.identity)
    && after.exit_ip === before.exit_ip;
  if (!result.session_unchanged_and_healthy) throw new Error('CIM process identity or proxy exit changed during the probe');
  // The packet capture, not this navigation, decides the DNS leak verdict.
  result.status = 'captured_request';
} catch (error) {
  result.error = error.message;
  exitCode = 1;
} finally {
  if (targetId && ws?.readyState === WebSocket.OPEN) {
    try {
      const closed = await send('Target.closeTarget', { targetId }, undefined, true);
      result.temporary_tab_closed = closed.success === true;
    } catch (error) { result.cleanup_error = error.message; }
  }
  if (targetId && !result.temporary_tab_closed) { exitCode = 1; result.status = 'inconclusive'; }
  for (const item of pending.values()) clearTimeout(item.timer);
  pending.clear();
  ws?.close();
  for (const child of children) child.kill('SIGKILL');
  try { save(); } catch (error) { console.error(error.message); exitCode = 1; }
  clearTimeout(hardStop);
}
console.log(JSON.stringify({ root_pid: result.root_pid, started_at: result.started_at,
  finished_at: result.finished_at, domain: result.domain, status: result.status,
  request_dispatched: result.request_dispatched, temporary_tab_closed: result.temporary_tab_closed,
  error: result.error }));
process.exit(exitCode);
