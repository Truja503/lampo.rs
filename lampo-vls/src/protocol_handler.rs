use lampo_common::vls::proxy::vls_protocol_client::{Error, Transport};
use lampo_common::vls::proxy::vls_protocol_signer::handler::Handler;
use lampo_common::vls::proxy::vls_protocol_signer::handler::{HandlerBuilder, RootHandler};
use lampo_common::vls::proxy::vls_protocol_signer::vls_protocol::{model::PubKey, msgs};
use lampo_common::vls::signer::bitcoin::{Address, Network};
use lampo_common::vls::signer::node::NodeServices;
use lampo_common::vls::signer::persist::DummyPersister;
use lampo_common::vls::signer::policy::simple_validator::make_simple_policy;
use lampo_common::vls::signer::policy::simple_validator::SimpleValidatorFactory;
use lampo_common::vls::signer::signer::ClockStartingTimeFactory;
use lampo_common::vls::signer::util::clock::StandardClock;

use std::sync::Arc;

#[allow(dead_code)]
/// The `LampoVLSInProcess` represents a VLS client with a Null Transport.
/// Designed to run VLS in-process, but still performs the VLS protocol, No Persistence.
pub struct LampoVLSInProcess {
    pub handler: RootHandler,
}

/// Describe method to handle messages using the VLS protocol for Singer and Channel API.
impl Transport for LampoVLSInProcess {
    /// Perform a call for the Signer Protocol API
    fn node_call(&self, msg: Vec<u8>) -> Result<Vec<u8>, Error> {
        // Deserialize the incoming message
        let message = msgs::from_vec(msg)?;
        // Handle the message using RootHandler
        let (result, _) = self.handler.handle(message).map_err(|_| Error::Transport)?;
        Ok(result.as_vec())
    }

    // Perform a call for the Channel Protocol API
    fn call(&self, db_id: u64, peer_id: PubKey, msg: Vec<u8>) -> Result<Vec<u8>, Error> {
        // Deserialize the incoming message
        let message = msgs::from_vec(msg)?;
        // Creating a ChannelHandler
        let handler = self.handler.for_new_client(0, peer_id, db_id);
        // Handle the message using ChannelHandler
        let (result, _) = handler.handle(message).map_err(|_| Error::Transport)?;
        Ok(result.as_vec())
    }
}

#[allow(dead_code)]
impl LampoVLSInProcess {
    // Initialize the ProtocolHandler with Default Configuration, No Persistence
    pub fn new(address: Address, network: Network, seed: [u8; 32]) -> Self {
        // Create a dummy persister (no persistence)
        let persister = Arc::new(DummyPersister);
        // Define an allowlist with the given address
        let allowlist = vec![address.to_string()];
        // Create a simple policy for the network
        let policy = make_simple_policy(network);
        // Create Validators using SimpleValidatorFactory with the policy
        let validator_factory = Arc::new(SimpleValidatorFactory::new_with_policy(policy));
        let starting_time_factory = ClockStartingTimeFactory::new();
        let clock = Arc::new(StandardClock());
        let services = NodeServices {
            validator_factory,
            starting_time_factory,
            persister,
            clock,
            trusted_oracle_pubkeys: vec![],
        };
        let (root_handler_builder, _) = HandlerBuilder::new(network, 0, services, seed)
            .allowlist(allowlist)
            .build()
            .expect("Cannot Build The Root Handler");
        let root_handler = root_handler_builder.root_handler();
        LampoVLSInProcess {
            handler: root_handler,
        }
    }
}
