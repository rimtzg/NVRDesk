//! ONVIF Profile S & Profile T server: WS-Discovery, WS-Security, Device/Media/Media2/Events & Snapshots.

pub mod events;
pub mod server;
pub mod soap;
pub mod ws_discovery;
pub mod ws_security;

pub use events::{EventBroker, OnvifEventRecord};
pub use server::{
    OnvifServer, SnapshotProvider, build_onvif_router, build_onvif_router_with_events,
};
pub use soap::{OnvifDeviceMeta, OnvifMediaProfile};
pub use ws_discovery::{WsDiscoveryConfig, run_ws_discovery, run_ws_discovery_watch};
pub use ws_security::OnvifAuthConfig;
