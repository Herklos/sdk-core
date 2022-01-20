//! This crate contains testing functionality that can be useful when building SDKs against Core,
//! or even when testing workflows written in SDKs that use Core.

pub mod canned_histories;

use futures::{stream::FuturesUnordered, StreamExt};
use log::LevelFilter;
use prost::Message;
use rand::{distributions::Standard, Rng};
use std::{
    convert::TryFrom, env, future::Future, net::SocketAddr, path::PathBuf, sync::Arc,
    time::Duration,
};
use temporal_sdk::TestRustWorker;
use temporal_sdk_core::{
    replay::{init_core_replay, ReplayCore},
    CoreInitOptions, CoreInitOptionsBuilder, ServerGatewayOptions, ServerGatewayOptionsBuilder,
    TelemetryOptions, TelemetryOptionsBuilder, WorkerConfig, WorkerConfigBuilder,
};
use temporal_sdk_core_api::Core;
use temporal_sdk_core_protos::{
    coresdk::{
        workflow_commands::{
            workflow_command, ActivityCancellationType, CompleteWorkflowExecution,
            ScheduleActivity, StartTimer,
        },
        workflow_completion::WorkflowActivationCompletion,
    },
    temporal::api::history::v1::History,
};
use url::Url;

pub const NAMESPACE: &str = "default";
pub const TEST_Q: &str = "q";
/// If set, turn export traces and metrics to the OTel collector at the given URL
const OTEL_URL_ENV_VAR: &str = "TEMPORAL_INTEG_OTEL_URL";
/// If set, enable direct scraping of prom metrics on the specified port
const PROM_ENABLE_ENV_VAR: &str = "TEMPORAL_INTEG_PROM_PORT";

/// Create a core instance which will use the provided test name to base the task queue and wf id
/// upon. Returns the instance and the task queue name (which is also the workflow id).
pub async fn init_core_and_create_wf(test_name: &str) -> (Arc<dyn Core>, String) {
    let mut starter = CoreWfStarter::new(test_name);
    let core = starter.get_core().await;
    starter.start_wf().await;
    (core, starter.get_task_queue().to_string())
}

/// Create a core replay instance preloaded with just one provided history. Returns the core impl
/// and the task queue name as in [init_core_and_create_wf].
pub fn init_core_replay_preloaded(test_name: &str, history: &History) -> (Arc<dyn Core>, String) {
    let replay_core = init_core_replay(get_integ_telem_options());
    let worker_cfg = WorkerConfigBuilder::default()
        .task_queue(test_name)
        .build()
        .expect("Configuration options construct properly");
    replay_core
        .make_replay_worker(worker_cfg, history)
        .expect("Worker registration works");
    (Arc::new(replay_core), test_name.to_string())
}

/// Load history from a file containing the protobuf serialization of it
pub async fn history_from_proto_binary(path_from_root: &str) -> Result<History, anyhow::Error> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("..");
    path.push(path_from_root);
    let bytes = tokio::fs::read(path).await?;
    Ok(History::decode(&*bytes)?)
}

/// Implements a builder pattern to help integ tests initialize core and create workflows
pub struct CoreWfStarter {
    /// Used for both the task queue and workflow id
    task_queue_name: String,
    core_options: CoreInitOptions,
    worker_config: WorkerConfig,
    wft_timeout: Option<Duration>,
    initted_core: Option<Arc<dyn Core>>,
}

impl CoreWfStarter {
    pub fn new(test_name: &str) -> Self {
        let rand_bytes: Vec<u8> = rand::thread_rng().sample_iter(&Standard).take(6).collect();
        let task_q_salt = base64::encode(rand_bytes);
        let task_queue = format!("{}_{}", test_name, task_q_salt);
        Self::new_tq_name(&task_queue)
    }

    pub fn new_tq_name(task_queue: &str) -> Self {
        Self {
            task_queue_name: task_queue.to_owned(),
            core_options: CoreInitOptionsBuilder::default()
                .gateway_opts(get_integ_server_options())
                .telemetry_opts(get_integ_telem_options())
                .build()
                .unwrap(),
            worker_config: WorkerConfigBuilder::default()
                .task_queue(task_queue)
                .max_cached_workflows(1000_usize)
                .build()
                .unwrap(),
            wft_timeout: None,
            initted_core: None,
        }
    }

    pub async fn worker(&mut self) -> TestRustWorker {
        TestRustWorker::new(
            self.get_core().await,
            self.worker_config.task_queue.clone(),
            self.wft_timeout,
        )
    }

    pub async fn shutdown(&mut self) {
        self.get_core().await.shutdown().await;
    }

    pub async fn get_core(&mut self) -> Arc<dyn Core> {
        if self.initted_core.is_none() {
            let core = temporal_sdk_core::init(self.core_options.clone())
                .await
                .unwrap();
            // Register a worker for the task queue
            core.register_worker(self.worker_config.clone()).unwrap();
            self.initted_core = Some(Arc::new(core));
        }
        self.initted_core.as_ref().unwrap().clone()
    }

    /// Start the workflow defined by the builder and return run id
    pub async fn start_wf(&self) -> String {
        self.start_wf_with_id(self.task_queue_name.clone()).await
    }

    pub async fn start_wf_with_id(&self, workflow_id: String) -> String {
        self.initted_core
            .as_ref()
            .expect(
                "Core must be initted before starting a workflow.\
                             Tests must call `get_core` first.",
            )
            .as_ref()
            .server_gateway()
            .start_workflow(
                vec![],
                self.worker_config.task_queue.clone(),
                workflow_id,
                self.task_queue_name.clone(),
                self.wft_timeout,
            )
            .await
            .unwrap()
            .run_id
    }

    /// Fetch the history for the indicated workflow and replay it using the provided worker.
    /// Can be used after completing workflows normally to ensure replay works as well.
    pub async fn fetch_history_and_replay(
        &mut self,
        wf_id: impl Into<String>,
        run_id: impl Into<String>,
        worker: &mut TestRustWorker,
    ) -> Result<(), anyhow::Error> {
        // Fetch history and replay it
        let history = self
            .get_core()
            .await
            .server_gateway()
            .get_workflow_execution_history(wf_id.into(), Some(run_id.into()), vec![])
            .await?
            .history
            .expect("history field must be populated");
        let (replay_core, _) = init_core_replay_preloaded(worker.task_queue(), &history);
        let mut replay_worker = worker.swap_core(replay_core);
        replay_worker.incr_expected_run_count(1);
        replay_worker.run_until_done().await.unwrap();
        Ok(())
    }

    pub fn get_task_queue(&self) -> &str {
        &self.worker_config.task_queue
    }

    pub fn get_wf_id(&self) -> &str {
        &self.task_queue_name
    }

    pub fn max_cached_workflows(&mut self, num: usize) -> &mut Self {
        self.worker_config.max_cached_workflows = num;
        self
    }

    pub fn max_wft(&mut self, max: usize) -> &mut Self {
        self.worker_config.max_outstanding_workflow_tasks = max;
        self
    }

    pub fn max_at(&mut self, max: usize) -> &mut Self {
        self.worker_config.max_outstanding_activities = max;
        self
    }

    pub fn max_local_at(&mut self, max: usize) -> &mut Self {
        self.worker_config.max_outstanding_local_activities = max;
        self
    }

    pub fn max_at_polls(&mut self, max: usize) -> &mut Self {
        self.worker_config.max_concurrent_at_polls = max;
        self
    }

    pub fn wft_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.wft_timeout = Some(timeout);
        self
    }
}

pub fn get_integ_server_options() -> ServerGatewayOptions {
    let temporal_server_address = match env::var("TEMPORAL_SERVICE_ADDRESS") {
        Ok(addr) => addr,
        Err(_) => "http://localhost:7233".to_owned(),
    };
    let url = Url::try_from(&*temporal_server_address).unwrap();
    ServerGatewayOptionsBuilder::default()
        .namespace(NAMESPACE.to_string())
        .identity("integ_tester".to_string())
        .worker_binary_id("fakebinaryid".to_string())
        .target_url(url)
        .client_name("temporal-core".to_string())
        .client_version("0.1.0".to_string())
        .build()
        .unwrap()
}

pub fn get_integ_telem_options() -> TelemetryOptions {
    let mut ob = TelemetryOptionsBuilder::default();
    if let Some(url) = env::var(OTEL_URL_ENV_VAR)
        .ok()
        .map(|x| x.parse::<Url>().unwrap())
    {
        ob.otel_collector_url(url);
    }
    if let Some(addr) = env::var(PROM_ENABLE_ENV_VAR)
        .ok()
        .map(|x| SocketAddr::new([127, 0, 0, 1].into(), x.parse().unwrap()))
    {
        ob.prometheus_export_bind_address(addr);
    }
    ob.tracing_filter(env::var("RUST_LOG").unwrap_or_else(|_| "temporal_sdk_core=INFO".to_string()))
        .log_forwarding_level(LevelFilter::Off)
        .build()
        .unwrap()
}

pub fn schedule_activity_cmd(
    seq: u32,
    task_q: &str,
    activity_id: &str,
    cancellation_type: ActivityCancellationType,
    activity_timeout: Duration,
    heartbeat_timeout: Duration,
) -> workflow_command::Variant {
    ScheduleActivity {
        seq,
        activity_id: activity_id.to_string(),
        activity_type: "test_activity".to_string(),
        namespace: NAMESPACE.to_owned(),
        task_queue: task_q.to_owned(),
        schedule_to_start_timeout: Some(activity_timeout.into()),
        start_to_close_timeout: Some(activity_timeout.into()),
        schedule_to_close_timeout: Some(activity_timeout.into()),
        heartbeat_timeout: Some(heartbeat_timeout.into()),
        cancellation_type: cancellation_type as i32,
        ..Default::default()
    }
    .into()
}

pub fn start_timer_cmd(seq: u32, duration: Duration) -> workflow_command::Variant {
    StartTimer {
        seq,
        start_to_fire_timeout: Some(duration.into()),
    }
    .into()
}

/// Given a desired number of concurrent executions and a provided function that produces a future,
/// run that many instances of the future concurrently.
///
/// Annoyingly, because of a sorta-bug in the way async blocks work, the async block produced by
/// the closure must be `async move` if it uses the provided iteration number. On the plus side,
/// since you're usually just accessing core in the closure, if core is a reference everything just
/// works. See <https://github.com/rust-lang/rust/issues/81653>
pub async fn fanout_tasks<FutureMaker, Fut>(num: usize, fm: FutureMaker)
where
    FutureMaker: Fn(usize) -> Fut,
    Fut: Future,
{
    let mut tasks = FuturesUnordered::new();
    for i in 0..num {
        tasks.push(fm(i));
    }

    while tasks.next().await.is_some() {}
}

#[async_trait::async_trait]
pub trait CoreTestHelpers {
    async fn complete_execution(&self, task_q: &str, run_id: &str);
    async fn complete_timer(&self, task_q: &str, run_id: &str, seq: u32, duration: Duration);
}

#[async_trait::async_trait]
impl<T> CoreTestHelpers for T
where
    T: Core + ?Sized,
{
    async fn complete_execution(&self, task_q: &str, run_id: &str) {
        self.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
            task_q.to_string(),
            run_id.to_string(),
            vec![CompleteWorkflowExecution { result: None }.into()],
        ))
        .await
        .unwrap();
    }

    async fn complete_timer(&self, task_q: &str, run_id: &str, seq: u32, duration: Duration) {
        self.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
            task_q.to_string(),
            run_id.to_string(),
            vec![StartTimer {
                seq,
                start_to_fire_timeout: Some(duration.into()),
            }
            .into()],
        ))
        .await
        .unwrap();
    }
}