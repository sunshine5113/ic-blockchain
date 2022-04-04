use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdaptersConfig {
    pub bitcoin_mainnet_uds_path: Option<PathBuf>,
    pub bitcoin_testnet_uds_path: Option<PathBuf>,
    pub canister_http_uds_path: Option<PathBuf>,
}
