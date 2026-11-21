let currentConfig = null;

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
  const [devices, cfg] = await Promise.all([
    fetchJson("/api/v1/devices"),
    fetchJson("/api/v1/config"),
  ]);
  currentConfig = cfg;

  populateSelect(document.getElementById("cfg-display"), devices.monitors, cfg.source.display, "Primary Display");
  populateSelect(document.getElementById("cfg-webcam-device"), devices.webcams, cfg.webcam.device_index, "No Webcam Found");
  populateMicSelect(document.getElementById("cfg-audio-device"), devices.microphones, cfg.audio.device_name);

  document.getElementById("cfg-encoder-backend").value = cfg.encoder.backend;
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

  document.getElementById("cfg-rec-enabled").checked = cfg.recording.enabled;
  document.getElementById("cfg-rec-max-mb").value = cfg.recording.max_total_mb;
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
    document.getElementById("stat-onvif").textContent = st.onvif_enabled ? "Enabled" : "Disabled";

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
  updated.source.display = Number(document.getElementById("cfg-display").value) || 0;
  updated.encoder.backend = document.getElementById("cfg-encoder-backend").value;
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

  updated.recording.enabled = document.getElementById("cfg-rec-enabled").checked;
  updated.recording.max_total_mb = Math.max(64, Number(document.getElementById("cfg-rec-max-mb").value) || 1024);

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
