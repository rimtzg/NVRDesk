let currentConfig = null;
let privacyMasksState = [];
let draftMasks = [];
let selectedMaskIdx = -1;
let dragOp = null;

async function fetchJson(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return res.json();
}

function populateSelect(el, items, selectedIdx, defaultLabel) {
  el.innerHTML = "";
  if (!items || items.length === 0) {
    const opt = document.createElement("option");
    opt.value = "0";
    opt.textContent = defaultLabel;
    el.appendChild(opt);
    return;
  }
  for (const item of items) {
    const opt = document.createElement("option");
    opt.value = String(item.index);
    opt.textContent = `${item.name} (${item.detail})`;
    if (item.index === selectedIdx) opt.selected = true;
    el.appendChild(opt);
  }
}

function populateMicSelect(el, items, selectedName) {
  el.innerHTML = "";
  const def = document.createElement("option");
  def.value = "";
  def.textContent = "System Default Microphone";
  el.appendChild(def);
  for (const item of items || []) {
    const opt = document.createElement("option");
    opt.value = item.name;
    opt.textContent = `${item.name} (${item.detail})`;
    if (selectedName && item.name === selectedName) opt.selected = true;
    el.appendChild(opt);
  }
}

async function loadInitial() {
  const [devices, cfg, templates] = await Promise.all([
    fetchJson("/api/v1/devices"),
    fetchJson("/api/v1/config"),
    fetchJson("/api/v1/fleet/templates"),
  ]);
  currentConfig = cfg;

  const tplSelect = document.getElementById("fleet-template-select");
  tplSelect.innerHTML = "";
  for (const tpl of templates || []) {
    const opt = document.createElement("option");
    opt.value = tpl.name;
    opt.textContent = tpl.name;
    if (cfg.fleet && cfg.fleet.template === tpl.name) opt.selected = true;
    tplSelect.appendChild(opt);
  }

  document.getElementById("cfg-source-kind").value = cfg.source.kind || "screen";
  document.getElementById("cfg-source-uri").value = cfg.source.uri || "";
  populateSelect(document.getElementById("cfg-display"), devices.monitors, cfg.source.display, "Primary Display");
  populateSelect(document.getElementById("cfg-webcam-device"), devices.webcams, cfg.webcam.device_index, "No Webcam Found");
  populateMicSelect(document.getElementById("cfg-audio-device"), devices.microphones, cfg.audio.device_name);

  document.getElementById("cfg-encoder-backend").value = cfg.encoder.backend;
  document.getElementById("cfg-main-codec").value = cfg.main_stream.codec || "h264";
  document.getElementById("cfg-main-fps").value = cfg.main_stream.fps;
  document.getElementById("cfg-main-bitrate").value = cfg.main_stream.bitrate_bps;
  document.getElementById("cfg-keystroke-mode").value = cfg.input_overlay.keystroke_mode;
  document.getElementById("cfg-show-clicks").checked = cfg.input_overlay.show_clicks;
  document.getElementById("cfg-tray-enabled").checked = cfg.daemon.tray_enabled;

  document.getElementById("cfg-webcam-enabled").checked = cfg.webcam.enabled;
  document.getElementById("cfg-webcam-mode").value = cfg.webcam.mode;
  document.getElementById("cfg-pip-pos").value = cfg.webcam.pip_position;

  document.getElementById("cfg-audio-enabled").checked = cfg.audio.enabled;
  document.getElementById("cfg-audio-codec").value = cfg.audio.codec;
  document.getElementById("cfg-backchannel-enabled").checked = Boolean(cfg.audio.backchannel_enabled);

  document.getElementById("cfg-rec-enabled").checked = cfg.recording.enabled;
  document.getElementById("cfg-rec-max-mb").value = cfg.recording.max_total_mb;

  document.getElementById("cfg-motion-enabled").checked = cfg.motion ? cfg.motion.enabled : true;
  document.getElementById("cfg-https-enabled").checked = cfg.tls ? cfg.tls.https_enabled : false;
  document.getElementById("cfg-rtsps-enabled").checked = cfg.tls ? cfg.tls.rtsps_enabled : false;
  privacyMasksState = Array.isArray(cfg.pos && cfg.pos.privacy_masks)
    ? structuredClone(cfg.pos.privacy_masks)
    : [];
  updateMasksSummary();

  document.getElementById("cfg-srt-enabled").checked = cfg.srt ? cfg.srt.enabled : false;
  document.getElementById("cfg-srt-mode").value = cfg.srt ? cfg.srt.mode : "caller";
  document.getElementById("cfg-srt-target").value = (cfg.srt && cfg.srt.target) ? cfg.srt.target : "";
  document.getElementById("cfg-srt-latency").value = cfg.srt ? cfg.srt.latency_ms : 120;
  document.getElementById("cfg-fleet-id").value = cfg.fleet ? `${cfg.fleet.site}/${cfg.fleet.terminal_id}` : "default-site/pos-01";
}

async function refreshStatus() {
  try {
    const st = await fetchJson("/api/v1/status");
    const badge = document.getElementById("status-badge");
    const pauseBtn = document.getElementById("toggle-pause-btn");
    if (st.paused) {
      badge.textContent = "Paused";
      badge.className = "badge paused";
      pauseBtn.textContent = "Resume";
    } else {
      badge.textContent = st.active_players > 0 ? "Streaming" : "Ready (Zero-Idle)";
      badge.className = "badge active";
      pauseBtn.textContent = "Pause";
    }
    pauseBtn.dataset.paused = String(st.paused);

    document.getElementById("stat-players").textContent = String(st.active_players);
    document.getElementById("stat-main-frames").textContent = String(st.main_frames);
    document.getElementById("stat-sub-frames").textContent = String(st.sub_frames);
    document.getElementById("stat-encoder").textContent = st.active_encoder || "idle";
    const secMode = [
      st.onvif_enabled ? "ONVIF" : null,
      st.https_enabled ? "HTTPS" : null,
      st.rtsps_port ? "RTSPS" : null,
      st.srt && st.srt.enabled ? (st.srt.connected ? "SRT:Live" : "SRT:Wait") : null,
    ].filter(Boolean).join(" / ") || "Disabled";
    document.getElementById("stat-onvif").textContent = secMode;

    if (st.recording && st.recording.enabled) {
      const mb = (st.recording.total_bytes / (1024 * 1024)).toFixed(1);
      document.getElementById("stat-recording").textContent = `${mb} MB (${st.recording.segment_count} files)`;
    } else {
      document.getElementById("stat-recording").textContent = "Off";
    }

    const endpoints = document.getElementById("endpoints-list");
    endpoints.innerHTML = st.endpoints
      .map((ep) => `<div class="endpoint-row"><span>${ep.label}</span><code>${ep.url}</code></div>`)
      .join("");

    const feed = document.getElementById("events-feed");
    const events = st.recent_events || [];
    feed.innerHTML = events.length === 0
      ? "<div class=\"event-row\"><span>No events yet</span><span>-</span></div>"
      : events.slice(0, 6).map((ev) => (
        `<div class="event-row"><span>${ev.topic}: ${ev.data_value}</span><span>${ev.utc_time}</span></div>`
      )).join("");
  } catch (err) {
    document.getElementById("status-badge").textContent = "Offline";
  }
}

document.getElementById("preview-profile").addEventListener("change", (e) => {
  document.getElementById("preview-img").src = `/preview/${e.target.value}.mjpeg?t=${Date.now()}`;
});

document.getElementById("toggle-pause-btn").addEventListener("click", async (e) => {
  const paused = e.target.dataset.paused === "true";
  await fetch(paused ? "/api/v1/control/resume" : "/api/v1/control/pause", { method: "POST" });
  await refreshStatus();
});

document.getElementById("save-cfg-btn").addEventListener("click", async () => {
  if (!currentConfig) return;
  const updated = structuredClone(currentConfig);
  updated.source.kind = document.getElementById("cfg-source-kind").value;
  const sourceUri = document.getElementById("cfg-source-uri").value.trim();
  updated.source.uri = sourceUri ? sourceUri : null;
  updated.source.display = Number(document.getElementById("cfg-display").value) || 0;
  updated.encoder.backend = document.getElementById("cfg-encoder-backend").value;
  updated.main_stream.codec = document.getElementById("cfg-main-codec").value;
  updated.main_stream.fps = Math.max(1, Number(document.getElementById("cfg-main-fps").value) || 15);
  updated.main_stream.bitrate_bps = Math.max(100000, Number(document.getElementById("cfg-main-bitrate").value) || 2000000);
  updated.input_overlay.keystroke_mode = document.getElementById("cfg-keystroke-mode").value;
  updated.input_overlay.show_clicks = document.getElementById("cfg-show-clicks").checked;
  updated.daemon.tray_enabled = document.getElementById("cfg-tray-enabled").checked;

  updated.webcam.enabled = document.getElementById("cfg-webcam-enabled").checked;
  updated.webcam.device_index = Number(document.getElementById("cfg-webcam-device").value) || 0;
  updated.webcam.mode = document.getElementById("cfg-webcam-mode").value;
  updated.webcam.pip_position = document.getElementById("cfg-pip-pos").value;

  updated.audio.enabled = document.getElementById("cfg-audio-enabled").checked;
  const micVal = document.getElementById("cfg-audio-device").value;
  updated.audio.device_name = micVal ? micVal : null;
  updated.audio.codec = document.getElementById("cfg-audio-codec").value;
  updated.audio.backchannel_enabled = document.getElementById("cfg-backchannel-enabled").checked;

  updated.recording.enabled = document.getElementById("cfg-rec-enabled").checked;
  updated.recording.max_total_mb = Math.max(64, Number(document.getElementById("cfg-rec-max-mb").value) || 1024);

  updated.motion = updated.motion || {};
  updated.motion.enabled = document.getElementById("cfg-motion-enabled").checked;
  updated.tls = updated.tls || {};
  updated.tls.https_enabled = document.getElementById("cfg-https-enabled").checked;
  updated.tls.rtsps_enabled = document.getElementById("cfg-rtsps-enabled").checked;
  updated.pos = updated.pos || {};
  updated.pos.privacy_masks = structuredClone(privacyMasksState);

  updated.srt = updated.srt || {};
  updated.srt.enabled = document.getElementById("cfg-srt-enabled").checked;
  updated.srt.mode = document.getElementById("cfg-srt-mode").value;
  const srtTarget = document.getElementById("cfg-srt-target").value.trim();
  updated.srt.target = srtTarget ? srtTarget : null;
  updated.srt.latency_ms = Math.max(20, Number(document.getElementById("cfg-srt-latency").value) || 120);

  const fleetRaw = document.getElementById("cfg-fleet-id").value.trim();
  if (fleetRaw.includes("/")) {
    const [site, terminalId] = fleetRaw.split("/", 2);
    updated.fleet = updated.fleet || {};
    if (site.trim()) updated.fleet.site = site.trim();
    if (terminalId.trim()) updated.fleet.terminal_id = terminalId.trim();
  }

  const msg = document.getElementById("save-msg");
  try {
    currentConfig = await fetchJson("/api/v1/config", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(updated),
    });
    msg.textContent = "Configuration saved and applied.";
    setTimeout(() => { msg.textContent = ""; }, 3000);
  } catch (err) {
    msg.textContent = `Save failed: ${err.message}`;
  }
});

loadInitial().then(refreshStatus);
setInterval(refreshStatus, 2000);

document.getElementById("send-osd-btn").addEventListener("click", async () => {
  const input = document.getElementById("osd-text-input");
  const level = document.getElementById("osd-level-select").value;
  const text = input.value.trim();
  if (!text) return;
  await fetch("/api/v1/osd/text", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      slot: "ticket",
      text,
      level,
      emit_event: level !== "info",
    }),
  });
  await refreshStatus();
});

document.getElementById("clear-osd-btn").addEventListener("click", async () => {
  await fetch("/api/v1/osd/text", { method: "DELETE" });
  document.getElementById("osd-text-input").value = "";
  await refreshStatus();
});

document.querySelectorAll(".tab-btn").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab-btn").forEach((b) => b.classList.remove("active"));
    document.querySelectorAll(".tab-panel").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    const panel = document.getElementById(btn.dataset.tab);
    if (panel) panel.classList.add("active");
  });
});

function clamp01(val) {
  return Math.max(0, Math.min(1, val));
}

function round3(val) {
  return Math.round(val * 1000) / 1000;
}

function updateMasksSummary() {
  const el = document.getElementById("masks-summary");
  if (!el) return;
  const count = privacyMasksState.length;
  el.textContent = count === 0
    ? "No masks configured"
    : `${count} mask${count === 1 ? "" : "s"} configured (click Save & Apply to persist)`;
}

function openMasksModal() {
  draftMasks = structuredClone(privacyMasksState);
  selectedMaskIdx = draftMasks.length > 0 ? 0 : -1;
  document.getElementById("masks-preview-img").src = `/preview/main.mjpeg?t=${Date.now()}`;
  const modal = document.getElementById("masks-modal");
  modal.classList.add("open");
  modal.setAttribute("aria-hidden", "false");
  renderMasksEditor();
}

function closeMasksModal() {
  const modal = document.getElementById("masks-modal");
  modal.classList.remove("open");
  modal.setAttribute("aria-hidden", "true");
  document.getElementById("masks-preview-img").src = "";
  dragOp = null;
}

function renderMasksEditor() {
  const overlay = document.getElementById("masks-overlay");
  const list = document.getElementById("masks-list");
  overlay.innerHTML = "";
  list.innerHTML = "";

  draftMasks.forEach((m, idx) => {
    const isSelected = idx === selectedMaskIdx;
    const box = document.createElement("div");
    box.className = `mask-box${isSelected ? " selected" : ""}`;
    box.style.left = `${m.x * 100}%`;
    box.style.top = `${m.y * 100}%`;
    box.style.width = `${m.width * 100}%`;
    box.style.height = `${m.height * 100}%`;
    box.dataset.idx = String(idx);

    const lbl = document.createElement("span");
    lbl.className = "mask-box-label";
    lbl.textContent = m.label || `Mask #${idx + 1}`;
    box.appendChild(lbl);

    if (isSelected) {
      ["nw", "n", "ne", "e", "se", "s", "sw", "w"].forEach((dir) => {
        const h = document.createElement("div");
        h.className = "mask-handle";
        h.dataset.dir = dir;
        h.dataset.idx = String(idx);
        box.appendChild(h);
      });
    }
    overlay.appendChild(box);

    const item = document.createElement("div");
    item.className = `mask-item${isSelected ? " selected" : ""}`;
    item.addEventListener("click", () => {
      selectedMaskIdx = idx;
      renderMasksEditor();
    });

    const row = document.createElement("div");
    row.className = "mask-item-row";
    const input = document.createElement("input");
    input.type = "text";
    input.placeholder = `Mask #${idx + 1} label`;
    input.value = m.label || "";
    input.addEventListener("click", (e) => e.stopPropagation());
    input.addEventListener("input", (e) => {
      draftMasks[idx].label = e.target.value;
      lbl.textContent = e.target.value || `Mask #${idx + 1}`;
    });

    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.className = "btn btn-danger";
    delBtn.textContent = "Delete";
    delBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      draftMasks.splice(idx, 1);
      if (selectedMaskIdx >= draftMasks.length) {
        selectedMaskIdx = draftMasks.length - 1;
      }
      renderMasksEditor();
    });
    row.appendChild(input);
    row.appendChild(delBtn);

    const coords = document.createElement("div");
    coords.className = "mask-coords";
    coords.textContent = `x: ${m.x.toFixed(2)}, y: ${m.y.toFixed(2)} | ${Math.round(m.width * 100)}% x ${Math.round(m.height * 100)}%`;

    item.appendChild(row);
    item.appendChild(coords);
    list.appendChild(item);
  });

  if (draftMasks.length === 0) {
    const empty = document.createElement("div");
    empty.className = "field-hint";
    empty.textContent = "No masks yet. Click '+ Add Mask' or drag on the preview.";
    list.appendChild(empty);
  }
}

function getNormCoords(evt) {
  const rect = document.getElementById("masks-overlay").getBoundingClientRect();
  return {
    nx: clamp01((evt.clientX - rect.left) / rect.width),
    ny: clamp01((evt.clientY - rect.top) / rect.height),
  };
}

document.getElementById("masks-overlay").addEventListener("pointerdown", (e) => {
  const { nx, ny } = getNormCoords(e);
  if (e.target.classList.contains("mask-handle")) {
    const idx = Number(e.target.dataset.idx);
    selectedMaskIdx = idx;
    dragOp = {
      type: "resize",
      dir: e.target.dataset.dir,
      idx,
      startX: nx,
      startY: ny,
      orig: { ...draftMasks[idx] },
    };
  } else if (e.target.classList.contains("mask-box")) {
    const idx = Number(e.target.dataset.idx);
    selectedMaskIdx = idx;
    dragOp = {
      type: "move",
      idx,
      startX: nx,
      startY: ny,
      orig: { ...draftMasks[idx] },
    };
    renderMasksEditor();
  } else {
    const newMask = { x: round3(nx), y: round3(ny), width: 0.02, height: 0.02, label: "MASK" };
    draftMasks.push(newMask);
    selectedMaskIdx = draftMasks.length - 1;
    dragOp = { type: "draw", idx: selectedMaskIdx, startX: nx, startY: ny };
    renderMasksEditor();
  }
  e.target.setPointerCapture?.(e.pointerId);
});

window.addEventListener("pointermove", (e) => {
  if (!dragOp) return;
  const { nx, ny } = getNormCoords(e);
  const m = draftMasks[dragOp.idx];
  if (!m) return;
  const minSize = 0.02;

  if (dragOp.type === "draw") {
    const x0 = Math.min(dragOp.startX, nx);
    const y0 = Math.min(dragOp.startY, ny);
    m.x = round3(x0);
    m.y = round3(y0);
    m.width = round3(Math.max(minSize, Math.abs(nx - dragOp.startX)));
    m.height = round3(Math.max(minSize, Math.abs(ny - dragOp.startY)));
  } else if (dragOp.type === "move") {
    const dx = nx - dragOp.startX;
    const dy = ny - dragOp.startY;
    m.x = round3(Math.max(0, Math.min(1 - dragOp.orig.width, dragOp.orig.x + dx)));
    m.y = round3(Math.max(0, Math.min(1 - dragOp.orig.height, dragOp.orig.y + dy)));
  } else if (dragOp.type === "resize") {
    const o = dragOp.orig;
    let left = o.x;
    let top = o.y;
    let right = o.x + o.width;
    let bottom = o.y + o.height;
    if (dragOp.dir.includes("w")) left = Math.min(right - minSize, nx);
    if (dragOp.dir.includes("e")) right = Math.max(left + minSize, nx);
    if (dragOp.dir.includes("n")) top = Math.min(bottom - minSize, ny);
    if (dragOp.dir.includes("s")) bottom = Math.max(top + minSize, ny);
    m.x = round3(left);
    m.y = round3(top);
    m.width = round3(right - left);
    m.height = round3(bottom - top);
  }
  renderMasksEditor();
});

window.addEventListener("pointerup", () => {
  dragOp = null;
});

document.getElementById("open-masks-modal-btn").addEventListener("click", openMasksModal);
document.getElementById("close-masks-modal-btn").addEventListener("click", closeMasksModal);
document.getElementById("cancel-masks-btn").addEventListener("click", closeMasksModal);
document.getElementById("add-mask-btn").addEventListener("click", () => {
  draftMasks.push({
    x: 0.35,
    y: 0.35,
    width: 0.28,
    height: 0.22,
    label: `PIN-${draftMasks.length + 1}`,
  });
  selectedMaskIdx = draftMasks.length - 1;
  renderMasksEditor();
});
document.getElementById("apply-masks-btn").addEventListener("click", () => {
  privacyMasksState = structuredClone(draftMasks);
  updateMasksSummary();
  closeMasksModal();
});

document.getElementById("apply-template-btn").addEventListener("click", async () => {
  const tpl = document.getElementById("fleet-template-select").value;
  const fleetRaw = document.getElementById("cfg-fleet-id").value.trim();
  const overrides = {};
  if (fleetRaw.includes("/")) {
    const [site, terminalId] = fleetRaw.split("/", 2);
    if (site.trim()) overrides.site = site.trim();
    if (terminalId.trim()) overrides.terminal_id = terminalId.trim();
  }
  const msg = document.getElementById("save-msg");
  try {
    currentConfig = await fetchJson("/api/v1/fleet/apply-template", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ template: tpl, overrides }),
    });
    await loadInitial();
    await refreshStatus();
    msg.textContent = `Applied fleet template '${tpl}'.`;
    setTimeout(() => { msg.textContent = ""; }, 3000);
  } catch (err) {
    msg.textContent = `Template apply failed: ${err.message}`;
  }
});
