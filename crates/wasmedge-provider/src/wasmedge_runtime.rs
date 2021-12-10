use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, instrument, trace, warn};

use tempfile::NamedTempFile;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use kubelet::container::Handle as ContainerHandle;
use kubelet::container::Status;
use kubelet::handle::StopHandler;

use wasmedge_sys::{vm::Vm, Config, ImportObj, Module};

pub struct Runtime {
    handle: JoinHandle<anyhow::Result<()>>,
}

#[async_trait::async_trait]
impl StopHandler for Runtime {
    async fn stop(&mut self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "Currently, WasmEdge do not support interruput"
        ))
    }

    async fn wait(&mut self) -> anyhow::Result<()> {
        (&mut self.handle).await??;
        Ok(())
    }
}

/// WasmedgeRuntime provides a WASI compatible runtime.
/// A runtime should be used for each "instance" of a process.
pub struct WasmedgeRuntime {
    /// name of the process
    name: String,
    /// Data needed for the runtime
    data: Arc<Data>,
    /// The tempfile that output from the wasmtime process writes to
    output: Arc<NamedTempFile>,
    /// A channel to send status updates on the runtime
    status_sender: Sender<Status>,
}

// Configuration for WASI http.
#[derive(Clone, Default)]
pub struct WasiHttpConfig {
    pub allowed_domains: Option<Vec<String>>,
    pub max_concurrent_requests: Option<u32>,
}

struct Data {
    /// binary module data to be run as a wasm module
    module_data: Vec<u8>,
    /// key/value environment variables made available to the wasm process
    env: HashMap<String, String>,
    /// the arguments passed as the command-line arguments list
    args: Vec<String>,
    /// a hash map of local file system paths to optional path names in the runtime
    /// (e.g. /tmp/foo/myfile -> /app/config). If the optional value is not given,
    /// the same path will be allowed in the runtime
    dirs: HashMap<PathBuf, Option<PathBuf>>,
}

/// Holds our tempfile handle.
pub struct HandleFactory {
    temp: Arc<NamedTempFile>,
}

impl kubelet::log::HandleFactory<tokio::fs::File> for HandleFactory {
    /// Creates `tokio::fs::File` on demand for log reading.
    fn new_handle(&self) -> tokio::fs::File {
        tokio::fs::File::from_std(self.temp.reopen().unwrap())
    }
}

impl WasmedgeRuntime {
    /// Creates a new WasmedgeRuntime
    ///
    /// # Arguments
    ///
    /// * `module_path` - the path to the WebAssembly binary
    /// * `env` - a collection of key/value pairs containing the environment variables
    /// * `args` - the arguments passed as the command-line arguments list
    /// * `dirs` - a map of local file system paths to optional path names in the runtime
    ///     (e.g. /tmp/foo/myfile -> /app/config). If the optional value is not given,
    ///     the same path will be allowed in the runtime
    /// * `log_dir` - location for storing logs
    #[allow(clippy::too_many_arguments)]
    pub async fn new<L: AsRef<Path> + Send + Sync + 'static>(
        name: String,
        module_data: Vec<u8>,
        env: HashMap<String, String>,
        args: Vec<String>,
        dirs: HashMap<PathBuf, Option<PathBuf>>,
        log_dir: L,
        status_sender: Sender<Status>,
        _http_config: WasiHttpConfig,
    ) -> anyhow::Result<Self> {
        let temp = tokio::task::spawn_blocking(move || -> anyhow::Result<NamedTempFile> {
            Ok(NamedTempFile::new_in(log_dir)?)
        })
        .await??;

        // We need to use named temp file because we need multiple file handles
        // and if we are running in the temp dir, we run the possibility of the
        // temp file getting cleaned out from underneath us while running. If we
        // think it necessary, we can make these permanent files with a cleanup
        // loop that runs elsewhere. These will get deleted when the reference
        // is dropped
        Ok(WasmedgeRuntime {
            name,
            data: Arc::new(Data {
                module_data,
                env,
                args,
                dirs,
            }),
            output: Arc::new(temp),
            status_sender,
        })
    }

    pub async fn start(&self) -> anyhow::Result<ContainerHandle<Runtime, HandleFactory>> {
        let temp = self.output.clone();
        // Because a reopen is blocking, run in a blocking task to get new
        // handles to the tempfile
        let output_write = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
            Ok(temp.reopen()?)
        })
        .await??;

        let handle = self
            .spawn_wasmedge(tokio::fs::File::from_std(output_write))
            .await?;

        let log_handle_factory = HandleFactory {
            temp: self.output.clone(),
        };

        Ok(ContainerHandle::new(Runtime { handle }, log_handle_factory))
    }

    // Spawns a running wasmedge instance with the given context and status
    // channel.
    #[instrument(level = "info", skip(self, _output_write), fields(name = %self.name))]
    async fn spawn_wasmedge(
        &self,
        _output_write: tokio::fs::File,
    ) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
        // Clone the module data Arc so it can be moved
        let data = self.data.clone();
        let status_sender = self.status_sender.clone();

        // Log this info here so it isn't on _every_ log line
        trace!(env = ?data.env, args = ?data.args, dirs = ?data.dirs, "Starting setup of wasmtime module");

        let name = self.name.clone();
        let module_data = data.module_data.clone();

        let handle = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let span = tracing::info_span!("wasmedge_vm_run", %name);
            let _enter = span.enter();

            let envs: Vec<String> = data
                .env
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();

            // Note:
            // currently the Wasmedge VM is not thread safe, so we create VM in each thread.
            //
            // TODO:
            // There are two important feature to refactor this and reuse VM
            // One is implement Sync, Send for VM
            // The other is implement wasi unload and reload for VM
            let config = Config::create()
                .map_err(|e| anyhow::anyhow!("wasmedge config create fail: {:?}", e))?;

            let import_obj =
                ImportObj::create_wasi(None, Some(envs.iter()), None).map_err(|e| {
                    anyhow::anyhow!("Wasmedge load buffer to wasm module fail: {:?}", e)
                })?;
            let mut module = Module::load_from_buffer(&config, &module_data).map_err(|e| {
                anyhow::anyhow!("Wasmedge load buffer to wasm module fail: {:?}", e)
            })?;

            // TODO
            // handle stdout, stderr here
            let mut vm = Vm::create(Some(&config), None)
                .expect("Wasmedge create fail")
                .register_module_from_import(import_obj)
                .map_err(|e| anyhow::anyhow!("Wasmedge import wasi fail: {:?}", e))?
                .load_wasm_from_ast_module(&mut module)
                .map_err(|e| anyhow::anyhow!("Wasmedge vm load module fail: {:?}", e))?
                .validate()
                .map_err(|e| anyhow::anyhow!("Wasmedge validate fail: {:?}", e))?
                .instantiate()
                .map_err(|e| anyhow::anyhow!("Wasmedge instantiate fail: {:?}", e))?;

            let params = vec![];
            let result = vm
                .run("_start", &params)
                .map_err(|e| anyhow::anyhow!("Wasmedge vm run fail: {:?}", e));

            info!("module run complete");
            send(
                &status_sender,
                &name,
                Status::Terminated {
                    failed: result.is_err(), // GetExitCode
                    message: "Module run completed".into(),
                    timestamp: chrono::Utc::now(),
                },
            );
            Ok(())
        });
        Ok(handle)
    }
}

#[instrument(level = "info", skip(sender, status))]
fn send(sender: &Sender<Status>, name: &str, status: Status) {
    match sender.blocking_send(status) {
        Err(e) => warn!(error = %e, "error sending wasi status"),
        Ok(_) => debug!("send completed"),
    }
}
