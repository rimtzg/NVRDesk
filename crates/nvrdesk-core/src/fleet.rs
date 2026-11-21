//! POS fleet provisioning, configuration templates, SRT streaming config, and validation.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    KeystrokeMode, NvrDeskConfig, NvrDeskError, PipPosition, PrivacyMaskRect, VideoCodecKind,
    WebcamMode,
};

/// SRT socket transport mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SrtMode {
    /// Connect out to a remote VMS / SRT relay server (`ip:port` or `host:port`).
    #[default]
    Caller,
    /// Listen on local UDP port awaiting remote VMS/relay SRT connection.
    Listener,
}

/// SRT (Secure Reliable Transport) cloud/WAN push configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SrtConfig {
    pub enabled: bool,
    pub mode: SrtMode,
    /// Remote host:port (Caller) or bind address:port (Listener), e.g. `vms.example.com:9000`.
    pub target: Option<String>,
    /// Optional SRT `streamid` access-control string (e.g. `#!::r=pos-01,m=publish`).
    pub stream_id: Option<String>,
    /// Optional AES passphrase (10-79 ASCII chars) for encrypted SRT transport.
    pub passphrase: Option<String>,
    /// SRT latency / ARQ buffer in milliseconds (default `120`).
    pub latency_ms: u32,
    /// Source RTSP profile to mirror over SRT (`"main"` or `"sub"`).
    pub source_stream: String,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SrtMode::Caller,
            target: None,
            stream_id: None,
            passphrase: None,
            latency_ms: 120,
            source_stream: "main".to_string(),
        }
    }
}

/// Enterprise fleet identification and optional central HTTP/HTTPS config pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Store / branch identifier (e.g. `"sucursal-centro"`).
    pub site: String,
    /// Terminal / checkout register identifier (e.g. `"caja-01"`).
    pub terminal_id: String,
    /// Template name used when generating or auditing this terminal config.
    pub template: Option<String>,
    /// Optional HTTP(S) URL returning a JSON `NvrDeskConfig` for periodic fleet sync.
    pub controller_url: Option<String>,
    /// Sync interval in seconds when `controller_url` is set.
    pub poll_interval_secs: u64,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            site: "default-site".to_string(),
            terminal_id: "pos-01".to_string(),
            template: None,
            controller_url: None,
            poll_interval_secs: 300,
        }
    }
}

/// Summary of a built-in POS fleet provisioning profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetTemplateInfo {
    pub name: String,
    pub description: String,
    pub recommended_for: String,
}

/// Optional overrides applied on top of a fleet template when provisioning a terminal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetOverrides {
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub terminal_id: Option<String>,
    #[serde(default)]
    pub camera_name: Option<String>,
    #[serde(default)]
    pub serial_number: Option<String>,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub rtsp_port: Option<u16>,
    #[serde(default)]
    pub http_port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub srt_target: Option<String>,
    #[serde(default)]
    pub srt_passphrase: Option<String>,
    #[serde(default)]
    pub srt_stream_id: Option<String>,
    #[serde(default)]
    pub controller_url: Option<String>,
}

/// Outcome of validating an [`NvrDeskConfig`] prior to deployment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationReport {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Returns the catalog of built-in fleet templates for POS and enterprise deployments.
#[must_use]
pub fn list_fleet_templates() -> Vec<FleetTemplateInfo> {
    vec![
        FleetTemplateInfo {
            name: "pos-cashier".into(),
            description: "1080p@15fps H.264, masked_digits PCI keystroke OSD, click ripples, motion events, zero-idle.".into(),
            recommended_for: "Standard attended POS checkout registers (Windows / Linux / macOS)".into(),
        },
        FleetTemplateInfo {
            name: "pos-self-checkout".into(),
            description: "1080p@15fps H.264 + customer PiP webcam + G.711 microphone audio + local ring backup.".into(),
            recommended_for: "Self-checkout lanes and unattended retail kiosks".into(),
        },
        FleetTemplateInfo {
            name: "kiosk-audit".into(),
            description: "Lightweight 720p@10fps profile with shortcuts-only keystroke OSD for low-power hardware.".into(),
            recommended_for: "Thin clients, Linux embedded POS, and low-bandwidth branches".into(),
        },
        FleetTemplateInfo {
            name: "pci-strict".into(),
            description: "Keystrokes & clicks disabled, bottom-right PIN mask active, HTTPS + RTSPS TLS enabled.".into(),
            recommended_for: "Payment card terminals under strict PCI-DSS audit controls".into(),
        },
        FleetTemplateInfo {
            name: "cloud-srt".into(),
            description: "1080p@15fps H.264 always-on stream configured for encrypted SRT push + local ring backup.".into(),
            recommended_for: "Remote branches pushing POS video across WAN/LTE to a central cloud VMS".into(),
        },
    ]
}

/// Builds a complete [`NvrDeskConfig`] from a named fleet template and terminal overrides.
pub fn build_template_config(
    template_name: &str,
    overrides: &FleetOverrides,
) -> Result<NvrDeskConfig, NvrDeskError> {
    let mut cfg = NvrDeskConfig::default();
    let normalized = template_name.trim().to_ascii_lowercase();

    match normalized.as_str() {
        "pos-cashier" => {
            cfg.input_overlay.keystroke_mode = KeystrokeMode::MaskedDigits;
            cfg.input_overlay.show_clicks = true;
            cfg.pos.osd_enabled = true;
            cfg.pos.osd_position = PipPosition::TopLeft;
            cfg.motion.enabled = true;
        }
        "pos-self-checkout" => {
            cfg.input_overlay.keystroke_mode = KeystrokeMode::MaskedDigits;
            cfg.input_overlay.show_clicks = true;
            cfg.webcam.enabled = true;
            cfg.webcam.mode = WebcamMode::Pip;
            cfg.webcam.pip_position = PipPosition::TopRight;
            cfg.audio.enabled = true;
            cfg.recording.enabled = true;
            cfg.recording.max_total_mb = 1024;
        }
        "kiosk-audit" => {
            cfg.main_stream.max_width = 1280;
            cfg.main_stream.fps = 10;
            cfg.main_stream.bitrate_bps = 900_000;
            cfg.sub_stream.fps = 5;
            cfg.sub_stream.bitrate_bps = 250_000;
            cfg.input_overlay.keystroke_mode = KeystrokeMode::ShortcutsAndSpecial;
        }
        "pci-strict" => {
            cfg.input_overlay.enabled = false;
            cfg.input_overlay.show_clicks = false;
            cfg.input_overlay.show_keystrokes = false;
            cfg.input_overlay.keystroke_mode = KeystrokeMode::Off;
            cfg.tls.https_enabled = true;
            cfg.tls.rtsps_enabled = true;
            cfg.pos.privacy_masks.push(PrivacyMaskRect {
                x: 0.70,
                y: 0.72,
                width: 0.28,
                height: 0.25,
                label: Some("PIN-PAD".into()),
            });
        }
        "cloud-srt" => {
            cfg.always_on = true;
            cfg.main_stream.codec = VideoCodecKind::H264;
            cfg.recording.enabled = true;
            cfg.recording.max_total_mb = 1024;
            cfg.srt.enabled = true;
            cfg.srt.mode = SrtMode::Caller;
            cfg.srt.latency_ms = 160;
            cfg.srt.target = Some("vms.example.com:9000".into());
        }
        _ => {
            return Err(NvrDeskError::Config(format!(
                "unknown fleet template '{template_name}'"
            )));
        }
    }

    cfg.fleet.template = Some(normalized);
    apply_fleet_overrides(&mut cfg, overrides);
    Ok(cfg)
}

/// Applies fleet site/terminal/network overrides and derives deterministic ONVIF serial/UUID.
pub fn apply_fleet_overrides(cfg: &mut NvrDeskConfig, ov: &FleetOverrides) {
    if let Some(site) = &ov.site
        && !site.trim().is_empty()
    {
        cfg.fleet.site = site.trim().to_string();
    }
    if let Some(tid) = &ov.terminal_id
        && !tid.trim().is_empty()
    {
        cfg.fleet.terminal_id = tid.trim().to_string();
    }
    if let Some(name) = &ov.camera_name
        && !name.trim().is_empty()
    {
        cfg.onvif.device_name = name.trim().to_string();
    } else if ov.site.is_some() || ov.terminal_id.is_some() {
        cfg.onvif.device_name = format!("POS {} ({})", cfg.fleet.terminal_id, cfg.fleet.site);
    }

    if let Some(serial) = &ov.serial_number
        && !serial.trim().is_empty()
    {
        cfg.onvif.serial_number = serial.trim().to_string();
    } else if ov.site.is_some() || ov.terminal_id.is_some() {
        cfg.onvif.serial_number = format!("NVRDESK-{}-{}", cfg.fleet.site, cfg.fleet.terminal_id);
    }

    // Keep ONVIF WS-Discovery URN stable across reprovisioning for the same site + terminal.
    let identity = format!("{}:{}", cfg.fleet.site, cfg.fleet.terminal_id);
    cfg.onvif.device_uuid =
        Some(Uuid::new_v5(&Uuid::NAMESPACE_DNS, identity.as_bytes()).to_string());

    if let Some(bind) = &ov.bind
        && !bind.trim().is_empty()
    {
        cfg.rtsp.bind = bind.trim().to_string();
    }
    if let Some(port) = ov.rtsp_port {
        cfg.rtsp.port = port;
    }
    if let Some(port) = ov.http_port {
        cfg.onvif.http_port = port;
    }
    if let Some(user) = &ov.username {
        let trimmed = user.trim();
        let val = (!trimmed.is_empty()).then(|| trimmed.to_string());
        cfg.rtsp.username = val.clone();
        cfg.onvif.username = val;
    }
    if let Some(pass) = &ov.password {
        let val = (!pass.is_empty()).then(|| pass.to_string());
        cfg.rtsp.password = val.clone();
        cfg.onvif.password = val;
    }
    if let Some(target) = &ov.srt_target
        && !target.trim().is_empty()
    {
        cfg.srt.enabled = true;
        cfg.srt.target = Some(target.trim().to_string());
    }
    if let Some(pass) = &ov.srt_passphrase
        && !pass.is_empty()
    {
        cfg.srt.passphrase = Some(pass.clone());
    }
    if let Some(stream_id) = &ov.srt_stream_id
        && !stream_id.trim().is_empty()
    {
        cfg.srt.stream_id = Some(stream_id.trim().to_string());
    } else if cfg.srt.enabled && cfg.srt.stream_id.is_none() {
        cfg.srt.stream_id = Some(format!(
            "#!::r={}/{},m=publish",
            cfg.fleet.site, cfg.fleet.terminal_id
        ));
    }
    if let Some(url) = &ov.controller_url
        && !url.trim().is_empty()
    {
        cfg.fleet.controller_url = Some(url.trim().to_string());
    }
}

/// Validates an [`NvrDeskConfig`] and collects fatal deployment errors plus operational warnings.
#[must_use]
pub fn validate_config(cfg: &NvrDeskConfig) -> ValidationReport {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if cfg.rtsp.bind.parse::<IpAddr>().is_err() {
        errors.push(format!(
            "rtsp.bind '{}' is not a valid IP address",
            cfg.rtsp.bind
        ));
    }
    if (cfg.onvif.enabled || cfg.daemon.web_ui_enabled)
        && cfg.rtsp.port > 0
        && cfg.rtsp.port == cfg.onvif.http_port
    {
        errors.push(format!(
            "port conflict: rtsp.port ({}) equals onvif.http_port ({})",
            cfg.rtsp.port, cfg.onvif.http_port
        ));
    }
    if cfg.tls.rtsps_enabled && cfg.tls.rtsps_port > 0 && cfg.tls.rtsps_port == cfg.rtsp.port {
        errors.push("port conflict: tls.rtsps_port equals rtsp.port".into());
    }

    if cfg.source.kind.is_ffmpeg_pull()
        && cfg
            .source
            .uri
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        errors.push(format!(
            "source.uri is required when source.kind is '{:?}'",
            cfg.source.kind
        ));
    }

    for (idx, mask) in cfg.pos.privacy_masks.iter().enumerate() {
        if !(0.0..=1.0).contains(&mask.x)
            || !(0.0..=1.0).contains(&mask.y)
            || mask.width <= 0.0
            || mask.height <= 0.0
        {
            errors.push(format!(
                "pos.privacy_masks[{idx}] has invalid coordinates ({}, {}, {}x{})",
                mask.x, mask.y, mask.width, mask.height
            ));
        }
    }

    if cfg.srt.enabled {
        let target = cfg.srt.target.as_deref().map(str::trim).unwrap_or("");
        if target.is_empty() || !target.contains(':') {
            errors.push("srt.target must be in host:port format when srt.enabled is true".into());
        }
        if let Some(pass) = &cfg.srt.passphrase
            && !(10..=79).contains(&pass.len())
        {
            errors.push("srt.passphrase must be between 10 and 79 characters".into());
        }
        if cfg.srt.source_stream != "main" && cfg.srt.source_stream != "sub" {
            errors.push("srt.source_stream must be 'main' or 'sub'".into());
        }
    }

    if cfg.rtsp.username.is_none() || cfg.rtsp.password.is_none() {
        warnings.push("RTSP authentication is disabled (recommended to set username/password in production POS)".into());
    }
    if !cfg.always_on && cfg.srt.enabled && cfg.srt.mode == SrtMode::Caller {
        warnings.push("always_on is false while srt.enabled=true; SRT caller keeps stream active only while connected".into());
    }

    ValidationReport {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

/// Applies `NVRDESK_*` environment overrides (useful for containers, systemd, and GPO wrappers).
pub fn apply_env_overrides(cfg: &mut NvrDeskConfig) {
    let env_str = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());

    let ov = FleetOverrides {
        site: env_str("NVRDESK_SITE"),
        terminal_id: env_str("NVRDESK_TERMINAL_ID"),
        camera_name: env_str("NVRDESK_CAMERA_NAME"),
        serial_number: env_str("NVRDESK_SERIAL_NUMBER"),
        bind: env_str("NVRDESK_BIND"),
        rtsp_port: env_str("NVRDESK_RTSP_PORT").and_then(|v| v.parse().ok()),
        http_port: env_str("NVRDESK_HTTP_PORT").and_then(|v| v.parse().ok()),
        username: env_str("NVRDESK_USERNAME"),
        password: env_str("NVRDESK_PASSWORD"),
        srt_target: env_str("NVRDESK_SRT_TARGET"),
        srt_passphrase: env_str("NVRDESK_SRT_PASSPHRASE"),
        srt_stream_id: env_str("NVRDESK_SRT_STREAM_ID"),
        controller_url: env_str("NVRDESK_CONTROLLER_URL"),
    };
    if ov.site.is_some()
        || ov.terminal_id.is_some()
        || ov.camera_name.is_some()
        || ov.bind.is_some()
        || ov.rtsp_port.is_some()
        || ov.http_port.is_some()
        || ov.username.is_some()
        || ov.password.is_some()
        || ov.srt_target.is_some()
        || ov.srt_passphrase.is_some()
        || ov.srt_stream_id.is_some()
        || ov.controller_url.is_some()
    {
        apply_fleet_overrides(cfg, &ov);
    }

    if let Some(uri) = env_str("NVRDESK_SOURCE_URI") {
        cfg.source.uri = Some(uri);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureSourceKind;

    #[test]
    fn template_generates_stable_device_uuid() {
        let ov = FleetOverrides {
            site: Some("store-42".into()),
            terminal_id: Some("caja-03".into()),
            ..Default::default()
        };
        let a = build_template_config("pos-cashier", &ov).expect("template build");
        let b = build_template_config("pos-cashier", &ov).expect("template build");
        assert_eq!(a.onvif.device_uuid, b.onvif.device_uuid);
        assert!(a.onvif.device_uuid.is_some());
        assert_eq!(a.onvif.serial_number, "NVRDESK-store-42-caja-03");
    }

    #[test]
    fn validate_catches_port_collisions_and_bad_srt() {
        let mut cfg = NvrDeskConfig::default();
        cfg.rtsp.port = 8080;
        cfg.srt.enabled = true;
        cfg.srt.target = Some("invalid-no-port".into());
        cfg.srt.passphrase = Some("short".into());
        cfg.source.kind = CaptureSourceKind::Mp4;

        let report = validate_config(&cfg);
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("port conflict")));
        assert!(report.errors.iter().any(|e| e.contains("srt.target")));
        assert!(report.errors.iter().any(|e| e.contains("srt.passphrase")));
        assert!(report.errors.iter().any(|e| e.contains("source.uri")));
    }

    #[test]
    fn pci_strict_template_passes_validation_with_credentials() {
        let ov = FleetOverrides {
            site: Some("hq".into()),
            terminal_id: Some("pos-1".into()),
            username: Some("admin".into()),
            password: Some("SuperSecret123!".into()),
            ..Default::default()
        };
        let cfg = build_template_config("pci-strict", &ov).expect("pci-strict");
        let report = validate_config(&cfg);
        assert!(report.valid, "errors: {:?}", report.errors);
        assert!(report.warnings.is_empty());
        assert!(cfg.tls.https_enabled);
        assert_eq!(cfg.pos.privacy_masks.len(), 1);
    }
}
