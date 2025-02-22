use std::{array::TryFromSliceError, fmt::Debug, num::ParseIntError, rc::Rc};

use clap::Parser;
use rand::Rng;
use sha2::{Digest, Sha256};
use soroban_env_host::{
    budget::Budget,
    storage::Storage,
    xdr::{
        AccountId, Error as XdrError, Hash, HashIdPreimage, HashIdPreimageSourceAccountContractId,
        HostFunction, InvokeHostFunctionOp, LedgerFootprint, LedgerKey::ContractData,
        LedgerKeyContractData, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
        PublicKey, ScHostStorageErrorCode, ScMap, ScMapEntry, ScObject,
        ScStatic::LedgerKeyContractCode, ScStatus, ScVal, ScVec, SequenceNumber, Transaction,
        TransactionEnvelope, TransactionExt, Uint256, VecM, WriteXdr,
    },
    Host, HostError,
};
use stellar_strkey::StrkeyPublicKeyEd25519;

use crate::{
    rpc::{Client, Error as SorobanRpcError},
    snapshot, utils,
};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("reading file {filepath}: {error}")]
    CannotReadLedgerFile {
        filepath: std::path::PathBuf,
        error: snapshot::Error,
    },
    #[error("committing file {filepath}: {error}")]
    CannotCommitLedgerFile {
        filepath: std::path::PathBuf,
        error: snapshot::Error,
    },
    #[error("cannot parse private key")]
    CannotParsePrivateKey,
    #[error("cannot parse salt: {salt}")]
    CannotParseSalt { salt: String },
    #[error(transparent)]
    // TODO: the Display impl of host errors is pretty user-unfriendly
    //       (it just calls Debug). I think we can do better than that
    Host(#[from] HostError),
    #[error("invalid asset code: {asset}")]
    InvalidAssetCode { asset: String },
    #[error("error parsing int: {0}")]
    ParseIntError(#[from] ParseIntError),
    #[error(transparent)]
    Client(#[from] SorobanRpcError),
    #[error("internal conversion error: {0}")]
    TryFromSliceError(#[from] TryFromSliceError),
    #[error("xdr processing error: {0}")]
    Xdr(#[from] XdrError),
}

#[derive(Parser, Debug)]
pub struct Cmd {
    /// Administrator account for the token, will default to --private-strkey if not set
    #[clap(long)]
    admin: Option<StrkeyPublicKeyEd25519>,

    /// Number of decimal places for the token
    #[clap(long, default_value = "7")]
    decimal: u32,

    /// Long name of the token, e.g. "Stellar Lumens"
    #[clap(long)]
    name: String,

    /// Short name of the token, e.g. "XLM"
    #[clap(long)]
    symbol: String,

    /// Custom salt 32-byte salt for the token id
    #[clap(
        long,
        default_value = "0000000000000000000000000000000000000000000000000000000000000000"
    )]
    salt: String,

    /// File to persist ledger state (if using the sandbox)
    #[clap(
        long,
        parse(from_os_str),
        default_value = ".soroban/ledger.json",
        conflicts_with = "rpc-server-url"
    )]
    ledger_file: std::path::PathBuf,

    /// RPC server endpoint
    #[clap(
        long,
        conflicts_with = "ledger-file",
        requires = "private-strkey",
        requires = "network-passphrase"
    )]
    rpc_server_url: Option<String>,
    /// Private key to sign the transaction sent to the rpc server
    #[clap(long = "private-strkey", env)]
    private_strkey: Option<String>,
    /// Network passphrase to sign the transaction sent to the rpc server
    #[clap(long = "network-passphrase")]
    network_passphrase: Option<String>,
}

impl Cmd {
    pub async fn run(&self) -> Result<(), Error> {
        // Hack: re-use contract_id_from_str to parse the 32-byte salt hex.
        let salt: [u8; 32] =
            utils::contract_id_from_str(&self.salt).map_err(|_| Error::CannotParseSalt {
                salt: self.salt.clone(),
            })?;

        if self.symbol.len() > 12 {
            return Err(Error::InvalidAssetCode {
                asset: self.symbol.clone(),
            });
        }

        let res_str = if self.rpc_server_url.is_some() {
            self.run_against_rpc_server(
                salt,
                self.admin.map(|a| a.0),
                &self.name,
                &self.symbol,
                self.decimal,
            )
            .await?
        } else {
            self.run_in_sandbox(salt, self.admin, &self.name, &self.symbol, self.decimal)?
        };
        println!("{}", res_str);
        Ok(())
    }

    fn run_in_sandbox(
        &self,
        salt: [u8; 32],
        admin_param: Option<StrkeyPublicKeyEd25519>,
        name: &str,
        symbol: &str,
        decimal: u32,
    ) -> Result<String, Error> {
        // Use 0s as default admin key
        let admin = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            admin_param
                .unwrap_or_else(|| {
                    StrkeyPublicKeyEd25519::from_string(
                        "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
                    )
                    .unwrap()
                })
                .0,
        )));

        // Initialize storage and host
        // TODO: allow option to separate input and output file
        let state = snapshot::read(&self.ledger_file).map_err(|e| Error::CannotReadLedgerFile {
            filepath: self.ledger_file.clone(),
            error: e,
        })?;

        let snap = Rc::new(snapshot::Snap {
            ledger_entries: state.1.clone(),
        });
        let h = Host::with_storage_and_budget(
            Storage::with_recording_footprint(snap),
            Budget::default(),
        );

        h.set_source_account(admin.clone());

        let mut ledger_info = state.0.clone();
        ledger_info.sequence_number += 1;
        ledger_info.timestamp += 5;
        h.set_ledger_info(ledger_info.clone());

        let res = h.invoke_function(
            HostFunction::CreateTokenContractWithSourceAccount,
            vec![ScVal::Object(Some(ScObject::Bytes(salt.try_into()?)))].try_into()?,
        )?;
        let res_str = utils::vec_to_hash(&res)?;

        let contract_id = get_contract_id(salt, admin.clone())?;
        h.invoke_function(
            HostFunction::InvokeContract,
            init_parameters(contract_id, &admin, name, symbol, decimal),
        )?;

        let (storage, _, _) = h.try_finish().map_err(|_h| {
            HostError::from(ScStatus::HostStorageError(
                ScHostStorageErrorCode::UnknownError,
            ))
        })?;

        snapshot::commit(state.1, ledger_info, &storage.map, &self.ledger_file).map_err(|e| {
            Error::CannotCommitLedgerFile {
                filepath: self.ledger_file.clone(),
                error: e,
            }
        })?;
        Ok(res_str)
    }

    async fn run_against_rpc_server(
        &self,
        salt: [u8; 32],
        admin: Option<[u8; 32]>,
        name: &str,
        symbol: &str,
        decimal: u32,
    ) -> Result<String, Error> {
        let client = Client::new(self.rpc_server_url.as_ref().unwrap());
        let key = utils::parse_private_key(self.private_strkey.as_ref().unwrap())
            .map_err(|_| Error::CannotParsePrivateKey)?;
        let salt_val = if salt == [0; 32] {
            rand::thread_rng().gen::<[u8; 32]>()
        } else {
            salt
        };

        let admin_key = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            admin.unwrap_or_else(|| key.public.to_bytes()),
        )));

        // Get the account sequence number
        let public_strkey =
            stellar_strkey::StrkeyPublicKeyEd25519(key.public.to_bytes()).to_string();
        // TODO: use symbols for the method names (both here and in serve)
        let account_details = client.get_account(&public_strkey).await?;
        // TODO: create a cmdline parameter for the fee instead of simply using the minimum fee
        let fee: u32 = 100;
        let sequence = account_details.sequence.parse::<i64>()?;
        let contract_id = get_contract_id(salt_val, admin_key.clone())?;

        client
            .send_transaction(&build_tx(
                build_create_token_op(&Hash(contract_id), salt_val)?,
                sequence + 1,
                fee,
                self.network_passphrase.as_ref().unwrap(),
                &key,
            )?)
            .await?;

        client
            .send_transaction(&build_tx(
                build_init_op(
                    &Hash(contract_id),
                    init_parameters(contract_id, &admin_key, name, symbol, decimal),
                )?,
                sequence + 2,
                fee,
                self.network_passphrase.as_ref().unwrap(),
                &key,
            )?)
            .await?;

        Ok(hex::encode(&contract_id))
    }
}

fn get_contract_id(salt: [u8; 32], source_account: AccountId) -> Result<[u8; 32], Error> {
    let preimage =
        HashIdPreimage::ContractIdFromSourceAccount(HashIdPreimageSourceAccountContractId {
            source_account,
            salt: Uint256(salt),
        });
    let preimage_xdr = preimage.to_xdr()?;
    Ok(Sha256::digest(preimage_xdr).into())
}

fn build_tx(
    op: Operation,
    sequence: i64,
    fee: u32,
    network_passphrase: &str,
    key: &ed25519_dalek::Keypair,
) -> Result<TransactionEnvelope, Error> {
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(key.public.to_bytes())),
        fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into()?,
        ext: TransactionExt::V0,
    };

    Ok(utils::sign_transaction(key, &tx, network_passphrase)?)
}

fn build_create_token_op(contract_id: &Hash, salt: [u8; 32]) -> Result<Operation, Error> {
    let lk = ContractData(LedgerKeyContractData {
        contract_id: contract_id.clone(),
        key: ScVal::Static(LedgerKeyContractCode),
    });

    let parameters: VecM<ScVal, 256_000> =
        vec![ScVal::Object(Some(ScObject::Bytes(salt.try_into()?)))].try_into()?;

    Ok(Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            function: HostFunction::CreateTokenContractWithSourceAccount,
            parameters: parameters.into(),
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: vec![lk].try_into()?,
            },
        }),
    })
}

fn init_parameters(
    contract_id: [u8; 32],
    admin: &AccountId,
    name: &str,
    symbol: &str,
    decimals: u32,
) -> ScVec {
    vec![
        // Contract ID
        ScVal::Object(Some(ScObject::Bytes(contract_id.try_into().unwrap()))),
        // Method
        ScVal::Symbol("init".try_into().unwrap()),
        // Admin Identifier
        ScVal::Object(Some(ScObject::Vec(
            vec![
                ScVal::Symbol("Account".try_into().unwrap()),
                ScVal::Object(Some(ScObject::AccountId(admin.clone()))),
            ]
            .try_into()
            .unwrap(),
        ))),
        // TokenMetadata
        ScVal::Object(Some(ScObject::Map(
            ScMap::sorted_from(vec![
                ScMapEntry {
                    key: ScVal::Symbol("decimals".try_into().unwrap()),
                    val: ScVal::U32(decimals),
                },
                ScMapEntry {
                    key: ScVal::Symbol("name".try_into().unwrap()),
                    val: ScVal::Object(Some(ScObject::Bytes(name.try_into().unwrap()))),
                },
                ScMapEntry {
                    key: ScVal::Symbol("symbol".try_into().unwrap()),
                    val: ScVal::Object(Some(ScObject::Bytes(symbol.try_into().unwrap()))),
                },
            ])
            .unwrap(),
        ))),
    ]
    .try_into()
    .unwrap()
}

fn build_init_op(contract_id: &Hash, parameters: ScVec) -> Result<Operation, Error> {
    Ok(Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            function: HostFunction::InvokeContract,
            parameters,
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: vec![
                    ContractData(LedgerKeyContractData {
                        contract_id: contract_id.clone(),
                        key: ScVal::Symbol("Admin".try_into().unwrap()),
                    }),
                    ContractData(LedgerKeyContractData {
                        contract_id: contract_id.clone(),
                        key: ScVal::Symbol("Metadata".try_into().unwrap()),
                    }),
                ]
                .try_into()?,
            },
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tx() {
        let contract_id = Hash([0u8; 32]);
        let salt = [0u8; 32];
        let op = build_create_token_op(&contract_id, salt);
        assert!(op.is_ok());
        let result = build_tx(
            op.unwrap(),
            300,
            1,
            "Public Global Stellar Network ; September 2015",
            &utils::parse_private_key("SBFGFF27Y64ZUGFAIG5AMJGQODZZKV2YQKAVUUN4HNE24XZXD2OEUVUP")
                .unwrap(),
        );

        assert!(result.is_ok());
    }
}
