// Opt-in real regression probe; run explicitly as the CIM user with Node 22+.
// It temporarily SIGSTOPs only the verified daemon-owned sing-box process.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { randomUUID } from 'node:crypto';
import { execFile, execFileSync } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';

const scriptPath = fileURLToPath(import.meta.url);
const fingerprintArgs = pid => ['-p', String(pid), '-o', 'pid=,ppid=,uid=,lstart=,comm='];
const normalized = text => text.trim().replace(/\s+/g, ' ');
function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  fs.renameSync(temp, file);
  fs.chmodSync(file, 0o600);
}
function resumeVerifiedSync(record) {
  if (!record) return { resumed: false, reason: 'no recorded proxy' };
  try {
    const fingerprint = normalized(execFileSync('/bin/ps', fingerprintArgs(record.pid), { encoding: 'utf8', timeout: 2500 }));
    const texts = execFileSync('/usr/sbin/lsof', ['-a', '-p', String(record.pid), '-d', 'txt', '-Fn'], { encoding: 'utf8', timeout: 2500 });
    if (fingerprint !== record.fingerprint || !texts.split('\n').includes(`n${record.executable}`)) {
      return { resumed: false, reason: 'PID identity changed; signal refused' };
    }
    process.kill(record.pid, 'SIGCONT');
    return { resumed: true, at: new Date().toISOString() };
  } catch (error) { return { resumed: false, reason: error.message }; }
}

// launchd owns this process, so it is independent of the probe/tool process
// tree. It always revalidates PID/start time/executable before sending CONT.
if (process.argv[2] === '--watchdog') {
  const payload = JSON.parse(Buffer.from(process.argv[3], 'base64url').toString('utf8'));
  if (process.getuid() !== payload.uid || payload.uid === 0) process.exit(2);
  writeJson(payload.log, { status: 'armed', armed_at: new Date().toISOString(), proxy_pid: payload.proxy.pid, watchdog_pid: process.pid });
  setTimeout(() => {
    writeJson(payload.log, { status: 'fired', fired_at: new Date().toISOString(), ...resumeVerifiedSync(payload.proxy) });
    process.exit(0);
  }, 90000);
} else {
  await main();
}

async function main() {
  if (process.getuid() === 0 || typeof WebSocket !== 'function' || !process.argv[2]) {
    console.error('Usage: node tests/web_proxy_fault_probe.mjs /absolute/path/evidence.json (normal CIM user, Node >= 22)');
    process.exit(2);
  }
  const destination = path.resolve(process.argv[2]);
  const uid = process.getuid();
  const deadline = Date.now() + 180000;
  const evidence = { probe_id: randomUUID(), started_at: new Date().toISOString(), verdict: 'UNKNOWN',
    steps: [], status_samples: [], target_ids: [], limits: 'One existing CIM Web session; no user tabs or cookies are read.' };
  const children = new Set();
  const command = promisify(execFile);
  let proxy, daemonPid, webRootPid, browserPid, browserPort, watchdogLabel;
  let stopped = false, resumed = false, client;
  let exitCode = 1;
  const stamp = (name, details = {}) => { evidence.steps.push({ name, at: new Date().toISOString(), ...details }); writeJson(destination, evidence); };
  const budget = (max = 10000) => {
    const left = deadline - Date.now();
    if (left <= 0) throw new Error('180-second probe deadline reached');
    return Math.min(max, left);
  };
  const sleep = ms => new Promise(resolve => setTimeout(resolve, budget(ms)));
  const run = async (program, args, timeout = 5000) => {
    const promise = command(program, args, { encoding: 'utf8', timeout: budget(timeout), killSignal: 'SIGKILL', maxBuffer: 4 * 1024 * 1024 });
    if (promise.child) children.add(promise.child);
    try { return (await promise).stdout; }
    finally { if (promise.child) children.delete(promise.child); }
  };
  const get = async url => {
    const response = await fetch(url, { signal: AbortSignal.timeout(budget(8000)) });
    if (!response.ok) throw new Error(`HTTP ${response.status} from local control endpoint`);
    return response.json();
  };
  const api = 'http://127.0.0.1:8080';
  const status = async () => {
    const state = await get(`${api}/api/status`);
    const sample = { at: new Date().toISOString(), proxy_healthy: state.proxy?.healthy,
      exit_ip: state.proxy?.exit_ip, firewall_mode: state.firewall?.mode,
      global_verified: state.firewall?.global_verified, web: state.instances?.find(item => item.kind === 'web') };
    evidence.status_samples.push(sample);
    return sample;
  };
  const rows = async () => (await run('/bin/ps', ['-axo', 'pid=,ppid=,uid=,command='])).split('\n').flatMap(line => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.+)$/);
    return match ? [{ pid: +match[1], ppid: +match[2], uid: +match[3], command: match[4] }] : [];
  });
  const listener = async port => {
    const pids = [...new Set((await run('/usr/sbin/lsof', ['-nP', `-iTCP:${port}`, '-sTCP:LISTEN', '-t']))
      .trim().split(/\s+/).filter(Boolean).map(Number))];
    if (pids.length !== 1 || !Number.isInteger(pids[0]) || pids[0] <= 1) throw new Error(`Expected one verified listener on ${port}`);
    return pids[0];
  };
  const verifyProxy = async () => {
    if (await listener(8080) !== daemonPid || await listener(7890) !== proxy.pid || await listener(7891) !== proxy.pid) {
      throw new Error('Daemon/proxy listener identity changed');
    }
    const current = (await rows()).find(row => row.pid === proxy.pid);
    const fingerprint = normalized(await run('/bin/ps', fingerprintArgs(proxy.pid)));
    const texts = await run('/usr/sbin/lsof', ['-a', '-p', String(proxy.pid), '-d', 'txt', '-Fn']);
    if (!current || current.ppid !== daemonPid || current.uid !== uid || fingerprint !== proxy.fingerprint
        || !texts.split('\n').includes(`n${proxy.executable}`)) throw new Error('Proxy process ownership/start time/executable changed');
  };
  const waitStatus = async (name, predicate, timeout) => {
    const end = Date.now() + budget(timeout);
    do {
      const sample = await status();
      if (sample.web?.pid !== webRootPid) throw new Error('CIM Web instance changed during the fault test');
      if (predicate(sample)) { stamp(name, { status: sample }); return sample; }
      await sleep(700);
    } while (Date.now() < end);
    throw new Error(`${name} was not observed within the allotted window`);
  };
  const healthy = sample => sample.proxy_healthy === true && sample.firewall_mode === 'allowed' && sample.global_verified === true;

  async function connect() {
    client?.close();
    const currentRows = await rows();
    const browser = currentRows.find(row => row.pid === browserPid && row.uid === uid);
    if (!browser || !browser.command.includes(`--remote-debugging-port=${browserPort}`)) throw new Error('Original CIM Chrome browser is gone');
    const version = await get(`http://127.0.0.1:${browserPort}/json/version`);
    const url = new URL(version.webSocketDebuggerUrl);
    if (url.protocol !== 'ws:' || url.hostname !== '127.0.0.1' || url.port !== String(browserPort)) throw new Error('Unexpected CDP endpoint');
    const ws = new WebSocket(url);
    let serial = 0;
    const pending = new Map();
    const events = [];
    const failPending = reason => {
      for (const item of pending.values()) { clearTimeout(item.timer); item.reject(new Error(`CDP ${reason}`)); }
      pending.clear();
    };
    ws.onmessage = event => {
      let message;
      try { message = JSON.parse(event.data); } catch { return; }
      if (!message.id) { if (message.method?.startsWith('Network.')) events.push(message); return; }
      const item = pending.get(message.id);
      if (!item) return;
      pending.delete(message.id); clearTimeout(item.timer);
      if (message.error) item.reject(new Error(`CDP ${message.error.message}`)); else item.resolve(message.result);
    };
    ws.onclose = () => failPending('connection closed');
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('CDP connect timed out')), budget(7000));
      ws.onopen = () => { clearTimeout(timer); resolve(); };
      ws.onerror = () => { clearTimeout(timer); reject(new Error('CDP connection failed')); };
    });
    client = { ws, events, sessionId: null,
      close() { failPending('client closed'); ws.close(); },
      send(method, params = {}, sessionId, timeout = 12000) {
        return new Promise((resolve, reject) => {
          if (ws.readyState !== WebSocket.OPEN) { reject(new Error('CDP is disconnected')); return; }
          const id = ++serial;
          const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP ${method} timed out`)); }, budget(timeout));
          pending.set(id, { resolve, reject, timer });
          try { ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) })); }
          catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
        });
      },
    };
    let targetId = evidence.target_ids.at(-1);
    if (targetId) {
      try { client.sessionId = (await client.send('Target.attachToTarget', { targetId, flatten: true })).sessionId; }
      catch { targetId = null; }
    }
    if (!targetId) {
      targetId = (await client.send('Target.createTarget', { url: 'about:blank', background: true })).targetId;
      evidence.target_ids.push(targetId);
      client.sessionId = (await client.send('Target.attachToTarget', { targetId, flatten: true })).sessionId;
    }
    await client.send('Page.enable', {}, client.sessionId);
    await client.send('Network.enable', {}, client.sessionId);
    await client.send('Network.setCacheDisabled', { cacheDisabled: true }, client.sessionId);
    await client.send('Network.setBypassServiceWorker', { bypass: true }, client.sessionId);
  }

  async function navigate(phase) {
    // Retry only a broken CDP control connection; a network result is retained.
    for (let attempt = 1; attempt <= 2; attempt++) {
      const observation = { phase, attempt, started_at: new Date().toISOString(), outcome: 'inconclusive' };
      try {
        if (!client || client.ws.readyState !== WebSocket.OPEN || attempt > 1) await connect();
        const sessionId = client.sessionId;
        const url = `https://example.com/?cim_fault=${evidence.probe_id}&phase=${phase}&nonce=${randomUUID()}`;
        client.events.length = 0;
        let navigation;
        try { navigation = await client.send('Page.navigate', { url }, sessionId, 14000); }
        catch (error) { observation.control_error = error.message; throw error; }
        observation.navigation = navigation;
        const end = Date.now() + budget(6000);
        do {
          const requestIds = new Set(client.events.filter(event => event.sessionId === sessionId
            && event.method === 'Network.requestWillBeSent' && event.params?.type === 'Document'
            && event.params.request?.url === url).map(event => event.params.requestId));
          const response = client.events.find(event => event.sessionId === sessionId && event.method === 'Network.responseReceived'
            && event.params?.type === 'Document' && event.params.response?.url === url)?.params.response;
          const failure = client.events.find(event => event.sessionId === sessionId && event.method === 'Network.loadingFailed'
            && requestIds.has(event.params?.requestId))?.params;
          if (response) {
            observation.response = { status: response.status, protocol: response.protocol,
              fromDiskCache: response.fromDiskCache === true, fromServiceWorker: response.fromServiceWorker === true };
            observation.outcome = response.status === 200 && !response.fromDiskCache && !response.fromServiceWorker ? 'http_200' : 'http_response';
            break;
          }
          if (failure || navigation.errorText) {
            const error = failure?.errorText || navigation.errorText;
            observation.network_error = error;
            // ERR_ABORTED, CDP failures and generic timeouts are not proof of
            // blocked egress. Require a concrete browser network failure.
            if (/^net::ERR_(CONNECTION_(REFUSED|RESET|CLOSED|FAILED)|PROXY_CONNECTION_FAILED|SOCKS_CONNECTION_FAILED|TUNNEL_CONNECTION_FAILED|TIMED_OUT|NETWORK_CHANGED|INTERNET_DISCONNECTED)$/.test(error)) {
              await client.send('Runtime.evaluate', { expression: '1', returnByValue: true }, sessionId, 2500);
              observation.outcome = 'network_failed';
            }
            break;
          }
          await sleep(100);
        } while (Date.now() < end);
        observation.finished_at = new Date().toISOString();
        stamp(`https_${phase}`, observation);
        return observation;
      } catch (error) {
        observation.finished_at = new Date().toISOString(); observation.control_error = error.message;
        stamp(`https_${phase}_control_error`, observation);
        if (attempt === 2) return observation;
      }
    }
  }

  function emergency(reason) {
    for (const child of children) child.kill('SIGKILL');
    evidence.error = reason; evidence.verdict = 'UNKNOWN';
    evidence.emergency_resume = stopped ? resumeVerifiedSync(proxy) : { resumed: false, reason: 'proxy was not stopped' };
    evidence.finished_at = new Date().toISOString();
    try { writeJson(destination, evidence); } catch {}
    // Leave the independent launchd guardian armed if normal cleanup did not
    // complete; it will validate identity and issue CONT at 90 seconds.
    process.exit(1);
  }
  const hardStop = setTimeout(() => emergency('180-second total limit reached'), 180000);
  process.once('SIGINT', () => emergency('Interrupted by SIGINT'));
  process.once('SIGTERM', () => emergency('Interrupted by SIGTERM'));

  try {
    const initial = await status();
    if (!healthy(initial) || !Number.isInteger(initial.web?.pid) || initial.web.pid <= 1) throw new Error('Web and globally verified PF must already be healthy');
    webRootPid = initial.web.pid;
    daemonPid = await listener(8080);
    const proxyPid = await listener(7890);
    const allRows = await rows();
    const daemon = allRows.find(row => row.pid === daemonPid);
    if (!daemon || daemon.uid !== uid || !/\/cim daemon(?:\s|$)/.test(daemon.command)) throw new Error('Port 8080 listener is not the current user CIM daemon');
    const executable = fs.realpathSync(path.join(os.homedir(), '.cim/sing-box/sing-box'));
    proxy = { pid: proxyPid, daemon_pid: daemonPid, executable,
      fingerprint: normalized(await run('/bin/ps', fingerprintArgs(proxyPid))) };
    await verifyProxy();
    evidence.proxy_identity = proxy;
    const managed = new Set([webRootPid]);
    for (let changed = true; changed;) {
      changed = false;
      for (const row of allRows) if (managed.has(row.ppid) && !managed.has(row.pid)) { managed.add(row.pid); changed = true; }
    }
    const marker = `--user-data-dir=${os.homedir()}/.cim/instances/web/profile`;
    const browser = allRows.find(row => managed.has(row.pid) && row.uid === uid && row.command.includes(marker)
      && !row.command.includes('--type=') && /--remote-debugging-port=\d+/.test(row.command));
    if (!browser) throw new Error('Original CIM-owned Web browser was not found');
    browserPid = browser.pid;
    browserPort = +browser.command.match(/--remote-debugging-port=(\d+)/)[1];
    evidence.web_identity = { root_pid: webRootPid, browser_pid: browserPid, cdp_port: browserPort };
    stamp('ownership_verified');
    const normal = await navigate('normal');
    if (normal.outcome !== 'http_200') throw new Error('Normal HTTPS baseline did not return uncached HTTP 200; fault not injected');

    watchdogLabel = `com.cim.audit-resume.${evidence.probe_id}`;
    const watchdogLog = `${destination}.watchdog.json`;
    const payload = Buffer.from(JSON.stringify({ uid, proxy, log: watchdogLog })).toString('base64url');
    await run('/bin/launchctl', ['submit', '-l', watchdogLabel, '--', process.execPath, scriptPath, '--watchdog', payload]);
    let armed = false;
    for (let attempt = 0; attempt < 30; attempt++) {
      try { const ack = JSON.parse(fs.readFileSync(watchdogLog, 'utf8')); armed = ack.status === 'armed' && ack.proxy_pid === proxy.pid; }
      catch {}
      if (armed) break;
      await sleep(100);
    }
    if (!armed) throw new Error('Independent 90-second SIGCONT guardian did not acknowledge; fault not injected');
    evidence.watchdog = { label: watchdogLabel, log: watchdogLog, timeout_seconds: 90 };
    stamp('watchdog_armed');
    await verifyProxy();
    stopped = true;
    process.kill(proxy.pid, 'SIGSTOP');
    stamp('proxy_sigstop', { pid: proxy.pid });
    const blockedStatus = await waitStatus('verified_blocked', sample => sample.proxy_healthy === false
      && sample.firewall_mode === 'blocked' && sample.global_verified === true, 40000);
    const blocked = await navigate('blocked');
    const stillBlocked = await status();
    if (blocked.outcome === 'http_200' || blocked.outcome === 'http_response') {
      evidence.verdict = 'FAIL'; throw new Error('An HTTPS document response arrived during verified proxy outage');
    }
    if (blocked.outcome !== 'network_failed' || stillBlocked.proxy_healthy !== false
        || stillBlocked.firewall_mode !== 'blocked' || stillBlocked.global_verified !== true) {
      throw new Error('Blocked navigation was not conclusively observed inside a verified blocked window');
    }
    evidence.blocked_probe = { passed: true, start_status: blockedStatus, end_status: stillBlocked };

    const resumedResult = resumeVerifiedSync(proxy);
    stamp('proxy_sigcont', resumedResult);
    if (!resumedResult.resumed) throw new Error('Could not safely resume the recorded proxy');
    resumed = true;
    await waitStatus('automatically_recovered', healthy, 60000);
    await verifyProxy();
    const recovered = await navigate('recovered');
    if (recovered.outcome !== 'http_200') throw new Error('Recovered HTTPS did not return uncached HTTP 200');
    const final = await status();
    if (!healthy(final) || final.web?.pid !== webRootPid) throw new Error('Final Web/PF state changed');
    evidence.verdict = 'PASS';
    exitCode = 0;
  } catch (error) {
    evidence.error = error.message;
    if (evidence.verdict !== 'FAIL') evidence.verdict = 'UNKNOWN';
    stamp('probe_error', { error: error.message });
  } finally {
    if (stopped) {
      const finalResume = resumeVerifiedSync(proxy);
      resumed ||= finalResume.resumed;
      stamp('finally_sigcont', finalResume);
    }
    if (client?.ws.readyState === WebSocket.OPEN) {
      evidence.closed_targets = [];
      for (const targetId of evidence.target_ids) {
        try { if ((await client.send('Target.closeTarget', { targetId }, undefined, 1200)).success) evidence.closed_targets.push(targetId); }
        catch {}
      }
    }
    evidence.temporary_tabs_closed = evidence.target_ids.every(id => evidence.closed_targets?.includes(id));
    client?.close();
    if (!evidence.temporary_tabs_closed && evidence.target_ids.length) {
      evidence.cleanup_error = 'Some temporary tab closures could not be confirmed';
      if (evidence.verdict === 'PASS') evidence.verdict = 'UNKNOWN';
      exitCode = 1;
    }
    if (watchdogLabel && (!stopped || resumed)) {
      try { await run('/bin/launchctl', ['remove', watchdogLabel]); evidence.watchdog_removed = true; }
      catch (error) { evidence.watchdog_cleanup = error.message; }
    }
    if (stopped && !resumed) {
      evidence.recovery_unconfirmed = true; evidence.verdict = 'UNKNOWN'; exitCode = 1;
    }
    for (const child of children) child.kill('SIGKILL');
    clearTimeout(hardStop);
    evidence.finished_at = new Date().toISOString();
    writeJson(destination, evidence);
  }
  console.log(JSON.stringify({ verdict: evidence.verdict, evidence: destination, error: evidence.error,
    proxy_resumed: resumed, temporary_tabs_closed: evidence.temporary_tabs_closed }, null, 2));
  process.exit(exitCode);
}
