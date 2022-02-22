use crate::error::{OrchestratorError, OrchestratorResult};
use crate::{metrics::OrchestratorMetrics, registry_helper::RegistryHelper};
use ic_logger::{debug, warn, ReplicaLogger};
use ic_registry_client::helper::unassigned_nodes::UnassignedNodeRegistry;
use ic_types::{RegistryVersion, SubnetId};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Provides function to continuously check the Registry to determine if there
/// has been a change in the readonly and backup public key sets.If so, updates
/// the accesss to the node accordingly.
pub(crate) struct SshAccessManager {
    registry: Arc<RegistryHelper>,
    metrics: Arc<OrchestratorMetrics>,
    logger: ReplicaLogger,
    last_used_check_parameters: (RegistryVersion, Option<SubnetId>),
}

impl SshAccessManager {
    pub(crate) fn new(
        registry: Arc<RegistryHelper>,
        metrics: Arc<OrchestratorMetrics>,
        logger: ReplicaLogger,
    ) -> Self {
        Self {
            registry,
            metrics,
            logger,
            last_used_check_parameters: Default::default(),
        }
    }

    /// Checks for changes in the keysets, and updates the node accordingly.
    pub(crate) async fn check_for_keyset_changes(&mut self, subnet_id: Option<SubnetId>) {
        let registry_version = self.registry.get_latest_version();
        if self.last_used_check_parameters == (registry_version, subnet_id) {
            return;
        }
        debug!(
            self.logger,
            "Checking for the access keys in the registry version {} for subnet_id {:?}",
            registry_version,
            subnet_id
        );

        let (new_readonly_keys, new_backup_keys) =
            match self.get_readonly_and_backup_keysets(subnet_id, registry_version) {
                Err(error) => {
                    warn!(
                        every_n_seconds => 300,
                        self.logger,
                        "Cannot retrieve the readonly & backup keysets from the registry {}", error
                    );
                    return;
                }
                Ok(keys) => keys,
            };

        // Update the readonly & backup keys. If it fails, log why.
        if self.update_access_keys(&new_readonly_keys, &new_backup_keys) {
            self.last_used_check_parameters = (registry_version, subnet_id);
            self.metrics
                .ssh_access_registry_version
                .set(registry_version.get() as i64);
        }
    }

    fn update_access_keys(&self, readonly_keys: &[String], backup_keys: &[String]) -> bool {
        let update = |account, keys| {
            self.update_access_to_one_account(account, keys)
                .map_err(|e| {
                    warn!(
                        every_n_seconds => 300,
                        self.logger,
                        "Could not update the {} keys due to a script failure: {}", account, e
                    );
                })
                .is_ok()
        };
        let result = update("readonly", readonly_keys);
        update("backup", backup_keys) && result
    }

    fn update_access_to_one_account(&self, account: &str, keys: &[String]) -> Result<(), String> {
        let mut cmd = Command::new("sudo")
            .arg("/opt/ic/bin/provision-ssh-keys.sh")
            .arg(account)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn child process: {}", e))?;

        let mut stdin = cmd
            .stdin
            .take()
            .ok_or_else(|| "Failed to open stdin".to_string())?;
        let key_list = keys.join("\n");
        stdin
            .write_all(key_list.as_bytes())
            .map_err(|e| format!("Failed to write to stdin: {}", e))?;
        drop(stdin);

        match cmd.wait_with_output() {
            Err(e) => Err(e.to_string()),
            _ => Ok(()),
        }
    }

    fn get_readonly_and_backup_keysets(
        &self,
        subnet_id: Option<SubnetId>,
        version: RegistryVersion,
    ) -> OrchestratorResult<(Vec<String>, Vec<String>)> {
        match subnet_id {
            None => match self
                .registry
                .registry_client
                .get_unassigned_nodes_config(version)
                .map_err(OrchestratorError::RegistryClientError)?
            {
                // Unassigned nodes do not need backup keys
                Some(record) => Ok((record.ssh_readonly_access, vec![])),
                None => Ok((vec![], vec![])),
            },
            Some(subnet_id) => {
                self.registry
                    .get_subnet_record(subnet_id, version)
                    .map(|subnet_record| {
                        (
                            subnet_record.ssh_readonly_access,
                            subnet_record.ssh_backup_access,
                        )
                    })
            }
        }
    }
}
