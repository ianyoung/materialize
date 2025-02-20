// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Rehydration of storage replicas.
//!
//! Rehydration is the process of bringing a crashed storage replica back up to
//! date. The [`RehydratingStorageClient`] records all commands it observes in a
//! minimal form. If it observes a send or receive failure while communicating
//! with the underlying client, it will reconnect the client and replay the
//! command stream.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use differential_dataflow::lattice::Lattice;
use futures::{Stream, StreamExt};
use mz_persist_types::codec_impls::UnitSchema;
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use tokio::pin;
use tokio::select;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::warn;

use mz_build_info::BuildInfo;
use mz_ore::retry::Retry;
use mz_ore::task::{AbortOnDropHandle, JoinHandleExt};
use mz_persist_client::cache::PersistClientCache;
use mz_persist_types::Codec64;
use mz_repr::{Diff, GlobalId};
use mz_service::client::GenericClient;

use crate::client::{
    CreateSinkCommand, CreateSourceCommand, StorageClient, StorageCommand, StorageGrpcClient,
    StorageResponse,
};
use crate::controller::ResumptionFrontierCalculator;
use crate::metrics::RehydratingStorageClientMetrics;
use crate::types::parameters::StorageParameters;
use crate::types::sources::SourceData;

/// A storage client that replays the command stream on failure.
///
/// See the [module documentation](self) for details.
#[derive(Debug)]
pub struct RehydratingStorageClient<T> {
    command_tx: UnboundedSender<RehydrationCommand<T>>,
    response_rx: UnboundedReceiverStream<StorageResponse<T>>,
    _task: AbortOnDropHandle<()>,
}

impl<T> RehydratingStorageClient<T>
where
    T: Timestamp + Lattice + Codec64,
    StorageGrpcClient: StorageClient<T>,
{
    /// Creates a `RehydratingStorageClient` that is not yet connected to
    /// a storage replica.
    pub fn new(
        build_info: &'static BuildInfo,
        persist: Arc<PersistClientCache>,
        metrics: RehydratingStorageClientMetrics,
    ) -> RehydratingStorageClient<T> {
        let (command_tx, command_rx) = unbounded_channel();
        let (response_tx, response_rx) = unbounded_channel();
        let mut task = RehydrationTask {
            build_info,
            command_rx,
            response_tx,
            sources: BTreeMap::new(),
            sinks: BTreeMap::new(),
            uppers: BTreeMap::new(),
            initialized: false,
            config: Default::default(),
            persist,
            metrics,
        };
        let task = mz_ore::task::spawn(|| "rehydration", async move { task.run().await });
        RehydratingStorageClient {
            command_tx,
            response_rx: UnboundedReceiverStream::new(response_rx),
            _task: task.abort_on_drop(),
        }
    }

    /// Connects to the storage replica at the specified network address.
    pub fn connect(&mut self, addr: String) {
        self.command_tx
            .send(RehydrationCommand::Connect { addr })
            .expect("rehydration task should not drop first");
    }

    /// Sends a command to the underlying client.
    pub fn send(&mut self, cmd: StorageCommand<T>) {
        self.command_tx
            .send(RehydrationCommand::Send(cmd))
            .expect("rehydration task should not drop first");
    }

    /// Returns a stream that produces responses from the underlying client.
    pub fn response_stream(&mut self) -> impl Stream<Item = StorageResponse<T>> + '_ {
        &mut self.response_rx
    }
}

#[derive(Debug, Clone)]
enum RehydrationCommand<T> {
    /// (Re)connect to a storage replica.
    Connect {
        /// The network address of the storage replica.
        addr: String,
    },
    /// Send the contained storage command to the replica.
    Send(StorageCommand<T>),
}

/// A task that manages rehydration.
struct RehydrationTask<T> {
    /// The build information for this process.
    build_info: &'static BuildInfo,
    /// A channel upon which commands intended for the storage replica are
    /// delivered.
    command_rx: UnboundedReceiver<RehydrationCommand<T>>,
    /// A channel upon which responses from the storage replica are delivered.
    response_tx: UnboundedSender<StorageResponse<T>>,
    /// The sources that have been observed.
    sources: BTreeMap<GlobalId, CreateSourceCommand<T>>,
    /// The exports that have been observed.
    sinks: BTreeMap<GlobalId, CreateSinkCommand<T>>,
    /// The upper frontier information received.
    uppers: BTreeMap<GlobalId, Antichain<T>>,
    /// Set to `true` once [`StorageCommand::InitializationComplete`] has been
    /// observed.
    initialized: bool,
    /// Storage configuration that has been observed.
    config: StorageParameters,
    /// A handle to Persist
    persist: Arc<PersistClientCache>,
    /// Prometheus metrics
    metrics: RehydratingStorageClientMetrics,
}

enum RehydrationTaskState {
    /// Wait for the address of the storage replica to connect to.
    AwaitAddress,
    /// The storage replica should be (re)hydrated.
    Rehydrate {
        /// The network address of the storage replica.
        addr: String,
    },
    /// Communication with the storage replica is live. Commands and responses
    /// should be forwarded until an error occurs.
    Pump {
        /// The network address of the storage replica.
        addr: String,
        /// The connected client for the replica.
        client: StorageGrpcClient,
    },
    /// The caller has asked us to shut down communication with this storage
    /// cluster.
    Done,
}

impl<T> RehydrationTask<T>
where
    T: Timestamp + Lattice + Codec64,
    StorageGrpcClient: StorageClient<T>,
{
    async fn run(&mut self) {
        let mut state = RehydrationTaskState::AwaitAddress;
        loop {
            state = match state {
                RehydrationTaskState::AwaitAddress => self.step_await_address().await,
                RehydrationTaskState::Rehydrate { addr } => self.step_rehydrate(addr).await,
                RehydrationTaskState::Pump { addr, client } => self.step_pump(addr, client).await,
                RehydrationTaskState::Done => break,
            }
        }
    }

    async fn step_await_address(&mut self) -> RehydrationTaskState {
        loop {
            match self.command_rx.recv().await {
                None => break RehydrationTaskState::Done,
                Some(RehydrationCommand::Connect { addr }) => {
                    break RehydrationTaskState::Rehydrate { addr }
                }
                Some(RehydrationCommand::Send(command)) => {
                    self.absorb_command(&command);
                }
            }
        }
    }

    async fn step_rehydrate(&mut self, addr: String) -> RehydrationTaskState {
        // Reconnect to the storage replica.
        let stream = Retry::default()
            .clamp_backoff(Duration::from_secs(1))
            .into_retry_stream();
        pin!(stream);
        let client = loop {
            let state = stream.next().await.expect("infinite stream");

            // Drain any pending commands, in case we've been told to connect
            // to a new storage replica.
            loop {
                match self.command_rx.try_recv() {
                    Ok(RehydrationCommand::Connect { addr }) => {
                        return RehydrationTaskState::Rehydrate { addr };
                    }
                    Ok(RehydrationCommand::Send(command)) => {
                        self.absorb_command(&command);
                    }
                    Err(TryRecvError::Disconnected) => return RehydrationTaskState::Done,
                    Err(TryRecvError::Empty) => break,
                }
            }

            let client = StorageGrpcClient::connect(
                addr.clone(),
                self.build_info.semver_version(),
                self.metrics.clone(),
            )
            .await;

            match client {
                Ok(client) => break client,
                Err(e) => {
                    if state.i >= mz_service::retry::INFO_MIN_RETRIES {
                        tracing::info!(
                            "error connecting to storage cluster {addr}, retrying in {:?}: {e}",
                            state.next_backoff.unwrap()
                        );
                    } else {
                        tracing::debug!(
                            "error connecting to storage cluster {addr}, retrying in {:?}: {e}",
                            state.next_backoff.unwrap()
                        );
                    }
                }
            }
        };

        for ingest in self.sources.values_mut() {
            let mut state = ingest.description.initialize_state(&self.persist).await;
            let resume_upper = ingest
                .description
                .calculate_resumption_frontier(&mut state)
                .await;
            ingest.resume_upper = resume_upper;
        }

        for export in self.sinks.values_mut() {
            let persist_client = self
                .persist
                .open(
                    export
                        .description
                        .from_storage_metadata
                        .persist_location
                        .clone(),
                )
                .await
                .expect("error creating persist client");
            let from_read_handle = persist_client
                .open_leased_reader::<SourceData, (), T, Diff>(
                    export.description.from_storage_metadata.data_shard,
                    "rehydration since",
                    // This is also `from_desc`, but this would be the _only_ usage
                    // of `from_desc` in storage, and we try to be consistent about
                    // where we get `RelationDesc`s for perist clients
                    Arc::new(
                        export
                            .description
                            .from_storage_metadata
                            .relation_desc
                            .clone(),
                    ),
                    Arc::new(UnitSchema),
                )
                .await
                .expect("from collection disappeared");

            let cached_as_of = &export.description.as_of;
            // The controller has the dependency recorded in it's `exported_collections` so this
            // should not change at least until the sink is started up (because the storage
            // controller will not downgrade the source's since).
            let from_since = from_read_handle.since();
            export.description.as_of = cached_as_of.maybe_fast_forward(from_since);
        }

        // Rehydrate all commands.
        let mut commands = vec![
            StorageCommand::UpdateConfiguration(self.config.clone()),
            StorageCommand::CreateSources(self.sources.values().cloned().collect()),
            StorageCommand::CreateSinks(self.sinks.values().cloned().collect()),
        ];
        if self.initialized {
            commands.push(StorageCommand::InitializationComplete)
        }
        self.send_commands(addr, client, commands).await
    }

    async fn step_pump(
        &mut self,
        addr: String,
        mut client: StorageGrpcClient,
    ) -> RehydrationTaskState {
        select! {
            // Command from controller to forward to storage cluster.
            command = self.command_rx.recv() => match command {
                None => RehydrationTaskState::Done,
                Some(RehydrationCommand::Connect { addr }) => RehydrationTaskState::Rehydrate { addr },
                Some(RehydrationCommand::Send(command)) => {
                    self.absorb_command(&command);
                    self.send_commands(addr, client, vec![command]).await
                }
            },
            // Response from storage cluster to forward to controller.
            response = client.recv() => {
                let response = match response.transpose() {
                    None => {
                        // In the future, if a storage cluster politely hangs
                        // up, we might want to take it as a signal that a new
                        // controller has taken over. For now we just try to
                        // reconnect.
                        Err(anyhow!("storage cluster unexpectedly gracefully terminated connection"))
                    }
                    Some(response) => response,
                };

                self.send_response(addr, client, response)
            }
        }
    }

    async fn send_commands(
        &mut self,
        addr: String,
        mut client: StorageGrpcClient,
        commands: impl IntoIterator<Item = StorageCommand<T>>,
    ) -> RehydrationTaskState {
        for command in commands {
            if let Err(e) = client.send(command).await {
                return self.send_response(addr, client, Err(e));
            }
        }
        RehydrationTaskState::Pump { addr, client }
    }

    fn send_response(
        &mut self,
        addr: String,
        client: StorageGrpcClient,
        response: Result<StorageResponse<T>, anyhow::Error>,
    ) -> RehydrationTaskState {
        match response {
            Ok(response) => {
                if let Some(response) = self.absorb_response(response) {
                    if self.response_tx.send(response).is_err() {
                        RehydrationTaskState::Done
                    } else {
                        RehydrationTaskState::Pump { addr, client }
                    }
                } else {
                    RehydrationTaskState::Pump { addr, client }
                }
            }
            Err(e) => {
                warn!("storage cluster produced error, reconnecting: {e}");
                RehydrationTaskState::Rehydrate { addr }
            }
        }
    }

    fn absorb_command(&mut self, command: &StorageCommand<T>) {
        match command {
            StorageCommand::InitializationComplete => self.initialized = true,
            StorageCommand::UpdateConfiguration(params) => {
                self.config.update(params.clone());
            }
            StorageCommand::CreateSources(ingestions) => {
                for ingestion in ingestions {
                    self.sources.insert(ingestion.id, ingestion.clone());
                    // Initialize the uppers we are tracking
                    for &export_id in ingestion.description.source_exports.keys() {
                        self.uppers
                            .insert(export_id, Antichain::from_elem(T::minimum()));
                    }
                }
            }
            StorageCommand::CreateSinks(exports) => {
                for export in exports {
                    self.sinks.insert(export.id, export.clone());
                    // Initialize the uppers we are tracking
                    self.uppers
                        .insert(export.id, Antichain::from_elem(T::minimum()));
                }
            }
            StorageCommand::AllowCompaction(_frontiers) => {}
        }
    }

    fn absorb_response(&mut self, response: StorageResponse<T>) -> Option<StorageResponse<T>> {
        match response {
            StorageResponse::FrontierUppers(list) => {
                let mut new_uppers = Vec::new();

                for (id, new_upper) in list {
                    let reported = match self.uppers.get_mut(&id) {
                        Some(reported) => reported,
                        None => panic!("Reference to absent collection: {id}"),
                    };
                    if PartialOrder::less_than(reported, &new_upper) {
                        reported.clone_from(&new_upper);
                        new_uppers.push((id, new_upper));
                    }
                }
                if !new_uppers.is_empty() {
                    Some(StorageResponse::FrontierUppers(new_uppers))
                } else {
                    None
                }
            }
            StorageResponse::DroppedIds(dropped_ids) => {
                for id in dropped_ids.iter() {
                    self.sources.remove(id);
                    self.sinks.remove(id);
                    self.uppers.remove(id);
                }
                Some(StorageResponse::DroppedIds(dropped_ids))
            }
            StorageResponse::StatisticsUpdates(source_stats, sink_stats) => {
                // Just forward it along.
                Some(StorageResponse::StatisticsUpdates(source_stats, sink_stats))
            }
        }
    }
}
