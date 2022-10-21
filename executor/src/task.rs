use core::iter;
use jsonrpsee::ws_client::WsClient;
use serde::{Deserialize, Serialize};
use smoldot::{
    executor::{
        host::{Config, HeapPages, HostVmPrototype},
        runtime_host::{self, RuntimeHostVm},
        storage_diff,
    },
    json_rpc::methods::HexString,
};

use crate::runner_api::RpcApiClient;

#[derive(Serialize, Deserialize, Debug)]
pub enum TaskKind {
    Call,
    RuntimeVersion,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub kind: TaskKind,
    pub wasm: HexString,
    pub block_hash: HexString,
    pub calls: Option<Vec<(String, HexString)>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CallResponse {
	result: HexString,
	storage_diff: Vec<(HexString, Option<HexString>)>
}

#[derive(Serialize, Deserialize, Debug)]
pub enum TaskResponse {
    Call(CallResponse),
    RuntimeVersion(HexString),
	Error(String)
}

impl Task {
    pub async fn run(&self, task_id: u32, client: &WsClient) -> Result<(), jsonrpsee::core::Error> {
        let resp = match self.kind {
            TaskKind::Call => self.call(task_id, client).await,
            TaskKind::RuntimeVersion => self.runtime_version(task_id, client).await,
        }?;

        client.task_result(task_id, resp).await?;

        Ok(())
    }

    async fn call(
        &self,
        task_id: u32,
        client: &WsClient,
    ) -> Result<TaskResponse, jsonrpsee::core::Error> {
        let mut storage_top_trie_changes = storage_diff::StorageDiff::empty();
        let mut offchain_storage_changes = storage_diff::StorageDiff::empty();

        let vm_proto = HostVmPrototype::new(Config {
            module: &self.wasm,
            heap_pages: HeapPages::from(2048),
            exec_hint: smoldot::executor::vm::ExecHint::Oneshot,
            allow_unresolved_imports: false,
        })
        .unwrap();

		let mut ret: Vec<u8> = vec![];

        for (call, params) in self.calls.as_ref().unwrap() {
            let mut vm = runtime_host::run(runtime_host::Config {
                virtual_machine: vm_proto.clone(),
                function_to_call: &call,
                parameter: iter::once(params.as_ref()),
                top_trie_root_calculation_cache: None,
                storage_top_trie_changes,
                offchain_storage_changes,
            })
            .unwrap();

            println!("Calling {}", call);

            let res = loop {
                vm = match vm {
                    RuntimeHostVm::Finished(res) => {
                        break res;
                    }
                    RuntimeHostVm::StorageGet(req) => {
                        let key = req.key_as_vec();
                        let mut value = client
                            .storage_get(task_id, &self.block_hash, HexString(key))
                            .await?;
                        if let Some(val) = &value {
                            if val.0.is_empty() {
                                value = None;
                            }
                        }
                        req.inject_value(value.map(|v| iter::once(v.0)))
                    }
                    RuntimeHostVm::PrefixKeys(req) => {
                        let prefix = req.prefix().as_ref().to_vec();
                        if prefix.is_empty() {
                            // this must be coming from `ExternalStorageRoot` trying to get all keys in order to calculate storage root digest
                            // we are not going to fetch all the storages for that, so a dummy value is returned
                            // this means the storage root digest will be wrong, and failed the final check
                            // so we should just avoid doing final check by not supporting execute_block
                            req.inject_keys_ordered(iter::empty::<Vec<u8>>())
                        } else {
                            let keys = client
                                .prefix_keys(task_id, &self.block_hash, HexString(prefix))
                                .await?;
                            req.inject_keys_ordered(keys.into_iter().map(|v| v.0))
                        }
                    }
                    RuntimeHostVm::NextKey(req) => {
                        let key = req.key().as_ref().to_vec();
                        let next_key = client
                            .next_key(task_id, &self.block_hash, HexString(key))
                            .await?;
                        req.inject_key(next_key.map(|k| k.0))
                    }
                }
            };

            println!("Completed {}", call);

            let res = res.unwrap();

			ret = res.virtual_machine.value().as_ref().to_vec();

            storage_top_trie_changes = res.storage_top_trie_changes;
            offchain_storage_changes = res.offchain_storage_changes;
        }

        let diff = storage_top_trie_changes
            .diff_into_iter_unordered()
            .map(|(k, v)| (HexString(k), v.map(HexString)))
            .collect();

        Ok(TaskResponse::Call(CallResponse { result: HexString(ret), storage_diff: diff }))
    }

    async fn runtime_version(
        &self,
        _task_id: u32,
        _client: &WsClient,
    ) -> Result<TaskResponse, jsonrpsee::core::Error> {
        let vm_proto = HostVmPrototype::new(Config {
            module: &self.wasm,
            heap_pages: HeapPages::from(2048),
            exec_hint: smoldot::executor::vm::ExecHint::Oneshot,
            allow_unresolved_imports: false,
        })
        .unwrap();

        let resp = vm_proto.runtime_version();

        Ok(TaskResponse::RuntimeVersion(HexString(resp.as_ref().to_vec())))
    }
}