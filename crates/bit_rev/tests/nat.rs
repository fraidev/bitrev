use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bit_rev::dht::DhtOptions;
use bit_rev::nat::{
    MapBackend, MapProto, Mapping, NatError, NatOptions, NatProtocol, PortMapper, NAT_LEASE,
};
use bit_rev::session::{Session, SessionOptions};
use bit_rev::utp::UtpOptions;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::Layer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Add(MapProto, u16),
    Renew(MapProto, u16),
    Delete(MapProto, u16),
}

struct Mock {
    log: Arc<Mutex<Vec<Call>>>,
    fail: bool,
    external_ip: Option<IpAddr>,
}

#[async_trait]
impl PortMapper for Mock {
    async fn add(
        &self,
        proto: MapProto,
        internal_port: u16,
        lease: Duration,
    ) -> Result<Mapping, NatError> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Add(proto, internal_port));
        if self.fail {
            return Err(NatError::Message("gateway refused".into()));
        }
        Ok(mapping(proto, internal_port, lease, self.external_ip))
    }

    async fn renew(&self, mapping: &Mapping, lease: Duration) -> Result<Mapping, NatError> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Renew(mapping.protocol, mapping.internal_port));
        if self.fail {
            return Err(NatError::Message("gateway refused".into()));
        }
        Ok(Mapping {
            lifetime: lease,
            ..mapping.clone()
        })
    }

    async fn delete(&self, mapping: &Mapping) -> Result<(), NatError> {
        self.log
            .lock()
            .unwrap()
            .push(Call::Delete(mapping.protocol, mapping.internal_port));
        if self.fail {
            return Err(NatError::Message("gateway refused".into()));
        }
        Ok(())
    }
}

fn mapping(proto: MapProto, port: u16, lease: Duration, external_ip: Option<IpAddr>) -> Mapping {
    Mapping {
        protocol: proto,
        internal_port: port,
        external_port: port,
        external_ip,
        lifetime: lease,
        backend: MapBackend::NatPmp,
    }
}

fn options(enabled: bool, utp: bool, dht: bool) -> SessionOptions {
    SessionOptions {
        listen_port: 0,
        state_dir: None,
        lpd: false,
        pex: false,
        utp: UtpOptions {
            enabled: utp,
            port: 0,
        },
        dht: DhtOptions {
            enabled: dht,
            port: 0,
            bootstrap_nodes: Vec::new(),
        },
        nat: NatOptions {
            enabled,
            protocol: NatProtocol::Auto,
        },
        ..SessionOptions::default()
    }
}

fn mock(log: Arc<Mutex<Vec<Call>>>, fail: bool, external_ip: Option<IpAddr>) -> Arc<Mock> {
    Arc::new(Mock {
        log,
        fail,
        external_ip,
    })
}

async fn wait_until(pred: impl Fn() -> bool) {
    for _ in 0..100 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for NAT task");
}

fn adds(log: &[Call]) -> Vec<(MapProto, u16)> {
    log.iter()
        .filter_map(|call| match call {
            Call::Add(proto, port) => Some((*proto, *port)),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn mock_gateway_maps_renews_at_half_lease_and_unmaps() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let session = Session::with_nat_mapper(
        options(true, false, false),
        Some(mock(log.clone(), false, None)),
    );
    let port = session.listen_port();
    wait_until(|| adds(&log.lock().unwrap()) == vec![(MapProto::Tcp, port)]).await;

    tokio::time::advance(NAT_LEASE / 2).await;
    wait_until(|| {
        log.lock()
            .unwrap()
            .iter()
            .any(|call| matches!(call, Call::Renew(MapProto::Tcp, got) if *got == port))
    })
    .await;
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|call| matches!(call, Call::Renew(_, _)))
            .count(),
        1
    );

    session.shutdown_graceful().await;
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|call| matches!(call, Call::Delete(MapProto::Tcp, got) if *got == port)));
}

#[tokio::test]
async fn disabled_nat_makes_no_mapper_calls() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let session = Session::with_nat_mapper(
        options(false, false, false),
        Some(mock(log.clone(), false, None)),
    );
    session.wait_listening().await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(log.lock().unwrap().is_empty());
    session.shutdown_graceful().await;
    assert!(log.lock().unwrap().is_empty());
}

struct WarnCount(Arc<AtomicUsize>);

struct MessageVisitor<'a>(&'a mut String);

impl Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?}"));
        }
    }
}

impl<S: Subscriber> Layer<S> for WarnCount {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::WARN {
            return;
        }
        let mut message = String::new();
        event.record(&mut MessageVisitor(&mut message));
        if message.contains("NAT port mapping failed") {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[tokio::test]
async fn failing_mapper_leaves_the_session_up_and_warns_once() {
    let warns = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(WarnCount(warns.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);
    let log = Arc::new(Mutex::new(Vec::new()));
    let session = Session::open_with_mapper(
        options(true, false, false),
        Some(mock(log.clone(), true, None)),
    )
    .await
    .expect("mapper errors must not fail Session::open");
    let addr = session.wait_listening().await;
    wait_until(|| !log.lock().unwrap().is_empty()).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(warns.load(Ordering::SeqCst), 1);
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(session.external_address().is_none());
    assert!(session.port_open().is_none());
    assert_eq!(session.listen_port(), addr.port());
    session.shutdown_graceful().await;
}

#[tokio::test]
async fn udp_ports_are_mapped_only_when_enabled() {
    let tcp_only = mapped(false, false).await;
    assert!(tcp_only.iter().all(|(proto, _)| *proto == MapProto::Tcp));
    assert_eq!(tcp_only.len(), 1);

    let utp_only = mapped(true, false).await;
    assert_eq!(
        utp_only
            .iter()
            .filter(|(proto, _)| *proto == MapProto::Udp)
            .count(),
        1
    );

    let dht_only = mapped(false, true).await;
    assert_eq!(
        dht_only
            .iter()
            .filter(|(proto, _)| *proto == MapProto::Udp)
            .count(),
        1
    );

    let shared = mapped(true, true).await;
    assert_eq!(
        shared
            .iter()
            .filter(|(proto, _)| *proto == MapProto::Udp)
            .count(),
        1
    );
    let tcp = shared
        .iter()
        .find(|(proto, _)| *proto == MapProto::Tcp)
        .unwrap()
        .1;
    let udp = shared
        .iter()
        .find(|(proto, _)| *proto == MapProto::Udp)
        .unwrap()
        .1;
    assert_eq!(tcp, udp);
}

async fn mapped(utp: bool, dht: bool) -> Vec<(MapProto, u16)> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let session = Session::with_nat_mapper(
        options(true, utp, dht),
        Some(mock(log.clone(), false, None)),
    );
    let expect = 1 + usize::from(utp || dht);
    wait_until(|| adds(&log.lock().unwrap()).len() == expect).await;
    let got = adds(&log.lock().unwrap());
    session.shutdown_graceful().await;
    got
}
