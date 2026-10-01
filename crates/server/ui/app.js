"use strict";

const $ = (id) => document.getElementById(id);

const ui = {
  torrents: [],
  selected: null,
  fileSignature: "",
  sse: false,
};

const SSE_NAMES = ["added", "removed", "state_changed", "progress", "completed", "error"];

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function formatBytes(value) {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let number = Number(value) || 0;
  let index = 0;
  while (number >= 1024 && index < units.length - 1) {
    number /= 1024;
    index += 1;
  }
  const digits = index === 0 || number >= 100 ? 0 : 1;
  return `${number.toFixed(digits)} ${units[index]}`;
}

function formatRate(value) {
  return `${formatBytes(value)}/s`;
}

function formatEta(torrent) {
  const left = Number(torrent.left) || 0;
  const rate = Number(torrent.download_rate) || 0;
  if (left === 0 || rate === 0) return "inf";
  const seconds = Math.floor(left / rate);
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${secs}s`;
  return `${secs}s`;
}

function stateLabel(state) {
  if (state && typeof state === "object") {
    return state.error ? "error" : "unknown";
  }
  return String(state || "unknown");
}

function errorText(torrent) {
  if (torrent.error) return torrent.error;
  if (torrent.state && typeof torrent.state === "object" && torrent.state.error) {
    return torrent.state.error;
  }
  return "";
}

function report(error) {
  if (!error || error.message === "forbidden") return;
  showStatus(error.message || String(error));
}

function showStatus(message, kind) {
  const node = $("status");
  node.classList.toggle("ok", kind === "ok");
  if (!message) {
    node.hidden = true;
    node.textContent = "";
    return;
  }
  node.hidden = false;
  node.textContent = message;
}

async function api(path, options) {
  const response = await fetch(path, { credentials: "same-origin", ...options });
  if (response.status === 403) {
    location.assign("/login");
    throw new Error("forbidden");
  }
  return response;
}

async function apiJson(path, options) {
  const response = await api(path, options);
  const text = await response.text();
  const body = text ? JSON.parse(text) : null;
  if (!response.ok) {
    const message = body && body.error ? body.error : `request failed (${response.status})`;
    throw new Error(message);
  }
  return body;
}

function showView(name) {
  $("view-torrents").hidden = name !== "torrents";
  $("view-settings").hidden = name !== "settings";
  $("nav-torrents").setAttribute("aria-current", name === "torrents" ? "page" : "false");
  $("nav-settings").setAttribute("aria-current", name === "settings" ? "page" : "false");
  if (name === "settings") {
    loadSettings().catch(report);
  }
}

async function refreshSession() {
  const session = await apiJson("/api/v1/session");
  $("down-rate").textContent = `Down ${formatRate(session.download_rate)}`;
  $("up-rate").textContent = `Up ${formatRate(session.upload_rate)}`;
  $("listen-port").textContent = `Port ${session.listen_port}`;
  $("version").textContent = session.version || "";
}

function torrentRows(torrents) {
  return torrents.map((torrent) => {
    const id = escapeHtml(torrent.id);
    const progress = Math.max(0, Math.min(100, (Number(torrent.progress) || 0) * 100));
    const selected = torrent.id === ui.selected ? " selected" : "";
    const ratio = Number(torrent.ratio);
    return `<tr data-id="${id}" class="${selected.trim()}">
      <td class="name" data-label="Name">${escapeHtml(torrent.name || torrent.id)}</td>
      <td data-label="Size">${formatBytes(torrent.size)}</td>
      <td data-label="Progress"><span class="bar"><span style="width:${progress.toFixed(1)}%"></span></span>${progress.toFixed(1)}%</td>
      <td data-label="State">${escapeHtml(stateLabel(torrent.state))}</td>
      <td data-label="Down">${formatRate(torrent.download_rate)}</td>
      <td data-label="Up">${formatRate(torrent.upload_rate)}</td>
      <td data-label="Ratio">${Number.isFinite(ratio) ? ratio.toFixed(2) : "0.00"}</td>
      <td data-label="ETA">${formatEta(torrent)}</td>
      <td data-label="Category">${escapeHtml(torrent.category || "")}</td>
      <td class="actions" data-label="Actions">
        <button type="button" data-action="pause">Pause</button>
        <button type="button" data-action="resume">Resume</button>
        <button type="button" data-action="recheck">Recheck</button>
        <button type="button" data-action="remove" class="danger">Remove</button>
      </td>
    </tr>`;
  }).join("");
}

function renderTorrents() {
  const empty = $("empty");
  const table = document.querySelector(".torrent-table");
  empty.hidden = ui.torrents.length !== 0;
  table.hidden = ui.torrents.length === 0;
  $("torrent-body").innerHTML = torrentRows(ui.torrents);
  if (ui.selected && !ui.torrents.some((torrent) => torrent.id === ui.selected)) {
    closeDetail();
  } else if (ui.selected) {
    const current = ui.torrents.find((torrent) => torrent.id === ui.selected);
    if (current) fillDetailMeta(current);
  }
}

async function refreshTorrents() {
  ui.torrents = await apiJson("/api/v1/torrents");
  renderTorrents();
  if (ui.selected) {
    await refreshFiles(ui.selected);
  }
}

async function refreshCategories() {
  const categories = await apiJson("/api/v1/categories");
  $("category-list").innerHTML = categories
    .map((category) => `<option value="${escapeHtml(category.name)}"></option>`)
    .join("");
}

function fillDetailMeta(torrent) {
  $("detail-name").textContent = torrent.name || torrent.id;
  $("detail-meta").textContent = `${stateLabel(torrent.state)} · ${formatBytes(torrent.size)} · ${torrent.save_path || ""}`;
  const message = errorText(torrent);
  $("detail-error").hidden = !message;
  $("detail-error").textContent = message;
}

function fileSignature(files) {
  return files.map((file) => `${file.index}:${file.priority}:${file.length}`).join("|");
}

function renderFiles(hash, files) {
  const signature = fileSignature(files);
  const list = $("detail-files");
  if (signature === ui.fileSignature && list.children.length) {
    for (const file of files) {
      const progress = document.querySelector(`[data-file-progress="${file.index}"]`);
      if (progress) {
        const pct = Math.max(0, Math.min(100, (Number(file.progress) || 0) * 100));
        progress.textContent = `${formatBytes(file.length)} · ${pct.toFixed(1)}%`;
      }
    }
    return;
  }
  ui.fileSignature = signature;
  if (!files.length) {
    list.innerHTML = `<li class="muted">No file list yet.</li>`;
    return;
  }
  const options = ["skip", "low", "normal", "high"];
  list.innerHTML = files.map((file) => {
    const pct = Math.max(0, Math.min(100, (Number(file.progress) || 0) * 100));
    const choices = options.map((name) => {
      const selected = name === file.priority ? " selected" : "";
      return `<option value="${name}"${selected}>${name}</option>`;
    }).join("");
    return `<li>
      <span class="file-path">${escapeHtml(file.path)}</span>
      <span class="muted" data-file-progress="${file.index}">${formatBytes(file.length)} · ${pct.toFixed(1)}%</span>
      <label>Priority
        <select data-file-index="${file.index}" data-hash="${escapeHtml(hash)}">${choices}</select>
      </label>
    </li>`;
  }).join("");
}

async function openDetail(hash) {
  ui.selected = hash;
  ui.fileSignature = "";
  $("detail").hidden = false;
  document.querySelector(".layout").classList.add("has-detail");
  const torrent = ui.torrents.find((item) => item.id === hash);
  if (torrent) fillDetailMeta(torrent);
  renderTorrents();
  await refreshFiles(hash);
}

function closeDetail() {
  ui.selected = null;
  ui.fileSignature = "";
  $("detail").hidden = true;
  document.querySelector(".layout").classList.remove("has-detail");
  renderTorrents();
}

async function refreshFiles(hash) {
  const torrent = await apiJson(`/api/v1/torrents/${hash}`);
  const known = ui.torrents.findIndex((item) => item.id === hash);
  if (known >= 0) ui.torrents[known] = torrent;
  fillDetailMeta(torrent);
  const files = await apiJson(`/api/v1/torrents/${hash}/files`);
  renderFiles(hash, files);
}

async function torrentAction(hash, action) {
  if (action === "remove") {
    const torrent = ui.torrents.find((item) => item.id === hash);
    const name = torrent ? torrent.name : hash;
    const deleteFiles = $("delete-files").checked;
    const extra = deleteFiles ? " Files on disk will be deleted." : "";
    if (!window.confirm(`Remove ${name}?${extra}`)) return;
    const query = deleteFiles ? "?delete_files=true" : "?delete_files=false";
    await apiJson(`/api/v1/torrents/${hash}${query}`, { method: "DELETE" });
    if (ui.selected === hash) closeDetail();
  } else {
    await apiJson(`/api/v1/torrents/${hash}/${action}`, { method: "POST" });
  }
  await refreshTorrents();
}

async function addTorrent(event) {
  event.preventDefault();
  showStatus("");
  const magnet = $("add-magnet").value.trim();
  const file = $("add-file").files[0];
  const savePath = $("add-save-path").value.trim();
  const category = $("add-category").value.trim();
  const paused = $("add-paused").checked;
  if (!magnet && !file) {
    showStatus("Add a magnet link or choose a .torrent file.");
    return;
  }
  let response;
  if (file) {
    const data = new FormData();
    data.append("torrent", file, file.name);
    if (magnet) data.append("magnet", magnet);
    if (savePath) data.append("save_path", savePath);
    if (category) data.append("category", category);
    if (paused) data.append("paused", "true");
    response = await api("/api/v1/torrents", { method: "POST", body: data });
  } else {
    response = await api("/api/v1/torrents", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        magnet,
        save_path: savePath || null,
        category: category || null,
        paused,
      }),
    });
  }
  const body = await response.json().catch(() => null);
  if (!response.ok) {
    showStatus(body && body.error ? body.error : `add failed (${response.status})`);
    return;
  }
  $("add-magnet").value = "";
  $("add-file").value = "";
  $("add-paused").checked = false;
  await refreshTorrents();
  await refreshCategories();
  if (body && body.id) await openDetail(body.id);
}

function yesNo(value) {
  return value ? "yes" : "no";
}

function renderReadonly(settings) {
  const rows = [
    ["Download directory", settings.download_dir],
    ["Engine listen port", settings.listen_port],
    ["Max peers per torrent", settings.max_peers_per_torrent],
    ["Max connections", settings.max_connections],
    ["DHT", yesNo(settings.dht)],
    ["PEX", yesNo(settings.pex)],
    ["LPD", yesNo(settings.lpd)],
    ["NAT", yesNo(settings.nat)],
    ["Web seeds", yesNo(settings.webseed)],
    ["Seed ratio limit", settings.seed_ratio_limit],
    ["Seed time limit (minutes)", settings.seed_time_limit],
    ["Max active downloads", settings.max_active_downloads],
    ["Max active uploads", settings.max_active_uploads],
    ["Max active", settings.max_active],
    ["HTTP host", settings.server_host],
    ["HTTP port", settings.server_port],
    ["Username", settings.server_username],
    ["qBittorrent compat", yesNo(settings.qbittorrent_compat)],
  ];
  $("settings-readonly").innerHTML = rows
    .map(([label, value]) => `<dt>${escapeHtml(label)}</dt><dd>${escapeHtml(value)}</dd>`)
    .join("");
}

async function loadSettings() {
  const settings = await apiJson("/api/v1/settings");
  $("set-download-limit").value = settings.download_limit;
  $("set-upload-limit").value = settings.upload_limit;
  $("set-alt-download-limit").value = settings.alt_download_limit;
  $("set-alt-upload-limit").value = settings.alt_upload_limit;
  $("set-alt-mode").checked = Boolean(settings.alt_mode);
  renderReadonly(settings);
}

async function saveSettings(event) {
  event.preventDefault();
  showStatus("");
  await apiJson("/api/v1/settings", {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      download_limit: Number($("set-download-limit").value),
      upload_limit: Number($("set-upload-limit").value),
      alt_download_limit: Number($("set-alt-download-limit").value),
      alt_upload_limit: Number($("set-alt-upload-limit").value),
      alt_mode: $("set-alt-mode").checked,
    }),
  });
  showStatus("Speed limits saved.", "ok");
  await loadSettings();
}

function connectEvents() {
  const source = new EventSource("/api/v1/events");
  source.onopen = () => {
    ui.sse = true;
  };
  source.onerror = () => {
    ui.sse = false;
  };
  for (const name of SSE_NAMES) {
    source.addEventListener(name, () => {
      refreshTorrents().catch(report);
    });
  }
}

$("nav-torrents").addEventListener("click", () => showView("torrents"));
$("nav-settings").addEventListener("click", () => showView("settings"));
$("logout").addEventListener("click", async () => {
  await fetch("/api/v1/logout", { method: "POST", credentials: "same-origin" });
  location.assign("/login");
});
$("add-form").addEventListener("submit", (event) => {
  addTorrent(event).catch(report);
});
$("settings-form").addEventListener("submit", (event) => {
  saveSettings(event).catch(report);
});
$("detail-close").addEventListener("click", closeDetail);
$("torrent-body").addEventListener("click", (event) => {
  const button = event.target.closest("button");
  const row = event.target.closest("tr");
  if (!row) return;
  if (button) {
    event.stopPropagation();
    torrentAction(row.dataset.id, button.dataset.action).catch(report);
    return;
  }
  openDetail(row.dataset.id).catch(report);
});
$("detail-files").addEventListener("change", (event) => {
  const select = event.target.closest("select");
  if (!select) return;
  const hash = select.dataset.hash;
  const index = Number(select.dataset.fileIndex);
  apiJson(`/api/v1/torrents/${hash}/files`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ files: [{ index, priority: select.value }] }),
  }).then((files) => {
    ui.fileSignature = "";
    renderFiles(hash, files);
  }).catch(report);
});

connectEvents();
setInterval(() => {
  refreshSession().catch(report);
  if (!ui.sse) {
    refreshTorrents().catch(report);
  }
}, 1000);

refreshSession()
  .then(() => Promise.all([refreshTorrents(), refreshCategories()]))
  .catch(report);
