//! ONVIF Profile S SOAP request parser and response XML builders (Device & Media services).

use chrono::{Datelike, Timelike, Utc};
use quick_xml::{Reader, events::Event};

/// ONVIF Media Profile metadata exposed via `GetProfiles` / `GetStreamUri` / `GetSnapshotUri`.
#[derive(Debug, Clone)]
pub struct OnvifMediaProfile {
    pub token: String,
    pub name: String,
    pub encoder_token: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gov_length: u32,
    pub rtsp_path: String,
    pub snapshot_slug: String,
}

/// Device metadata used in ONVIF Device service responses.
#[derive(Debug, Clone)]
pub struct OnvifDeviceMeta {
    pub device_name: String,
    pub manufacturer: String,
    pub model: String,
    pub firmware_version: String,
    pub serial_number: String,
    pub hardware_id: String,
    pub rtsp_port: u16,
    pub http_port: u16,
}

/// Parsed SOAP Body operation name and relevant inner elements.
#[derive(Debug, Default)]
pub struct SoapRequestInfo {
    pub action: String,
    pub profile_token: Option<String>,
    pub configuration_token: Option<String>,
}

/// Extracts the SOAP Body root element local name and optional `ProfileToken` / `ConfigurationToken`.
#[must_use]
pub fn parse_soap_request(xml: &str) -> Option<SoapRequestInfo> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_body = false;
    let mut body_depth = 0usize;
    let mut info = SoapRequestInfo::default();
    let mut current_tag: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_string();
                if local == "Body" {
                    in_body = true;
                    body_depth = 0;
                } else if in_body {
                    body_depth += 1;
                    if body_depth == 1 && info.action.is_empty() {
                        info.action = local.clone();
                    }
                    current_tag = Some(local);
                }
            }
            Ok(Event::Empty(e)) => {
                let local = e.local_name().as_ref().to_string();
                if in_body && body_depth == 0 && info.action.is_empty() {
                    info.action = local;
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name().as_ref().to_string();
                if in_body {
                    if local == "Body" {
                        break;
                    }
                    body_depth = body_depth.saturating_sub(1);
                    if current_tag.as_deref() == Some(local.as_str()) {
                        current_tag = None;
                    }
                }
            }
            Ok(Event::Text(e)) if in_body => {
                let txt = e.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match current_tag.as_deref() {
                    Some("ProfileToken") => info.profile_token = Some(txt.into_owned()),
                    Some("ConfigurationToken") => info.configuration_token = Some(txt.into_owned()),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if info.action.is_empty() {
        None
    } else {
        Some(info)
    }
}

fn soap_envelope(body_xml: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl"
            xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
            xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    {body_xml}
  </s:Body>
</s:Envelope>"#
    )
}

/// Builds a standard ONVIF SOAP 1.2 fault envelope.
#[must_use]
pub fn soap_fault(subcode: &str, reason: &str) -> String {
    let escaped_reason = escape_xml(reason);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:ter="http://www.onvif.org/ver10/error">
  <s:Body>
    <s:Fault>
      <s:Code>
        <s:Value>s:Sender</s:Value>
        <s:Subcode>
          <s:Value>{subcode}</s:Value>
        </s:Subcode>
      </s:Code>
      <s:Reason>
        <s:Text xml:lang="en">{escaped_reason}</s:Text>
      </s:Reason>
    </s:Fault>
  </s:Body>
</s:Envelope>"#
    )
}

#[must_use]
pub fn get_system_date_and_time_response() -> String {
    let now = Utc::now();
    soap_envelope(&format!(
        r#"<tds:GetSystemDateAndTimeResponse>
      <tds:SystemDateAndTime>
        <tt:DateTimeType>NTP</tt:DateTimeType>
        <tt:DaylightSavings>false</tt:DaylightSavings>
        <tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone>
        <tt:UTCDateTime>
          <tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time>
          <tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date>
        </tt:UTCDateTime>
      </tds:SystemDateAndTime>
    </tds:GetSystemDateAndTimeResponse>"#,
        now.hour(),
        now.minute(),
        now.second(),
        now.year(),
        now.month(),
        now.day(),
    ))
}

#[must_use]
pub fn get_device_information_response(meta: &OnvifDeviceMeta) -> String {
    soap_envelope(&format!(
        r#"<tds:GetDeviceInformationResponse>
      <tds:Manufacturer>{}</tds:Manufacturer>
      <tds:Model>{}</tds:Model>
      <tds:FirmwareVersion>{}</tds:FirmwareVersion>
      <tds:SerialNumber>{}</tds:SerialNumber>
      <tds:HardwareId>{}</tds:HardwareId>
    </tds:GetDeviceInformationResponse>"#,
        escape_xml(&meta.manufacturer),
        escape_xml(&meta.model),
        escape_xml(&meta.firmware_version),
        escape_xml(&meta.serial_number),
        escape_xml(&meta.hardware_id),
    ))
}

#[must_use]
pub fn get_capabilities_response(host: &str, http_port: u16) -> String {
    let base = format!("http://{host}:{http_port}");
    soap_envelope(&format!(
        r#"<tds:GetCapabilitiesResponse>
      <tds:Capabilities>
        <tt:Device>
          <tt:XAddr>{base}/onvif/device_service</tt:XAddr>
          <tt:Network>
            <tt:IPFilter>false</tt:IPFilter>
            <tt:ZeroConfiguration>false</tt:ZeroConfiguration>
            <tt:IPVersion6>false</tt:IPVersion6>
            <tt:DynDNS>false</tt:DynDNS>
          </tt:Network>
          <tt:System>
            <tt:DiscoveryResolve>true</tt:DiscoveryResolve>
            <tt:DiscoveryBye>false</tt:DiscoveryBye>
            <tt:RemoteDiscovery>false</tt:RemoteDiscovery>
            <tt:SystemBackup>false</tt:SystemBackup>
            <tt:SystemLogging>false</tt:SystemLogging>
            <tt:FirmwareUpgrade>false</tt:FirmwareUpgrade>
            <tt:SupportedVersions><tt:Major>2</tt:Major><tt:Minor>50</tt:Minor></tt:SupportedVersions>
          </tt:System>
          <tt:IO><tt:InputConnectors>0</tt:InputConnectors><tt:RelayOutputs>0</tt:RelayOutputs></tt:IO>
          <tt:Security>
            <tt:TLS1.1>false</tt:TLS1.1>
            <tt:TLS1.2>false</tt:TLS1.2>
            <tt:OnboardKeyGeneration>false</tt:OnboardKeyGeneration>
            <tt:AccessPolicyConfig>false</tt:AccessPolicyConfig>
            <tt:X.509Token>false</tt:X.509Token>
            <tt:SAMLToken>false</tt:SAMLToken>
            <tt:KerberosToken>false</tt:KerberosToken>
            <tt:RELToken>false</tt:RELToken>
          </tt:Security>
        </tt:Device>
        <tt:Media>
          <tt:XAddr>{base}/onvif/media_service</tt:XAddr>
          <tt:StreamingCapabilities>
            <tt:RTPMulticast>false</tt:RTPMulticast>
            <tt:RTP_TCP>true</tt:RTP_TCP>
            <tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP>
          </tt:StreamingCapabilities>
        </tt:Media>
      </tds:Capabilities>
    </tds:GetCapabilitiesResponse>"#
    ))
}

#[must_use]
pub fn get_services_response(host: &str, http_port: u16) -> String {
    let base = format!("http://{host}:{http_port}");
    soap_envelope(&format!(
        r#"<tds:GetServicesResponse>
      <tds:Service>
        <tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace>
        <tds:XAddr>{base}/onvif/device_service</tds:XAddr>
        <tds:Version><tt:Major>2</tt:Major><tt:Minor>50</tt:Minor></tds:Version>
      </tds:Service>
      <tds:Service>
        <tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace>
        <tds:XAddr>{base}/onvif/media_service</tds:XAddr>
        <tds:Version><tt:Major>2</tt:Major><tt:Minor>50</tt:Minor></tds:Version>
      </tds:Service>
    </tds:GetServicesResponse>"#
    ))
}

#[must_use]
pub fn get_service_capabilities_response() -> String {
    soap_envelope(
        r#"<tds:GetServiceCapabilitiesResponse>
      <tds:Capabilities>
        <tds:Network IPFilter="false" ZeroConfiguration="false" IPVersion6="false" DynDNS="false"/>
        <tds:Security TLS1.0="false" TLS1.1="false" TLS1.2="false" OnboardKeyGeneration="false" AccessPolicyConfig="false" DefaultAccessPolicy="false" Dot1X="false" RemoteUserHandling="false" X.509Token="false" SAMLToken="false" KerberosToken="false" UsernameToken="true" HttpDigest="true" RELToken="false"/>
        <tds:System DiscoveryResolve="true" DiscoveryBye="false" RemoteDiscovery="false" SystemBackup="false" SystemLogging="false" FirmwareUpgrade="false"/>
      </tds:Capabilities>
    </tds:GetServiceCapabilitiesResponse>"#,
    )
}

#[must_use]
pub fn get_scopes_response(meta: &OnvifDeviceMeta) -> String {
    let name = escape_xml(&meta.device_name.replace(' ', "%20"));
    let model = escape_xml(&meta.model.replace(' ', "%20"));
    soap_envelope(&format!(
        r#"<tds:GetScopesResponse>
      <tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem></tds:Scopes>
      <tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/type/NetworkVideoTransmitter</tt:ScopeItem></tds:Scopes>
      <tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/Profile/Streaming</tt:ScopeItem></tds:Scopes>
      <tds:Scopes><tt:ScopeDef>Configurable</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/name/{name}</tt:ScopeItem></tds:Scopes>
      <tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/hardware/{model}</tt:ScopeItem></tds:Scopes>
      <tds:Scopes><tt:ScopeDef>Configurable</tt:ScopeDef><tt:ScopeItem>onvif://www.onvif.org/location/POS</tt:ScopeItem></tds:Scopes>
    </tds:GetScopesResponse>"#
    ))
}

#[must_use]
pub fn get_network_interfaces_response(host: &str) -> String {
    soap_envelope(&format!(
        r#"<tds:GetNetworkInterfacesResponse>
      <tds:NetworkInterfaces token="eth0">
        <tt:Enabled>true</tt:Enabled>
        <tt:Info>
          <tt:Name>eth0</tt:Name>
          <tt:HwAddress>02:4E:56:52:44:01</tt:HwAddress>
          <tt:MTU>1500</tt:MTU>
        </tt:Info>
        <tt:IPv4>
          <tt:Enabled>true</tt:Enabled>
          <tt:Config>
            <tt:Manual><tt:Address>{}</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:Manual>
            <tt:DHCP>false</tt:DHCP>
          </tt:Config>
        </tt:IPv4>
      </tds:NetworkInterfaces>
    </tds:GetNetworkInterfacesResponse>"#,
        escape_xml(host)
    ))
}

#[must_use]
pub fn get_hostname_response() -> String {
    soap_envelope(
        r#"<tds:GetHostnameResponse>
      <tds:HostnameInformation>
        <tt:FromDHCP>false</tt:FromDHCP>
        <tt:Name>nvrdesk</tt:Name>
      </tds:HostnameInformation>
    </tds:GetHostnameResponse>"#,
    )
}

#[must_use]
pub fn get_dns_response() -> String {
    soap_envelope(
        r#"<tds:GetDNSResponse>
      <tds:DNSInformation>
        <tt:FromDHCP>false</tt:FromDHCP>
      </tds:DNSInformation>
    </tds:GetDNSResponse>"#,
    )
}

fn render_profile_xml(tag: &str, p: &OnvifMediaProfile, src_w: u32, src_h: u32) -> String {
    format!(
        r#"<{tag} token="{token}" fixed="true">
      <tt:Name>{name}</tt:Name>
      <tt:VideoSourceConfiguration token="VideoSourceConfig_1">
        <tt:Name>ScreenSourceConfig</tt:Name>
        <tt:UseCount>2</tt:UseCount>
        <tt:SourceToken>VideoSource_1</tt:SourceToken>
        <tt:Bounds x="0" y="0" width="{src_w}" height="{src_h}"/>
      </tt:VideoSourceConfiguration>
      <tt:VideoEncoderConfiguration token="{enc_token}">
        <tt:Name>{name}H264</tt:Name>
        <tt:UseCount>1</tt:UseCount>
        <tt:Encoding>H264</tt:Encoding>
        <tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>
        <tt:Quality>5.0</tt:Quality>
        <tt:RateControl>
          <tt:FrameRateLimit>{fps}</tt:FrameRateLimit>
          <tt:EncodingInterval>1</tt:EncodingInterval>
          <tt:BitrateLimit>{kbps}</tt:BitrateLimit>
        </tt:RateControl>
        <tt:H264>
          <tt:GovLength>{gov}</tt:GovLength>
          <tt:H264Profile>Baseline</tt:H264Profile>
        </tt:H264>
        <tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address><tt:Port>0</tt:Port><tt:TTL>1</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast>
        <tt:SessionTimeout>PT60S</tt:SessionTimeout>
      </tt:VideoEncoderConfiguration>
    </{tag}>"#,
        token = escape_xml(&p.token),
        name = escape_xml(&p.name),
        enc_token = escape_xml(&p.encoder_token),
        w = p.width,
        h = p.height,
        fps = p.fps,
        kbps = p.bitrate_kbps,
        gov = p.gov_length,
    )
}

#[must_use]
pub fn get_profiles_response(profiles: &[OnvifMediaProfile]) -> String {
    let (src_w, src_h) = profiles
        .first()
        .map_or((1920, 1080), |p| (p.width, p.height));
    let mut inner = String::new();
    for p in profiles {
        inner.push_str(&render_profile_xml("trt:Profiles", p, src_w, src_h));
    }
    soap_envelope(&format!(
        "<trt:GetProfilesResponse>{inner}</trt:GetProfilesResponse>"
    ))
}

#[must_use]
pub fn get_profile_response(profile: &OnvifMediaProfile, src_w: u32, src_h: u32) -> String {
    let xml = render_profile_xml("trt:Profile", profile, src_w, src_h);
    soap_envelope(&format!(
        "<trt:GetProfileResponse>{xml}</trt:GetProfileResponse>"
    ))
}

#[must_use]
pub fn get_video_sources_response(src_w: u32, src_h: u32, fps: u32) -> String {
    soap_envelope(&format!(
        r#"<trt:GetVideoSourcesResponse>
      <trt:VideoSources token="VideoSource_1">
        <tt:Framerate>{fps}.0</tt:Framerate>
        <tt:Resolution><tt:Width>{src_w}</tt:Width><tt:Height>{src_h}</tt:Height></tt:Resolution>
      </trt:VideoSources>
    </trt:GetVideoSourcesResponse>"#
    ))
}

#[must_use]
pub fn get_video_source_configurations_response(src_w: u32, src_h: u32) -> String {
    soap_envelope(&format!(
        r#"<trt:GetVideoSourceConfigurationsResponse>
      <trt:Configurations token="VideoSourceConfig_1">
        <tt:Name>ScreenSourceConfig</tt:Name>
        <tt:UseCount>2</tt:UseCount>
        <tt:SourceToken>VideoSource_1</tt:SourceToken>
        <tt:Bounds x="0" y="0" width="{src_w}" height="{src_h}"/>
      </trt:Configurations>
    </trt:GetVideoSourceConfigurationsResponse>"#
    ))
}

#[must_use]
pub fn get_video_encoder_configurations_response(profiles: &[OnvifMediaProfile]) -> String {
    let mut inner = String::new();
    for p in profiles {
        inner.push_str(&format!(
            r#"<trt:Configurations token="{enc_token}">
        <tt:Name>{name}H264</tt:Name>
        <tt:UseCount>1</tt:UseCount>
        <tt:Encoding>H264</tt:Encoding>
        <tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>
        <tt:Quality>5.0</tt:Quality>
        <tt:RateControl>
          <tt:FrameRateLimit>{fps}</tt:FrameRateLimit>
          <tt:EncodingInterval>1</tt:EncodingInterval>
          <tt:BitrateLimit>{kbps}</tt:BitrateLimit>
        </tt:RateControl>
        <tt:H264>
          <tt:GovLength>{gov}</tt:GovLength>
          <tt:H264Profile>Baseline</tt:H264Profile>
        </tt:H264>
        <tt:SessionTimeout>PT60S</tt:SessionTimeout>
      </trt:Configurations>"#,
            enc_token = escape_xml(&p.encoder_token),
            name = escape_xml(&p.name),
            w = p.width,
            h = p.height,
            fps = p.fps,
            kbps = p.bitrate_kbps,
            gov = p.gov_length,
        ));
    }
    soap_envelope(&format!(
        "<trt:GetVideoEncoderConfigurationsResponse>{inner}</trt:GetVideoEncoderConfigurationsResponse>"
    ))
}

#[must_use]
pub fn get_video_encoder_configuration_options_response(profiles: &[OnvifMediaProfile]) -> String {
    let mut res_xml = String::new();
    for p in profiles {
        res_xml.push_str(&format!(
            "<tt:ResolutionsAvailable><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:ResolutionsAvailable>",
            p.width, p.height
        ));
    }
    soap_envelope(&format!(
        r#"<trt:GetVideoEncoderConfigurationOptionsResponse>
      <trt:Options>
        <tt:QualityRange><tt:Min>1</tt:Min><tt:Max>10</tt:Max></tt:QualityRange>
        <tt:H264>
          {res_xml}
          <tt:GovLengthRange><tt:Min>1</tt:Min><tt:Max>120</tt:Max></tt:GovLengthRange>
          <tt:FrameRateRange><tt:Min>1</tt:Min><tt:Max>30</tt:Max></tt:FrameRateRange>
          <tt:EncodingIntervalRange><tt:Min>1</tt:Min><tt:Max>4</tt:Max></tt:EncodingIntervalRange>
          <tt:H264ProfilesSupported>Baseline</tt:H264ProfilesSupported>
          <tt:H264ProfilesSupported>Main</tt:H264ProfilesSupported>
        </tt:H264>
      </trt:Options>
    </trt:GetVideoEncoderConfigurationOptionsResponse>"#
    ))
}

#[must_use]
pub fn set_video_encoder_configuration_response() -> String {
    soap_envelope("<trt:SetVideoEncoderConfigurationResponse/>")
}

#[must_use]
pub fn get_stream_uri_response(rtsp_uri: &str) -> String {
    soap_envelope(&format!(
        r#"<trt:GetStreamUriResponse>
      <trt:MediaUri>
        <tt:Uri>{}</tt:Uri>
        <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
        <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
        <tt:Timeout>PT60S</tt:Timeout>
      </trt:MediaUri>
    </trt:GetStreamUriResponse>"#,
        escape_xml(rtsp_uri)
    ))
}

#[must_use]
pub fn get_snapshot_uri_response(snapshot_uri: &str) -> String {
    soap_envelope(&format!(
        r#"<trt:GetSnapshotUriResponse>
      <trt:MediaUri>
        <tt:Uri>{}</tt:Uri>
        <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
        <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
        <tt:Timeout>PT60S</tt:Timeout>
      </trt:MediaUri>
    </trt:GetSnapshotUriResponse>"#,
        escape_xml(snapshot_uri)
    ))
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_soap_body_action_and_profile_token() {
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
            <s:Body>
                <trt:GetStreamUri xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
                    <trt:ProfileToken>SubProfile</trt:ProfileToken>
                </trt:GetStreamUri>
            </s:Body>
        </s:Envelope>"#;
        let info = parse_soap_request(xml).expect("parsed soap");
        assert_eq!(info.action, "GetStreamUri");
        assert_eq!(info.profile_token.as_deref(), Some("SubProfile"));
    }
}
