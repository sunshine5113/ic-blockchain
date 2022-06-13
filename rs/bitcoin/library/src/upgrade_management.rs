use crate::{BitcoinAgent, BitcoinCanister};
use bitcoin::{Address, Network};
use ic_btc_library_types::{AddressUsingPrimitives, BitcoinAgentState, EcdsaPubKey, UtxosState};
use std::collections::HashMap;
use std::str::FromStr;

/// Returns the Bitcoin agent state.
pub(crate) fn get_state<C: BitcoinCanister>(bitcoin_agent: &BitcoinAgent<C>) -> BitcoinAgentState {
    let ecdsa_pub_key_addresses: HashMap<AddressUsingPrimitives, EcdsaPubKey> = bitcoin_agent
        .ecdsa_pub_key_addresses
        .iter()
        .map(|(address, ecdsa_pub_key)| {
            (get_address_using_primitives(address), ecdsa_pub_key.clone())
        })
        .collect();

    let utxos_state_addresses: HashMap<AddressUsingPrimitives, UtxosState> = bitcoin_agent
        .utxos_state_addresses
        .iter()
        .map(|(address, utxos_state)| (get_address_using_primitives(address), utxos_state.clone()))
        .collect();

    BitcoinAgentState {
        network: ic_btc_library_types::Network::Regtest,
        main_address_type: bitcoin_agent.main_address_type,
        ecdsa_pub_key_addresses,
        utxos_state_addresses,
    }
}

/// Returns the associated Bitcoin agent with the given `bitcoin_agent_state`.
pub(crate) fn from_state<C: BitcoinCanister>(
    bitcoin_agent_state: BitcoinAgentState,
) -> BitcoinAgent<C> {
    // TODO(ER-2726): Add guards for Bitcoin concurrent access.
    let ecdsa_pub_key_addresses: HashMap<Address, EcdsaPubKey> = bitcoin_agent_state
        .ecdsa_pub_key_addresses
        .into_iter()
        .map(|(address_using_primitives, ecdsa_pub_key)| {
            (get_address(address_using_primitives), ecdsa_pub_key)
        })
        .collect();

    let utxos_state_addresses: HashMap<Address, UtxosState> = bitcoin_agent_state
        .utxos_state_addresses
        .into_iter()
        .map(|(address_using_primitives, utxos_state)| {
            (get_address(address_using_primitives), utxos_state)
        })
        .collect();

    let bitcoin_canister = C::new(ic_btc_library_types::Network::Regtest);
    BitcoinAgent {
        bitcoin_canister,
        main_address_type: bitcoin_agent_state.main_address_type,
        ecdsa_pub_key_addresses,
        utxos_state_addresses,
    }
}

/// Returns the `AddressUsingPrimitives` associated with a given `bitcoin::Address`.
fn get_address_using_primitives(address: &Address) -> AddressUsingPrimitives {
    (address.to_string(), ic_btc_library_types::Network::Regtest)
}

/// Returns the `bitcoin::Address` associated with a given `AddressUsingPrimitives`.
fn get_address((address_string, _address_network): AddressUsingPrimitives) -> Address {
    let mut address = Address::from_str(&address_string).unwrap();
    address.network = Network::Regtest;
    address
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::agent;
    use crate::canister_mock::BitcoinCanisterMock;
    use ic_btc_library_types::AddressType;

    /// Check that `get_state` and `from_state` return respectively the Bitcoin agent state and the Bitcoin agent associated with the former Bitcoin agent state.
    #[test]
    fn check_upgrade() {
        // Every field of the `BitcoinAgentState` is filled with non-default value during the `BitcoinAgent` instantiation.
        let pre_upgrade_bitcoin_agent = agent::new_mock(&Network::Regtest, &AddressType::P2pkh);

        let pre_upgrade_state = pre_upgrade_bitcoin_agent.get_state();
        let post_upgrade_bitcoin_agent: BitcoinAgent<BitcoinCanisterMock> =
            BitcoinAgent::from_state(pre_upgrade_state.clone());

        assert_eq!(post_upgrade_bitcoin_agent.get_state(), pre_upgrade_state)
    }
}
