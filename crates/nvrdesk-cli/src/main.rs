//! NVRDesk CLI entrypoint launching the daemon (RTSP/ONVIF/Web UI) and System Tray.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use nvrdesk_core::{
    AudioCodecKind, CaptureSourceKind, EncoderBackendKind, FleetOverrides, KeystrokeMode,
    NvrDeskConfig, SrtMode, VideoCodecKind, WebcamMode, build_template_config,
    list_fleet_templates,
};
use nvrdesk_daemon::start_daemon;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSourceKind {
    Screen,
    TestPattern,
    Mp4,
    Mjpeg,
    Rtmp,
    RtspRelay,
    FfmpegUri,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSrtMode {
    Caller,
    Listener,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliKeystrokeMode {
    All,
    ShortcutsAndSpecial,
    MaskedDigits,
    Off,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliEncoderBackend {
    Auto,
    Openh264,
    Ffmpeg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliWebcamMode {
    Pip,
    Channel,
    VirtualOnvif,
    PipAndVirtualOnvif,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliAudioCodec {
    Pcmu,
    Pcma,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliVideoCodec {
    H264,
    H265,
}

#[derive(Debug, Parser)]
#[command(
    name = "nvrdesk",
    version,
    about = "NVRDesk: Open-source ONVIF/RTSP virtual IP camera for POS & desktop monitoring"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to TOML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Initialize configuration from a built-in fleet template (e.g. `pos-cashier`, `pci-strict`).
    #[arg(long)]
    template: Option<String>,

    /// Store / site identifier for fleet inventory and ONVIF UUID derivation.
    #[arg(long)]
    site: Option<String>,

    /// POS terminal identifier for fleet inventory and ONVIF UUID derivation.
    #[arg(long)]
    terminal_id: Option<String>,

    /// Print the effective TOML configuration and exit.
    #[arg(long, default_value_t = false)]
    print_config: bool,

    /// IP address to bind RTSP and HTTP (Web UI / ONVIF) servers to.
    #[arg(long)]
    bind: Option<IpAddr>,

    /// RTSP listening port.
    #[arg(long)]
    port: Option<u16>,

    /// HTTP listening port for Embedded Web UI, ONVIF SOAP, and JPEG Snapshots.
    #[arg(long)]
    http_port: Option<u16>,

    /// Disable Embedded Web UI (port still serves ONVIF if enabled).
    #[arg(long, default_value_t = false)]
    no_web_ui: bool,

    /// Disable System Tray icon (headless / service mode).
    #[arg(long, default_value_t = false)]
    no_tray: bool,

    /// Disable ONVIF HTTP/SOAP and WS-Discovery servers.
    #[arg(long, default_value_t = false)]
    no_onvif: bool,

    /// Disable ONVIF WS-Discovery multicast responder.
    #[arg(long, default_value_t = false)]
    no_discovery: bool,

    /// Video source backend (`screen`, `test-pattern`, `mp4`, `mjpeg`, `rtmp`, `rtsp-relay`, `ffmpeg-uri`).
    #[arg(long, value_enum)]
    source: Option<CliSourceKind>,

    /// Input path or URI for FFmpeg pull sources (`mp4`, `mjpeg`, `rtmp`, `rtsp-relay`, `ffmpeg-uri`).
    #[arg(long)]
    source_uri: Option<String>,

    /// Disable looping when playing MP4/file sources.
    #[arg(long, default_value_t = false)]
    no_loop: bool,

    /// Display index (0 = primary) when using `--source screen`.
    #[arg(long)]
    display: Option<usize>,

    /// Video encoder backend (`auto`, `openh264`, or `ffmpeg`).
    #[arg(long, value_enum)]
    encoder: Option<CliEncoderBackend>,

    /// Path to external `ffmpeg` binary (when using `--encoder auto|ffmpeg`).
    #[arg(long)]
    ffmpeg_path: Option<String>,

    /// Explicit FFmpeg video codec (e.g. `h264_videotoolbox`, `h264_nvenc`, `libx264`).
    #[arg(long)]
    ffmpeg_codec: Option<String>,

    /// MainStream target FPS.
    #[arg(long)]
    fps: Option<u32>,

    /// MainStream target H.264 bitrate in bps.
    #[arg(long)]
    bitrate: Option<u32>,

    /// MainStream maximum width.
    #[arg(long)]
    max_width: Option<u32>,

    /// Disable the SubStream profile (`/stream2`).
    #[arg(long, default_value_t = false)]
    no_substream: bool,

    /// Hide the OS mouse cursor in screen capture.
    #[arg(long, default_value_t = false)]
    no_cursor: bool,

    /// Disable mouse click and keystroke overlays.
    #[arg(long, default_value_t = false)]
    no_overlays: bool,

    /// Keystroke OSD visibility/privacy mode.
    #[arg(long, value_enum)]
    keystroke_mode: Option<CliKeystrokeMode>,

    /// Enable webcam capture (PiP, `/webcam` RTSP channel, or standalone Virtual ONVIF camera).
    #[arg(long, default_value_t = false)]
    webcam: bool,

    /// Webcam integration mode (`pip`, `channel`, `virtual-onvif`, or `pip-and-virtual-onvif`).
    #[arg(long, value_enum)]
    webcam_mode: Option<CliWebcamMode>,

    /// Also expose a standalone Virtual ONVIF camera alongside PiP/Channel mode.
    #[arg(long, default_value_t = false)]
    webcam_virtual_onvif: bool,

    /// Enable microphone audio on the Virtual ONVIF / dedicated webcam stream.
    #[arg(long, default_value_t = false)]
    webcam_audio: bool,

    /// Optional dedicated microphone device name for the webcam stream.
    #[arg(long)]
    webcam_audio_device: Option<String>,

    /// Webcam device index.
    #[arg(long)]
    webcam_device: Option<usize>,

    /// ONVIF HTTP(S) listening port for `virtual-onvif` webcam mode (default `8081`).
    #[arg(long)]
    webcam_http_port: Option<u16>,

    /// RTSP listening port for `virtual-onvif` webcam mode (default `8555`).
    #[arg(long)]
    webcam_rtsp_port: Option<u16>,

    /// Enable microphone audio capture (G.711 PCMU/PCMA on `trackID=1`).
    #[arg(long, default_value_t = false)]
    audio: bool,

    /// Audio codec (`pcmu` or `pcma`).
    #[arg(long, value_enum)]
    audio_codec: Option<CliAudioCodec>,

    /// Enable audio backchannel (VMS -> POS speakers on `trackID=2`).
    #[arg(long, default_value_t = false)]
    backchannel: bool,

    /// Video codec for MainStream (`h264` or `h265`).
    #[arg(long, value_enum)]
    codec: Option<CliVideoCodec>,

    /// Enable HTTPS TLS on the HTTP/Web UI/ONVIF port.
    #[arg(long, default_value_t = false)]
    https: bool,

    /// Enable RTSPS (TLS-encrypted RTSP) listener.
    #[arg(long, default_value_t = false)]
    rtsps: bool,

    /// RTSPS TLS listening port.
    #[arg(long)]
    rtsps_port: Option<u16>,

    /// Disable motion detection ONVIF events.
    #[arg(long, default_value_t = false)]
    no_motion: bool,

    /// Enable local ring-buffer H.264 recording.
    #[arg(long, default_value_t = false)]
    record: bool,

    /// Directory for local ring-buffer recordings.
    #[arg(long)]
    record_dir: Option<PathBuf>,

    /// Optional username for RTSP Digest & ONVIF WS-Security authentication.
    #[arg(long)]
    username: Option<String>,

    /// Optional password for RTSP Digest & ONVIF WS-Security authentication.
    #[arg(long)]
    password: Option<String>,

    /// Keep capturing and encoding even when zero RTSP clients are connected.
    #[arg(long, default_value_t = false)]
    always_on: bool,

    /// SRT transport mode (`caller` or `listener`) — enables SRT when paired with `--srt-target`.
    #[arg(long, value_enum)]
    srt_mode: Option<CliSrtMode>,

    /// SRT destination/bind address in `host:port` form (e.g. `vms.example.com:9000`).
    #[arg(long)]
    srt_target: Option<String>,

    /// SRT stream ID access control string (e.g. `#!::r=pos-01,m=publish`).
    #[arg(long)]
    srt_stream_id: Option<String>,

    /// Optional SRT AES passphrase (10-79 characters).
    #[arg(long)]
    srt_passphrase: Option<String>,

    /// SRT latency buffer in milliseconds (default 120).
    #[arg(long)]
    srt_latency_ms: Option<u32>,

    /// Install `/etc/nvrdesk/nvrdesk.toml` and register/start a systemd user-session service (Linux).
    #[arg(long, default_value_t = false)]
    install_service: bool,

    /// Target Linux desktop username for `--install-service` (defaults to `$SUDO_USER` / `$USER`).
    #[arg(long)]
    service_user: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// POS fleet provisioning templates, TOML generation, and config validation.
    Fleet {
        #[command(subcommand)]
        action: FleetCommands,
    },
}

#[derive(Debug, Subcommand)]
enum FleetCommands {
    /// List built-in POS and enterprise fleet templates.
    Templates {
        /// Output templates as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Generate a terminal `nvrdesk.toml` from a built-in fleet template.
    Init {
        /// Template name (`pos-cashier`, `pos-self-checkout`, `kiosk-audit`, `pci-strict`, `cloud-srt`).
        template: String,
        /// Write generated TOML to file (prints to stdout when omitted).
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Store / branch identifier.
        #[arg(long)]
        site: Option<String>,
        /// Terminal / register identifier.
        #[arg(long)]
        terminal_id: Option<String>,
        /// Override ONVIF camera display name.
        #[arg(long)]
        camera_name: Option<String>,
        /// Optional RTSP & ONVIF username.
        #[arg(long)]
        username: Option<String>,
        /// Optional RTSP & ONVIF password.
        #[arg(long)]
        password: Option<String>,
        /// Optional remote SRT endpoint (`host:port`).
        #[arg(long)]
        srt_target: Option<String>,
        /// Optional SRT encryption passphrase (10-79 chars).
        #[arg(long)]
        srt_passphrase: Option<String>,
    },
    /// Validate an `nvrdesk.toml` configuration before deployment.
    Validate {
        /// Path to configuration file (validates defaults when omitted).
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,xcap=warn,nokhwa=warn")),
        )
        .init();

    let cli = Cli::parse();
    if let Some(Commands::Fleet { action }) = cli.command {
        return handle_fleet_command(action);
    }

    let config_path = if cli.install_service && cli.config.is_none() {
        Some(
            nvrdesk_core::default_snap_config_path()
                .unwrap_or_else(|| PathBuf::from("/etc/nvrdesk/nvrdesk.toml")),
        )
    } else {
        cli.config
            .clone()
            .or_else(nvrdesk_core::default_snap_config_path)
    };

    let mut cfg = if let Some(template_name) = &cli.template {
        build_template_config(
            template_name,
            &FleetOverrides {
                site: cli.site.clone(),
                terminal_id: cli.terminal_id.clone(),
                ..Default::default()
            },
        )?
    } else if let Some(path) = &config_path {
        if path.exists() {
            NvrDeskConfig::from_toml_file(path)?
        } else if cli.install_service {
            NvrDeskConfig::default()
        } else {
            let default = NvrDeskConfig::default();
            default.save_toml_file(path)?;
            default
        }
    } else {
        NvrDeskConfig::default()
    };

    cfg.apply_env_overrides();
    apply_cli_overrides(&cli, &mut cfg);

    if cli.print_config {
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }

    let validation = cfg.validate();
    for w in &validation.warnings {
        warn!(warning = %w, "configuration warning");
    }
    if !validation.valid {
        anyhow::bail!(
            "invalid configuration:\n - {}",
            validation.errors.join("\n - ")
        );
    }

    if cli.install_service {
        let target_cfg = config_path.as_deref().expect("service config path set");
        return install_systemd_service(target_cfg, &cfg, cli.service_user.as_deref(), cli.no_tray);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let use_tray_loop = !cli.no_tray;

    if use_tray_loop {
        // Run Tokio on a background thread so the OS main thread is free for the System Tray loop.
        let (tx, rx) = mpsc::sync_channel(1);
        let rt_shutdown = Arc::clone(&shutdown);
        let rt_thread = std::thread::Builder::new()
            .name("nvrdesk-tokio-rt".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async move {
                    match start_daemon(cfg, config_path, Arc::clone(&rt_shutdown)).await {
                        Ok((handle, worker)) => {
                            let _ = tx.send(Ok(handle));
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("received Ctrl+C, shutting down");
                                    rt_shutdown.store(true, Ordering::Release);
                                }
                                _ = wait_shutdown_flag(&rt_shutdown) => {}
                            }
                            let _ = worker.join();
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err));
                        }
                    }
                });
            })?;

        let handle = rx
            .recv()
            .map_err(|_| anyhow::anyhow!("daemon startup channel closed"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        nvrdesk_tray::run_tray_loop(handle, Arc::clone(&shutdown));
        shutdown.store(true, Ordering::Release);
        let _ = rt_thread.join();
        Ok(())
    } else {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            let (_handle, worker) = start_daemon(cfg, config_path, Arc::clone(&shutdown)).await?;
            tokio::signal::ctrl_c().await?;
            info!("received Ctrl+C, shutting down");
            shutdown.store(true, Ordering::Release);
            let _ = worker.join();
            Ok::<(), anyhow::Error>(())
        })
    }
}

async fn wait_shutdown_flag(flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn handle_fleet_command(action: FleetCommands) -> Result<()> {
    match action {
        FleetCommands::Templates { json } => {
            let templates = list_fleet_templates();
            if json {
                println!("{}", serde_json::to_string_pretty(&templates)?);
            } else {
                for tpl in templates {
                    println!(
                        "{:<20} {}\n  recommended: {}\n",
                        tpl.name, tpl.description, tpl.recommended_for
                    );
                }
            }
            Ok(())
        }
        FleetCommands::Init {
            template,
            out,
            site,
            terminal_id,
            camera_name,
            username,
            password,
            srt_target,
            srt_passphrase,
        } => {
            let cfg = build_template_config(
                &template,
                &FleetOverrides {
                    site,
                    terminal_id,
                    camera_name,
                    username,
                    password,
                    srt_target,
                    srt_passphrase,
                    ..Default::default()
                },
            )?;
            let toml_text = toml::to_string_pretty(&cfg)?;
            if let Some(path) = out {
                cfg.save_toml_file(&path)?;
                println!("wrote fleet configuration to {}", path.display());
            } else {
                println!("{toml_text}");
            }
            Ok(())
        }
        FleetCommands::Validate { config } => {
            let cfg = match config {
                Some(path) => NvrDeskConfig::from_toml_file(&path)?,
                None => NvrDeskConfig::default(),
            };
            let report = cfg.validate();
            for warning in &report.warnings {
                eprintln!("WARN: {warning}");
            }
            if !report.valid {
                for err in &report.errors {
                    eprintln!("ERROR: {err}");
                }
                anyhow::bail!(
                    "configuration validation failed ({} error(s))",
                    report.errors.len()
                );
            }
            println!(
                "Configuration OK ({} warning(s), site={}, terminal={})",
                report.warnings.len(),
                cfg.fleet.site,
                cfg.fleet.terminal_id
            );
            Ok(())
        }
    }
}

fn apply_cli_overrides(cli: &Cli, cfg: &mut NvrDeskConfig) {
    if let Some(site) = &cli.site {
        cfg.fleet.site = site.clone();
    }
    if let Some(tid) = &cli.terminal_id {
        cfg.fleet.terminal_id = tid.clone();
    }
    if let Some(bind) = cli.bind {
        cfg.rtsp.bind = bind.to_string();
    }
    if let Some(port) = cli.port {
        cfg.rtsp.port = port;
    }
    if let Some(http_port) = cli.http_port {
        cfg.onvif.http_port = http_port;
    }
    if cli.no_web_ui {
        cfg.daemon.web_ui_enabled = false;
    }
    if cli.no_tray {
        cfg.daemon.tray_enabled = false;
    }
    if cli.no_onvif {
        cfg.onvif.enabled = false;
    }
    if cli.no_discovery {
        cfg.onvif.discovery_enabled = false;
    }
    if let Some(source) = cli.source {
        cfg.source.kind = match source {
            CliSourceKind::Screen => CaptureSourceKind::Screen,
            CliSourceKind::TestPattern => CaptureSourceKind::TestPattern,
            CliSourceKind::Mp4 => CaptureSourceKind::Mp4,
            CliSourceKind::Mjpeg => CaptureSourceKind::Mjpeg,
            CliSourceKind::Rtmp => CaptureSourceKind::Rtmp,
            CliSourceKind::RtspRelay => CaptureSourceKind::RtspRelay,
            CliSourceKind::FfmpegUri => CaptureSourceKind::FfmpegUri,
        };
    }
    if let Some(uri) = &cli.source_uri {
        cfg.source.uri = Some(uri.clone());
    }
    if cli.no_loop {
        cfg.source.loop_input = false;
    }
    if let Some(display) = cli.display {
        cfg.source.display = display;
    }
    if let Some(enc) = cli.encoder {
        cfg.encoder.backend = match enc {
            CliEncoderBackend::Auto => EncoderBackendKind::Auto,
            CliEncoderBackend::Openh264 => EncoderBackendKind::Openh264,
            CliEncoderBackend::Ffmpeg => EncoderBackendKind::Ffmpeg,
        };
    }
    if let Some(path) = &cli.ffmpeg_path {
        cfg.encoder.ffmpeg_path = path.clone();
    }
    if let Some(codec) = &cli.ffmpeg_codec {
        cfg.encoder.ffmpeg_codec = Some(codec.clone());
    }
    if let Some(fps) = cli.fps {
        cfg.main_stream.fps = fps.max(1);
    }
    if let Some(bitrate) = cli.bitrate {
        cfg.main_stream.bitrate_bps = bitrate.max(100_000);
    }
    if let Some(max_width) = cli.max_width {
        cfg.main_stream.max_width = max_width;
    }
    if cli.no_substream {
        cfg.sub_stream.enabled = false;
    }
    if cli.no_cursor {
        cfg.source.show_cursor = false;
    }
    if cli.no_overlays {
        cfg.input_overlay.enabled = false;
    }
    if let Some(mode) = cli.keystroke_mode {
        cfg.input_overlay.keystroke_mode = match mode {
            CliKeystrokeMode::All => KeystrokeMode::All,
            CliKeystrokeMode::ShortcutsAndSpecial => KeystrokeMode::ShortcutsAndSpecial,
            CliKeystrokeMode::MaskedDigits => KeystrokeMode::MaskedDigits,
            CliKeystrokeMode::Off => KeystrokeMode::Off,
        };
    }
    if cli.webcam {
        cfg.webcam.enabled = true;
    }
    if let Some(mode) = cli.webcam_mode {
        cfg.webcam.mode = match mode {
            CliWebcamMode::Pip => WebcamMode::Pip,
            CliWebcamMode::Channel => WebcamMode::Channel,
            CliWebcamMode::VirtualOnvif => WebcamMode::VirtualOnvif,
            CliWebcamMode::PipAndVirtualOnvif => WebcamMode::PipAndVirtualOnvif,
        };
    }
    if cli.webcam_virtual_onvif {
        cfg.webcam.virtual_onvif_enabled = true;
    }
    if cli.webcam_audio {
        cfg.webcam.audio_enabled = true;
    }
    if let Some(dev) = &cli.webcam_audio_device {
        cfg.webcam.audio_device_name = Some(dev.clone());
    }
    if let Some(idx) = cli.webcam_device {
        cfg.webcam.device_index = idx;
    }
    if let Some(port) = cli.webcam_http_port {
        cfg.webcam.onvif_http_port = port;
    }
    if let Some(port) = cli.webcam_rtsp_port {
        cfg.webcam.rtsp_port = port;
    }
    if cli.audio {
        cfg.audio.enabled = true;
    }
    if cli.backchannel {
        cfg.audio.backchannel_enabled = true;
    }
    if let Some(codec) = cli.audio_codec {
        cfg.audio.codec = match codec {
            CliAudioCodec::Pcmu => AudioCodecKind::Pcmu,
            CliAudioCodec::Pcma => AudioCodecKind::Pcma,
        };
    }
    if let Some(vcodec) = cli.codec {
        cfg.main_stream.codec = match vcodec {
            CliVideoCodec::H264 => VideoCodecKind::H264,
            CliVideoCodec::H265 => VideoCodecKind::H265,
        };
    }
    if cli.https {
        cfg.tls.https_enabled = true;
    }
    if cli.rtsps {
        cfg.tls.rtsps_enabled = true;
    }
    if let Some(port) = cli.rtsps_port {
        cfg.tls.rtsps_port = port;
    }
    if cli.no_motion {
        cfg.motion.enabled = false;
    }
    if cli.record {
        cfg.recording.enabled = true;
    }
    if let Some(dir) = &cli.record_dir {
        cfg.recording.directory = dir.clone();
    }
    if let Some(user) = &cli.username {
        cfg.rtsp.username = Some(user.clone());
        cfg.onvif.username = Some(user.clone());
    }
    if let Some(pass) = &cli.password {
        cfg.rtsp.password = Some(pass.clone());
        cfg.onvif.password = Some(pass.clone());
    }
    if cli.always_on {
        cfg.always_on = true;
    }
    if let Some(mode) = cli.srt_mode {
        cfg.srt.mode = match mode {
            CliSrtMode::Caller => SrtMode::Caller,
            CliSrtMode::Listener => SrtMode::Listener,
        };
    }
    if let Some(target) = &cli.srt_target {
        cfg.srt.enabled = true;
        cfg.srt.target = Some(target.clone());
    }
    if let Some(stream_id) = &cli.srt_stream_id {
        cfg.srt.stream_id = Some(stream_id.clone());
    }
    if let Some(pass) = &cli.srt_passphrase {
        cfg.srt.passphrase = Some(pass.clone());
    }
    if let Some(lat) = cli.srt_latency_ms {
        cfg.srt.latency_ms = lat;
    }
}

#[cfg(any(target_os = "linux", test))]
fn resolve_service_user(
    explicit: Option<&str>,
    sudo_user: Option<&str>,
    env_user: Option<&str>,
) -> Result<String> {
    let candidate = explicit
        .or(sudo_user)
        .or(env_user)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match candidate {
        Some("root") => anyhow::bail!(
            "cannot infer non-root desktop user (running as root); pass `--service-user <desktop_user>` or invoke via `sudo nvrdesk --install-service`"
        ),
        Some(user) => Ok(user.to_string()),
        None => anyhow::bail!(
            "could not detect target Linux desktop user; pass `--service-user <username>`"
        ),
    }
}

#[cfg(any(target_os = "linux", test))]
fn render_systemd_unit(exe_path: &str, config_path: &str, display: &str) -> String {
    format!(
        "[Unit]\n\
         Description=NVRDesk Virtual ONVIF/RTSP Camera (%i POS Desktop Session)\n\
         After=graphical.target network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User=%i\n\
         Environment=DISPLAY={display}\n\
         Environment=RUST_LOG=info,xcap=warn,nokhwa=warn\n\
         ExecStart={exe_path} --config {config_path} --no-tray\n\
         Restart=always\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

#[cfg(any(target_os = "linux", test))]
fn snap_autostart_desktop_path(snap_user_data: &str) -> PathBuf {
    PathBuf::from(snap_user_data)
        .join(".config")
        .join("autostart")
        .join("nvrdesk.desktop")
}

#[cfg(any(target_os = "linux", test))]
fn render_snap_autostart_desktop(headless: bool) -> String {
    let exec_cmd = if headless {
        "nvrdesk.headless"
    } else {
        "nvrdesk"
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=NVRDesk\n\
         Comment=Open-source ONVIF/RTSP/SRT virtual IP camera for POS & desktop monitoring\n\
         Exec={exec_cmd}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(target_os = "linux")]
fn install_snap_autostart(
    config_path: &std::path::Path,
    cfg: &NvrDeskConfig,
    snap_user_data: &str,
    headless: bool,
) -> Result<()> {
    cfg.save_toml_file(config_path)?;
    let desktop_path = snap_autostart_desktop_path(snap_user_data);
    if let Some(parent) = desktop_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&desktop_path, render_snap_autostart_desktop(headless))?;
    println!("Configured NVRDesk Snap session autostart:");
    println!("  Config   : {}", config_path.display());
    println!("  Autostart: {}", desktop_path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd_service(
    config_path: &std::path::Path,
    cfg: &NvrDeskConfig,
    explicit_user: Option<&str>,
    headless: bool,
) -> Result<()> {
    use anyhow::Context;
    use std::process::Command;

    if let Ok(snap_user_data) = std::env::var("SNAP_USER_DATA")
        && !snap_user_data.trim().is_empty()
    {
        return install_snap_autostart(config_path, cfg, snap_user_data.trim(), headless);
    }

    let sudo_user = std::env::var("SUDO_USER").ok();
    let env_user = std::env::var("USER").ok();
    let user = resolve_service_user(explicit_user, sudo_user.as_deref(), env_user.as_deref())?;

    cfg.save_toml_file(config_path).with_context(|| {
        format!(
            "failed to write {} (run with sudo: `sudo nvrdesk --install-service`)",
            config_path.display()
        )
    })?;

    // Allow the desktop service user to persist updates saved from the embedded Web UI.
    let _ = Command::new("chown").arg(&user).arg(config_path).status();

    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map_or_else(
            || "/usr/bin/nvrdesk".to_string(),
            |p| p.display().to_string(),
        );
    let display = std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| ":0".to_string());

    let unit_path = PathBuf::from("/etc/systemd/system/nvrdesk@.service");
    let unit_content = render_systemd_unit(&exe_path, &config_path.display().to_string(), &display);
    std::fs::write(&unit_path, unit_content).with_context(|| {
        format!(
            "failed to write {} (run with sudo: `sudo nvrdesk --install-service`)",
            unit_path.display()
        )
    })?;

    let reload = Command::new("systemctl")
        .arg("daemon-reload")
        .status()
        .context("failed to run `systemctl daemon-reload`")?;
    if !reload.success() {
        anyhow::bail!("`systemctl daemon-reload` exited with {reload}");
    }

    let unit_instance = format!("nvrdesk@{user}.service");
    let enable = Command::new("systemctl")
        .args(["enable", "--now", &unit_instance])
        .status()
        .with_context(|| format!("failed to enable {unit_instance}"))?;
    if !enable.success() {
        anyhow::bail!("`systemctl enable --now {unit_instance}` exited with {enable}");
    }

    let _ = Command::new("systemctl")
        .args(["restart", &unit_instance])
        .status();

    println!("Installed NVRDesk systemd service:");
    println!("  Config : {}", config_path.display());
    println!("  Unit   : {}", unit_path.display());
    println!("  Service: {unit_instance} (enabled & started)");
    println!("  Logs   : journalctl -u {unit_instance} -f");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_systemd_service(
    config_path: &std::path::Path,
    cfg: &NvrDeskConfig,
    explicit_user: Option<&str>,
    headless: bool,
) -> Result<()> {
    let _ = (config_path, cfg, explicit_user, headless);
    anyhow::bail!(
        "`--install-service` is only supported on Linux (systemd). Use `packaging/fleet/windows/Install-NvrDeskPos.ps1` on Windows or `packaging/fleet/macos/com.nvrdesk.agent.plist` on macOS."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_desktop_user_from_explicit_or_sudo() {
        assert_eq!(
            resolve_service_user(Some("pos"), Some("other"), Some("root")).expect("explicit"),
            "pos"
        );
        assert_eq!(
            resolve_service_user(None, Some("desiteg"), Some("root")).expect("sudo"),
            "desiteg"
        );
        assert!(resolve_service_user(None, None, Some("root")).is_err());
        assert!(resolve_service_user(None, None, None).is_err());
    }

    #[test]
    fn renders_systemd_unit_with_expected_fields() {
        let unit = render_systemd_unit("/usr/bin/nvrdesk", "/etc/nvrdesk/nvrdesk.toml", ":0");
        assert!(unit.contains("User=%i"));
        assert!(unit.contains("Environment=DISPLAY=:0"));
        assert!(
            unit.contains(
                "ExecStart=/usr/bin/nvrdesk --config /etc/nvrdesk/nvrdesk.toml --no-tray"
            )
        );
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn renders_snap_autostart_desktop_and_path() {
        let path = snap_autostart_desktop_path("/home/pos/snap/nvrdesk/42");
        assert_eq!(
            path,
            PathBuf::from("/home/pos/snap/nvrdesk/42/.config/autostart/nvrdesk.desktop")
        );
        let gui_desktop = render_snap_autostart_desktop(false);
        assert!(gui_desktop.contains("Exec=nvrdesk\n"));
        let headless_desktop = render_snap_autostart_desktop(true);
        assert!(headless_desktop.contains("Exec=nvrdesk.headless\n"));
    }
}
