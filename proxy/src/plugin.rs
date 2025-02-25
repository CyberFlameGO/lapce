use anyhow::{anyhow, Result};
use home::home_dir;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::io::Read;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use toml;
use wasmer::ChainableNamedResolver;
use wasmer::ImportObject;
use wasmer::Store;
use wasmer::WasmerEnv;
use wasmer_wasi::Pipe;
use wasmer_wasi::WasiEnv;
use wasmer_wasi::WasiState;
use xi_rpc::Handler;
use xi_rpc::RpcLoop;
use xi_rpc::RpcPeer;

use crate::buffer::BufferId;
use crate::core_proxy::CoreProxy;
use crate::dispatch::Dispatcher;

pub type PluginName = String;

#[derive(Clone, Debug, Default)]
pub struct Counter(usize);

impl Counter {
    pub fn next(&mut self) -> usize {
        let n = self.0;
        self.0 = n + 1;
        n + 1
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct PluginId(pub usize);

#[derive(Deserialize, Clone)]
pub struct PluginDescription {
    pub name: String,
    pub version: String,
    pub exec_path: PathBuf,
    dir: Option<PathBuf>,
    configuration: Option<Value>,
}

#[derive(WasmerEnv, Clone)]
pub(crate) struct PluginEnv {
    wasi_env: WasiEnv,
    dispatcher: Dispatcher,
}

pub(crate) struct PluginNew {
    instance: wasmer::Instance,
    env: PluginEnv,
}

pub struct Plugin {
    id: PluginId,
    dispatcher: Dispatcher,
    configuration: Option<Value>,
    peer: RpcPeer,
    name: String,
    process: Child,
}

pub struct PluginCatalog {
    id_counter: Counter,
    items: HashMap<PluginName, PluginDescription>,
    plugins: HashMap<PluginId, PluginNew>,
    store: wasmer::Store,
}

impl PluginCatalog {
    pub fn new() -> PluginCatalog {
        PluginCatalog {
            id_counter: Counter::default(),
            items: HashMap::new(),
            plugins: HashMap::new(),
            store: wasmer::Store::default(),
        }
    }

    pub fn reload(&mut self) {
        eprintln!("plugin reload from paths");
        self.items.clear();
        self.plugins.clear();
        self.load();
    }

    pub fn load(&mut self) {
        let all_manifests = find_all_manifests();
        for manifest_path in &all_manifests {
            match load_manifest(manifest_path) {
                Err(e) => eprintln!("load manifest err {}", e),
                Ok(manifest) => {
                    self.items.insert(manifest.name.clone(), manifest);
                }
            }
        }
    }

    pub fn start_all(&mut self, dispatcher: Dispatcher) {
        for (_, manifest) in self.items.clone().iter() {
            if let Ok(plugin) =
                self.start_plugin(dispatcher.clone(), manifest.clone())
            {
                let id = self.next_plugin_id();
                self.plugins.insert(id, plugin);
            }
        }
    }

    fn start_plugin(
        &mut self,
        dispatcher: Dispatcher,
        plugin_desc: PluginDescription,
    ) -> Result<PluginNew> {
        let module = wasmer::Module::from_file(&self.store, plugin_desc.exec_path)?;

        let output = Pipe::new();
        let input = Pipe::new();
        let mut wasi_env = WasiState::new("Lapce")
            .stdin(Box::new(input))
            .stdout(Box::new(output))
            .finalize()?;
        let wasi = wasi_env.import_object(&module)?;

        let plugin_env = PluginEnv {
            wasi_env,
            dispatcher,
        };
        let lapce = lapce_exports(&self.store, &plugin_env);
        let instance = wasmer::Instance::new(&module, &lapce.chain_back(wasi))?;

        let initialize = instance.exports.get_function("initialize")?;
        wasi_write_object(
            &plugin_env.wasi_env,
            &plugin_desc.configuration.unwrap_or(serde_json::json!({})),
        );
        initialize.call(&[])?;

        Ok(PluginNew {
            instance,
            env: plugin_env,
        })
    }

    pub fn next_plugin_id(&mut self) -> PluginId {
        PluginId(self.id_counter.next())
    }
}

pub(crate) fn lapce_exports(store: &Store, plugin_env: &PluginEnv) -> ImportObject {
    macro_rules! lapce_export {
        ($($host_function:ident),+ $(,)?) => {
            wasmer::imports! {
                "lapce" => {
                    $(stringify!($host_function) =>
                        wasmer::Function::new_native_with_env(store, plugin_env.clone(), $host_function),)+
                }
            }
        }
    }

    lapce_export! {
        host_handle_notification,
    }
}

fn host_handle_notification(plugin_env: &PluginEnv) {
    let notification: Result<PluginNotification> =
        wasi_read_object(&plugin_env.wasi_env);
    if let Ok(notification) = notification {
        match notification {
            PluginNotification::StartLspServer {
                exec_path,
                language_id,
                options,
            } => {
                plugin_env.dispatcher.lsp.lock().start_server(
                    &exec_path,
                    &language_id,
                    options.clone(),
                );
            }
        }
    }
}

pub fn wasi_read_string(wasi_env: &WasiEnv) -> Result<String> {
    let mut state = wasi_env.state();
    let wasi_file = state
        .fs
        .stdout_mut()?
        .as_mut()
        .ok_or(anyhow!("can't get stdout"))?;
    let mut buf = String::new();
    wasi_file.read_to_string(&mut buf)?;
    Ok(buf)
}

pub fn wasi_read_object<T: DeserializeOwned>(wasi_env: &WasiEnv) -> Result<T> {
    let json = wasi_read_string(wasi_env)?;
    Ok(serde_json::from_str(&json)?)
}

pub fn wasi_write_string(wasi_env: &WasiEnv, buf: &str) {
    let mut state = wasi_env.state();
    let wasi_file = state.fs.stdin_mut().unwrap().as_mut().unwrap();
    writeln!(wasi_file, "{}\r", buf).unwrap();
}

pub fn wasi_write_object(wasi_env: &WasiEnv, object: &(impl Serialize + ?Sized)) {
    wasi_write_string(wasi_env, &serde_json::to_string(&object).unwrap());
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "method", content = "params")]
pub enum PluginNotification {
    StartLspServer {
        exec_path: String,
        language_id: String,
        options: Option<Value>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum PluginRequest {}

pub struct PluginHandler {
    dispatcher: Dispatcher,
}

impl Handler for PluginHandler {
    type Notification = PluginNotification;
    type Request = PluginRequest;

    fn handle_notification(
        &mut self,
        ctx: &xi_rpc::RpcCtx,
        rpc: Self::Notification,
    ) {
        match &rpc {
            PluginNotification::StartLspServer {
                exec_path,
                language_id,
                options,
            } => {
                self.dispatcher.lsp.lock().start_server(
                    exec_path,
                    language_id,
                    options.clone(),
                );
            }
        }
    }

    fn handle_request(
        &mut self,
        ctx: &xi_rpc::RpcCtx,
        rpc: Self::Request,
    ) -> Result<serde_json::Value, xi_rpc::RemoteError> {
        Err(xi_rpc::RemoteError::InvalidRequest(None))
    }
}

impl Plugin {
    pub fn initialize(&self) {
        self.peer.send_rpc_notification(
            "initialize",
            &json!({
                "plugin_id": self.id,
                "configuration": self.configuration,
            }),
        )
    }
}

fn find_all_manifests() -> Vec<PathBuf> {
    let mut manifest_paths = Vec::new();
    let home = home_dir().unwrap();
    let path = home.join(".lapce").join("plugins");
    path.read_dir().map(|dir| {
        dir.flat_map(|item| item.map(|p| p.path()).ok())
            .map(|dir| dir.join("manifest.toml"))
            .filter(|f| f.exists())
            .for_each(|f| manifest_paths.push(f))
    });
    eprintln!("proxy mainfiest paths {:?}", manifest_paths);
    manifest_paths
}

fn load_manifest(path: &PathBuf) -> Result<PluginDescription> {
    let mut file = fs::File::open(&path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut manifest: PluginDescription = toml::from_str(&contents)?;
    // normalize relative paths
    //manifest.dir = Some(path.parent().unwrap().canonicalize()?);
    //    if manifest.exec_path.starts_with("./") {
    manifest.dir = Some(path.parent().unwrap().canonicalize()?);
    manifest.exec_path = path
        .parent()
        .unwrap()
        .join(manifest.exec_path)
        .canonicalize()?;
    //   }
    Ok(manifest)
}
