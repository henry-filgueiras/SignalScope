//! DNS latency sensor.
//!
//! Periodically resolves a small rotating set of canary names against the
//! system resolver (or a caller-supplied resolver) and emits a
//! [`DnsLatencyObservation`] for each. The intent isn't load testing — it's
//! detecting *DNS pathology*: resolvers that drop queries, fall back slowly
//! between upstreams, or spike under congestion.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_resolver::{config::ResolverConfig, config::ResolverOpts, TokioAsyncResolver};
use signalscope_core::EventBus;
use signalscope_events::{DnsLatencyObservation, Event, SensorId};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::Sensor;

#[derive(Debug, Clone)]
pub struct DnsSensorConfig {
    pub interval: Duration,
    pub queries: Vec<String>,
    /// Override the resolver. `None` uses the system resolver.
    pub resolver: Option<DnsResolverChoice>,
    pub query_timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
pub enum DnsResolverChoice {
    /// 1.1.1.1 / 1.0.0.1
    Cloudflare,
    /// 8.8.8.8 / 8.8.4.4
    Google,
    /// 9.9.9.9
    Quad9,
}

impl Default for DnsSensorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3),
            queries: vec![
                "cloudflare.com.".into(),
                "apple.com.".into(),
                "wikipedia.org.".into(),
            ],
            resolver: None,
            query_timeout: Duration::from_millis(1500),
        }
    }
}

#[derive(Debug)]
pub struct DnsSensor {
    cfg: DnsSensorConfig,
}

impl DnsSensor {
    pub fn new(cfg: DnsSensorConfig) -> Self {
        Self { cfg }
    }
}

impl Sensor for DnsSensor {
    fn id(&self) -> SensorId {
        SensorId::new("dns")
    }

    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()> {
        let cfg = self.cfg;
        let id = self.id();
        tokio::spawn(async move { run(id, cfg, bus).await })
    }
}

async fn run(id: SensorId, cfg: DnsSensorConfig, bus: Arc<EventBus>) {
    use tokio::time::{interval, MissedTickBehavior};

    let resolver = match build_resolver(&cfg) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "could not build DNS resolver; DNS sensor disabled");
            return;
        }
    };
    let resolver_label = resolver_label(&cfg.resolver);

    let mut idx = 0usize;
    let mut tick = interval(cfg.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        let query = cfg.queries[idx % cfg.queries.len()].clone();
        idx = idx.wrapping_add(1);

        let started = Instant::now();
        let probe = tokio::time::timeout(cfg.query_timeout, resolver.lookup_ip(query.clone())).await;
        let rtt = started.elapsed();

        let obs = match probe {
            Ok(Ok(_)) => DnsLatencyObservation {
                resolver: resolver_label.clone(),
                query,
                rtt,
                answered: true,
                error: None,
            },
            Ok(Err(e)) => DnsLatencyObservation {
                resolver: resolver_label.clone(),
                query,
                rtt,
                answered: false,
                error: Some(e.to_string()),
            },
            Err(_) => DnsLatencyObservation {
                resolver: resolver_label.clone(),
                query,
                rtt,
                answered: false,
                error: Some("timeout".into()),
            },
        };

        bus.publish(id.clone(), Event::DnsLatency(obs));
    }
}

fn build_resolver(cfg: &DnsSensorConfig) -> anyhow::Result<TokioAsyncResolver> {
    let mut opts = ResolverOpts::default();
    opts.timeout = cfg.query_timeout;
    opts.attempts = 1;
    opts.cache_size = 0;

    let cfg = match cfg.resolver {
        Some(DnsResolverChoice::Cloudflare) => ResolverConfig::cloudflare(),
        Some(DnsResolverChoice::Google) => ResolverConfig::google(),
        Some(DnsResolverChoice::Quad9) => ResolverConfig::quad9(),
        None => {
            // System resolver may be unavailable in sandboxes; fall back to
            // Cloudflare in that case so the sensor still produces signal.
            match hickory_resolver::system_conf::read_system_conf() {
                Ok((sys_cfg, sys_opts)) => {
                    return Ok(TokioAsyncResolver::tokio(sys_cfg, sys_opts));
                }
                Err(_) => ResolverConfig::cloudflare(),
            }
        }
    };

    Ok(TokioAsyncResolver::tokio(cfg, opts))
}

fn resolver_label(choice: &Option<DnsResolverChoice>) -> String {
    match choice {
        Some(DnsResolverChoice::Cloudflare) => "1.1.1.1".into(),
        Some(DnsResolverChoice::Google) => "8.8.8.8".into(),
        Some(DnsResolverChoice::Quad9) => "9.9.9.9".into(),
        None => "system".into(),
    }
}
