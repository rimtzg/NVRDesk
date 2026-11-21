//! NVRDesk CLI entrypoint launching the daemon (RTSP/ONVIF/Web UI) and System Tray.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use nvrdesk_core::{
    AudioCodecKind, CaptureSourceKind, EncoderBackendKind, KeystrokeMode, NvrDeskConfig,
    VideoCodecKind, WebcamMode,
};
use nvrdesk_daemon::start_daemon;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSourceKind {
    Screen,
    TestPattern,
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
    /// Path to TOML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

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

    /// Video source backend.
    #[arg(long, value_enum)]
    source: Option<CliSourceKind>,

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

    /// Enable webcam capture (PiP or dedicated `/webcam` RTSP channel).
    #[arg(long, default_value_t = false)]
    webcam: bool,

    /// Webcam integration mode (`pip` or `channel`).
    #[arg(long, value_enum)]
    webcam_mode: Option<CliWebcamMode>,

    /// Webcam device index.
    #[arg(long)]
    webcam_device: Option<usize>,

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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,xcap=warn,nokhwa=warn")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = if let Some(path) = &cli.config {
        if path.exists() {
            NvrDeskConfig::from_toml_file(path)?
        } else {
            let default = NvrDeskConfig::default();
            default.save_toml_file(path)?;
            default
        }
    } else {
        NvrDeskConfig::default()
    };

    apply_cli_overrides(&cli, &mut cfg);

    if cli.print_config {
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let use_tray_loop = !cli.no_tray;
    let config_path = cli.config.clone();

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

fn apply_cli_overrides(cli: &Cli, cfg: &mut NvrDeskConfig) {
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
        };
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
        };
    }
    if let Some(idx) = cli.webcam_device {
        cfg.webcam.device_index = idx;
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
}
