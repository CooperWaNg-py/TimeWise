/* TimeWise UI: role screen, worker pairing, master dashboard.
   Talks to the Tauri shell via window.__TAURI__.core.invoke and to the
   embedded master API over plain fetch (localhost). */

const TAURI = window.__TAURI__;
// Demo/dev fallback: outside the Tauri webview (plain browser), behave as a
// master on the default port so the dashboard can be exercised standalone.
const invoke = TAURI
  ? TAURI.core.invoke
  : async (cmd) => {
      if (cmd === "get_state") return { role: "master", port: 47820, worker_id: "demo", masters: [] };
      throw new Error("not running inside the TimeWise app");
    };
const $ = (sel) => document.querySelector(sel);

let uiState = null;
let apiBase = null; // set when role === 'master'
let summary = [];
let charts = {};

function show(viewId) {
  for (const s of document.querySelectorAll("main > section")) s.classList.add("hidden");
  $("#" + viewId).classList.remove("hidden");
  if (["overview", "child", "settings"].includes(viewId.replace("view-", ""))) {
    for (const b of document.querySelectorAll("nav button"))
      b.classList.toggle("active", b.dataset.view === viewId.replace("view-", ""));
  }
}

function fmt(s) {
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

function renderChart(id, config) {
  if (charts[id]) charts[id].destroy();
  charts[id] = new Chart($("#" + id), config);
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
    show("view-overview");
    refreshAll();
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

/* ---------- worker ---------- */

async function bootWorker() {
  show("view-worker");
  refreshWorkerStatus();
  setInterval(refreshWorkerStatus, 10000);
}

async function refreshWorkerStatus() {
  uiState = await invoke("get_state");
  const el = $("#worker-status");
  if (uiState.masters.length === 0) {
    el.textContent = "Not paired with any parent yet. Tracking has started; data is stored safely on this device until a parent approves you.";
  } else {
    el.innerHTML = "Tracking is on. Paired with:<br>" + uiState.masters.map((m) => `• ${m.base_url}`).join("<br>");
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
    refreshWorkerStatus();
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
  renderPending();
  renderOverview();
  fillChildSelects();
  if (!$("#view-child").classList.contains("hidden")) renderChildDetail();
  if (!$("#view-settings").classList.contains("hidden")) renderSettings();
}

function renderPending() {
  const pending = summary.filter((c) => !c.approved);
  $("#pending-card").classList.toggle("hidden", pending.length === 0);
  $("#pending-list").innerHTML = pending
    .map(
      (c) => `<div class="row"><span>${c.hostname || c.worker_id} (${c.os_user || "?"})</span>
        <input placeholder="Child's name" id="name-${c.worker_id}" size="12" />
        <button class="btn primary" onclick="approve('${c.worker_id}')">Approve</button></div>`
    )
    .join("");
}

async function approve(id) {
  const name = $("#name-" + id).value.trim() || "Child";
  await api(`/children/${id}/approve`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ child_name: name }),
  });
  refreshAll();
}

function renderOverview() {
  const approved = summary.filter((c) => c.approved);
  $("#children-grid").innerHTML = approved
    .map(
      (c) => `<div class="card">
        <h2><span class="dot ${c.online ? "on" : "off"}"></span>${c.child_name || "Child"}</h2>
        <div class="row"><span class="big">${fmt(c.today_s)}</span><span class="muted">today</span></div>
        <div class="muted">${fmt(c.week_s)} this week · ⭐ ${c.points_balance} points</div>
        ${c.online ? "" : '<div class="error">Disconnected — check the child’s computer</div>'}
      </div>`
    )
    .join("");
  renderChart("chart-overview", {
    type: "bar",
    data: {
      labels: approved.map((c) => c.child_name || "Child"),
      datasets: [{ label: "Today (minutes)", data: approved.map((c) => Math.round(c.today_s / 60)), backgroundColor: "#4f7cff" }],
    },
    options: { plugins: { legend: { display: false } }, scales: { y: { beginAtZero: true } } },
  });
}

function fillChildSelects() {
  const approved = summary.filter((c) => c.approved);
  for (const sel of [$("#child-select"), $("#goal-child")]) {
    const cur = sel.value;
    sel.innerHTML = approved.map((c) => `<option value="${c.worker_id}">${c.child_name || "Child"}</option>`).join("");
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
  renderChart("chart-tod", {
    type: "bar",
    data: {
      labels: ["Morning", "Afternoon", "Evening", "Night"],
      datasets: [{
        label: "Minutes",
        data: [d.tod.morning_s, d.tod.afternoon_s, d.tod.evening_s, d.tod.night_s].map((s) => Math.round(s / 60)),
        backgroundColor: ["#8ecae6", "#4f7cff", "#e0a02e", "#6c5ce7"],
      }],
    },
    options: { plugins: { legend: { display: false } }, scales: { y: { beginAtZero: true } } },
  });
  $("#points-balance").textContent = d.points_balance;
  $("#points-table tbody").innerHTML = d.points_history
    .map((p) => `<tr><td>${p.date}</td><td>+${p.points}</td><td>${p.reason.replaceAll("_", " ")}</td></tr>`)
    .join("") || '<tr><td colspan="3" class="muted">No points yet — set a goal in Settings.</td></tr>';
}

async function renderSettings() {
  const id = $("#goal-child").value;
  if (!id) return;
  const d = await api(`/dashboard/child/${id}`);
  $("#goal-daily").value = d.goal.daily_min ?? "";
  $("#goal-weekly").value = d.goal.weekly_min ?? "";
  const unc = await api(`/dashboard/uncategorized/${id}`);
  const cats = ["Games", "Educational", "Entertainment", "Social Media", "Productivity", "Browsers", "Other"];
  $("#uncat-table tbody").innerHTML = unc
    .map(
      (a) => `<tr><td>${a.app_name}</td><td>${fmt(a.total_s)}</td><td>
        <select id="cat-${CSS.escape(a.app_name)}">${cats.map((c) => `<option>${c}</option>`).join("")}</select>
        <button class="btn" onclick="setOverride('${a.app_name.replaceAll("'", "\\'")}')">Save</button></td></tr>`
    )
    .join("") || '<tr><td colspan="3" class="muted">Everything is categorized. 🎉</td></tr>';
}

async function setOverride(appName) {
  const sel = document.getElementById("cat-" + CSS.escape(appName));
  await api("/categories/override", {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ app_name: appName, category: sel.value }),
  });
  renderSettings();
}

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
