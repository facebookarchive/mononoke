/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use std::{sync::Arc, time::Instant};

use anyhow::{bail, format_err, Error};
use clap::Arg;
use cloned::cloned;
use fbinit::FacebookInit;
use futures::{future, stream::Stream, Future, IntoFuture};
use futures_ext::FutureExt;
use serde_derive::{Deserialize, Serialize};
use tokio_compat::runtime;

use blobstore::Blobstore;
use blobstore_sync_queue::{BlobstoreSyncQueue, BlobstoreSyncQueueEntry, SqlBlobstoreSyncQueue};
use cmdlib::args;
use context::CoreContext;
use manifoldblob::{ManifoldRange, ThriftManifoldBlob};
use metaconfig_types::{BlobConfig, BlobstoreId, MetadataDBConfig, StorageConfig};
use mononoke_types::{BlobstoreBytes, DateTime, RepositoryId};
use sql_ext::{MysqlOptions, SqlConstructors};

/// Save manifold continuation token each once per `PRESERVE_STATE_RATIO` entries
const PRESERVE_STATE_RATIO: usize = 10_000;
/// PRESERVE_STATE_RATIO should be divisible by CHUNK_SIZE as otherwise progress
/// reporting will be broken
const CHUNK_SIZE: usize = 5000;
const INIT_COUNT_VALUE: usize = 0;

const FLAT_NAMESPACE_PREFIX: &str = "flat/";

#[derive(Debug)]
struct ManifoldArgs {
    bucket: String,
    prefix: String,
}

/// Configuration options
#[derive(Debug)]
struct Config {
    db_address: String,
    myrouter_port: u16,
    manifold_args: ManifoldArgs,
    repo_id: RepositoryId,
    src_blobstore_id: BlobstoreId,
    dst_blobstore_id: BlobstoreId,
    start_key: Option<String>,
    end_key: Option<String>,
    ctx: CoreContext,
    state_key: Option<String>,
    dry_run: bool,
    started_at: Instant,
    readonly_storage: bool,
}

/// State used to resume iteration in case of restart
#[derive(Debug, Clone)]
struct State {
    count: usize,
    init_range: Arc<ManifoldRange>,
    current_range: Arc<ManifoldRange>,
}

impl State {
    fn from_init(init_range: Arc<ManifoldRange>) -> Self {
        Self {
            count: INIT_COUNT_VALUE,
            current_range: init_range.clone(),
            init_range,
        }
    }

    fn with_current_many(self, current_range: Arc<ManifoldRange>, num: usize) -> Self {
        let State {
            count, init_range, ..
        } = self;
        Self {
            count: count + num,
            init_range,
            current_range,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StateSerde {
    init_range: ManifoldRange,
    current_range: ManifoldRange,
}

impl From<StateSerde> for State {
    fn from(state: StateSerde) -> Self {
        Self {
            count: INIT_COUNT_VALUE,
            init_range: Arc::new(state.init_range),
            current_range: Arc::new(state.current_range),
        }
    }
}

impl<'a> From<&'a State> for StateSerde {
    fn from(state: &'a State) -> Self {
        Self {
            init_range: (*state.init_range).clone(),
            current_range: (*state.current_range).clone(),
        }
    }
}

fn parse_args(fb: FacebookInit) -> Result<Config, Error> {
    let app = args::MononokeApp::new("populate healer queue")
        .build()
        .version("0.0.0")
        .about("Populate blobstore queue from existing manifold bucket")
        .arg(
            Arg::with_name("storage-id")
                .long("storage-id")
                .short("S")
                .takes_value(true)
                .value_name("STORAGEID")
                .help("Storage identifier"),
        )
        .arg(
            Arg::with_name("source-blobstore-id")
                .long("source-blobstore-id")
                .short("s")
                .takes_value(true)
                .value_name("SOURCE")
                .help("source blobstore identifier"),
        )
        .arg(
            Arg::with_name("destination-blobstore-id")
                .long("destination-blobstore-id")
                .short("d")
                .takes_value(true)
                .value_name("DESTINATION")
                .help("destination blobstore identifier"),
        )
        .arg(
            Arg::with_name("start-key")
                .long("start-key")
                .takes_value(true)
                .value_name("START_KEY")
                .help("if specified iteration will start from this key"),
        )
        .arg(
            Arg::with_name("end-key")
                .long("end-key")
                .takes_value(true)
                .value_name("END_KEY")
                .help("if specified iteration will end at this key"),
        )
        .arg(
            Arg::with_name("resume-state-key")
                .long("resume-state-key")
                .takes_value(true)
                .value_name("STATE_MANIFOLD_KEY")
                .help(
                    "manifold key which contains current iteration state and can be used to resume",
                ),
        )
        .arg(
            Arg::with_name("dry-run")
                .long("dry-run")
                .help("do not add entries to a queue"),
        );

    let matches = app.get_matches();
    let repo_id = args::get_repo_id(fb, &matches)?;
    let logger = args::init_logging(fb, &matches);
    let ctx = CoreContext::new_with_logger(fb, logger.clone());

    let storage_id = matches
        .value_of("storage-id")
        .ok_or(Error::msg("`storage-id` argument required"))?;

    let storage_config = args::read_storage_configs(fb, &matches)?
        .remove(storage_id)
        .ok_or(Error::msg("Unknown `storage-id`"))?;

    let src_blobstore_id = matches
        .value_of("source-blobstore-id")
        .ok_or(Error::msg("`source-blobstore-id` argument is required"))
        .and_then(|src| src.parse::<u64>().map_err(Error::from))
        .map(BlobstoreId::new)?;
    let dst_blobstore_id = matches
        .value_of("destination-blobstore-id")
        .ok_or(Error::msg(
            "`destination-blobstore-id` argument is required",
        ))
        .and_then(|dst| dst.parse::<u64>().map_err(Error::from))
        .map(BlobstoreId::new)?;
    if src_blobstore_id == dst_blobstore_id {
        bail!("`source-blobstore-id` and `destination-blobstore-id` can not be equal");
    }

    let (blobstores, db_address) = match storage_config {
        StorageConfig {
            dbconfig: MetadataDBConfig::Mysql { db_address, .. },
            blobstore: BlobConfig::Multiplexed { blobstores, .. },
        } => (blobstores, db_address),
        storage => return Err(format_err!("unsupported storage: {:?}", storage)),
    };
    let manifold_args = blobstores
        .iter()
        .filter(|(id, _)| src_blobstore_id == *id)
        .map(|(_, args)| args)
        .next()
        .ok_or(format_err!(
            "failed to find source blobstore id: {:?}",
            src_blobstore_id,
        ))
        .and_then(|args| match args {
            BlobConfig::Manifold { bucket, prefix } => Ok(ManifoldArgs {
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            }),
            _ => bail!("source blobstore must be a manifold"),
        })?;

    let myrouter_port = args::parse_mysql_options(&matches)
        .myrouter_port
        .ok_or(Error::msg("myrouter-port must be specified"))?;

    let readonly_storage = args::parse_readonly_storage(&matches);

    Ok(Config {
        repo_id,
        db_address: db_address.clone(),
        myrouter_port,
        manifold_args,
        src_blobstore_id,
        dst_blobstore_id,
        start_key: matches.value_of("start-key").map(String::from),
        end_key: matches.value_of("end-key").map(String::from),
        state_key: matches.value_of("resume-state-key").map(String::from),
        ctx,
        dry_run: matches.is_present("dry-run"),
        started_at: Instant::now(),
        readonly_storage: readonly_storage.0,
    })
}

fn get_resume_state(
    manifold: &ThriftManifoldBlob,
    config: &Config,
) -> impl Future<Item = State, Error = Error> {
    let resume_state = match &config.state_key {
        Some(state_key) => manifold
            .get(config.ctx.clone(), state_key.clone())
            .map(|data| {
                data.and_then(|data| serde_json::from_slice::<StateSerde>(&*data.into_bytes()).ok())
                    .map(State::from)
            })
            .left_future(),
        None => future::ok(None).right_future(),
    };

    let init_state = {
        let start = format!(
            "{}repo{:04}.{}",
            FLAT_NAMESPACE_PREFIX,
            config.repo_id.id(),
            config.start_key.clone().unwrap_or_else(|| "".to_string())
        );
        let end = format!(
            "{}repo{:04}.{}",
            FLAT_NAMESPACE_PREFIX,
            config.repo_id.id(),
            config.end_key.clone().unwrap_or_else(|| "\x7f".to_string()),
        );
        State::from_init(Arc::new(ManifoldRange::from(start..end)))
    };

    resume_state.map(move |resume_state| match resume_state {
        None => init_state,
        // if initial_state mismatch, start from provided initial state
        Some(ref resume_state) if resume_state.init_range != init_state.init_range => init_state,
        Some(resume_state) => resume_state,
    })
}

fn put_resume_state(
    manifold: &ThriftManifoldBlob,
    config: &Config,
    state: State,
) -> impl Future<Item = State, Error = Error> {
    match &config.state_key {
        Some(state_key) if state.count % PRESERVE_STATE_RATIO == INIT_COUNT_VALUE => {
            let started_at = config.started_at;
            let ctx = config.ctx.clone();
            cloned!(state_key, manifold);
            serde_json::to_vec(&StateSerde::from(&state))
                .map(|state_json| BlobstoreBytes::from_bytes(state_json))
                .map_err(Error::from)
                .into_future()
                .and_then(move |state_data| manifold.put(ctx, state_key, state_data))
                .map(move |_| {
                    if termion::is_tty(&std::io::stderr()) {
                        let elapsed = started_at.elapsed().as_secs() as f64;
                        let count = state.count as f64;
                        eprintln!(
                            "Keys processed: {:.0} speed: {:.2}/s",
                            count,
                            count / elapsed
                        );
                    }
                    state
                })
                .left_future()
        }
        _ => future::ok(state).right_future(),
    }
}

fn populate_healer_queue(
    manifold: ThriftManifoldBlob,
    queue: Arc<dyn BlobstoreSyncQueue>,
    config: Arc<Config>,
) -> impl Future<Item = State, Error = Error> {
    get_resume_state(&manifold, &config).and_then(move |state| {
        manifold
            .enumerate((*state.current_range).clone())
            .and_then(|mut entry| {
                // We are enumerating Manifold's flat/ namespace
                // and all the keys contain the flat/ prefix, which
                // we need to strip
                if !entry.key.starts_with(FLAT_NAMESPACE_PREFIX) {
                    future::err(format_err!(
                        "Key {} is expected to start with {}, but does not",
                        entry.key,
                        FLAT_NAMESPACE_PREFIX
                    ))
                } else {
                    // safe to unwrap here, since we know exactly how the string starts
                    entry.key = entry.key.get(FLAT_NAMESPACE_PREFIX.len()..).unwrap().into();
                    future::ok(entry)
                }
            })
            .chunks(CHUNK_SIZE)
            .fold(state, move |state, entries| {
                let range = entries[0].range.clone();
                let state = state.with_current_many(range, entries.len());
                let src_blobstore_id = config.src_blobstore_id;

                let enqueue = if config.dry_run {
                    future::ok(()).left_future()
                } else {
                    let iterator_box = Box::new(entries.into_iter().map(move |entry| {
                        BlobstoreSyncQueueEntry::new(entry.key, src_blobstore_id, DateTime::now())
                    }));
                    queue
                        .add_many(config.ctx.clone(), iterator_box)
                        .right_future()
                };

                enqueue.and_then({
                    cloned!(manifold, config);
                    move |_| put_resume_state(&manifold, &config, state)
                })
            })
    })
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let config = Arc::new(parse_args(fb)?);
    let manifold = ThriftManifoldBlob::new(fb, config.manifold_args.bucket.clone())?.into_inner();
    let queue: Arc<dyn BlobstoreSyncQueue> = Arc::new(SqlBlobstoreSyncQueue::with_myrouter(
        config.db_address.clone(),
        config.myrouter_port,
        MysqlOptions::default().myrouter_read_service_type(),
        config.readonly_storage,
    ));
    let mut runtime = runtime::Runtime::new()?;
    runtime.block_on(populate_healer_queue(manifold, queue, config))?;
    Ok(())
}
