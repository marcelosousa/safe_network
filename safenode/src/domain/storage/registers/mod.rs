// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

/// Register type.
pub mod register;

use self::register::{Action, EntryHash, Register, User};

use super::{prefix_tree_path, Error, RegisterAddress, Result};

use crate::{
    domain::storage::list_files_in,
    protocol::{
        error::Error as ProtocolError,
        messages::{
            EditRegister, QueryResponse, RegisterCmd, RegisterQuery, ReplicatedRegisterLog,
            SignedRegisterCreate, SignedRegisterEdit,
        },
    },
};

use bincode::serialize;
use std::path::{Path, PathBuf};
use tokio::{
    fs::{create_dir_all, read, remove_file, File},
    io::AsyncWriteExt,
};
use tracing::trace;

pub(super) type RegisterLog = Vec<RegisterCmd>;

const REGISTERS_STORE_DIR_NAME: &str = "registers";

#[derive(Clone, Debug)]
struct StoredRegister {
    state: Option<Register>,
    op_log: RegisterLog,
    op_log_path: PathBuf,
}

/// Operations over the Register data type and its storage.
#[derive(Clone)]
pub(crate) struct RegisterStorage {
    file_store_path: PathBuf,
}

impl RegisterStorage {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            file_store_path: path.join(REGISTERS_STORE_DIR_NAME),
        }
    }

    /// Read from the Register's log based on provided RegisterQuery.
    pub(crate) async fn read(&self, read: &RegisterQuery, requester: User) -> QueryResponse {
        trace!("Reading register: {:?}", read.dst());
        use RegisterQuery::*;
        match read {
            Get(address) => QueryResponse::GetRegister(
                self.get_register(address, Action::Read, requester)
                    .await
                    .map_err(ProtocolError::Storage),
            ),
            Read(address) => self.read_register(*address, requester).await,
            GetOwner(address) => self.get_owner(*address, requester).await,
            GetEntry { address, hash } => self.get_entry(*address, *hash, requester).await,
            GetPolicy(address) => self.get_policy(*address, requester).await,
            GetUserPermissions { address, user } => {
                self.get_user_permissions(*address, *user, requester).await
            }
        }
    }

    /// Write a RegisterCmd to the Register's log.
    pub(crate) async fn write(&self, cmd: &RegisterCmd) -> Result<()> {
        info!("Writing register cmd: {cmd:?}");
        let addr = cmd.dst();
        // First try to load and reconstruct the replica of the register
        // we have in local storage, to then try to apply the new cmd to it.
        let mut stored_reg = self.try_load_stored_register(&addr).await?;

        self.try_to_apply_cmd_against_register_state(cmd, &mut stored_reg)?;

        // Everything went fine, write the new cmd to disk.
        self.write_log_to_disk(&vec![cmd.clone()], &stored_reg.op_log_path)
            .await
    }

    /// This is to be used when a node is shrinking the address range it is responsible for.
    #[allow(dead_code)]
    pub(super) async fn remove(&self, address: &RegisterAddress) -> Result<()> {
        trace!("Removing Register: {address:?}");
        let filepath = self.address_to_filepath(address)?;
        remove_file(filepath).await?;
        Ok(())
    }

    /// Update our Register's replica on receiving data from other nodes.
    #[allow(dead_code)]
    pub(super) async fn update(&self, data: &ReplicatedRegisterLog) -> Result<()> {
        let addr = data.address;
        debug!("Updating Register store: {addr:?}");
        let mut stored_reg = self.try_load_stored_register(&addr).await?;

        let mut log_to_write = Vec::new();
        for replicated_cmd in &data.op_log {
            if let Err(err) =
                self.try_to_apply_cmd_against_register_state(replicated_cmd, &mut stored_reg)
            {
                warn!("Discarding ReplicatedRegisterLog cmd {replicated_cmd:?}: {err:?}",);
            } else {
                log_to_write.push(replicated_cmd.clone());
            }
        }

        // Write the new cmds all to disk
        self.write_log_to_disk(&log_to_write, &stored_reg.op_log_path)
            .await
    }

    /// ---------------------------------------------------
    /// ----------------- Private fns ---------------------
    /// ---------------------------------------------------

    /// Persists a RegisterLog to disk
    async fn write_log_to_disk(&self, log: &RegisterLog, path: &Path) -> Result<()> {
        trace!(
            "Writing to register log with {} cmd/s at {}",
            log.len(),
            path.display()
        );
        if log.is_empty() {
            return Ok(());
        }

        create_dir_all(path).await?;

        let mut last_err = None;

        for cmd in log {
            if let Err(err) = self.write_register_cmd(cmd, path).await {
                error!("Failed to write Register cmd {cmd:?} to disk: {err:?}");
                last_err = Some(err);
            }
        }

        if let Some(err) = last_err {
            Err(err)
        } else {
            trace!(
                "Log of {} cmd/s written successfully at {}",
                log.len(),
                path.display()
            );
            Ok(())
        }
    }

    /// Persists a RegisterCmd to disk.
    async fn write_register_cmd(&self, cmd: &RegisterCmd, path: &Path) -> Result<()> {
        let addr = cmd.dst();
        let reg_cmd_id = register_op_id(cmd)?;
        let path = path.join(&reg_cmd_id);

        trace!(
            "Writing cmd register log for {addr:?} at {}",
            path.display()
        );

        let entry_hash = if let RegisterCmd::Edit(edit_cmd) = cmd {
            let entry_hash = EntryHash(edit_cmd.op.edit.crdt_op.hash());
            trace!(
                "Writing RegisterEdit cmd log for {addr:?}, entry hash: {entry_hash}, at {}",
                path.display()
            );
            Some(entry_hash)
        } else {
            trace!(
                "Writing RegisterCreate cmd log for {addr:?} at {}",
                path.display()
            );
            None
        };

        // It's deterministic, so they are exactly the same op so we can leave.
        if path.exists() {
            trace!("RegisterCmd exists on disk for {addr:?}, entry hash: {entry_hash:?}, so was not written: {cmd:?}");
            return Ok(());
        }

        let mut file = File::create(&path).await?;

        let serialized_data = serialize(cmd)?;
        file.write_all(&serialized_data).await?;
        // Sync OS data to disk to reduce the chances of
        // concurrent reading failing by reading an empty/incomplete file.
        file.sync_data().await?;

        trace!(
            "RegisterCmd writing successful for {addr:?}, id {reg_cmd_id}, at {}, entry hash: {entry_hash:?}",
            path.display()
        );

        Ok(())
    }

    /// Get `Register` from the store and check permissions.
    async fn get_register(
        &self,
        address: &RegisterAddress,
        action: Action,
        requester: User,
    ) -> Result<Register> {
        let stored_reg = self.try_load_stored_register(address).await?;
        if let Some(register) = stored_reg.state {
            register.check_permissions(action, Some(requester))?;

            Ok(register)
        } else {
            Err(Error::RegisterNotFound(*address))
        }
    }

    async fn read_register(&self, address: RegisterAddress, requester: User) -> QueryResponse {
        let result = match self.get_register(&address, Action::Read, requester).await {
            Ok(register) => Ok(register.read()),
            Err(error) => Err(error),
        }
        .map_err(ProtocolError::Storage);

        QueryResponse::ReadRegister(result)
    }

    async fn get_owner(&self, address: RegisterAddress, requester: User) -> QueryResponse {
        let result = match self.get_register(&address, Action::Read, requester).await {
            Ok(res) => Ok(res.owner()),
            Err(error) => Err(error),
        }
        .map_err(ProtocolError::Storage);

        QueryResponse::GetRegisterOwner(result)
    }

    async fn get_entry(
        &self,
        address: RegisterAddress,
        hash: EntryHash,
        requester: User,
    ) -> QueryResponse {
        let result = self
            .get_register(&address, Action::Read, requester)
            .await
            .and_then(|register| register.get(hash).map(|c| c.clone()))
            .map_err(ProtocolError::Storage);

        QueryResponse::GetRegisterEntry(result)
    }

    async fn get_user_permissions(
        &self,
        address: RegisterAddress,
        user: User,
        requester: User,
    ) -> QueryResponse {
        let result = self
            .get_register(&address, Action::Read, requester)
            .await
            .and_then(|register| register.permissions(user))
            .map_err(ProtocolError::Storage);

        QueryResponse::GetRegisterUserPermissions(result)
    }

    async fn get_policy(&self, address: RegisterAddress, requester_pk: User) -> QueryResponse {
        let result = self
            .get_register(&address, Action::Read, requester_pk)
            .await
            .map(|register| register.policy().clone())
            .map_err(ProtocolError::Storage);

        QueryResponse::GetRegisterPolicy(result)
    }

    fn address_to_filepath(&self, address: &RegisterAddress) -> Result<PathBuf> {
        // This is a unique identifier of the Register,
        // since it encodes both the xorname and tag.
        let reg_id = address.id();
        let path = prefix_tree_path(&self.file_store_path, reg_id);

        // We need to append a folder for the file specifically so bit depth is an issue when low.
        // We use hex to get full id, not just first bytes.
        Ok(path.join(hex::encode(reg_id)))
    }

    // Private helper which does all verification and tries to apply given cmd to given Register
    // state. It accumulates the cmd, if valid, into the log so further calls can be made with
    // the same state and log, as used by the `update` function.
    // Note the cmd is always pushed to the log even if it's a duplicated cmd.
    fn try_to_apply_cmd_against_register_state(
        &self,
        cmd: &RegisterCmd,
        stored_reg: &mut StoredRegister,
    ) -> Result<()> {
        // If we have the target Register, try to apply the cmd, otherwise let's keep
        // the cmd in the log anyway, whenever we receive the 'Register create' cmd
        // it can be reconstructed from all cmds we hold in the log. If this is a 'Register create'
        // cmd let's verify it's valid before accepting it, however 'Edits cmds' cannot be
        // verified until we have the `Register create` cmd.
        match (stored_reg.state.as_mut(), cmd) {
            (Some(_), RegisterCmd::Create { .. }) => return Ok(()), // no op, since already created
            (Some(ref mut register), RegisterCmd::Edit(_)) => self.apply(cmd, register)?,
            (None, RegisterCmd::Create(cmd)) => {
                // the target Register is not in our store or we don't have the 'Register create',
                // let's verify the create cmd we received is valid and try to apply stored cmds we may have.
                let SignedRegisterCreate { op, auth } = cmd;
                auth.verify_authority(serialize(op)?)?;

                trace!("Creating new register: {:?}", cmd.dst());
                // let's do a final check, let's try to apply all cmds to it,
                // those which are new cmds were not validated yet, so let's do it now.
                let mut register =
                    Register::new(*op.policy.owner(), op.name, op.tag, op.policy.clone());

                for cmd in &stored_reg.op_log {
                    self.apply(cmd, &mut register)?;
                }

                stored_reg.state = Some(register);
            }
            (None, _edit_cmd) => { /* we cannot validate it right now, but we'll store it */ }
        }

        stored_reg.op_log.push(cmd.clone());
        Ok(())
    }

    // Try to apply the provided cmd to the register state, performing all op validations
    fn apply(&self, cmd: &RegisterCmd, register: &mut Register) -> Result<()> {
        let addr = cmd.dst();
        if &addr != register.address() {
            return Err(Error::RegisterAddrMismatch {
                cmd_dst_addr: addr,
                reg_addr: *register.address(),
            });
        }

        match cmd {
            RegisterCmd::Create { .. } => Ok(()),
            RegisterCmd::Edit(SignedRegisterEdit { op, auth }) => {
                auth.verify_authority(serialize(op)?)?;

                info!("Editing Register: {addr:?}");
                let public_key = auth.public_key;
                register.check_permissions(Action::Write, Some(User::Key(public_key)))?;
                let result = register.apply_op(op.edit.clone());

                match result {
                    Ok(()) => {
                        trace!("Editing Register success: {addr:?}");
                        Ok(())
                    }
                    Err(err) => {
                        trace!("Editing Register failed {addr:?}: {err:?}");
                        Err(err)
                    }
                }
            }
        }
    }

    // Gets stored register log from disk, trying to reconstruct the Register
    // Note this doesn't perform any cmd sig/perms validation, it's only used when the log
    // is read from disk which has already been validated before storing it.
    async fn try_load_stored_register(&self, addr: &RegisterAddress) -> Result<StoredRegister> {
        let mut stored_reg = self.open_reg_log_from_disk(addr).await?;
        // if we have the Register creation cmd, apply all ops to reconstruct the Register
        if let Some(register) = &mut stored_reg.state {
            for cmd in &stored_reg.op_log {
                if let RegisterCmd::Edit(SignedRegisterEdit { op, .. }) = cmd {
                    let EditRegister { edit, .. } = op;
                    register.apply_op(edit.clone())?;
                }
            }
        }

        Ok(stored_reg)
    }

    /// Opens the log of RegisterCmds for a given register address.
    /// Creates a new log if no data is found.
    async fn open_reg_log_from_disk(&self, addr: &RegisterAddress) -> Result<StoredRegister> {
        let path = self.address_to_filepath(addr)?;
        let mut stored_reg = StoredRegister {
            state: None,
            op_log: RegisterLog::new(),
            op_log_path: path.clone(),
        };

        if !path.exists() {
            trace!(
                "Register log path for {addr:?} does not exist yet: {}",
                path.display()
            );
            return Ok(stored_reg);
        }

        trace!("Register log path for {addr:?} exists: {}", path.display());
        for filepath in list_files_in(&path) {
            match read(&filepath)
                .await
                .map(|serialized_data| bincode::deserialize::<RegisterCmd>(&serialized_data))
            {
                Ok(Ok(reg_cmd)) => {
                    stored_reg.op_log.push(reg_cmd.clone());

                    if let RegisterCmd::Create(cmd) = reg_cmd {
                        let SignedRegisterCreate { op, .. } = cmd;
                        let register =
                            Register::new(*op.policy.owner(), op.name, op.tag, op.policy);
                        match &stored_reg.state {
                            Some(s) => {
                                if s != &register {
                                    warn!("Unexpectedly found multiple different RegisterCmd::Create for {addr:?}: {s:?} and {register:?}");
                                } else {
                                    warn!("Unexpectedly found multiple identical RegisterCmd::Create for {addr:?}: {s:?}");
                                }
                            }
                            None => {
                                stored_reg.state = Some(register);
                            }
                        }
                    }
                }
                other => {
                    warn!(
                        "Ignoring corrupted Register cmd from storage, for {addr:?}, found at {}: {other:?}",
                        filepath.display()
                    )
                }
            }
        }

        Ok(stored_reg)
    }

    /// Used for replication of data to new nodes.
    /// Currently only used by the tests.
    /// TODO: to be used by replication logic.
    #[cfg(test)]
    async fn get_register_replica(
        &self,
        address: &RegisterAddress,
    ) -> Result<ReplicatedRegisterLog> {
        let stored_reg = self.try_load_stored_register(address).await?;
        // Build the replicated register log assuming ops stored are all valid and correctly
        // signed since we performed such validations before storing them.
        Ok(ReplicatedRegisterLog {
            address: *address,
            op_log: stored_reg.op_log,
        })
    }

    #[cfg(test)]
    async fn stored_addrs(&self) -> Vec<RegisterAddress> {
        use bincode::deserialize;
        use std::collections::{btree_map::Entry, BTreeMap};

        trace!("Listing all register addrs");

        let iter = list_files_in(&self.file_store_path)
            .into_iter()
            .filter_map(|e| e.parent().map(|parent| (parent.to_path_buf(), e.clone())));

        let mut addrs = BTreeMap::new();
        for (parent, op_file) in iter {
            if let Entry::Vacant(vacant) = addrs.entry(parent) {
                if let Ok(Ok(cmd)) = read(op_file)
                    .await
                    .map(|serialized_data| deserialize::<RegisterCmd>(&serialized_data))
                {
                    let _existing = vacant.insert(cmd.dst());
                }
            }
        }

        trace!("Listing all register addrs done.");

        addrs.into_values().collect()
    }
}

// Gets an operation id, deterministic for a RegisterCmd, it takes
// the full Cmd and all signers into consideration
fn register_op_id(cmd: &RegisterCmd) -> Result<String> {
    use tiny_keccak::Hasher;
    let mut hasher = tiny_keccak::Sha3::v256();
    let bytes = serialize(cmd)?;
    let mut output = [0; 64];
    hasher.update(&bytes);
    hasher.finalize(&mut output);
    let id = hex::encode(output);
    Ok(id)
}

#[cfg(test)]
mod test {
    use super::{
        register::{DataAuthority, EntryHash, Policy, Register, User},
        Error, RegisterStorage,
    };

    use crate::protocol::{
        error::Error as ProtocolError,
        messages::{
            CreateRegister, EditRegister, QueryResponse, RegisterCmd, RegisterQuery,
            SignedRegisterCreate, SignedRegisterEdit,
        },
    };

    use bincode::serialize;
    use bls::SecretKey;
    use eyre::{bail, Result};
    use rand::{distributions::Alphanumeric, Rng};
    use std::collections::BTreeSet;
    use xor_name::XorName;

    #[tokio::test]
    async fn test_register_try_load_stored() -> Result<()> {
        let store = new_store();

        let (cmd_create, _, sk, name, policy) = create_register()?;
        let addr = cmd_create.dst();
        let log_path = store.address_to_filepath(&addr)?;
        let mut register = Register::new(*policy.owner(), name, 0, policy);

        let stored_reg = store.try_load_stored_register(&addr).await?;
        // It should *not* contain the create cmd.
        assert!(stored_reg.state.is_none());
        assert!(stored_reg.op_log.is_empty());
        assert_eq!(stored_reg.op_log_path, log_path);

        store.write(&cmd_create).await?;
        let stored_reg = store.try_load_stored_register(&addr).await?;
        // It should contain the create cmd.
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log, vec![cmd_create.clone()]);
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.map(|reg| reg.size()), Some(0));

        // Edit the register.
        let cmd_edit = edit_register(&mut register, &sk)?;
        store.write(&cmd_edit).await?;

        let stored_reg = store.try_load_stored_register(&addr).await?;
        // It should contain the create and edit cmds.
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log.len(), 2);
        assert!(
            stored_reg
                .op_log
                .iter()
                .all(|op| [&cmd_create, &cmd_edit].contains(&op)),
            "Op log doesn't match"
        );
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.map(|reg| reg.size()), Some(1));

        Ok(())
    }

    #[tokio::test]
    async fn test_register_try_load_stored_inverted_cmds_order() -> Result<()> {
        let store = new_store();

        let (cmd_create, _, sk, name, policy) = create_register()?;
        let addr = cmd_create.dst();
        let log_path = store.address_to_filepath(&addr)?;
        let mut register = Register::new(*policy.owner(), name, 0, policy);

        // Store an edit cmd for the register.
        let cmd_edit = edit_register(&mut register, &sk)?;
        store.write(&cmd_edit).await?;

        let stored_reg = store.try_load_stored_register(&addr).await?;
        // It should contain only the edit cmd.
        assert_eq!(stored_reg.state, None);
        assert_eq!(stored_reg.op_log, vec![cmd_edit.clone()]);
        assert_eq!(stored_reg.op_log_path, log_path);

        // Store the create cmd for the register.
        store.write(&cmd_create).await?;

        let stored_reg = store.try_load_stored_register(&addr).await?;
        // It should contain the create and edit cmds.
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log.len(), 2);
        assert!(
            stored_reg
                .op_log
                .iter()
                .all(|op| [&cmd_create, &cmd_edit].contains(&op)),
            "Op log doesn't match"
        );
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.map(|reg| reg.size()), Some(1));

        Ok(())
    }

    #[tokio::test]
    async fn test_register_apply_cmd_against_state() -> Result<()> {
        let store = new_store();

        let (cmd_create, _, sk, name, policy) = create_register()?;
        let addr = cmd_create.dst();
        let log_path = store.address_to_filepath(&addr)?;
        let mut register = Register::new(*policy.owner(), name, 0, policy);
        let mut stored_reg = store.try_load_stored_register(&addr).await?;

        store.try_to_apply_cmd_against_register_state(&cmd_create, &mut stored_reg)?;

        // It should contain the create cmd.
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log, vec![cmd_create.clone()]);
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.as_ref().map(|reg| reg.size()), Some(0));

        // Apply the create cmd again should change nothing.
        match store.try_to_apply_cmd_against_register_state(&cmd_create, &mut stored_reg) {
            Ok(()) => (),
            Err(err) => bail!(
                "An error should not occur when applying create cmd again: {:?}",
                err
            ),
        }

        // Apply an edit cmd.
        let cmd_edit = edit_register(&mut register, &sk)?;
        store.try_to_apply_cmd_against_register_state(&cmd_edit, &mut stored_reg)?;
        // It should contain the create and edit cmds.
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log.len(), 2);
        assert!(
            stored_reg
                .op_log
                .iter()
                .all(|op| [&cmd_create, &cmd_edit].contains(&op)),
            "Op log doesn't match"
        );
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.as_ref().map(|reg| reg.size()), Some(1));

        // Applying the edit cmd again shouldn't fail or alter the register content,
        // although the log will contain the edit cmd duplicated.
        store.try_to_apply_cmd_against_register_state(&cmd_edit, &mut stored_reg)?;
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log.len(), 3);
        assert!(
            stored_reg
                .op_log
                .iter()
                .all(|op| [&cmd_create, &cmd_edit].contains(&op)),
            "Op log doesn't match"
        );
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.map(|reg| reg.size()), Some(1));

        Ok(())
    }

    #[tokio::test]
    async fn test_register_apply_cmd_against_state_inverted_cmds_order() -> Result<()> {
        let store = new_store();

        let (cmd_create, _, sk, name, policy) = create_register()?;
        let addr = cmd_create.dst();
        let log_path = store.address_to_filepath(&addr)?;
        let mut register = Register::new(*policy.owner(), name, 0, policy);
        let mut stored_reg = store.try_load_stored_register(&addr).await?;

        // Apply an edit cmd first.
        let cmd_edit = edit_register(&mut register, &sk)?;
        store.try_to_apply_cmd_against_register_state(&cmd_edit, &mut stored_reg)?;
        // It should contain the edit cmd.
        assert_eq!(stored_reg.state, None);
        assert_eq!(stored_reg.op_log, vec![cmd_edit.clone()]);
        assert_eq!(stored_reg.op_log_path, log_path);

        // Applying the edit cmd again shouldn't fail,
        // although the log will contain the edit cmd duplicated.
        store.try_to_apply_cmd_against_register_state(&cmd_edit, &mut stored_reg)?;
        assert_eq!(stored_reg.state, None);
        assert_eq!(stored_reg.op_log.len(), 2);
        assert!(
            stored_reg.op_log.iter().all(|op| op == &cmd_edit),
            "Op log doesn't match"
        );
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.as_ref().map(|reg| reg.size()), None);

        // Apply the create cmd now.
        store.try_to_apply_cmd_against_register_state(&cmd_create, &mut stored_reg)?;
        // It should contain the create and edit cmds.
        assert_eq!(stored_reg.state.as_ref(), Some(&register));
        assert_eq!(stored_reg.op_log.len(), 3);
        assert!(
            stored_reg
                .op_log
                .iter()
                .all(|op| [&cmd_create, &cmd_edit].contains(&op)),
            "Op log doesn't match"
        );
        assert_eq!(stored_reg.op_log_path, log_path);
        assert_eq!(stored_reg.state.as_ref().map(|reg| reg.size()), Some(1));

        // Apply the create cmd again should change nothing.
        match store.try_to_apply_cmd_against_register_state(&cmd_create, &mut stored_reg) {
            Ok(()) => (),
            Err(err) => bail!(
                "An error should not occur when applying create cmd again: {:?}",
                err
            ),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_register_write() -> Result<()> {
        let store = new_store();

        let (cmd, authority, _, _, _) = create_register()?;
        store.write(&cmd).await?;

        let addr = cmd.dst();
        match store.read(&RegisterQuery::Get(addr), authority).await {
            QueryResponse::GetRegister(Ok(reg)) => {
                assert_eq!(reg.address(), &addr, "Should have same address!");
                assert_eq!(reg.owner(), authority, "Should have same owner!");
            }
            e => bail!("Could not read register! {:?}", e),
        }

        // Apply the create cmd again should change nothing.
        match store.write(&cmd).await {
            Ok(()) => Ok(()),
            Err(err) => bail!(
                "An error should not occur when applying create cmd again: {:?}",
                err
            ),
        }
    }

    #[tokio::test]
    async fn test_register_export() -> Result<()> {
        let store = new_store();

        let (cmd_create, authority, sk, name, policy) = create_register()?;
        let addr = cmd_create.dst();
        let mut register = Register::new(*policy.owner(), name, 0, policy);

        // Store the register and a few edit ops.
        store.write(&cmd_create).await?;
        for _ in 0..10 {
            let cmd_edit = edit_register(&mut register, &sk)?;
            store.write(&cmd_edit).await?;
        }

        // Create cmd should be idempotent.
        match store.write(&cmd_create).await {
            Ok(()) => (),
            Err(err) => bail!(
                "An error should not occur when applying create cmd again: {:?}",
                err
            ),
        }

        // Export Registers, get all data we held in storage.
        let stored_addrs = store.stored_addrs().await;

        // Create new store and update it with the data from first store
        let new_store = new_store();
        for addr in stored_addrs {
            let replica = store.get_register_replica(&addr).await?;
            new_store.update(&replica).await?;
        }

        // Assert the same tests hold as for the first store
        // create cmd should be idempotent, also on this new store.
        match new_store.write(&cmd_create).await {
            Ok(()) => (),
            Err(err) => bail!(
                "An error should not occur when applying create cmd again: {:?}",
                err
            ),
        }

        // Should be able to read the same value from this new store as well.
        let res = new_store.read(&RegisterQuery::Get(addr), authority).await;

        match res {
            QueryResponse::GetRegister(Ok(reg)) => {
                assert_eq!(reg.address(), &addr, "Should have same address!");
                assert_eq!(reg.owner(), authority, "Should have same owner!");
            }
            e => panic!("Could not read! {e:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_register_non_existing_entry() -> Result<()> {
        let store = new_store();

        let (cmd_create, authority, _, _, _) = create_register()?;
        store.write(&cmd_create).await?;

        let hash = EntryHash(rand::thread_rng().gen::<[u8; 32]>());

        // Try get permissions of random user.
        let address = cmd_create.dst();
        let res = store
            .read(&RegisterQuery::GetEntry { address, hash }, authority)
            .await;
        match res {
            QueryResponse::GetRegisterEntry(Err(e)) => {
                assert_eq!(e, ProtocolError::Storage(Error::NoSuchEntry(hash)))
            }
            QueryResponse::GetRegisterEntry(Ok(entry)) => {
                panic!("Should not exist any entry for random hash! {entry:?}")
            }
            e => panic!("Could not read! {e:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_register_non_existing_permissions() -> Result<()> {
        let store = new_store();

        let (cmd_create, authority, _, _, _) = create_register()?;
        store.write(&cmd_create).await?;

        let (user, _) = random_user();

        // Try get permissions of random user.
        let address = cmd_create.dst();
        let res = store
            .read(
                &RegisterQuery::GetUserPermissions { address, user },
                authority,
            )
            .await;
        match res {
            QueryResponse::GetRegisterUserPermissions(Err(e)) => {
                assert_eq!(e, ProtocolError::Storage(Error::NoSuchUser(user)))
            }
            QueryResponse::GetRegisterUserPermissions(Ok(perms)) => {
                panic!("Should not exist any permissions for random user! {perms:?}",)
            }
            e => panic!("Could not read! {e:?}"),
        }

        Ok(())
    }

    fn random_user() -> (User, SecretKey) {
        let sk = SecretKey::random();
        let authority = User::Key(sk.public_key());
        (authority, sk)
    }

    fn create_register() -> Result<(RegisterCmd, User, SecretKey, XorName, Policy)> {
        let (authority, sk) = random_user();
        let policy = Policy {
            owner: authority,
            permissions: Default::default(),
        };
        let xorname = xor_name::rand::random();
        let cmd = create_reg_w_policy(xorname, 0, policy.clone(), &sk)?;

        Ok((cmd, authority, sk, xorname, policy))
    }

    fn edit_register(register: &mut Register, sk: &SecretKey) -> Result<RegisterCmd> {
        let data = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(15)
            .collect();
        let (_, edit) = register.write(data, BTreeSet::default())?;
        let op = EditRegister {
            address: *register.address(),
            edit,
        };
        let signature = sk.sign(serialize(&op)?);

        Ok(RegisterCmd::Edit(SignedRegisterEdit {
            op,
            auth: DataAuthority {
                public_key: sk.public_key(),
                signature,
            },
        }))
    }

    fn new_store() -> RegisterStorage {
        let tmp_dir = assert_fs::TempDir::new().expect("Should be able to create a temp dir.");
        let path = tmp_dir.path();
        RegisterStorage::new(path)
    }

    // Helper functions temporarily used for spentbook logic, but also used for tests.
    // This shouldn't be required outside of tests once we have a Spentbook data type.
    fn create_reg_w_policy(
        name: XorName,
        tag: u64,
        policy: Policy,
        sk: &SecretKey,
    ) -> Result<RegisterCmd> {
        let op = CreateRegister { name, tag, policy };
        let signature = sk.sign(serialize(&op)?);

        let auth = DataAuthority {
            public_key: sk.public_key(),
            signature,
        };

        Ok(RegisterCmd::Create(SignedRegisterCreate { op, auth }))
    }
}
