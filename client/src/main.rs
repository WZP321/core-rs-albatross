#[macro_use]
extern crate log;

extern crate nimiq_lib as nimiq;


use std::convert::TryFrom;
use std::time::Duration;

use futures::{future, Future, Stream, IntoFuture};
use tokio;
use tokio::timer::Interval;

use nimiq::prelude::*;
use nimiq::extras::logging::{initialize_logging, log_error_cause_chain};
use nimiq::extras::deadlock::initialize_deadlock_detection;


fn main_inner() -> Result<(), Error> {
    initialize_logging()?;
    initialize_deadlock_detection();

    // Parse command line.
    let command_line = CommandLine::from_args();
    trace!("Command line: {:#?}", command_line);

    // Parse config file - this will obey the `--config` command line option.
    let config_file = ConfigFile::find(Some(&command_line))?;
    trace!("Config file: {:#?}", config_file);

    // Create config builder and apply command line and config file
    // You usually want the command line to override config settings, so the order is important
    let mut builder = ClientConfig::builder();
    builder.config_file(&config_file)?;
    builder.command_line(&command_line)?;

    // finalize config
    let config = builder.build()?;
    debug!("Final configuration: {:#?}", config);

    // We need to instantiate the client when the tokio runtime is already alive, so we use
    // a lazy future for it.
    tokio::run(
        // TODO: Return this from `Client::into_future()`
        future::lazy(move || {
            // TODO: This is the initialization future

            // Clone those now, because we pass ownership of config to Client
            let rpc_config = config.rpc_server.clone();
            let metrics_config = config.metrics_server.clone();
            //let ws_rpc_config = config.ws_rpc_server.clone();

            // Create client from config
            info!("Initializing client");
            let client: Client = Client::try_from(config)?;
            client.initialize()?;

            // Initialize RPC server
            if let Some(rpc_config) = rpc_config {
                use nimiq::extras::rpc_server::initialize_rpc_server;
                let rpc_server = initialize_rpc_server(&client, rpc_config)
                    .expect("Failed to initialize RPC server");
                tokio::spawn(rpc_server.into_future());
            }

            // Initialize metrics server
            if let Some(metrics_config) = metrics_config {
                use nimiq::extras::metrics_server::initialize_metrics_server;
                initialize_metrics_server(&client, metrics_config);
            }

            // Initialize network stack and connect
            info!("Connecting to network");

            client.connect()?;

            // The Nimiq client is now running and we can access it trough the `client` object.

            // TODO: RPC server and metrics server need to be instantiated here
            Ok(client)
        })
            .and_then(|client| {
                // TODO: This is the "monitor" future, which keeps the Client object alive.
                //  This should be chosen by the consumer

                // Periodically show some info
                Interval::new_interval(Duration::from_secs(10))
                    .map_err(|e| panic!("Timer failed: {}", e))
                    .for_each(move |_| {
                        let peer_count = client.network().connections.peer_count();
                        let head = client.blockchain().head().clone();
                        info!("Head: #{} - {}, Peers: {}", head.block_number(), head.hash(), peer_count);

                        future::ok::<(), Error>(())
                    })
            })
            .map_err(|e: Error| warn!("{}", e)));

    Ok(())
}

fn main() {
    if let Err(e) = main_inner() {
        log_error_cause_chain(&e);
    }
}
