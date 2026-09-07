//! External-address cache ownership. Media reads only immutable snapshots.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Notify, watch};

use crate::config::ExternalAddress;
use crate::media::addressing::{
    ExternalAddressCache, ExternalResolutionError, HostResolver, ResolvedExternalAddresses,
};

#[derive(Clone)]
struct ResolutionGeneration(Arc<()>);

impl ResolutionGeneration {
    fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for ResolutionGeneration {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ResolutionGeneration {}

#[derive(Eq, PartialEq)]
enum ResolutionSource {
    Disabled,
    Address(std::net::IpAddr),
    Hostname(String),
}

impl ResolutionSource {
    fn from_policy(external: Option<&ExternalAddress>) -> Self {
        match external {
            None => Self::Disabled,
            Some(ExternalAddress::Address(address)) => Self::Address(*address),
            Some(ExternalAddress::Hostname { name, .. }) => Self::Hostname(name.clone()),
        }
    }
}

#[derive(Clone)]
struct Configuration {
    external: Option<ExternalAddress>,
    generation: ResolutionGeneration,
    // A new identity invalidates cache entries even if coalesced configuration
    // updates return to a previously used hostname. Lifetime-only edits retain it.
    source: Arc<ResolutionSource>,
    fallback: ResolvedExternalAddresses,
    closing: bool,
}

#[derive(Clone)]
struct Snapshot {
    addresses: ResolvedExternalAddresses,
    generation: ResolutionGeneration,
    refresh_at: Option<Instant>,
}

impl Configuration {
    fn visible_addresses(&self, snapshot: &Snapshot) -> ResolvedExternalAddresses {
        match &self.external {
            None => ResolvedExternalAddresses::default(),
            Some(ExternalAddress::Address(address)) => {
                ResolvedExternalAddresses::from_address(*address)
            }
            Some(ExternalAddress::Hostname { .. }) => {
                if self.generation == snapshot.generation {
                    snapshot.addresses
                } else {
                    self.fallback
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("external-address worker failed")]
pub(crate) struct ResolutionWorkerError {
    #[source]
    cause: tokio::task::JoinError,
}

pub(crate) struct ExternalAddressHandle {
    configuration: watch::Sender<Configuration>,
    snapshot: watch::Receiver<Snapshot>,
    demand: Arc<Notify>,
}

impl ExternalAddressHandle {
    pub fn configure(&self, external: Option<ExternalAddress>) {
        self.configuration.send_if_modified(|config| {
            if config.closing || config.external == external {
                return false;
            }
            let fallback = config.visible_addresses(&self.snapshot.borrow());
            let source = ResolutionSource::from_policy(external.as_ref());
            if *config.source != source {
                config.source = Arc::new(source);
            }
            config.external = external;
            config.fallback = fallback;
            config.generation = ResolutionGeneration::new();
            true
        });
    }

    pub fn current(&self) -> ResolvedExternalAddresses {
        let configuration = self.configuration.borrow();
        let snapshot = self.snapshot.borrow();
        if !configuration.closing
            && matches!(
                configuration.external,
                Some(ExternalAddress::Hostname { .. })
            )
            && (snapshot.generation != configuration.generation
                || snapshot
                    .refresh_at
                    .is_none_or(|deadline| Instant::now() >= deadline))
        {
            self.demand.notify_one();
        }
        configuration.visible_addresses(&snapshot)
    }

    pub fn close(&self) {
        self.configuration
            .send_modify(|config| config.closing = true);
    }
}

pub(crate) struct ExternalAddressOwner<R> {
    cache: ExternalAddressCache<R>,
    cache_source: Arc<ResolutionSource>,
    configuration: watch::Receiver<Configuration>,
    snapshot: watch::Sender<Snapshot>,
    demand: Arc<Notify>,
    report: fn(&ExternalResolutionError),
}

impl<R: HostResolver + Send + 'static> ExternalAddressOwner<R> {
    pub fn new(
        cache: ExternalAddressCache<R>,
        external: Option<ExternalAddress>,
        report: fn(&ExternalResolutionError),
    ) -> (ExternalAddressHandle, Self) {
        let generation = ResolutionGeneration::new();
        let source = Arc::new(ResolutionSource::from_policy(external.as_ref()));
        let (configuration_tx, configuration) = watch::channel(Configuration {
            external,
            source: Arc::clone(&source),
            generation: generation.clone(),
            fallback: cache.current(),
            closing: false,
        });
        let (snapshot, snapshot_rx) = watch::channel(Snapshot {
            addresses: cache.current(),
            generation,
            refresh_at: cache.refresh_deadline(),
        });
        let demand = Arc::new(Notify::new());
        (
            ExternalAddressHandle {
                configuration: configuration_tx,
                snapshot: snapshot_rx,
                demand: Arc::clone(&demand),
            },
            Self {
                cache,
                cache_source: source,
                configuration,
                snapshot,
                demand,
                report,
            },
        )
    }

    pub async fn run(self) -> Result<(), ResolutionWorkerError> {
        let Self {
            mut cache,
            mut cache_source,
            mut configuration,
            snapshot,
            demand,
            report,
        } = self;
        loop {
            tokio::select! {
                result = configuration.changed() => { if result.is_err() { return Ok(()); } }
                _ = demand.notified() => {}
            }
            let requested = configuration.borrow_and_update().clone();
            if requested.closing {
                return Ok(());
            }
            let generation = requested.generation.clone();
            let source = Arc::clone(&requested.source);
            if !Arc::ptr_eq(&cache_source, &source) {
                cache.reset_to(requested.fallback);
                cache_source = Arc::clone(&source);
            }
            let checkpoint = cache.checkpoint();
            // Move the cache into the worker and get it back. No lock spans DNS,
            // and shutdown joins the worker before retiring this owner.
            let result = tokio::task::spawn_blocking(move || {
                let result = cache.refresh(requested.external.as_ref(), Instant::now());
                if let Err(error) = &result {
                    report(error);
                }
                (cache, result)
            })
            .await
            .map_err(|cause| ResolutionWorkerError { cause })?;
            let (returned, result) = result;
            cache = returned;
            let succeeded = result.is_ok();
            let current = configuration.borrow();
            if current.closing {
                return Ok(());
            }
            if current.generation == generation {
                snapshot.send_replace(Snapshot {
                    addresses: cache.current(),
                    generation,
                    refresh_at: if succeeded {
                        cache.refresh_deadline()
                    } else {
                        None
                    },
                });
            } else {
                cache.restore(checkpoint);
            }
            // A configuration change during lookup remains unseen by changed()
            // and is processed next. An older completion never overwrites it.
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::IpAddr;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    struct Resolver {
        gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        entered: tokio::sync::mpsc::UnboundedSender<String>,
    }

    impl HostResolver for Resolver {
        fn resolve(&self, hostname: &str) -> io::Result<Vec<IpAddr>> {
            self.entered.send(hostname.to_owned()).unwrap();
            if hostname == "slow.example"
                && let Some(gate) = self.gate.lock().unwrap().take()
            {
                let _ = gate.blocking_recv();
            }
            if hostname == "failed.example" {
                return Err(io::Error::other("fake DNS failure"));
            }
            Ok(vec!["203.0.113.10".parse().unwrap()])
        }
    }

    fn hostname(name: &str) -> Option<ExternalAddress> {
        Some(ExternalAddress::Hostname {
            name: name.into(),
            refresh_seconds: 60,
        })
    }

    #[tokio::test]
    async fn stalled_lookup_does_not_block_reads_reload_or_shutdown_admission() {
        let (release, gate) = tokio::sync::oneshot::channel();
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let cache = ExternalAddressCache::new(Resolver {
            gate: Mutex::new(Some(gate)),
            entered,
        });
        let (handle, owner) = ExternalAddressOwner::new(cache, hostname("slow.example"), |_| {});
        let task = tokio::spawn(owner.run());
        assert_eq!(handle.current(), ResolvedExternalAddresses::default());
        assert_eq!(entered_rx.recv().await.as_deref(), Some("slow.example"));
        for _ in 0..1000 {
            assert_eq!(handle.current(), ResolvedExternalAddresses::default());
        }
        let address = "198.51.100.9".parse().unwrap();
        handle.configure(Some(ExternalAddress::Address(address)));
        assert_eq!(
            handle.current(),
            ResolvedExternalAddresses::from_address(address)
        );
        handle.configure(None);
        assert_eq!(handle.current(), ResolvedExternalAddresses::default());
        handle.close();
        assert!(!task.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            handle.snapshot.borrow().addresses,
            ResolvedExternalAddresses::default()
        );
    }

    #[tokio::test]
    async fn failed_refresh_preserves_last_good_addresses() {
        let gate = Mutex::new(None);
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cache = ExternalAddressCache::new(Resolver { gate, entered });
        cache
            .refresh(hostname("good.example").as_ref(), Instant::now())
            .unwrap();
        entered_rx.recv().await.unwrap();
        let (handle, owner) = ExternalAddressOwner::new(cache, hostname("good.example"), |_| {});
        let good = handle.current();
        let task = tokio::spawn(owner.run());
        handle.configure(hostname("failed.example"));
        assert_eq!(entered_rx.recv().await.as_deref(), Some("failed.example"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.snapshot.borrow().generation != handle.configuration.borrow().generation {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.current(), good);
        handle.close();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stale_success_cannot_become_last_good_for_a_newer_failed_configuration() {
        let (release, gate) = tokio::sync::oneshot::channel();
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let cache = ExternalAddressCache::new(Resolver {
            gate: Mutex::new(Some(gate)),
            entered,
        });
        let (handle, owner) = ExternalAddressOwner::new(cache, hostname("slow.example"), |_| {});
        let task = tokio::spawn(owner.run());
        handle.current();
        assert_eq!(entered_rx.recv().await.as_deref(), Some("slow.example"));
        handle.configure(hostname("failed.example"));
        release.send(()).unwrap();
        assert_eq!(entered_rx.recv().await.as_deref(), Some("failed.example"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.snapshot.borrow().generation != handle.configuration.borrow().generation {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.current(), ResolvedExternalAddresses::default());
        assert!(handle.snapshot.borrow().refresh_at.is_none());
        handle.close();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn coalesced_reload_preserves_disabled_and_static_fallback_decisions() {
        for intermediate in [
            None,
            Some(ExternalAddress::Address("198.51.100.9".parse().unwrap())),
        ] {
            let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut cache = ExternalAddressCache::new(Resolver {
                gate: Mutex::new(None),
                entered,
            });
            cache
                .refresh(hostname("good.example").as_ref(), Instant::now())
                .unwrap();
            entered_rx.recv().await.unwrap();
            let (handle, owner) =
                ExternalAddressOwner::new(cache, hostname("good.example"), |_| {});
            handle.configure(intermediate);
            let fallback = handle.current();
            handle.configure(hostname("failed.example"));
            assert_eq!(
                handle.current(),
                fallback,
                "reload must not expose an older hostname snapshot"
            );
            let task = tokio::spawn(owner.run());
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), entered_rx.recv())
                    .await
                    .unwrap()
                    .as_deref(),
                Some("failed.example")
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                while handle.snapshot.borrow().generation
                    != handle.configuration.borrow().generation
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                handle.current(),
                fallback,
                "a failed lookup must preserve the latest policy decision"
            );
            handle.close();
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn changing_only_refresh_lifetime_keeps_the_existing_cache_deadline() {
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cache = ExternalAddressCache::new(Resolver {
            gate: Mutex::new(None),
            entered,
        });
        cache
            .refresh(hostname("good.example").as_ref(), Instant::now())
            .unwrap();
        let deadline = cache.refresh_deadline();
        entered_rx.recv().await.unwrap();
        let (handle, owner) = ExternalAddressOwner::new(cache, hostname("good.example"), |_| {});
        handle.configure(Some(ExternalAddress::Hostname {
            name: "good.example".into(),
            refresh_seconds: 120,
        }));
        let task = tokio::spawn(owner.run());
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.snapshot.borrow().generation != handle.configuration.borrow().generation {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.snapshot.borrow().refresh_at, deadline);
        assert!(
            entered_rx.try_recv().is_err(),
            "an unexpired address must not be resolved again"
        );
        handle.close();
        task.await.unwrap().unwrap();
    }
}
