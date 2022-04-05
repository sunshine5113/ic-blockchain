use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpStream},
    path::PathBuf,
    process::Command,
};

use crate::driver::driver_setup::SSH_AUTHORIZED_PUB_KEYS_DIR;

use super::{
    driver_setup::{
        BOUNDARY_NODE_IMG_SHA256, BOUNDARY_NODE_IMG_URL, FARM_BASE_URL, FARM_GROUP_NAME,
        JOURNALBEAT_HOSTS, SSH_AUTHORIZED_PRIV_KEYS_DIR,
    },
    farm::{CreateVmRequest, Farm, ImageLocation, VMCreateResponse},
    ic::{AmountOfMemoryKiB, NrOfVCPUs, VmResources},
    resource::DiskImage,
    test_env::TestEnv,
    test_env_api::{retry, RETRY_BACKOFF, RETRY_TIMEOUT},
};
use anyhow::{bail, Result};
use flate2::{write::GzEncoder, Compression};
use reqwest::Url;
use slog::info;
use ssh2::Session;

const DEFAULT_VCPUS_PER_VM: NrOfVCPUs = NrOfVCPUs::new(4);
const DEFAULT_MEMORY_KIB_PER_VM: AmountOfMemoryKiB = AmountOfMemoryKiB::new(25165824); // 24GiB

const BOUNDARY_NODE_VMS_DIR: &str = "boundary_node_vms";
const BOUNDARY_NODE_VM_PATH: &str = "vm.json";
const CONF_IMG_FNAME: &str = "config_disk.img";

const ADMIN_USER: &str = "admin";

fn mk_compressed_img_path() -> std::string::String {
    return format!("{}.gz", CONF_IMG_FNAME);
}
/// A builder for the initial configuration of an IC boundary node.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BoundaryNode {
    pub name: String,
    pub vm_resources: VmResources,
    pub has_ipv4: bool,
    pub boot_image: Option<DiskImage>,
    pub nns_node_urls: Vec<Url>,
    pub nns_public_key: Option<PathBuf>,
}

impl BoundaryNode {
    pub fn new(name: String) -> Self {
        Self {
            name,
            vm_resources: Default::default(),
            has_ipv4: true,
            boot_image: Default::default(),
            nns_node_urls: Default::default(),
            nns_public_key: Default::default(),
        }
    }

    pub fn with_nns_urls(mut self, nns_node_urls: Vec<Url>) -> Self {
        self.nns_node_urls = nns_node_urls;
        self
    }

    pub fn with_nns_public_key(mut self, nns_public_key: PathBuf) -> Self {
        self.nns_public_key = Some(nns_public_key);
        self
    }

    pub fn start(&self, env: &TestEnv) -> Result<()> {
        let group_name: String = env.read_object(FARM_GROUP_NAME)?;
        let logger = env.logger();
        let farm = Farm::new(env.read_object(FARM_BASE_URL)?, logger.clone());

        let create_vm_req = CreateVmRequest::new(
            self.name.clone(),
            self.vm_resources.vcpus.unwrap_or(DEFAULT_VCPUS_PER_VM),
            self.vm_resources
                .memory_kibibytes
                .unwrap_or(DEFAULT_MEMORY_KIB_PER_VM),
            match &self.boot_image {
                None => {
                    let url = env.read_object(BOUNDARY_NODE_IMG_URL)?;
                    let sha256 = env.read_object(BOUNDARY_NODE_IMG_SHA256)?;
                    ImageLocation::IcOsImageViaUrl { url, sha256 }
                }
                Some(disk_image) => From::from(disk_image.clone()),
            },
            self.has_ipv4,
        );
        let vm = farm.create_vm(&group_name, create_vm_req)?;

        env.write_boundary_node_vm(&self.name, &vm)?;

        let image_id = create_and_upload_config_disk_image(self, env, &group_name, &farm)?;

        farm.attach_disk_image(&group_name, &self.name, "usb-storage", image_id)?;

        farm.start_vm(&group_name, &self.name)?;

        Ok(())
    }
}

/// side-effectful function that creates the config disk images
/// in the boundary node directories.
pub fn create_and_upload_config_disk_image(
    boundary_node: &BoundaryNode,
    env: &TestEnv,
    group_name: &str,
    farm: &Farm,
) -> anyhow::Result<String> {
    let boundary_node_dir = env
        .base_path()
        .join(BOUNDARY_NODE_VMS_DIR)
        .join(boundary_node.name.clone());
    let img_path = boundary_node_dir.join(CONF_IMG_FNAME);

    let ci_project_dir: PathBuf = PathBuf::from(env::var("IC_ROOT").expect(
        "Expected the IC_ROOT environment variable to be set to the root of the IC repository!",
    ));
    let mut cmd = Command::new(
        ci_project_dir.join("ic-os/boundary-guestos/scripts/build-bootstrap-config-image.sh"),
    );

    let ssh_authorized_pub_keys_dir: PathBuf = env.get_path(SSH_AUTHORIZED_PUB_KEYS_DIR);
    let journalbeat_hosts: Vec<String> = env.read_object(JOURNALBEAT_HOSTS)?;

    cmd.arg(img_path.clone())
        .arg("--accounts_ssh_authorized_keys")
        .arg(ssh_authorized_pub_keys_dir)
        .arg("--journalbeat_tags")
        .arg(format!("system_test {}", group_name));

    if !boundary_node.nns_node_urls.is_empty() {
        cmd.arg("--nns_url").arg({
            let urls: Vec<&str> = boundary_node
                .nns_node_urls
                .iter()
                .map(|url| url.as_str())
                .collect();
            urls.join(" ")
        });
    }

    if let Some(nns_public_key) = boundary_node.nns_public_key.clone() {
        cmd.arg("--nns_public_key").arg(nns_public_key);
    }

    if !journalbeat_hosts.is_empty() {
        cmd.arg("--journalbeat_hosts")
            .arg(journalbeat_hosts.join(" "));
    }

    let output = cmd.output()?;

    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;

    if !output.status.success() {
        bail!("could not spawn image creation process");
    }

    let mut img_file = File::open(img_path)?;

    let compressed_img_path = boundary_node_dir.join(mk_compressed_img_path());
    let compressed_img_file = File::create(compressed_img_path.clone())?;

    let mut encoder = GzEncoder::new(compressed_img_file, Compression::default());
    let _ = io::copy(&mut img_file, &mut encoder)?;
    let mut write_stream = encoder.finish()?;
    write_stream.flush()?;

    let mut cmd = Command::new("sha256sum");
    cmd.arg(compressed_img_path.clone());
    let output = cmd.output()?;
    if !output.status.success() {
        bail!("could not create sha256 of image");
    }

    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;

    let image_id = farm.upload_file(compressed_img_path, &mk_compressed_img_path())?;
    info!(farm.logger, "Uploaded image: {}", image_id);

    Ok(image_id)
}

pub trait BoundaryNodeVm {
    fn get_deployed_boundary_node(&self, name: &str) -> Result<DeployedBoundaryNode>;

    fn write_boundary_node_vm(&self, name: &str, vm: &VMCreateResponse) -> Result<()>;
}

impl BoundaryNodeVm for TestEnv {
    fn get_deployed_boundary_node(&self, name: &str) -> Result<DeployedBoundaryNode> {
        let rel_boundary_node_dir: PathBuf = [BOUNDARY_NODE_VMS_DIR, name].iter().collect();
        let abs_boundary_node_dir = self.get_path(rel_boundary_node_dir);
        if abs_boundary_node_dir.is_dir() {
            Ok(DeployedBoundaryNode {
                env: self.clone(),
                name: name.to_string(),
            })
        } else {
            bail!("Did not find deployed boundary node '{name}'!")
        }
    }

    fn write_boundary_node_vm(&self, name: &str, vm: &VMCreateResponse) -> Result<()> {
        let vm_path: PathBuf = [BOUNDARY_NODE_VMS_DIR, name].iter().collect();
        self.write_object(vm_path.join(BOUNDARY_NODE_VM_PATH), &vm)
    }
}

pub struct DeployedBoundaryNode {
    env: TestEnv,
    name: String,
}

impl DeployedBoundaryNode {
    pub fn get_vm(&self) -> Result<VMCreateResponse> {
        let vm_path: PathBuf = [BOUNDARY_NODE_VMS_DIR, &self.name].iter().collect();
        self.env.read_object(vm_path.join(BOUNDARY_NODE_VM_PATH))
    }

    pub fn get_ssh_session(&self) -> Result<Session> {
        let vm = self.get_vm()?;
        let tcp = TcpStream::connect((vm.ipv6, 22))?;
        let mut sess = Session::new()?;
        sess.set_tcp_stream(tcp);
        sess.handshake()?;
        let admin_priv_key_path = self
            .env
            .get_path(SSH_AUTHORIZED_PRIV_KEYS_DIR)
            .join(ADMIN_USER);
        sess.userauth_pubkey_file(ADMIN_USER, None, admin_priv_key_path.as_path(), None)?;
        Ok(sess)
    }

    pub fn await_ssh_session(&self) -> Result<Session> {
        retry(self.env.logger(), RETRY_TIMEOUT, RETRY_BACKOFF, || {
            self.get_ssh_session()
        })
    }

    pub fn await_ipv4(&self) -> Result<Ipv4Addr> {
        let sess = self.await_ssh_session()?;
        let mut channel = sess.channel_session()?;
        channel.exec("bash").unwrap();

        let get_ipv4_script = r#"set -e -o pipefail
until ipv4=$(ip address show dev enp2s0 | grep 'inet.*scope global' | awk '{print $2}' | cut -d/ -f1); \
do
  sleep 1
done
echo "$ipv4"
"#;
        channel.write_all(get_ipv4_script.as_bytes())?;
        channel.flush()?;
        channel.send_eof()?;
        let mut out = String::new();
        channel.read_to_string(&mut out)?;
        let ipv4 = out.trim().parse::<Ipv4Addr>()?;
        Ok(ipv4)
    }
}