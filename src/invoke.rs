use std::num::ParseIntError;
use std::{fmt::Debug, fs, io, rc::Rc};

use clap::Parser;
use hex::FromHexError;
use soroban_env_host::xdr::{
    InvokeHostFunctionOp, LedgerFootprint, Memo, MuxedAccount, Operation, OperationBody,
    Preconditions, ScStatic, ScVec, SequenceNumber, Transaction, TransactionEnvelope,
    TransactionExt, VecM,
};
use soroban_env_host::{
    budget::{Budget, CostType},
    events::HostEvent,
    storage::Storage,
    xdr::{
        AccountId, Error as XdrError, HostFunction, PublicKey, ReadXdr, ScHostStorageErrorCode,
        ScObject, ScSpecEntry, ScStatus, ScVal, Uint256,
    },
    Host, HostError,
};
use soroban_spec::read::FromWasmError;
use stellar_strkey::StrkeyPublicKeyEd25519;

use crate::rpc::Client;
use crate::{
    rpc, snapshot,
    strval::{self, StrValError},
    utils,
};

#[derive(Parser, Debug)]
pub struct Cmd {
    /// Contract ID to invoke
    #[clap(long = "id")]
    contract_id: String,
    /// Account ID to invoke as
    #[clap(
        long = "account",
        default_value = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
        conflicts_with = "rpc-server-url"
    )]
    account_id: StrkeyPublicKeyEd25519,

    // TODO: as a workaround (RPC server doesn't yet implement getContractData)
    //       we allow supplying the wasm contract in the commandline
    //       later on we should add: conflicts_with = "rpc-server-url"
    /// WASM file to deploy to the contract ID and invoke
    #[clap(long, parse(from_os_str))]
    wasm: Option<std::path::PathBuf>,
    /// Function name to execute
    #[clap(long = "fn")]
    function: String,
    /// Argument to pass to the function
    #[clap(long = "arg", value_name = "arg", multiple = true)]
    args: Vec<String>,
    /// Argument to pass to the function (base64-encoded xdr)
    #[clap(long = "arg-xdr", value_name = "arg-xdr", multiple = true)]
    args_xdr: Vec<String>,
    /// Output the cost execution to stderr
    #[clap(long = "cost")]
    cost: bool,
    /// File to persist ledger state
    #[clap(
        long,
        parse(from_os_str),
        default_value(".soroban/ledger.json"),
        conflicts_with = "rpc-server-url"
    )]
    ledger_file: std::path::PathBuf,

    /// RPC server endpoint
    #[clap(
        long,
        conflicts_with = "account-id",
        requires = "secret-key",
        requires = "network-passphrase"
    )]
    rpc_server_url: Option<String>,
    /// Secret 'S' key used to sign the transaction sent to the rpc server
    #[clap(
        long = "secret-key",
        env = "SOROBAN_SECRET_KEY",
        requires = "rpc-server-url"
    )]
    secret_key: Option<String>,
    /// Network passphrase to sign the transaction sent to the rpc server
    #[clap(long = "network-passphrase", requires = "rpc-server-url")]
    network_passphrase: Option<String>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("parsing argument {arg}: {error}")]
    CannotParseArg { arg: String, error: StrValError },
    #[error("parsing XDR arg {arg}: {error}")]
    CannotParseXdrArg { arg: String, error: XdrError },
    #[error("cannot add contract to ledger entries: {0}")]
    CannotAddContractToLedgerEntries(XdrError),
    #[error(transparent)]
    // TODO: the Display impl of host errors is pretty user-unfriendly
    //       (it just calls Debug). I think we can do better than that
    Host(#[from] HostError),
    #[error("reading file {filepath}: {error}")]
    CannotReadLedgerFile {
        filepath: std::path::PathBuf,
        error: snapshot::Error,
    },
    #[error("reading file {filepath}: {error}")]
    CannotReadContractFile {
        filepath: std::path::PathBuf,
        error: io::Error,
    },
    #[error("committing file {filepath}: {error}")]
    CannotCommitLedgerFile {
        filepath: std::path::PathBuf,
        error: snapshot::Error,
    },
    #[error("cannot parse contract ID {contract_id}: {error}")]
    CannotParseContractId {
        contract_id: String,
        error: FromHexError,
    },
    #[error("function {0} was not found in the contract")]
    FunctionNotFoundInContractSpec(String),
    #[error("parsing contract spec: {0}")]
    CannotParseContractSpec(FromWasmError),
    #[error("unexpected number of arguments: {provided} (function {function} expects {expected} argument(s))")]
    UnexpectedArgumentCount {
        provided: usize,
        expected: usize,
        function: String,
    },
    #[error("function name {0} is too long")]
    FunctionNameTooLong(String),
    #[error("argument count ({current}) surpasses maximum allowed count ({maximum})")]
    MaxNumberOfArgumentsReached { current: usize, maximum: usize },
    #[error("cannot print result {result:?}: {error}")]
    CannotPrintResult { result: ScVal, error: StrValError },
    #[error("xdr processing error: {0}")]
    Xdr(#[from] XdrError),
    #[error("error parsing int: {0}")]
    ParseIntError(#[from] ParseIntError),
    #[error("cannot parse secret key")]
    CannotParseSecretKey,
    #[error(transparent)]
    Rpc(#[from] rpc::Error),
    #[error("unexpected contract code data type: {0:?}")]
    UnexpectedContractCodeDataType(ScVal),
}

#[derive(Clone, Debug)]
enum Arg {
    Arg(String),
    ArgXdr(String),
}

impl Cmd {
    fn build_host_function_parameters(
        &self,
        contract_id: [u8; 32],
        wasm: &[u8],
        matches: &clap::ArgMatches,
    ) -> Result<ScVec, Error> {
        // Get the function spec from the contract code
        let spec_entries =
            soroban_spec::read::from_wasm(wasm).map_err(Error::CannotParseContractSpec)?;
        let spec = spec_entries
            .iter()
            .find_map(|e| {
                if let ScSpecEntry::FunctionV0(f) = e {
                    if f.name.to_string_lossy() == self.function {
                        return Some(f);
                    }
                }
                None
            })
            .ok_or_else(|| Error::FunctionNotFoundInContractSpec(self.function.clone()))?;

        // Re-assemble the function args, to match the order given on the command line
        let indexed_args: Vec<(usize, Arg)> = matches
            .indices_of("args")
            .unwrap_or_default()
            .zip(self.args.iter())
            .map(|(a, b)| (a, Arg::Arg(b.to_string())))
            .collect();
        let indexed_args_xdr: Vec<(usize, Arg)> = matches
            .indices_of("args-xdr")
            .unwrap_or_default()
            .zip(self.args_xdr.iter())
            .map(|(a, b)| (a, Arg::ArgXdr(b.to_string())))
            .collect();
        let mut all_indexed_args: Vec<(usize, Arg)> = [indexed_args, indexed_args_xdr].concat();
        all_indexed_args.sort_by(|a, b| a.0.cmp(&b.0));

        // Parse the function arguments
        let inputs = &spec.inputs;
        if all_indexed_args.len() != inputs.len() {
            return Err(Error::UnexpectedArgumentCount {
                provided: all_indexed_args.len(),
                expected: inputs.len(),
                function: self.function.clone(),
            });
        }

        let parsed_args = all_indexed_args
            .iter()
            .zip(inputs.iter())
            .map(|(arg, input)| match &arg.1 {
                Arg::ArgXdr(s) => ScVal::from_xdr_base64(s).map_err(|e| Error::CannotParseXdrArg {
                    arg: s.clone(),
                    error: e,
                }),
                Arg::Arg(s) => {
                    strval::from_string(s, &input.type_).map_err(|e| Error::CannotParseArg {
                        arg: s.clone(),
                        error: e,
                    })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Add the contract ID and the function name to the arguments
        let mut complete_args = vec![
            ScVal::Object(Some(ScObject::Bytes(contract_id.try_into().unwrap()))),
            ScVal::Symbol(
                (&self.function)
                    .try_into()
                    .map_err(|_| Error::FunctionNameTooLong(self.function.clone()))?,
            ),
        ];
        complete_args.extend_from_slice(parsed_args.as_slice());
        let complete_args_len = complete_args.len();

        complete_args
            .try_into()
            .map_err(|_| Error::MaxNumberOfArgumentsReached {
                current: complete_args_len,
                maximum: ScVec::default().max_len(),
            })
    }

    pub async fn run(&self, matches: &clap::ArgMatches) -> Result<(), Error> {
        let contract_id: [u8; 32] =
            utils::contract_id_from_str(&self.contract_id).map_err(|e| {
                Error::CannotParseContractId {
                    contract_id: self.contract_id.clone(),
                    error: e,
                }
            })?;

        if self.rpc_server_url.is_some() {
            return self.run_against_rpc_server(contract_id, matches).await;
        }

        self.run_in_sandbox(contract_id, matches)
    }

    async fn run_against_rpc_server(
        &self,
        contract_id: [u8; 32],
        matches: &clap::ArgMatches,
    ) -> Result<(), Error> {
        let client = Client::new(self.rpc_server_url.as_ref().unwrap());
        let key = utils::parse_private_key(self.secret_key.as_ref().unwrap())
            .map_err(|_| Error::CannotParseSecretKey)?;

        // Get the account sequence number
        let public_strkey = StrkeyPublicKeyEd25519(key.public.to_bytes()).to_string();
        let account_details = client.get_account(&public_strkey).await?;
        // TODO: create a cmdline parameter for the fee instead of simply using the minimum fee
        let fee: u32 = 100;
        let sequence = account_details.sequence.parse::<i64>()?;

        // Get the contract
        let wasm = if let Some(f) = &self.wasm {
            // Get the contract from a file
            // TODO: as a workaround (RPC server doesn't yet implement getContractData)
            //       we allow supplying the contract in the commandline
            //       we should consider removing this later on
            fs::read(f).map_err(|e| Error::CannotReadContractFile {
                filepath: f.clone(),
                error: e,
            })?
        } else {
            // Get the contract from the network
            let contract_data = client
                .get_contract_data(
                    &hex::encode(contract_id),
                    ScVal::Static(ScStatic::LedgerKeyContractCode),
                )
                .await?;

            match ScVal::from_xdr_base64(contract_data.xdr)? {
                ScVal::Object(Some(ScObject::Bytes(bytes))) => bytes.to_vec(),
                scval => return Err(Error::UnexpectedContractCodeDataType(scval)),
            }
        };

        // Get the ledger footprint
        let host_function_params =
            self.build_host_function_parameters(contract_id, &wasm, matches)?;
        let tx_without_footprint = build_invoke_contract_tx(
            host_function_params.clone(),
            None,
            sequence + 1,
            fee,
            self.network_passphrase.as_ref().unwrap(),
            &key,
        )?;
        let simulation_response = client.simulate_transaction(&tx_without_footprint).await?;
        let footprint = LedgerFootprint::from_xdr_base64(simulation_response.footprint)?;

        // Send the final transaction with the actual footprint
        let tx = build_invoke_contract_tx(
            host_function_params,
            Some(footprint),
            sequence + 1,
            fee,
            self.network_passphrase.as_ref().unwrap(),
            &key,
        )?;

        client.send_transaction(&tx).await?;
        // TODO: print results
        // TODO: print cost

        Ok(())
    }

    fn run_in_sandbox(
        &self,
        contract_id: [u8; 32],
        matches: &clap::ArgMatches,
    ) -> Result<(), Error> {
        // Initialize storage and host
        // TODO: allow option to separate input and output file
        let mut state =
            snapshot::read(&self.ledger_file).map_err(|e| Error::CannotReadLedgerFile {
                filepath: self.ledger_file.clone(),
                error: e,
            })?;

        // If a file is specified, deploy the contract to storage
        if let Some(f) = &self.wasm {
            let contract = fs::read(f).map_err(|e| Error::CannotReadContractFile {
                filepath: f.clone(),
                error: e,
            })?;
            utils::add_contract_to_ledger_entries(&mut state.1, contract_id, contract)
                .map_err(Error::CannotAddContractToLedgerEntries)?;
        }

        let snap = Rc::new(snapshot::Snap {
            ledger_entries: state.1.clone(),
        });
        let mut storage = Storage::with_recording_footprint(snap);
        let wasm = utils::get_contract_wasm_from_storage(&mut storage, contract_id)?;
        let h = Host::with_storage_and_budget(storage, Budget::default());

        h.set_source_account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            self.account_id.0,
        ))));

        let mut ledger_info = state.0.clone();
        ledger_info.sequence_number += 1;
        ledger_info.timestamp += 5;
        h.set_ledger_info(ledger_info.clone());

        let host_function_params =
            self.build_host_function_parameters(contract_id, &wasm, matches)?;

        let res = h.invoke_function(HostFunction::InvokeContract, host_function_params)?;
        let res_str = strval::to_string(&res).map_err(|e| Error::CannotPrintResult {
            result: res,
            error: e,
        })?;

        println!("{}", res_str);

        let (storage, budget, events) = h.try_finish().map_err(|_h| {
            HostError::from(ScStatus::HostStorageError(
                ScHostStorageErrorCode::UnknownError,
            ))
        })?;

        if self.cost {
            eprintln!("Cpu Insns: {}", budget.get_cpu_insns_count());
            eprintln!("Mem Bytes: {}", budget.get_mem_bytes_count());
            for cost_type in CostType::variants() {
                eprintln!("Cost ({:?}): {}", cost_type, budget.get_input(*cost_type));
            }
        }

        for (i, event) in events.0.iter().enumerate() {
            eprint!("#{}: ", i);
            match event {
                HostEvent::Contract(e) => {
                    eprintln!("event: {}", serde_json::to_string(&e).unwrap());
                }
                HostEvent::Debug(e) => eprintln!("debug: {}", e),
            }
        }

        snapshot::commit(state.1, ledger_info, &storage.map, &self.ledger_file).map_err(|e| {
            Error::CannotCommitLedgerFile {
                filepath: self.ledger_file.clone(),
                error: e,
            }
        })?;
        Ok(())
    }
}

fn build_invoke_contract_tx(
    parameters: ScVec,
    footprint: Option<LedgerFootprint>,
    sequence: i64,
    fee: u32,
    network_passphrase: &str,
    key: &ed25519_dalek::Keypair,
) -> Result<TransactionEnvelope, Error> {
    // Use a default footprint if none provided
    let final_footprint = footprint.unwrap_or(LedgerFootprint {
        read_only: VecM::default(),
        read_write: VecM::default(),
    });
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            function: HostFunction::InvokeContract,
            parameters,
            footprint: final_footprint,
        }),
    };
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
