//! Gateway port mapping (NAT-PMP / PCP, then UPnP-IGD).
//!
//! Talks only to the local router. Private torrents do not turn this off.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

pub const NAT_LEASE: Duration = Duration::from_secs(30 * 60);
pub const REACHABILITY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NatProtocol {
    #[default]
    Auto,
    NatPmp,
    Upnp,
    Off,
}

impl NatProtocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "natpmp" => Some(Self::NatPmp),
            "upnp" => Some(Self::Upnp),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Engine knob. Disabled unless config or a caller turns it on, so tests
/// do not probe a gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatOptions {
    pub enabled: bool,
    pub protocol: NatProtocol,
}

impl Default for NatOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            protocol: NatProtocol::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MapProto {
    Tcp,
    Udp,
}

impl fmt::Display for MapProto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapBackend {
    NatPmp,
    Upnp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub protocol: MapProto,
    pub internal_port: u16,
    pub external_port: u16,
    pub external_ip: Option<IpAddr>,
    pub lifetime: Duration,
    pub backend: MapBackend,
}

#[derive(Debug, thiserror::Error)]
pub enum NatError {
    #[error("{0}")]
    Message(String),
}

#[async_trait]
pub trait PortMapper: Send + Sync {
    async fn add(
        &self,
        proto: MapProto,
        internal_port: u16,
        lease: Duration,
    ) -> Result<Mapping, NatError>;

    async fn renew(&self, mapping: &Mapping, lease: Duration) -> Result<Mapping, NatError>;

    async fn delete(&self, mapping: &Mapping) -> Result<(), NatError>;
}

pub fn mapper_for(protocol: NatProtocol) -> Arc<dyn PortMapper> {
    match protocol {
        NatProtocol::NatPmp => Arc::new(NatPmpMapper::new()),
        NatProtocol::Upnp => Arc::new(UpnpMapper::new()),
        NatProtocol::Auto | NatProtocol::Off => Arc::new(AutoMapper::new()),
    }
}

struct Endpoint {
    gateway: crab_nat::GatewayAddress,
    client: IpAddr,
}

fn lan_endpoint() -> Result<Endpoint, NatError> {
    let gateway = default_net::get_default_gateway().map_err(NatError::Message)?;
    let client = default_net::interface::get_local_ipaddr()
        .ok_or_else(|| NatError::Message("local address not found".into()))?;
    Ok(Endpoint {
        gateway: gateway.ip_addr.into(),
        client,
    })
}

fn lease_secs(lease: Duration) -> u32 {
    u32::try_from(lease.as_secs()).unwrap_or(u32::MAX).max(1)
}

fn crab_proto(proto: MapProto) -> crab_nat::InternetProtocol {
    match proto {
        MapProto::Tcp => crab_nat::InternetProtocol::Tcp,
        MapProto::Udp => crab_nat::InternetProtocol::Udp,
    }
}

pub struct NatPmpMapper {
    state: Mutex<NatPmpState>,
}

struct NatPmpState {
    endpoint: Option<Endpoint>,
    external_ip: Option<IpAddr>,
    maps: HashMap<(MapProto, u16), crab_nat::PortMapping>,
}

impl NatPmpMapper {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(NatPmpState {
                endpoint: None,
                external_ip: None,
                maps: HashMap::new(),
            }),
        }
    }

    async fn endpoint(&self) -> Result<(crab_nat::GatewayAddress, IpAddr), NatError> {
        let mut state = self.state.lock().await;
        if let Some(endpoint) = &state.endpoint {
            return Ok((endpoint.gateway, endpoint.client));
        }
        let endpoint = tokio::task::spawn_blocking(lan_endpoint)
            .await
            .map_err(|err| NatError::Message(err.to_string()))??;
        let pair = (endpoint.gateway, endpoint.client);
        state.endpoint = Some(endpoint);
        Ok(pair)
    }

    async fn external_ip_for(&self, mapping: &crab_nat::PortMapping) -> Option<IpAddr> {
        if let crab_nat::PortMappingType::Pcp { external_ip, .. } = mapping.mapping_type() {
            return Some(external_ip);
        }
        let state = self.state.lock().await;
        if let Some(ip) = state.external_ip {
            return Some(ip);
        }
        let gateway = state.endpoint.as_ref()?.gateway;
        drop(state);
        let ip = crab_nat::natpmp::external_address(gateway, None)
            .await
            .ok()
            .map(IpAddr::V4);
        if let Some(ip) = ip {
            self.state.lock().await.external_ip = Some(ip);
        }
        ip
    }
}

impl Default for NatPmpMapper {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PortMapper for NatPmpMapper {
    async fn add(
        &self,
        proto: MapProto,
        internal_port: u16,
        lease: Duration,
    ) -> Result<Mapping, NatError> {
        let internal = NonZeroU16::new(internal_port)
            .ok_or_else(|| NatError::Message("port 0 cannot be mapped".into()))?;
        let (gateway, client) = self.endpoint().await?;
        let options = crab_nat::PortMappingOptions {
            external_port: Some(internal),
            lifetime_seconds: Some(lease_secs(lease)),
            timeout_config: None,
        };
        let created =
            match crab_nat::PortMapping::new(gateway, client, crab_proto(proto), internal, options)
                .await
            {
                Ok(mapping) => mapping,
                Err(pcp_err) => {
                    crab_nat::natpmp::port_mapping(gateway, crab_proto(proto), internal, options)
                        .await
                        .map_err(|err| NatError::Message(format!("{pcp_err}; {err}")))?
                }
            };
        let external_ip = self.external_ip_for(&created).await;
        let external_port = created.external_port().get();
        self.state
            .lock()
            .await
            .maps
            .insert((proto, internal_port), created);
        Ok(Mapping {
            protocol: proto,
            internal_port,
            external_port,
            external_ip,
            lifetime: lease,
            backend: MapBackend::NatPmp,
        })
    }

    async fn renew(&self, mapping: &Mapping, lease: Duration) -> Result<Mapping, NatError> {
        let mut state = self.state.lock().await;
        let slot = state
            .maps
            .get_mut(&(mapping.protocol, mapping.internal_port))
            .ok_or_else(|| NatError::Message("mapping is not installed".into()))?;
        slot.renew()
            .await
            .map_err(|err| NatError::Message(err.to_string()))?;
        let external_port = slot.external_port().get();
        let external_ip = match slot.mapping_type() {
            crab_nat::PortMappingType::Pcp { external_ip, .. } => Some(external_ip),
            crab_nat::PortMappingType::NatPmp => state.external_ip,
        };
        Ok(Mapping {
            external_port,
            external_ip,
            lifetime: lease,
            ..mapping.clone()
        })
    }

    async fn delete(&self, mapping: &Mapping) -> Result<(), NatError> {
        let removed = self
            .state
            .lock()
            .await
            .maps
            .remove(&(mapping.protocol, mapping.internal_port));
        let Some(installed) = removed else {
            return Ok(());
        };
        installed
            .try_drop()
            .await
            .map_err(|(err, _)| NatError::Message(err.to_string()))
    }
}

pub struct UpnpMapper {
    gateway: Mutex<Option<igd_next::Gateway>>,
    external_ip: Mutex<Option<IpAddr>>,
}

impl UpnpMapper {
    pub fn new() -> Self {
        Self {
            gateway: Mutex::new(None),
            external_ip: Mutex::new(None),
        }
    }

    async fn gateway(&self) -> Result<igd_next::Gateway, NatError> {
        let mut slot = self.gateway.lock().await;
        if let Some(gateway) = slot.clone() {
            return Ok(gateway);
        }
        let gateway = tokio::task::spawn_blocking(|| {
            igd_next::search_gateway(Default::default()).map_err(|err| err.to_string())
        })
        .await
        .map_err(|err| NatError::Message(err.to_string()))?
        .map_err(NatError::Message)?;
        *slot = Some(gateway.clone());
        Ok(gateway)
    }

    async fn external_ip(&self, gateway: &igd_next::Gateway) -> Option<IpAddr> {
        if let Some(ip) = *self.external_ip.lock().await {
            return Some(ip);
        }
        let gateway = gateway.clone();
        let ip = tokio::task::spawn_blocking(move || gateway.get_external_ip().ok())
            .await
            .ok()
            .flatten();
        if let Some(ip) = ip {
            *self.external_ip.lock().await = Some(ip);
        }
        ip
    }

    fn local_addr(port: u16) -> Result<SocketAddr, NatError> {
        let ip = default_net::interface::get_local_ipaddr()
            .ok_or_else(|| NatError::Message("local address not found".into()))?;
        Ok(SocketAddr::new(ip, port))
    }
}

impl Default for UpnpMapper {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PortMapper for UpnpMapper {
    async fn add(
        &self,
        proto: MapProto,
        internal_port: u16,
        lease: Duration,
    ) -> Result<Mapping, NatError> {
        if internal_port == 0 {
            return Err(NatError::Message("port 0 cannot be mapped".into()));
        }
        let gateway = self.gateway().await?;
        let local = tokio::task::spawn_blocking(move || Self::local_addr(internal_port))
            .await
            .map_err(|err| NatError::Message(err.to_string()))??;
        let igd_proto = match proto {
            MapProto::Tcp => igd_next::PortMappingProtocol::TCP,
            MapProto::Udp => igd_next::PortMappingProtocol::UDP,
        };
        let secs = lease_secs(lease);
        tokio::task::spawn_blocking(move || {
            gateway
                .add_port(igd_proto, internal_port, local, secs, "bitrev")
                .map_err(|err| err.to_string())
        })
        .await
        .map_err(|err| NatError::Message(err.to_string()))?
        .map_err(NatError::Message)?;
        let external_ip = self.external_ip(&self.gateway().await?).await;
        Ok(Mapping {
            protocol: proto,
            internal_port,
            external_port: internal_port,
            external_ip,
            lifetime: lease,
            backend: MapBackend::Upnp,
        })
    }

    async fn renew(&self, mapping: &Mapping, lease: Duration) -> Result<Mapping, NatError> {
        self.add(mapping.protocol, mapping.internal_port, lease)
            .await
    }

    async fn delete(&self, mapping: &Mapping) -> Result<(), NatError> {
        let gateway = self.gateway().await?;
        let igd_proto = match mapping.protocol {
            MapProto::Tcp => igd_next::PortMappingProtocol::TCP,
            MapProto::Udp => igd_next::PortMappingProtocol::UDP,
        };
        let external_port = mapping.external_port;
        tokio::task::spawn_blocking(move || {
            gateway
                .remove_port(igd_proto, external_port)
                .map_err(|err| err.to_string())
        })
        .await
        .map_err(|err| NatError::Message(err.to_string()))?
        .map_err(NatError::Message)
    }
}

pub struct AutoMapper {
    natpmp: NatPmpMapper,
    upnp: UpnpMapper,
}

impl AutoMapper {
    pub fn new() -> Self {
        Self {
            natpmp: NatPmpMapper::new(),
            upnp: UpnpMapper::new(),
        }
    }

    fn dispatch(&self, backend: MapBackend) -> &dyn PortMapper {
        match backend {
            MapBackend::NatPmp => &self.natpmp,
            MapBackend::Upnp => &self.upnp,
        }
    }
}

impl Default for AutoMapper {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PortMapper for AutoMapper {
    async fn add(
        &self,
        proto: MapProto,
        internal_port: u16,
        lease: Duration,
    ) -> Result<Mapping, NatError> {
        match self.natpmp.add(proto, internal_port, lease).await {
            Ok(mapping) => Ok(mapping),
            Err(natpmp_err) => match self.upnp.add(proto, internal_port, lease).await {
                Ok(mapping) => Ok(mapping),
                Err(upnp_err) => Err(NatError::Message(format!("{natpmp_err}; {upnp_err}"))),
            },
        }
    }

    async fn renew(&self, mapping: &Mapping, lease: Duration) -> Result<Mapping, NatError> {
        self.dispatch(mapping.backend).renew(mapping, lease).await
    }

    async fn delete(&self, mapping: &Mapping) -> Result<(), NatError> {
        self.dispatch(mapping.backend).delete(mapping).await
    }
}

pub fn spawn(
    mapper: Arc<dyn PortMapper>,
    targets: Vec<(MapProto, u16)>,
    cancel: CancellationToken,
    external: Arc<StdMutex<Option<SocketAddr>>>,
    port_open: Arc<StdMutex<Option<bool>>>,
    done: Arc<StdMutex<Option<oneshot::Receiver<()>>>>,
) {
    let (tx, rx) = oneshot::channel();
    *done.lock().unwrap() = Some(rx);
    tokio::spawn(async move {
        run(mapper, targets, cancel, external, port_open).await;
        let _ = tx.send(());
    });
}

async fn run(
    mapper: Arc<dyn PortMapper>,
    targets: Vec<(MapProto, u16)>,
    cancel: CancellationToken,
    external: Arc<StdMutex<Option<SocketAddr>>>,
    port_open: Arc<StdMutex<Option<bool>>>,
) {
    let mut mapped = Vec::new();
    let mut failures = Vec::new();
    let tcp_port = targets
        .iter()
        .find(|(proto, _)| *proto == MapProto::Tcp)
        .map(|(_, port)| *port)
        .unwrap_or(0);
    for (proto, port) in targets {
        match mapper.add(proto, port, NAT_LEASE).await {
            Ok(mapping) => mapped.push(mapping),
            Err(err) => failures.push(format!("{proto} {port}: {err}")),
        }
    }
    if !failures.is_empty() {
        tracing::warn!(
            port = tcp_port,
            error = %failures.join("; "),
            "NAT port mapping failed; forward this port on the router if incoming peers are needed"
        );
    }
    if mapped.is_empty() {
        return;
    }

    if let Some(addr) = external_addr(&mapped) {
        *external.lock().unwrap() = Some(addr);
        tracing::info!(%addr, "NAT port mapping installed");
        tokio::select! {
            _ = cancel.cancelled() => {
                delete_all(&mapper, &mapped).await;
                return;
            }
            open = probe(addr) => {
                *port_open.lock().unwrap() = Some(open);
            }
        }
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                delete_all(&mapper, &mapped).await;
                return;
            }
            _ = tokio::time::sleep(NAT_LEASE / 2) => {
                let mut renewed = Vec::with_capacity(mapped.len());
                for mapping in mapped {
                    match mapper.renew(&mapping, NAT_LEASE).await {
                        Ok(next) => renewed.push(next),
                        Err(err) => {
                            tracing::warn!(
                                protocol = %mapping.protocol,
                                port = mapping.internal_port,
                                error = %err,
                                "NAT port mapping renew failed"
                            );
                            renewed.push(mapping);
                        }
                    }
                }
                mapped = renewed;
                if let Some(addr) = external_addr(&mapped) {
                    *external.lock().unwrap() = Some(addr);
                }
            }
        }
    }
}

fn external_addr(mapped: &[Mapping]) -> Option<SocketAddr> {
    let tcp = mapped
        .iter()
        .find(|mapping| mapping.protocol == MapProto::Tcp);
    let mapping = tcp.or_else(|| mapped.first())?;
    Some(SocketAddr::new(mapping.external_ip?, mapping.external_port))
}

async fn probe(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(REACHABILITY_TIMEOUT, tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

async fn delete_all(mapper: &Arc<dyn PortMapper>, mapped: &[Mapping]) {
    let mut failed = false;
    let mut error = String::new();
    for mapping in mapped {
        if let Err(err) = mapper.delete(mapping).await {
            failed = true;
            if error.is_empty() {
                error = err.to_string();
            }
        }
    }
    if failed {
        tracing::warn!(error = %error, "NAT port mapping delete failed");
    }
}
