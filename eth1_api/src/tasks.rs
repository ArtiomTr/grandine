use core::time::Duration;
use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use dedicated_executor::DedicatedExecutor;
use logging::{debug_with_peers, info_with_peers, warn_with_peers};
use web3::{
    Error, Transport as _,
    api::{Eth, Namespace as _},
    helpers::CallFuture,
    transports::Http,
};

use crate::{
    ClientVersionV1, Eth1Api,
    endpoints::Endpoint,
    eth1_api::{
        CAPABILITIES, ENGINE_GET_CLIENT_VERSION_V1, ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1,
        ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1,
    },
};

const ENGINE_EXCHANGE_CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(1);
const ENGINE_GET_CLIENT_VERSION_V1_TIMEOUT: Duration = Duration::from_secs(1);

/// Exchanges engine API capabilities and client versions with the execution client.
///
/// Blocks are served with their payloads reconstructed from the execution client when payload
/// storage is disabled, so an execution client that does not implement the payload bodies methods
/// makes historical blocks unservable. Pass `payload_reconstruction_required` to warn about it as
/// soon as capabilities are known.
pub fn spawn_exchange_capabilities_and_versions_task(
    eth1_api: Arc<Eth1Api>,
    dedicated_executor: &DedicatedExecutor,
    payload_reconstruction_required: bool,
) {
    dedicated_executor
        .spawn(async move {
            if let Err(error) =
                exchange_capabilities_and_versions(&eth1_api, payload_reconstruction_required).await
            {
                warn_with_peers!("failed to exchange capabilities and client versions: {error:?}");
            }
        })
        .detach();
}

async fn exchange_capabilities_and_versions(
    eth1_api: &Eth1Api,
    payload_reconstruction_required: bool,
) -> Result<()> {
    #[cfg(feature = "embed")]
    {
        let response = eth1_api
            .exec(|adapter| adapter.engine_exchange_capabilities(CAPABILITIES))
            .await;

        match response {
            Ok(capabilities) => {
                let capabilities = HashSet::from_iter(capabilities);

                let supports_client_version =
                    capabilities.contains(&ENGINE_GET_CLIENT_VERSION_V1.to_owned());

                let supports_payload_bodies = capabilities
                    .contains(ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1)
                    && capabilities.contains(ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1);

                eth1_api.set_capabilities(capabilities);

                info_with_peers!("updated capabilities for embedded EL");

                if payload_reconstruction_required && !supports_payload_bodies {
                    warn_with_peers!(
                        "embedded EL does not support {ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1} \
                        and {ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1}; blocks cannot be served \
                        without --store-payloads",
                    );
                }

                if supports_client_version {
                    exchange_client_versions(eth1_api).await?;
                } else {
                    debug_with_peers!(
                        "cannot get client version: embedded EL does not support \
                        {ENGINE_GET_CLIENT_VERSION_V1}",
                    );
                }
            }
            Err(error) => {
                warn_with_peers!("unable to update capabilities for embedded el: {error:?}",);
            }
        }

        Ok(())
    }

    #[cfg(not(feature = "embed"))]
    {
        let params = vec![serde_json::to_value(CAPABILITIES)?];
        let method = "engine_exchangeCapabilities";

        for endpoint in eth1_api.endpoints.endpoints_for_request(None) {
            let _timer = eth1_api.metrics.as_ref().map(|metrics| {
                prometheus_metrics::start_timer_vec(&metrics.eth1_api_request_times, method)
            });

            let api = eth1_api.build_api_for_request(endpoint);

            let response: Result<HashSet<String>, Error> =
                CallFuture::new(api.transport().execute_with_headers(
                    method,
                    params.clone(),
                    eth1_api.auth.headers()?,
                    Some(ENGINE_EXCHANGE_CAPABILITIES_TIMEOUT),
                ))
                .await;

            match response {
                Ok(capabilities) => {
                    let supports_client_version =
                        capabilities.contains(ENGINE_GET_CLIENT_VERSION_V1);

                    let supports_payload_bodies = capabilities
                        .contains(ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1)
                        && capabilities.contains(ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1);

                    eth1_api.on_ok_response(endpoint);
                    endpoint.set_capabilities(capabilities);

                    info_with_peers!("updated capabilities for eth1 endpoint: {}", endpoint.url());

                    if payload_reconstruction_required && !supports_payload_bodies {
                        warn_with_peers!(
                            "eth1 endpoint {} does not support \
                            {ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1} and \
                            {ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1}; blocks cannot be served \
                            without --store-payloads",
                            endpoint.url(),
                        );
                    }

                    if supports_client_version {
                        exchange_client_versions(eth1_api, &api, endpoint).await?;
                    } else {
                        debug_with_peers!(
                            "cannot get client version: {} does not support \
                        {ENGINE_GET_CLIENT_VERSION_V1}",
                            endpoint.url(),
                        );
                    }
                }
                Err(error) => {
                    eth1_api.on_error_response(endpoint);

                    warn_with_peers!(
                        "unable to update capabilities for eth1 endpoint: {} {error:?}",
                        endpoint.url(),
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(feature = "embed")]
async fn exchange_client_versions(eth1_api: &Eth1Api) -> Result<()> {
    let response = eth1_api
        .exec(|adapter| adapter.engine_get_client_version_v1(ClientVersionV1::own()))
        .await;

    match response {
        Ok(client_versions) => {
            eth1_api.set_client_versions(client_versions);

            info_with_peers!("updated client version for embedded EL",);
        }
        Err(error) => {
            warn_with_peers!("unable to update client version for embedded EL: {error:?}",);
        }
    }

    Ok(())
}

#[cfg(not(feature = "embed"))]
async fn exchange_client_versions(
    eth1_api: &Eth1Api,
    api: &Eth<Http>,
    endpoint: &Endpoint,
) -> Result<()> {
    let response = CallFuture::new(api.transport().execute_with_headers(
        ENGINE_GET_CLIENT_VERSION_V1,
        vec![serde_json::to_value(ClientVersionV1::own())?],
        eth1_api.auth.headers()?,
        Some(ENGINE_GET_CLIENT_VERSION_V1_TIMEOUT),
    ))
    .await;

    match response {
        Ok(client_versions) => {
            eth1_api.on_ok_response(endpoint);
            endpoint.set_client_versions(client_versions);

            info_with_peers!(
                "updated client version for eth1 endpoint: {}",
                endpoint.url()
            );
        }
        Err(error) => {
            eth1_api.on_error_response(endpoint);

            warn_with_peers!(
                "unable to update client version for eth1 endpoint: {} {error:?}",
                endpoint.url(),
            );
        }
    }

    Ok(())
}
