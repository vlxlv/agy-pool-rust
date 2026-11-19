// agy-pool dashboard: code shared by the Pool and Traffic pages.
// A page script fills `agyPage` with hooks (start, poll, tick, accounts, audit, stats), then calls agyBoot().
const agyPage = {};
let showEmail = false;
try { showEmail = localStorage.getItem('agy_show_email') === 'true'; } catch (_) {}
let showEmailAllowed = false;
let themeMode = window.__agyThemeSaved;
let accountsData = [];
let statusData = null;
// The server replays its last 50 events on connect; keep as many so both pages see the same window.
const EVENT_CAP = 50;
let eventList = [];
let sseAbort = null;
let reconnectTimer = null;

// Dashboard auth: the token is kept per tab (session storage) and sent as a Bearer header.
const TOKEN_KEY = 'agy_dashboard_token';
let authToken = '';
try { authToken = sessionStorage.getItem(TOKEN_KEY) || ''; } catch (_) {}
let authOk = false;
let started = false;
let controlEnabled = false;

const $ = (id) => document.getElementById(id);
const callHook = (name, ...args) => { if (typeof agyPage[name] === 'function') agyPage[name](...args); };

function saveToken(t) {
  authToken = t;
  try { if (t) { sessionStorage.setItem(TOKEN_KEY, t); } else { sessionStorage.removeItem(TOKEN_KEY); } } catch (_) {}
}

// keepToken: a 429 lockout may come from someone else, so the token stays valid.
function showTokenCard(message, keepToken) {
  authOk = false;
  $('config-dialog').close();
  if (!keepToken) saveToken('');
  if (sseAbort) { sseAbort.abort(); sseAbort = null; }
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  $('reconnect-banner').hidden = true;
  $('token-error').textContent = message || '';
  $('token-card').hidden = false;
  $('token-input').focus();
}

async function agyFetch(url, opts = {}) {
  const headers = Object.assign({}, opts.headers);
  if (authToken) headers['Authorization'] = `Bearer ${authToken}`;
  if ((opts.method || 'GET').toUpperCase() !== 'GET') {
    headers['X-Agy-Pool-Control'] = '1';
    headers['Content-Type'] = 'application/json';
  }
  const res = await fetch(url, Object.assign({}, opts, { headers }));
  if (res.status === 401) showTokenCard('Token rejected. Enter the dashboard token again.');
  if (res.status === 429) showTokenCard('Too many wrong tokens. Wait a minute, then try again.', true);
  return res;
}

let toastTimer = null;
function showToast(message, isError) {
  const el = $('toast');
  el.textContent = message;
  el.className = isError ? 'error' : '';
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, isError ? 6000 : 3000);
}

// POSTs a control action; returns the parsed body on success, or null after showing the error.
async function control(path, body) {
  try {
    const res = await agyFetch(path, { method: 'POST', body: JSON.stringify(body || {}) });
    let data = null;
    try { data = await res.json(); } catch (_) {}
    if (res.ok) return data || {};
    if (res.status !== 401 && res.status !== 429) {
      showToast((data && data.error && data.error.message) || `HTTP ${res.status}`, true);
    }
  } catch (err) {
    showToast(`Request failed: ${err}`, true);
  }
  return null;
}

// Formatting
function escapeHtml(str) {
  if (str === null || str === undefined) return '';
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}
function formatDuration(seconds) {
  if (seconds === null || seconds === undefined || isNaN(seconds)) return '—';
  if (seconds <= 0) return 'now';
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const mins = Math.floor((seconds % 3600) / 60);
  const secs = Math.floor(seconds % 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${mins}m ${secs}s`;
  if (mins > 0) return `${mins}m ${secs}s`;
  return `${secs}s`;
}
function formatTime(isoStr) {
  if (!isoStr) return '—';
  const d = new Date(isoStr);
  return isNaN(d.getTime()) ? String(isoStr) : d.toTimeString().split(' ')[0];
}
function fmtDurationParts(ms) {
  const n = Number(ms);
  if (ms == null || !Number.isFinite(n)) return { val: '—', unit: '' };
  if (n < 1000) return { val: String(Math.round(n)), unit: 'ms' };
  // Round once, then pick the unit, so 59999 ms is "1m 0.0s" rather than "60.00s".
  const cs = Math.round(n / 10);
  if (cs < 6000) return { val: (cs / 100).toFixed(2), unit: 's' };
  const ds = Math.round(n / 100);
  return { val: `${Math.floor(ds / 600)}m ${((ds % 600) / 10).toFixed(1)}s`, unit: '' };
}
function fmtMs(ms) {
  const p = fmtDurationParts(ms);
  return p.unit ? `${p.val}${p.unit}` : p.val;
}
function fmtTokens(n) {
  n = Number(n) || 0;
  if (n < 1000) return String(n);
  if (n < 1e6) return (n / 1e3).toFixed(1) + 'K';
  if (n < 1e9) return (n / 1e6).toFixed(2) + 'M';
  return (n / 1e9).toFixed(2) + 'B';
}
function sanitizeId(id) {
  return String(id || '').replace(/[^a-zA-Z0-9_-]/g, '_');
}
// Quota levels: under 20% is critical, under 40% is low.
function levelClass(pct) {
  if (pct < 20) return 'bad';
  if (pct < 40) return 'low';
  return '';
}
function clampPct(p) {
  return Math.min(100, Math.max(0, Number(p) || 0));
}
function secondsUntil(deadlineMs) {
  return Math.max(0, Math.ceil((deadlineMs - Date.now()) / 1000));
}
function accountInitials(name) {
  const parts = String(name || '?').trim().split(/[\s._@+-]+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return String(parts[0] || '?').slice(0, 2).toUpperCase();
}
function renderSparkBars(containerId) {
  const host = $(containerId);
  if (!host) return;
  const sample = eventList
    .filter((ev) => Number.isFinite(Number(ev.total_ms)) && Number(ev.total_ms) >= 0)
    .slice(0, 26)
    .reverse();
  if (sample.length === 0) {
    host.innerHTML = '<i style="height:18%;opacity:0.25"></i>';
    return;
  }
  const maxMs = Math.max(1, ...sample.map((ev) => Number(ev.total_ms)));
  host.innerHTML = sample.map((ev) => {
    const ms = Number(ev.total_ms);
    const h = Math.max(14, Math.round((ms / maxMs) * 100));
    const cls = Number(ev.status) >= 400 ? ' class="err"' : '';
    const tip = `${ev.model || 'rpc'} · ${ev.status} · ${fmtMs(ms)}`;
    return `<i${cls} style="height:${h}%" title="${escapeHtml(tip)}"></i>`;
  }).join('');
}

// Theme
const themeSwitcher = $('theme-switcher');
const themeButtons = Array.from(themeSwitcher.querySelectorAll('.theme-btn'));
const themeMedia = window.__agyThemeMedia;
function syncThemeUI() {
  const state = window.__applyAgyTheme(themeMode);
  themeMode = state.mode;
  for (const btn of themeButtons) {
    const active = btn.dataset.themeValue === themeMode;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  }
  const modeLabel = themeMode === 'system'
    ? `Auto (${state.effective === 'light' ? 'light' : 'dark'})`
    : (themeMode === 'light' ? 'Light' : 'Dark');
  themeSwitcher.title = `Theme: ${modeLabel}`;
}
syncThemeUI();
themeSwitcher.addEventListener('click', (e) => {
  const btn = e.target.closest('.theme-btn');
  if (!btn) return;
  themeMode = btn.dataset.themeValue || 'system';
  try { localStorage.setItem('agy_theme', themeMode); } catch (_) {}
  syncThemeUI();
});
if (themeMedia) {
  const onSystemThemeChange = () => { if (themeMode === 'system') syncThemeUI(); };
  if (themeMedia.addEventListener) themeMedia.addEventListener('change', onSystemThemeChange);
  else if (themeMedia.addListener) themeMedia.addListener(onSystemThemeChange);
}
window.addEventListener('storage', (e) => {
  // key is null when another tab clears all storage
  if (e.key === 'agy_theme' || e.key === null) {
    themeMode = e.newValue || 'system';
    syncThemeUI();
  }
});

// Gateway status: header version, footer endpoint and uptime
const versionBtn = $('version-btn');
const versionLabel = $('version-pill');
const versionBadge = $('version-update-badge');
function renderUptime() {
  if (!statusData) return;
  $('footer-uptime').textContent = `Up ${formatDuration(statusData.uptime_secs)}`;
  $('footer-requests').textContent = `${(statusData.total_requests || 0).toLocaleString()} requests served`;
}
async function fetchStatus() {
  try {
    const res = await agyFetch('/api/v1/status');
    if (!res.ok) return;
    statusData = await res.json();
    const endpoint = $('footer-endpoint');
    endpoint.textContent = `${window.location.host || statusData.listen}, ${statusData.strategy} routing`;
    if (statusData.listen) endpoint.title = `Listening on ${statusData.listen}`;
    if (!strategySelect.disabled) strategySelect.value = statusData.strategy;
    versionLabel.textContent = statusData.version;
    if (!versionBtn.classList.contains('has-update')) {
      versionBtn.title = `agy-pool ${statusData.version}. Click to check for updates.`;
    }
    renderUptime();
    renderPulse();
    callHook('status', statusData);
  } catch (err) {
    console.error('Failed to fetch status:', err);
  }
}

// Updates
const updateBanner = $('update-banner');
const updateTargetVersion = $('update-target-version');
const upgradeBtn = $('upgrade-btn');
const updateMsg = $('update-msg');
const updateDeb = $('update-deb');
const updateDebCmd = $('update-deb-cmd');
let updateDismissed = false;
$('update-dismiss-btn').addEventListener('click', () => {
  updateDismissed = true;
  updateBanner.hidden = true;
});
versionBtn.addEventListener('click', async () => {
  if (versionBtn.classList.contains('has-update')) {
    updateDismissed = false;
    updateBanner.hidden = false;
    const target = !upgradeBtn.hidden ? upgradeBtn : $('update-deb-copy');
    target.focus();
    return;
  }
  showToast('Checking for updates...');
  const result = await checkForUpdate(true);
  if (!result) showToast('Could not check for updates. Try again in a minute.', true);
  else if (!result.has_update) showToast(`agy-pool ${versionLabel.textContent} is up to date.`);
});
$('update-deb-copy').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(updateDebCmd.textContent);
    showToast('Command copied');
  } catch (_) {
    // navigator.clipboard needs HTTPS or localhost; over plain HTTP select the text instead.
    window.getSelection().selectAllChildren(updateDebCmd);
    showToast('Command selected. Press Ctrl+C to copy.');
  }
});
upgradeBtn.addEventListener('click', async () => {
  upgradeBtn.disabled = true;
  upgradeBtn.textContent = 'Upgrading...';
  updateMsg.className = 'update-msg';
  updateMsg.textContent = 'Downloading and applying the update...';
  try {
    const res = await agyFetch('/api/v1/update', { method: 'POST', body: JSON.stringify({}) });
    if (res.ok) {
      const data = await res.json().catch(() => ({}));
      const ver = (data.version || updateTargetVersion.textContent || '').replace(/^v/, '');
      upgradeBtn.textContent = 'Upgraded';
      updateMsg.className = 'update-msg success';
      let seconds = 3;
      const say = () => { updateMsg.textContent = `Upgraded to ${ver}. The gateway restarts; reloading in ${seconds}s...`; };
      say();
      const timer = setInterval(() => {
        seconds -= 1;
        if (seconds <= 0) {
          clearInterval(timer);
          window.location.reload();
        } else {
          say();
        }
      }, 1000);
    } else {
      let errMsg = `HTTP ${res.status}`;
      try { errMsg = (await res.json()).error?.message || errMsg; } catch (_) {}
      upgradeBtn.disabled = false;
      upgradeBtn.textContent = 'Retry upgrade';
      updateMsg.className = 'update-msg error';
      updateMsg.textContent = `Upgrade failed: ${errMsg}`;
    }
  } catch (err) {
    upgradeBtn.disabled = false;
    upgradeBtn.textContent = 'Retry upgrade';
    updateMsg.className = 'update-msg error';
    updateMsg.textContent = `Upgrade failed: ${err.message || err}`;
  }
});
async function checkForUpdate(force = false) {
  if (!authOk) return;
  try {
    const res = await agyFetch(force ? '/api/v1/update?refresh=true' : '/api/v1/update');
    if (!res.ok) return;
    const data = await res.json();
    if (data && data.has_update && data.latest_version) {
      const latestVer = data.latest_version.replace(/^v/, '');
      const curVer = (data.current_version || versionLabel.textContent).replace(/^v/, '');
      updateTargetVersion.textContent = latestVer;
      // A dpkg-owned binary cannot be replaced in place: show the apt command instead of the button.
      const deb = data.package_managed ? data.deb_package_name : null;
      upgradeBtn.hidden = !!data.package_managed || !controlEnabled;
      updateDeb.hidden = !data.package_managed;
      if (data.package_managed && /^https:\/\/github\.com\//.test(data.release_url || '')) {
        $('update-deb-link').href = data.release_url;
      }
      updateDebCmd.textContent = deb
        ? `curl -fLO ${data.release_url.replace('/releases/tag/', '/releases/download/')}/${deb} && sudo apt install ./${deb}`
        : 'sudo apt install ./<file>.deb';
      if (!updateDismissed) updateBanner.hidden = false;
      versionBtn.classList.add('has-update');
      versionBtn.title = `Update available: ${curVer} to ${latestVer}. Click to upgrade.`;
      versionBadge.hidden = false;
      versionBadge.textContent = latestVer;
    } else {
      if (!force) updateBanner.hidden = true;
      versionBtn.classList.remove('has-update');
      versionBtn.title = `agy-pool ${versionLabel.textContent}. Click to check for updates.`;
      versionBadge.hidden = true;
    }
    return data;
  } catch (err) {
    console.warn('Update check failed:', err);
  }
}

// Routing strategy
const strategySelect = $('strategy-select');
strategySelect.addEventListener('change', async () => {
  const wanted = strategySelect.value;
  strategySelect.disabled = true;
  try {
    await control('/api/v1/strategy', { strategy: wanted });
  } finally {
    strategySelect.disabled = false;
    fetchStatus();
  }
});

// Config & health drawer
const configView = $('config-view');
function renderConfig(data) {
  configView.textContent = data.config ? JSON.stringify(Object.assign({ config_path: data.config_path }, data.config), null, 2) : (data.error || '');
  const restart = data.restart_required_fields || [];
  const note = $('config-restart');
  note.hidden = restart.length === 0;
  note.textContent = `Restart the gateway to apply: ${restart.join(', ')}`;
}
let configPanelSeq = 0;
async function loadConfigPanel() {
  const seq = ++configPanelSeq;
  const list = $('diag-list');
  list.innerHTML = '<li class="faint">Running checks...</li>';
  let cfg = { error: 'Could not load the config.' };
  let items;
  try {
    const [cfgRes, diagRes] = await Promise.all([agyFetch('/api/v1/config'), agyFetch('/api/v1/diagnostics')]);
    cfg = cfgRes.ok ? await cfgRes.json() : { error: `Could not load the config (HTTP ${cfgRes.status}).` };
    if (!diagRes.ok) throw new Error(`HTTP ${diagRes.status}`);
    const d = await diagRes.json();
    items = d.checks.map(c => `<li class="diag-${escapeHtml(c.status)}"><strong>${escapeHtml(c.name)}</strong> ${escapeHtml(c.detail)}</li>`).join('');
  } catch (err) {
    items = `<li class="diag-error">Checks failed: ${escapeHtml(String(err))}</li>`;
  }
  // A later load (reload, rerun) owns the panel; a slow earlier answer must not overwrite it.
  if (seq !== configPanelSeq) return;
  renderConfig(cfg);
  list.innerHTML = items;
}
$('config-btn').addEventListener('click', () => {
  const dlg = $('config-dialog');
  if (!dlg.open) dlg.showModal();
  loadConfigPanel();
});
$('diag-rerun-btn').addEventListener('click', loadConfigPanel);
$('reload-config-btn').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  const data = await control('/api/v1/config/reload', {});
  btn.disabled = false;
  if (!data) return;
  const applied = data.applied_fields.length ? data.applied_fields.join(', ') : 'nothing changed';
  const restart = data.restart_required_fields;
  showToast(restart.length
    ? `Applied: ${applied}. Restart needed for: ${restart.join(', ')}`
    : `Config reloaded (${applied}).`, restart.length > 0);
  loadConfigPanel();
  fetchStatus();
});

// Clicking the dimmed area around a modal closes it; Esc already does.
document.addEventListener('click', (e) => {
  if (e.target instanceof HTMLDialogElement && e.target.open) e.target.close();
});

// Accounts (the Pool page shows them; both pages use them for the pulse)
async function fetchAccounts() {
  try {
    const res = await agyFetch(`/api/v1/accounts?show_email=${showEmail ? 'true' : 'false'}`);
    if (!res.ok) return;
    const data = await res.json();
    const fetchedAtMs = Date.now();
    showEmailAllowed = Boolean(data.show_email_allowed);
    accountsData = (data.accounts || []).map((acc) => {
      for (const win of [acc.five_hour, acc.weekly]) {
        if (win && win.reset_at_unix > 0) {
          win._deadlineMs = fetchedAtMs + (Number(win.reset_in_secs) || 0) * 1000;
        }
      }
      return acc;
    });
    renderPulse();
    callHook('accounts', data);
  } catch (err) {
    console.error('Failed to fetch accounts:', err);
  }
}

// Pool-wide quota: the average over accounts in rotation, and the soonest reset of each window.
function poolSummary() {
  const sum = { five: null, weekly: null, nextFiveMs: null, nextWeeklyMs: null, active: 0 };
  let five = 0, fiveN = 0, wk = 0, wkN = 0;
  for (const acc of accountsData) {
    if (acc.scheduler_state !== 'active') continue;
    sum.active += 1;
    if (acc.five_hour) {
      five += clampPct(acc.five_hour.remaining_percent);
      fiveN += 1;
      const d = acc.five_hour._deadlineMs;
      if (d !== undefined && (sum.nextFiveMs === null || d < sum.nextFiveMs)) sum.nextFiveMs = d;
    }
    if (acc.weekly) {
      wk += clampPct(acc.weekly.remaining_percent);
      wkN += 1;
      const d = acc.weekly._deadlineMs;
      if (d !== undefined && (sum.nextWeeklyMs === null || d < sum.nextWeeklyMs)) sum.nextWeeklyMs = d;
    }
  }
  if (fiveN) sum.five = five / fiveN;
  if (wkN) sum.weekly = wk / wkN;
  return sum;
}

// Generation speed counts output and thinking tokens over the whole request; prompt tokens are not generated.
function outputTokens(ev) {
  return (Number(ev.output_tokens) || 0) + (Number(ev.thinking_tokens) || 0);
}
function outputSpeed(ev) {
  const tok = outputTokens(ev);
  return ev.total_ms > 0 && tok > 0 ? (tok * 1000) / ev.total_ms : 0;
}

// Live pulse over the last 20 requests: smoothed speed and prompt-cache hit rate.
function pulseStats() {
  const recent = eventList.slice(0, 20);
  let speedSum = 0, speedN = 0, prompt = 0, cached = 0, thinking = 0, out = 0;
  for (const ev of recent) {
    const s = outputSpeed(ev);
    if (s > 0) { speedSum += s; speedN += 1; }
    const p = Number(ev.tokens) || 0;
    prompt += p;
    cached += Math.min(Number(ev.cached_tokens) || 0, p);
    thinking += Number(ev.thinking_tokens) || 0;
    out += Number(ev.output_tokens) || 0;
  }
  return {
    count: recent.length,
    speed: speedN ? speedSum / speedN : null,
    cacheRate: prompt > 0 ? (cached * 100) / prompt : null,
    cached,
    thinkingShare: out + thinking > 0 ? (thinking * 100) / (out + thinking) : null,
  };
}
function renderPulse() {
  const p = pulseStats();
  $('pulse-speed').textContent = p.speed === null ? '— tok/s' : `${p.speed.toFixed(1)} tok/s`;
  $('pulse-cache').textContent = p.cacheRate === null ? '— cache' : `${p.cacheRate.toFixed(1)}% cache`;
  const pool = poolSummary();
  $('pop-quota').textContent = pool.five === null ? '—' : `${pool.five.toFixed(1)}%`;
  $('pop-reset').textContent = pool.nextFiveMs === null ? '—' : formatDuration(secondsUntil(pool.nextFiveMs));
  $('pop-speed').textContent = p.speed === null ? '—' : `${p.speed.toFixed(1)} tok/s`;
  $('pop-thinking').textContent = p.thinkingShare === null ? '—' : `${p.thinkingShare.toFixed(0)}%`;
  const lat = statusData && statusData.latency;
  $('pop-ttft').textContent = lat ? `${fmtMs(lat.ttfb_p50_ms)} / ${fmtMs(lat.ttfb_p95_ms)}` : '—';
  $('pop-total').textContent = lat ? `${fmtMs(lat.total_p50_ms)} / ${fmtMs(lat.total_p95_ms)}` : '—';
  $('pop-cache').textContent = p.cacheRate === null ? '—' : `${p.cacheRate.toFixed(1)}%`;
  $('pop-cached').textContent = fmtTokens(p.cached);
  const low = accountsData.filter(a => a.scheduler_state === 'active' && a.five_hour && a.five_hour.remaining_percent <= 15);
  const alertBox = $('pop-alert');
  alertBox.hidden = low.length === 0;
  alertBox.textContent = low.length === 1
    ? `${low[0].name} is below 15% of its 5-hour quota.`
    : `${low.length} accounts are below 15% of their 5-hour quota.`;
  $('pop-sample').textContent = p.count ? `From the last ${p.count} requests.` : 'No requests yet.';
}

// Live event stream
const reconnectBanner = $('reconnect-banner');
const connDot = $('conn-dot');
function setConn(state, text) {
  connDot.className = `dot ${state}`;
  $('conn').title = text;
  $('conn-text').textContent = text;
}
function handleAuditEvent(data) {
  try {
    const ev = JSON.parse(data);
    const isDup = eventList.some(
      (item) => (ev.id && item.id === ev.id && item.timestamp === ev.timestamp)
        || (!ev.id && item.timestamp === ev.timestamp && item.account === ev.account && item.model === ev.model)
    );
    if (isDup) return;
    eventList.unshift(ev);
    if (eventList.length > EVENT_CAP) eventList.pop();
    renderPulse();
    callHook('audit', ev);
  } catch (err) {
    console.error('Failed to parse SSE audit event:', err);
  }
}
function handleStatsEvent(data) {
  try {
    const stats = JSON.parse(data);
    if (stats) callHook('stats', stats);
  } catch (err) {
    console.error('Failed to parse SSE stats event:', err);
  }
}
function scheduleReconnect() {
  setConn('down', 'Reconnecting');
  reconnectBanner.hidden = false;
  if (!reconnectTimer) {
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connectSSE();
    }, 3000);
  }
}
// EventSource cannot send an Authorization header, so the event stream is read through fetch.
async function connectSSE() {
  if (!authOk) return;
  if (sseAbort) sseAbort.abort();
  const ctrl = new AbortController();
  sseAbort = ctrl;
  setConn('wait', 'Connecting');
  try {
    const res = await agyFetch('/api/v1/events', { signal: ctrl.signal });
    if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
    setConn('live', 'Live');
    reconnectBanner.hidden = true;
    if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
    // The server replays recent events on connect; start from its copy.
    eventList = [];
    callHook('reset');
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let idx;
      while ((idx = buf.indexOf('\n\n')) >= 0) {
        const block = buf.slice(0, idx);
        buf = buf.slice(idx + 2);
        let event = 'message';
        const data = [];
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) event = line.slice(6).trim();
          else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
        }
        if (event === 'audit') handleAuditEvent(data.join('\n'));
        else if (event === 'stats') handleStatsEvent(data.join('\n'));
      }
    }
  } catch (err) {
    if (!ctrl.signal.aborted) console.error('SSE stream error:', err);
  }
  // Stop when replaced, aborted, or waiting for a token; otherwise the stream dropped.
  if (ctrl.signal.aborted || sseAbort !== ctrl || !authOk) return;
  scheduleReconnect();
}

// Startup
$('token-form').addEventListener('submit', (e) => {
  e.preventDefault();
  saveToken($('token-input').value.trim());
  checkAuthAndStart();
});
let authTimer = null;
async function checkAuthAndStart() {
  if (authTimer) { clearTimeout(authTimer); authTimer = null; }
  let s;
  try {
    const res = await fetch('/api/v1/auth/status', { headers: authToken ? { Authorization: `Bearer ${authToken}` } : {} });
    if (res.status === 429) { showTokenCard('Too many wrong tokens. Wait a minute, then try again.', true); return; }
    if (res.status >= 500) throw new Error(`HTTP ${res.status}`);
    if (!res.ok) { showTokenCard(`The dashboard refused the request (HTTP ${res.status}).`); return; }
    s = await res.json();
  } catch (err) {
    console.error('Auth check failed:', err);
    authTimer = setTimeout(checkAuthAndStart, 3000);
    return;
  }
  if (!s.authenticated) {
    showTokenCard(authToken ? 'Token rejected.' : '');
    return;
  }
  $('token-card').hidden = true;
  $('token-input').value = '';
  authOk = true;
  controlEnabled = Boolean(s.control_enabled);
  strategySelect.hidden = !controlEnabled;
  $('config-btn').hidden = !controlEnabled;
  upgradeBtn.hidden = !controlEnabled;
  start();
}
function start() {
  Promise.all([fetchStatus(), fetchAccounts(), checkForUpdate()]).catch(console.error);
  callHook('start');
  connectSSE();
  if (started) return;
  started = true;
  setInterval(() => {
    if (!authOk) return;
    fetchStatus();
    fetchAccounts();
    callHook('poll');
  }, 5000);
  // 1s local tick: countdowns and uptime move without refetching.
  setInterval(() => {
    if (statusData && statusData.uptime_secs !== undefined) {
      statusData.uptime_secs += 1;
      renderUptime();
    }
    $('pop-reset').textContent = (() => {
      const next = poolSummary().nextFiveMs;
      return next === null ? '—' : formatDuration(secondsUntil(next));
    })();
    callHook('tick');
  }, 1000);
  setInterval(() => {
    if (authOk) checkForUpdate();
  }, 15 * 60 * 1000);
}
function agyBoot() {
  checkAuthAndStart();
}
