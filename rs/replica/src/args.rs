use clap::Parser;
use ic_config::ConfigSource;
use ic_types::ReplicaVersion;
use std::convert::TryFrom;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[clap(
    name = "replica",
    about = "Arguments for the Internet Computer Replica.",
    version
)]
pub struct ReplicaArgs {
    /// Print a sample config if set
    #[clap(long)]
    pub print_sample_config: bool,

    /// The path to the Replica config file
    #[clap(long, parse(from_os_str))]
    pub config_file: Option<PathBuf>,

    /// A string representation of the Replica config
    #[clap(long)]
    pub config_literal: Option<String>,

    /// A path to a CBOR-encoded catch-up package to seed the Replica with
    #[clap(long, parse(from_os_str))]
    pub catch_up_package: Option<PathBuf>,

    /// The version of the Replica being run
    #[clap(long, parse(try_from_str = ReplicaVersion::try_from))]
    pub replica_version: ReplicaVersion,

    /// A path to the PEM file with the public key of the NNS subnet.
    /// It's used to certify responses of the registry canister.
    #[clap(long, parse(from_os_str))]
    pub nns_public_key_file: Option<PathBuf>,

    /// Force to use the given subnet ID. This is needed to upgrade NNS
    /// replicas. In that case, we already know which subnet ID we should be
    /// booting with, and trying to determine it from the registry will fail
    /// Example SubnetID: ak2jc-de3ae-aaaaa-aaaap-yai
    #[clap(long)]
    pub force_subnet: Option<String>,
}

impl From<&ReplicaArgs> for ConfigSource {
    fn from(args: &ReplicaArgs) -> ConfigSource {
        if let Some(path) = &args.config_file {
            ConfigSource::File(path.clone())
        } else if let Some(literal) = &args.config_literal {
            ConfigSource::Literal(literal.clone())
        } else {
            ConfigSource::Default
        }
    }
}
