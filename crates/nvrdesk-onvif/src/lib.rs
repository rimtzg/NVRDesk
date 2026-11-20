//! ONVIF Profile S server: WS-Discovery, WS-Security, Device/Media SOAP services, and JPEG snapshots.

pub mod server;
pub mod soap;
pub mod ws_discovery;
pub mod ws_security;

pub use server::{OnvifServer, SnapshotProvider};
pub use soap::{OnvifDeviceMeta, OnvifMediaProfile};
pub use ws_discovery::{WsDiscoveryConfig, run_ws_discovery};
pub use ws_security::OnvifAuthConfig;
