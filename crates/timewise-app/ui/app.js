/* TimeWise UI: role screen, worker pairing + child status, master dashboard.
   Talks to the Tauri shell via window.__TAURI__.core.invoke and to the
   embedded master API over plain fetch (localhost). */

const TAURI = window.__TAURI__;
// Demo/dev fallback: outside the Tauri webview (plain browser), behave as a
// master on the default port so the dashboard can be exercised standalone.
const invoke = TAURI
  ? TAURI.core.invoke
  : async (cmd) => {
      if (cmd === "get_state")
        return { role: "master", port: 47820, worker_id: "demo", masters: [], track_self: false, idle_threshold_s: 300 };
      throw new Error("not running inside the TimeWise app");
    };
const $ = (sel) => document.querySelector(sel);

let uiState = null;
let apiBase = null; // set when role === 'master'
let summary = { children: [], pending: [] };
let children = [];
let workers = [];
let charts = {};
let currentView = "overview";

function show(viewId) {
  currentView = viewId.replace("view-", "");
  for (const s of document.querySelectorAll("main > section")) s.classList.add("hidden");
  $("#" + viewId).classList.remove("hidden");
  for (const b of document.querySelectorAll("nav button"))
    b.classList.toggle("active", b.dataset.view === currentView);
}

function fmt(s) {
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/* Charts update IN PLACE (no destroy/recreate) so the page height never
   changes and the scroll position survives the 15s auto-refresh. */
function renderChart(id, labels, data, colors) {
  const existing = charts[id];
  if (existing) {
    existing.data.labels = labels;
    existing.data.datasets[0].data = data;
    existing.data.datasets[0].backgroundColor = colors;
    existing.update("none");
    return;
  }
  charts[id] = new Chart($("#" + id), {
    type: "bar",
    data: { labels, datasets: [{ data, backgroundColor: colors }] },
    options: {
      animation: false,
      plugins: { legend: { display: false } },
      scales: { y: { beginAtZero: true } },
    },
  });
}

/* ---------- boot ---------- */

async function boot() {
  uiState = await invoke("get_state");
  if (!uiState.role) {
    show("view-role");
  } else if (uiState.role === "worker") {
    bootWorker();
  } else {
    apiBase = `http://127.0.0.1:${uiState.port}/api/v1`;
    $("#nav").classList.remove("hidden");
    // Optional deep-link: index.html#view=overview|child|settings
    const hashView = new URLSearchParams(location.hash.slice(1)).get("view");
    const startView = ["overview", "child", "settings"].includes(hashView) ? hashView : "overview";
    show("view-" + startView);
    refreshAll().then(() => {
      if (startView === "child") renderChildDetail();
      if (startView === "settings") renderSettings();
    });
    setInterval(refreshAll, 15000);
  }
}

/* ---------- role screen ---------- */

document.querySelectorAll("#view-role .choice").forEach((el) =>
  el.addEventListener("click", async () => {
    await invoke("set_role", { role: el.dataset.role });
    uiState = await invoke("get_state");
    if (uiState.role === "worker") bootWorker();
    else location.reload();
  })
);

/* ---------- worker (child's own UI) ---------- */

async function bootWorker() {
  uiState = await invoke("get_state");
  if (uiState.masters.length === 0) {
    show("view-worker"); // pairing UI
  } else {
    show("view-child-status"); // the child's own screen
    refreshChildStatus();
    setInterval(refreshChildStatus, 30000);
  }
}

async function refreshChildStatus() {
  uiState = await invoke("get_state");
  const m = uiState.masters[0];
  if (!m) return;
  try {
    const resp = await fetch(`${m.base_url}/api/v1/config`, {
      headers: { authorization: `Bearer ${m.token}`, "x-worker-id": uiState.worker_id },
    });
    if (!resp.ok) throw new Error(String(resp.status));
    const cfg = await resp.json();
    $("#cs-today").textContent = fmt(cfg.usage.today_s);
    $("#cs-points").textContent = cfg.points_balance;
    if (cfg.goal.daily_min) {
      const goalS = cfg.goal.daily_min * 60;
      const pct = Math.min(100, Math.round((cfg.usage.today_s * 100) / goalS));
      $("#cs-goal-row").classList.remove("hidden");
      $("#cs-goal-text").textContent = `${pct}% of today's ${cfg.goal.daily_min}-minute goal`;
      $("#cs-goal-bar").style.width = pct + "%";
      $("#cs-goal-bar").style.background = pct >= 100 ? "var(--bad)" : pct >= 90 ? "var(--warn)" : "var(--good)";
    } else {
      $("#cs-goal-row").classList.add("hidden");
    }
    $("#cs-note").textContent = "Tracking is on. You're doing great!";
  } catch (e) {
    $("#cs-note").textContent = "Can't reach the parent right now — still tracking, everything is saved on this device.";
  }
}

$("#btn-discover").addEventListener("click", async () => {
  $("#discover-status").textContent = "Searching…";
  const urls = await invoke("discover");
  $("#discover-status").textContent = urls.length ? "" : "Nothing found — try the manual address below.";
  $("#discovered").innerHTML = urls
    .map((u) => `<div class="row"><span>${u}</span><button class="btn primary" onclick="pair('${u}')">Pair</button></div>`)
    .join("");
});

$("#btn-pair-manual").addEventListener("click", () => {
  let v = $("#manual-url").value.trim();
  if (v && !v.startsWith("http")) v = "http://" + v;
  pair(v);
});

async function pair(url) {
  $("#pair-status").textContent = "Pairing…";
  try {
    const msg = await invoke("pair_master", { baseUrl: url });
    $("#pair-status").textContent = msg + " — the parent must approve this device on their dashboard.";
    // Move to the child's own status screen right away.
    setTimeout(bootWorker, 1200);
  } catch (e) {
    $("#pair-status").innerHTML = `<span class="error">${e}</span>`;
  }
}

/* ---------- master dashboard ---------- */

async function api(path, opts) {
  const resp = await fetch(apiBase + path, opts);
  if (!resp.ok) throw new Error(`${resp.status} ${await resp.text()}`);
  return resp.status === 204 ? null : resp.json();
}

async function refreshAll() {
  summary = await api("/dashboard/summary");
  children = await api("/children");
  renderPending();
  renderOverview();
  fillChildSelects();
  if (currentView === "child") renderChildDetail();
  if (currentView === "settings") renderSettings();
}

function renderPending() {
  $("#pending-card").classList.toggle("hidden", summary.pending.length === 0);
  $("#pending-list").innerHTML = summary.pending
    .map(
      (w, i) => `<div class="row"><span><b>${w.hostname || "?"}</b> · user ${w.os_user || "?"} · ${w.os}</span>
        <input placeholder="Child's name" id="pend-name-${i}" list="children-names" size="14" />
        <button class="btn primary" onclick="approve('${w.worker_id}', ${i})">Approve</button></div>`
    )
    .join("");
  $("#children-names").innerHTML = children.map((c) => `<option value="${c.name}">`).join("");
}

async function approve(workerId, idx) {
  const name = ($("#pend-name-" + idx).value || "").trim() || "Child";
  await api(`/workers/${workerId}/approve`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ child_name: name }),
  });
  refreshAll();
}

function renderOverview() {
  $("#children-grid").innerHTML = summary.children
    .map(
      (c) => `<div class="card">
        <h2><span class="dot ${c.online ? "on" : "off"}"></span>${c.name}</h2>
        <div class="row"><span class="big">${fmt(c.today_s)}</span><span class="muted">today</span></div>
        <div class="muted">${fmt(c.week_s)} this week · ⭐ ${c.points_balance} points</div>
        ${c.online ? "" : '<div class="error">Disconnected — check the child’s computer</div>'}
      </div>`
    )
    .join("");
  renderChart(
    "chart-overview",
    summary.children.map((c) => c.name),
    summary.children.map((c) => Math.round(c.today_s / 60)),
    "#4f7cff"
  );
}

function fillChildSelects() {
  for (const sel of [$("#child-select"), $("#goal-child")]) {
    const cur = sel.value;
    sel.innerHTML = children.map((c) => `<option value="${c.id}">${c.name}</option>`).join("");
    if ([...sel.options].some((o) => o.value === cur)) sel.value = cur;
  }
}

function rangeBounds() {
  const now = Math.floor(Date.now() / 1000);
  if ($("#range-select").value === "week") {
    const d = new Date();
    const monday = new Date(d);
    monday.setHours(0, 0, 0, 0);
    monday.setDate(d.getDate() - ((d.getDay() + 6) % 7));
    return { from: Math.floor(monday.getTime() / 1000), to: now };
  }
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return { from: Math.floor(d.getTime() / 1000), to: now };
}

async function renderChildDetail() {
  const id = $("#child-select").value;
  if (!id) return;
  const { from, to } = rangeBounds();
  const d = await api(`/dashboard/child/${id}?from=${from}&to=${to}`);
  $("#child-usage").textContent = `today ${fmt(d.usage.today_s)} · week ${fmt(d.usage.week_s)}`;
  $("#app-table tbody").innerHTML = d.breakdown
    .map((b) => `<tr><td>${b.app_name}</td><td>${b.category}</td><td>${fmt(b.duration_s)}</td><td>${b.pct.toFixed(1)}%</td></tr>`)
    .join("") || '<tr><td colspan="4" class="muted">No data in this range yet.</td></tr>';
  renderChart(
    "chart-tod",
    ["Morning", "Afternoon", "Evening", "Night"],
    [d.tod.morning_s, d.tod.afternoon_s, d.tod.evening_s, d.tod.night_s].map((s) => Math.round(s / 60)),
    ["#8ecae6", "#4f7cff", "#e0a02e", "#6c5ce7"]
  );
  $("#points-balance").textContent = d.points_balance;
  $("#points-table tbody").innerHTML = d.points_history
    .map((p) => `<tr><td>${p.date}</td><td>+${p.points}</td><td>${p.reason.replaceAll("_", " ")}</td></tr>`)
    .join("") || '<tr><td colspan="3" class="muted">No points yet — set a goal in Settings.</td></tr>';
}

/* ---------- settings ---------- */

let uncatApps = [];

async function renderSettings() {
  uiState = await invoke("get_state");
  $("#track-self").checked = !!uiState.track_self;

  const id = $("#goal-child").value;
  if (id) {
    const d = await api(`/dashboard/child/${id}`);
    $("#goal-daily").value = d.goal.daily_min ?? "";
    $("#goal-weekly").value = d.goal.weekly_min ?? "";
    uncatApps = await api(`/dashboard/uncategorized/${id}`);
  }

  // Merge: every known device, with a child picker (iteration 2).
  workers = await api("/workers");
  $("#merge-table tbody").innerHTML = workers
    .map(
      (w) => `<tr><td>${w.hostname || "?"}</td><td>${w.os_user || "?"}</td><td>${w.os}</td>
        <td>${w.approved ? childPicker(w) : "<span class='muted'>pending approval</span>"}</td></tr>`
    )
    .join("");

  const cats = ["Games", "Educational", "Entertainment", "Social Media", "Productivity", "Browsers", "Other"];
  $("#uncat-table tbody").innerHTML = uncatApps
    .map(
      (a, i) => `<tr><td>${a.app_name}</td><td>${fmt(a.total_s)}</td><td>
        <select id="cat-${i}">${cats.map((c) => `<option>${c}</option>`).join("")}</select>
        <button class="btn" onclick="setOverride(${i})">Save</button></td></tr>`
    )
    .join("") || '<tr><td colspan="3" class="muted">Everything is categorized. 🎉</td></tr>';
}

function childPicker(w) {
  const opts = children
    .map((c) => `<option value="${c.id}" ${c.id === w.child_id ? "selected" : ""}>${c.name}</option>`)
    .join("");
  return `<select onchange="assignWorker('${w.worker_id}', this.value)">${opts}</select>`;
}

async function assignWorker(workerId, childId) {
  await api(`/workers/${workerId}/assign`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ child_id: childId }),
  });
  refreshAll();
}

async function setOverride(i) {
  const a = uncatApps[i];
  const sel = $("#cat-" + i);
  await api("/categories/override", {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ app_name: a.app_name, category: sel.value }),
  });
  renderSettings();
}

$("#track-self").addEventListener("change", async (e) => {
  await invoke("set_track_self", { enabled: e.target.checked });
});

$("#btn-save-goal").addEventListener("click", async () => {
  const id = $("#goal-child").value;
  const body = {
    daily_min: $("#goal-daily").value ? Number($("#goal-daily").value) : null,
    weekly_min: $("#goal-weekly").value ? Number($("#goal-weekly").value) : null,
  };
  await api(`/children/${id}/goal`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  $("#goal-status").textContent = "Saved ✓";
  setTimeout(() => ($("#goal-status").textContent = ""), 2000);
});

document.querySelectorAll("nav button").forEach((b) =>
  b.addEventListener("click", () => {
    show("view-" + b.dataset.view);
    if (b.dataset.view === "child") renderChildDetail();
    if (b.dataset.view === "settings") renderSettings();
  })
);
$("#child-select").addEventListener("change", renderChildDetail);
$("#range-select").addEventListener("change", renderChildDetail);
$("#goal-child").addEventListener("change", renderSettings);

boot();
