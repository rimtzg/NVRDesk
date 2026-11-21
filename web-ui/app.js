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
  document.getElementById("cfg-privacy-masks").value = JSON.stringify(
    (cfg.pos && cfg.pos.privacy_masks) ? cfg.pos.privacy_masks : []
  );
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
  try {
    const parsedMasks = JSON.parse(document.getElementById("cfg-privacy-masks").value || "[]");
    if (Array.isArray(parsedMasks)) {
      updated.pos.privacy_masks = parsedMasks;
    }
  } catch (maskErr) {
    document.getElementById("save-msg").textContent = `Invalid Privacy Masks JSON: ${maskErr.message}`;
    return;
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
