use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};
use tokio::{sync::watch, task::JoinHandle};
use tracing::{debug, warn};

use crate::{
    config::SharedSecretAuth, error::ZeroConfError, registry::Registry, types::AgentAnnouncement,
};

/// Background browser that turns mDNS browse events into registry updates.
#[derive(Clone)]
pub(crate) struct Listener {
    daemon: ServiceDaemon,
    service_type: String,
    local_agent_id: String,
    local_instance_name: String,
    shared_secret_auth: Option<SharedSecretAuth>,
}

impl Listener {
    pub(crate) fn new(
        daemon: ServiceDaemon,
        service_type: impl Into<String>,
        local_agent_id: impl Into<String>,
        local_instance_name: impl Into<String>,
        shared_secret_auth: Option<SharedSecretAuth>,
    ) -> Self {
        Self {
            daemon,
            service_type: service_type.into(),
            local_agent_id: local_agent_id.into(),
            local_instance_name: local_instance_name.into(),
            shared_secret_auth,
        }
    }

    pub(crate) fn spawn(
        self,
        registry: Registry,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<JoinHandle<()>, ZeroConfError> {
        let Self {
            daemon,
            service_type,
            local_agent_id,
            local_instance_name,
            shared_secret_auth,
        } = self;
        let receiver = daemon.browse(&service_type)?;

        Ok(tokio::spawn(async move {
            run_listener(
                receiver,
                registry,
                daemon,
                service_type,
                local_agent_id,
                local_instance_name,
                shared_secret_auth,
                &mut shutdown_rx,
            )
            .await;
        }))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "listener loop is wired from explicit runtime dependencies for clarity"
)]
async fn run_listener(
    receiver: Receiver<ServiceEvent>,
    registry: Registry,
    daemon: ServiceDaemon,
    service_type: String,
    local_agent_id: String,
    local_instance_name: String,
    shared_secret_auth: Option<SharedSecretAuth>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                let _ = daemon.stop_browse(&service_type);
                break;
            }
            event = receiver.recv_async() => {
                match event {
                    Ok(ServiceEvent::ServiceResolved(resolved)) => {
                        match AgentAnnouncement::from_resolved_service(&resolved) {
                            Ok(announcement) if announcement.agent_id() == local_agent_id => {}
                            Ok(announcement) => {
                                if let Some(auth) = shared_secret_auth.as_ref()
                                    && auth.verifies_incoming()
                                    && let Err(error) = announcement.verify_shared_secret_auth(auth)
                                {
                                    warn!(?error, fullname = resolved.get_fullname(), "failed to verify resolved service");
                                    continue;
                                }

                                let _ = registry.upsert_remote(announcement).await;
                            }
                            Err(error) => {
                                warn!(?error, fullname = resolved.get_fullname(), "failed to parse resolved service");
                            }
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                        if fullname != local_instance_name {
                            let _ = registry.remove_remote_by_instance_name(&fullname).await;
                        }
                    }
                    Ok(ServiceEvent::SearchStarted(ty_domain)) => {
                        debug!(%ty_domain, "started browsing service type");
                    }
                    Ok(ServiceEvent::SearchStopped(ty_domain)) => {
                        debug!(%ty_domain, "stopped browsing service type");
                        break;
                    }
                    Ok(ServiceEvent::ServiceFound(_, _)) => {}
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }
}
