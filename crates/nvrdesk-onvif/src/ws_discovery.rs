//! WS-Discovery UDP multicast responder (239.255.255.250:3702) for ONVIF auto-discovery.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};

use nvrdesk_core::NvrDeskError;
use quick_xml::{Reader, events::Event};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket, sync::watch};
use tracing::{debug, info, warn};
use uuid::Uuid;

const WS_DISCOVERY_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const WS_DISCOVERY_PORT: u16 = 3702;

/// Configuration for WS-Discovery ProbeMatches responses.
#[derive(Debug, Clone)]
pub struct WsDiscoveryConfig {
    pub device_uuid: Uuid,
    pub http_port: u16,
    pub device_name: String,
    pub model: String,
}

#[derive(Debug)]
struct ProbeInfo {
    message_id: String,
}

/// Runs the WS-Discovery multicast responder loop for a single ONVIF device.
pub async fn run_ws_discovery(cfg: WsDiscoveryConfig) -> Result<(), NvrDeskError> {
    let (_tx, rx) = watch::channel(vec![cfg]);
    run_ws_discovery_watch(rx).await
}

/// Runs the WS-Discovery multicast responder loop, advertising every active ONVIF virtual device in `configs_rx`.
pub async fn run_ws_discovery_watch(
    configs_rx: watch::Receiver<Vec<WsDiscoveryConfig>>,
) -> Result<(), NvrDeskError> {
    let std_sock = bind_multicast_socket()?;
    let udp = UdpSocket::from_std(std_sock)?;

    if let Err(err) = udp.join_multicast_v4(WS_DISCOVERY_MULTICAST, Ipv4Addr::UNSPECIFIED) {
        warn!(error = %err, "failed to join WS-Discovery multicast 239.255.255.250:3702");
        return Ok(());
    }

    for cfg in configs_rx.borrow().iter() {
        info!(
            uuid = %cfg.device_uuid,
            http_port = cfg.http_port,
            device_name = %cfg.device_name,
            "ONVIF WS-Discovery listening on 239.255.255.250:3702"
        );
    }

    let mut buf = vec![0u8; 8192];
    loop {
        let (n, peer_addr) = udp.recv_from(&mut buf).await?;
        let payload = String::from_utf8_lossy(&buf[..n]);
        if let Some(probe_info) = parse_probe(&payload) {
            let local_ip = resolve_local_ip_for_peer(peer_addr);
            let active_cfgs = configs_rx.borrow().clone();
            for cfg in &active_cfgs {
                let reply = build_probe_match(cfg, &probe_info.message_id, &local_ip);
                if let Err(err) = udp.send_to(reply.as_bytes(), peer_addr).await {
                    debug!(%peer_addr, http_port = cfg.http_port, error = %err, "failed to send WS-Discovery ProbeMatches");
                } else {
                    debug!(%peer_addr, %local_ip, http_port = cfg.http_port, "sent WS-Discovery ProbeMatches");
                }
            }
        }
    }
}

fn bind_multicast_socket() -> Result<StdUdpSocket, NvrDeskError> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    {
        let _ = sock.set_reuse_port(true);
    }
    sock.set_nonblocking(true)?;
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), WS_DISCOVERY_PORT);
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// Determines the local IPv4 address used to route packets back to `peer_addr`.
#[must_use]
pub fn resolve_local_ip_for_peer(peer_addr: SocketAddr) -> String {
    if let Ok(sock) = StdUdpSocket::bind("0.0.0.0:0")
        && sock.connect(peer_addr).is_ok()
        && let Ok(local) = sock.local_addr()
        && !local.ip().is_unspecified()
    {
        return local.ip().to_string();
    }
    "127.0.0.1".to_string()
}

fn parse_probe(xml: &str) -> Option<ProbeInfo> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut is_probe = false;
    let mut message_id: Option<String> = None;
    let mut types_text: Option<String> = None;
    let mut current_tag: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = e.local_name().as_ref().to_string();
                if local == "Probe" {
                    is_probe = true;
                }
                current_tag = Some(local);
            }
            Ok(Event::End(_)) => {
                current_tag = None;
            }
            Ok(Event::Text(e)) => {
                let txt = e.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match current_tag.as_deref() {
                    Some("MessageID") => message_id = Some(txt.into_owned()),
                    Some("Types") => types_text = Some(txt.into_owned()),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if !is_probe {
        return None;
    }

    // Respond if Types is empty or requests NetworkVideoTransmitter / Device.
    if let Some(types) = types_text {
        let lower = types.to_ascii_lowercase();
        if !lower.is_empty()
            && !lower.contains("networkvideotransmitter")
            && !lower.contains("device")
        {
            return None;
        }
    }

    Some(ProbeInfo {
        message_id: message_id.unwrap_or_else(|| format!("urn:uuid:{}", Uuid::new_v4())),
    })
}

/// Builds a WS-Discovery `ProbeMatches` SOAP 1.2 response envelope.
#[must_use]
pub fn build_probe_match(cfg: &WsDiscoveryConfig, relates_to: &str, local_ip: &str) -> String {
    let msg_id = format!("urn:uuid:{}", Uuid::new_v4());
    let endpoint_uuid = format!("urn:uuid:{}", cfg.device_uuid);
    let escaped_name = cfg.device_name.replace(' ', "%20");
    let escaped_model = cfg.model.replace(' ', "%20");
    let xaddr = format!("http://{local_ip}:{}/onvif/device_service", cfg.http_port);

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <s:Header>
    <a:MessageID>{msg_id}</a:MessageID>
    <a:RelatesTo>{relates_to}</a:RelatesTo>
    <a:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:To>
    <a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</a:Action>
  </s:Header>
  <s:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <a:EndpointReference>
          <a:Address>{endpoint_uuid}</a:Address>
        </a:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
        <d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/type/NetworkVideoTransmitter onvif://www.onvif.org/Profile/Streaming onvif://www.onvif.org/name/{escaped_name} onvif://www.onvif.org/hardware/{escaped_model} onvif://www.onvif.org/location/POS</d:Scopes>
        <d:XAddrs>{xaddr}</d:XAddrs>
        <d:MetadataVersion>1</d:MetadataVersion>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </s:Body>
</s:Envelope>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_onvif_probe_and_builds_probe_match() {
        let probe = r#"<?xml version="1.0" encoding="UTF-8"?>
        <e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
                    xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
                    xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
                    xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
          <e:Header>
            <w:MessageID>uuid:1234-5678</w:MessageID>
            <w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
            <w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
          </e:Header>
          <e:Body>
            <d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe>
          </e:Body>
        </e:Envelope>"#;

        let info = parse_probe(probe).expect("valid probe");
        assert_eq!(info.message_id, "uuid:1234-5678");

        let cfg = WsDiscoveryConfig {
            device_uuid: Uuid::nil(),
            http_port: 8080,
            device_name: "NVRDesk POS".into(),
            model: "VirtualCam".into(),
        };
        let reply = build_probe_match(&cfg, &info.message_id, "192.168.1.50");
        assert!(reply.contains("<a:RelatesTo>uuid:1234-5678</a:RelatesTo>"));
        assert!(reply.contains("http://192.168.1.50:8080/onvif/device_service"));
        assert!(reply.contains("onvif://www.onvif.org/Profile/Streaming"));
    }
}
