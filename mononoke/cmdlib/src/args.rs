/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use std::collections::{HashMap, HashSet};
use std::io;
use std::num::NonZeroU32;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, format_err, Error, Result};
use clap::{App, Arg, ArgGroup, ArgMatches};
use cloned::cloned;
use fbinit::FacebookInit;
use futures::Future;
use futures_ext::{try_boxfuture, BoxFuture, FutureExt};
use lazy_static::lazy_static;
use panichandler::{self, Fate};
use scuba::ScubaSampleBuilder;
use slog::{debug, info, o, warn, Drain, Level, Logger, Never, SendSyncRefUnwindSafeDrain};
use slog_term::TermDecorator;

use slog_glog_fmt::{kv_categorizer::FacebookCategorizer, kv_defaults::FacebookKV, GlogFormat};

use blobrepo::BlobRepo;
use blobrepo_factory::{open_blobrepo, Caching, ReadOnlyStorage};
use blobstore_factory::{BlobstoreOptions, Scrubbing, ThrottleOptions};
use changesets::SqlConstructors;
use metaconfig_parser::RepoConfigs;
use metaconfig_types::{
    BlobConfig, CommonConfig, Redaction, RepoConfig, ScrubAction, StorageConfig,
};
use mononoke_types::RepositoryId;
use slog_logview::LogViewDrain;
use sql_ext::MysqlOptions;

use crate::helpers::{
    create_runtime, init_cachelib_from_settings, open_sql_with_config_and_mysql_options,
    setup_repo_dir, CachelibSettings, CreateStorage,
};
use crate::log;

const REPO_ID: &str = "repo-id";
const REPO_NAME: &str = "repo-name";
const SOURCE_REPO_GROUP: &str = "source-repo";
const SOURCE_REPO_ID: &str = "source-repo-id";
const SOURCE_REPO_NAME: &str = "source-repo-name";
const TARGET_REPO_GROUP: &str = "target-repo";
const TARGET_REPO_ID: &str = "target-repo-id";
const TARGET_REPO_NAME: &str = "target-repo-name";
const ENABLE_MCROUTER: &str = "enable-mcrouter";
const MYSQL_MYROUTER_PORT: &str = "myrouter-port";
const MYSQL_MASTER_ONLY: &str = "mysql-master-only";
const RUNTIME_THREADS: &str = "runtime-threads";

const CACHE_SIZE_GB: &str = "cache-size-gb";
const USE_TUPPERWARE_SHRINKER: &str = "use-tupperware-shrinker";
const MAX_PROCESS_SIZE: &str = "max-process-size";
const MIN_PROCESS_SIZE: &str = "min-process-size";
pub const WITH_CONTENT_SHA1_CACHE: &str = "with-content-sha1-cache";
const SKIP_CACHING: &str = "skip-caching";
const CACHELIB_ONLY_BLOBSTORE: &str = "cachelib-only-blobstore";
const READONLY_STORAGE: &str = "readonly-storage";

const READ_QPS_ARG: &str = "blobstore-read-qps";
const WRITE_QPS_ARG: &str = "blobstore-write-qps";

const CACHE_ARGS: &[(&str, &str)] = &[
    ("blob-cache-size", "override size of the blob cache"),
    (
        "presence-cache-size",
        "override size of the blob presence cache",
    ),
    (
        "changesets-cache-size",
        "override size of the changesets cache",
    ),
    (
        "filenodes-cache-size",
        "override size of the filenodes cache",
    ),
    (
        "idmapping-cache-size",
        "override size of the bonsai/hg mapping cache",
    ),
    (
        "content-sha1-cache-size",
        "override size of the content SHA1 cache",
    ),
];

pub struct MononokeApp {
    /// The app name.
    name: String,

    /// Whether to hide advanced Manifold configuration from help. Note that the arguments will
    /// still be available, just not displayed in help.
    hide_advanced_args: bool,

    /// Whether to operate on all repos, and not provide options to select a repo.
    all_repos: bool,

    /// Whether to require the user select a repo.
    repo_required: bool,

    /// Adds --source-repo-id/repo-name and --target-repo-id/repo-name options.
    /// Necessary for crossrepo operations
    source_and_target_repos: bool,

    /// Adds just --source-repo-id/repo-name, for blobimport into a megarepo
    source_repo: bool,

    /// Adds --shutdown-grace-period and --shutdown-timeout for graceful shutdown.
    shutdown_timeout: bool,

    /// Adds --scuba-dataset and --scuba-log-file for scuba logging.
    scuba_logging: bool,
}

/// Create a default root logger for Facebook services
pub fn glog_drain() -> impl Drain<Ok = (), Err = Never> {
    let decorator = TermDecorator::new().build();
    // FacebookCategorizer is used for slog KV arguments.
    // At the time of writing this code FacebookCategorizer and FacebookKV
    // that was added below was mainly useful for logview logging and had no effect on GlogFormat
    let drain = GlogFormat::new(decorator, FacebookCategorizer).ignore_res();
    ::std::sync::Mutex::new(drain).ignore_res()
}

impl MononokeApp {
    /// Start building a new Mononoke app.  This adds the standard Mononoke args.  Use the `build`
    /// method to get a `clap::App` that you can then customize further.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            hide_advanced_args: false,
            all_repos: false,
            repo_required: false,
            source_and_target_repos: false,
            source_repo: false,
            shutdown_timeout: false,
            scuba_logging: false,
        }
    }

    /// Hide advanced args.
    pub fn with_advanced_args_hidden(mut self) -> Self {
        self.hide_advanced_args = true;
        self
    }

    /// This command operates on all configured repos, and removes the options for selecting a
    /// repo.  The default behaviour is for the arguments to specify the repo to be optional, which is
    /// probably not what you want, so you should call either this method or `with_repo_required`.
    pub fn with_all_repos(mut self) -> Self {
        self.all_repos = true;
        self
    }

    /// This command operates on a specific repos, so this makes the options for selecting a
    /// repo required.  The default behaviour is for the arguments to specify the repo to be
    /// optional, which is probably not what you want, so you should call either this method or
    /// `with_all_repos`.
    pub fn with_repo_required(mut self) -> Self {
        self.repo_required = true;
        self
    }

    /// This command might operate on two repos in the same time. This is normally used
    /// for two repos where one repo is synced into another.
    pub fn with_source_and_target_repos(mut self) -> Self {
        self.source_and_target_repos = true;
        self
    }

    /// This command operates on one repo (--repo-id/name), but needs to be aware that commits
    /// are sourced from another repo.
    pub fn with_source_repos(mut self) -> Self {
        self.source_repo = true;
        self
    }

    /// This command has arguments for graceful shutdown.
    pub fn with_shutdown_timeout_args(mut self) -> Self {
        self.shutdown_timeout = true;
        self
    }

    /// This command has arguments for scuba logging.
    pub fn with_scuba_logging_args(mut self) -> Self {
        self.scuba_logging = true;
        self
    }

    /// Build a `clap::App` for this Mononoke app, which can then be customized further.
    pub fn build<'a, 'b>(self) -> App<'a, 'b> {
        let mut app = App::new(self.name).arg(
            Arg::with_name("mononoke-config-path")
                .long("mononoke-config-path")
                .value_name("MONONOKE_CONFIG_PATH")
                .help("Path to the Mononoke configs"),
        );

        if !self.all_repos {
            let repo_conflicts: &[&str] = if self.source_repo {
                &[TARGET_REPO_ID, TARGET_REPO_NAME]
            } else {
                &[
                    SOURCE_REPO_ID,
                    SOURCE_REPO_NAME,
                    TARGET_REPO_ID,
                    TARGET_REPO_NAME,
                ]
            };

            app = app
                .arg(
                    Arg::with_name(REPO_ID)
                    .long(REPO_ID)
                    // This is an old form that some consumers use
                    .alias("repo_id")
                    .value_name("ID")
                    .help("numeric ID of repository")
                    .conflicts_with_all(repo_conflicts),
                )
                .arg(
                    Arg::with_name(REPO_NAME)
                        .long(REPO_NAME)
                        .value_name("NAME")
                        .help("Name of repository")
                        .conflicts_with_all(repo_conflicts),
                )
                .group(
                    ArgGroup::with_name("repo")
                        .args(&[REPO_ID, REPO_NAME])
                        .required(self.repo_required),
                );

            if self.source_repo || self.source_and_target_repos {
                app = app
                    .arg(
                        Arg::with_name(SOURCE_REPO_ID)
                        .long(SOURCE_REPO_ID)
                        .value_name("ID")
                        .help("numeric ID of source repository (used only for commands that operate on more than one repo)"),
                    )
                    .arg(
                        Arg::with_name(SOURCE_REPO_NAME)
                        .long(SOURCE_REPO_NAME)
                        .value_name("NAME")
                        .help("Name of source repository (used only for commands that operate on more than one repo)"),
                    )
                    .group(
                        ArgGroup::with_name(SOURCE_REPO_GROUP)
                            .args(&[SOURCE_REPO_ID, SOURCE_REPO_NAME])
                    )
            }

            if self.source_and_target_repos {
                app = app
                    .arg(
                        Arg::with_name(TARGET_REPO_ID)
                        .long(TARGET_REPO_ID)
                        .value_name("ID")
                        .help("numeric ID of target repository (used only for commands that operate on more than one repo)"),
                    )
                    .arg(
                        Arg::with_name(TARGET_REPO_NAME)
                        .long(TARGET_REPO_NAME)
                        .value_name("NAME")
                        .help("Name of target repository (used only for commands that operate on more than one repo)"),
                    )
                    .group(
                        ArgGroup::with_name(TARGET_REPO_GROUP)
                            .args(&[TARGET_REPO_ID, TARGET_REPO_NAME])
                    );
            }
        }

        app = add_logger_args(app);
        app = add_mysql_options_args(app);
        app = add_blobstore_args(app);
        app = add_cachelib_args(app, self.hide_advanced_args);
        app = add_runtime_args(app);

        if self.shutdown_timeout {
            app = add_shutdown_timeout_args(app);
        }

        if self.scuba_logging {
            app = add_scuba_logging_args(app);
        }

        app
    }
}

pub fn add_runtime_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(RUNTIME_THREADS)
            .long(RUNTIME_THREADS)
            .takes_value(true)
            .help("a number of threads to use in the tokio runtime"),
    )
}

pub fn add_logger_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("panic-fate")
            .long("panic-fate")
            .value_name("PANIC_FATE")
            .possible_values(&["continue", "exit", "abort"])
            .default_value("abort")
            .help("fate of the process when a panic happens"),
    )
    .arg(
        Arg::with_name("logview-category")
            .long("logview-category")
            .takes_value(true)
            .help("logview category to log to. Logview is not used if not set"),
    )
    .arg(
        Arg::with_name("debug")
            .short("d")
            .long("debug")
            .help("print debug output"),
    )
    .arg(
        Arg::with_name("log-level")
            .long("log-level")
            .help("log level to use (does not work with --debug)")
            .takes_value(true)
            .possible_values(&["CRITICAL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"])
            .conflicts_with("debug"),
    )
}

pub fn init_logging<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Logger {
    // Set the panic handler up here. Not really relevent to logger other than it emits output
    // when things go wrong. This writes directly to stderr as coredumper expects.
    let fate = match matches
        .value_of("panic-fate")
        .expect("no default on panic-fate")
    {
        "none" => None,
        "continue" => Some(Fate::Continue),
        "exit" => Some(Fate::Exit(101)),
        "abort" => Some(Fate::Abort),
        bad => panic!("bad panic-fate {}", bad),
    };
    if let Some(fate) = fate {
        panichandler::set_panichandler(fate);
    }

    let stdlog_env = "RUST_LOG";

    let level = if matches.is_present("debug") {
        Level::Debug
    } else {
        match matches.value_of("log-level") {
            Some(log_level_str) => Level::from_str(log_level_str)
                .expect(&format!("Unknown log level: {}", log_level_str)),
            None => Level::Info,
        }
    };

    let glog_drain = Arc::new(glog_drain());
    let root_log_drain: Arc<dyn SendSyncRefUnwindSafeDrain<Ok = (), Err = Never>> =
        match matches.value_of("logview-category") {
            Some(category) => {
                // // Sometimes scribe writes can fail due to backpressure - it's OK to drop these
                // // since logview is sampled anyway.
                let logview_drain = LogViewDrain::new(fb, category).ignore_res();
                let drain = slog::Duplicate::new(glog_drain, logview_drain);
                Arc::new(drain.ignore_res())
            }
            None => Arc::new(glog_drain.ignore_res()),
        };

    // NOTE: We pass an unfitlered Logger to init_stdlog_once. That's because we do the filtering
    // at the stdlog level there.
    let stdlog_level =
        log::init_stdlog_once(Logger::root(root_log_drain.clone(), o![]), stdlog_env);

    let root_log_drain = root_log_drain.filter_level(level).ignore_res();

    let kv = FacebookKV::new().expect("cannot initialize FacebookKV");

    let logger = if matches.is_present("fb303-thrift-port") {
        Logger::root(slog_stats::StatsDrain::new(root_log_drain), o![kv])
    } else {
        Logger::root(root_log_drain, o![kv])
    };

    debug!(
        logger,
        "enabled stdlog with level: {:?} (set {} to configure)", stdlog_level, stdlog_env
    );

    logger
}

fn get_repo_id_and_name_from_values<'a>(
    fb: FacebookInit,
    matches: &ArgMatches<'a>,
    option_repo_name: &str,
    option_repo_id: &str,
) -> Result<(RepositoryId, String)> {
    let repo_name = matches.value_of(option_repo_name);
    let repo_id = matches.value_of(option_repo_id);
    let configs = read_configs(fb, matches)?;

    match (repo_name, repo_id) {
        (Some(_), Some(_)) => bail!("both repo-name and repo-id parameters set"),
        (None, None) => bail!("neither repo-name nor repo-id parameter set"),
        (None, Some(repo_id)) => {
            let repo_id = RepositoryId::from_str(repo_id)?;
            let mut repo_config: Vec<_> = configs
                .repos
                .into_iter()
                .filter(|(_, repo_config)| repo_config.repoid == repo_id)
                .collect();
            if repo_config.is_empty() {
                Err(format_err!("unknown config for repo-id {:?}", repo_id))
            } else if repo_config.len() > 1 {
                Err(format_err!(
                    "multiple configs defined for repo-id {:?}",
                    repo_id
                ))
            } else {
                let (repo_name, repo_config) = repo_config.pop().unwrap();
                Ok((repo_config.repoid, repo_name))
            }
        }
        (Some(repo_name), None) => {
            let mut repo_config: Vec<_> = configs
                .repos
                .into_iter()
                .filter(|(name, _)| name == repo_name)
                .collect();
            if repo_config.is_empty() {
                Err(format_err!("unknown repo-name {:?}", repo_name))
            } else if repo_config.len() > 1 {
                Err(format_err!(
                    "multiple configs defined for repo-name {:?}",
                    repo_name
                ))
            } else {
                let (repo_name, repo_config) = repo_config.pop().unwrap();
                Ok((repo_config.repoid, repo_name))
            }
        }
    }
}

pub fn get_repo_id<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<RepositoryId> {
    let (repo_id, _) = get_repo_id_and_name_from_values(fb, matches, REPO_NAME, REPO_ID)?;
    Ok(repo_id)
}

pub fn get_repo_name<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<String> {
    let (_, repo_name) = get_repo_id_and_name_from_values(fb, matches, REPO_NAME, REPO_ID)?;
    Ok(repo_name)
}

pub fn get_source_repo_id<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<RepositoryId> {
    let (repo_id, _) =
        get_repo_id_and_name_from_values(fb, matches, SOURCE_REPO_NAME, SOURCE_REPO_ID)?;
    Ok(repo_id)
}

pub fn get_source_repo_id_opt<'a>(
    fb: FacebookInit,
    matches: &ArgMatches<'a>,
) -> Result<Option<RepositoryId>> {
    if matches.is_present(SOURCE_REPO_NAME) || matches.is_present(SOURCE_REPO_ID) {
        let (repo_id, _) =
            get_repo_id_and_name_from_values(fb, matches, SOURCE_REPO_NAME, SOURCE_REPO_ID)?;
        Ok(Some(repo_id))
    } else {
        Ok(None)
    }
}

pub fn get_target_repo_id<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<RepositoryId> {
    let (repo_id, _) =
        get_repo_id_and_name_from_values(fb, matches, TARGET_REPO_NAME, TARGET_REPO_ID)?;
    Ok(repo_id)
}

pub fn open_sql<T>(fb: FacebookInit, matches: &ArgMatches<'_>) -> BoxFuture<T, Error>
where
    T: SqlConstructors,
{
    let (_, config) = try_boxfuture!(get_config(fb, matches));
    let mysql_options = parse_mysql_options(matches);
    let readonly_storage = parse_readonly_storage(matches);
    open_sql_with_config_and_mysql_options(
        fb,
        config.storage_config.dbconfig,
        mysql_options,
        readonly_storage,
    )
}

pub fn open_source_sql<T>(fb: FacebookInit, matches: &ArgMatches<'_>) -> BoxFuture<T, Error>
where
    T: SqlConstructors,
{
    let source_repo_id = try_boxfuture!(get_source_repo_id(fb, matches));
    let (_, config) = try_boxfuture!(get_config_by_repoid(fb, matches, source_repo_id));
    let mysql_options = parse_mysql_options(matches);
    let readonly_storage = parse_readonly_storage(matches);
    open_sql_with_config_and_mysql_options(
        fb,
        config.storage_config.dbconfig,
        mysql_options,
        readonly_storage,
    )
}

/// Create a new `BlobRepo` -- for local instances, expect its contents to be empty.
#[inline]
pub fn create_repo<'a>(
    fb: FacebookInit,
    logger: &Logger,
    matches: &ArgMatches<'a>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    open_repo_internal(
        fb,
        logger,
        matches,
        true,
        parse_caching(matches),
        Scrubbing::Disabled,
        None,
    )
}

/// Create a new `BlobRepo` -- for local instances, expect its contents to be empty.
/// Make sure that the opened repo has redaction disabled
#[inline]
pub fn create_repo_unredacted<'a>(
    fb: FacebookInit,
    logger: &Logger,
    matches: &ArgMatches<'a>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    open_repo_internal(
        fb,
        logger,
        matches,
        true,
        parse_caching(matches),
        Scrubbing::Disabled,
        Some(Redaction::Disabled),
    )
}

/// Open an existing `BlobRepo` -- for local instances, expect contents to already be there.
#[inline]
pub fn open_repo<'a>(
    fb: FacebookInit,
    logger: &Logger,
    matches: &ArgMatches<'a>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    open_repo_internal(
        fb,
        logger,
        matches,
        false,
        parse_caching(matches),
        Scrubbing::Disabled,
        None,
    )
}

/// Open an existing `BlobRepo` -- for local instances, expect contents to already be there.
/// Make sure that the opened repo has redaction disabled
#[inline]
pub fn open_repo_unredacted<'a>(
    fb: FacebookInit,
    logger: &Logger,
    matches: &ArgMatches<'a>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    open_repo_internal(
        fb,
        logger,
        matches,
        false,
        parse_caching(matches),
        Scrubbing::Disabled,
        Some(Redaction::Disabled),
    )
}

/// Open an existing `BlobRepo` -- for local instances, expect contents to already be there.
/// If there are multiple backing blobstores, open them in scrub mode, where we check that
/// the blobstore contents all match.
#[inline]
pub fn open_scrub_repo<'a>(
    fb: FacebookInit,
    logger: &Logger,
    matches: &ArgMatches<'a>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    open_repo_internal(
        fb,
        logger,
        matches,
        false,
        parse_caching(matches),
        Scrubbing::Enabled,
        None,
    )
}

pub fn add_cachelib_args<'a, 'b>(app: App<'a, 'b>, hide_advanced_args: bool) -> App<'a, 'b> {
    let cache_args: Vec<_> = CACHE_ARGS
        .iter()
        .map(|(flag, help)| {
            // XXX figure out a way to get default values in here -- note that .default_value
            // takes a &'a str, so we may need to have MononokeApp own it or similar.
            Arg::with_name(flag)
                .long(flag)
                .value_name("SIZE")
                .hidden(hide_advanced_args)
                .help(help)
        })
        .collect();

    // Computed help strings with lifetime 'b is problematic, so use lazy_static instead:
    lazy_static! {
        static ref MIN_PROCESS_SIZE_HELP: std::string::String = format!(
            "process size at which cachelib will grow back to {} in GiB",
            CACHE_SIZE_GB
        );
    }

    app.arg(
        Arg::with_name(CACHE_SIZE_GB)
            .long(CACHE_SIZE_GB)
            .takes_value(true)
            .value_name("SIZE")
            .help("size of the cachelib cache, in GiB"),
    )
    .arg(
        Arg::with_name(USE_TUPPERWARE_SHRINKER)
            .long(USE_TUPPERWARE_SHRINKER)
            .help("Use the Tupperware-aware cache shrinker to avoid OOM"),
    )
    .arg(
        Arg::with_name(MAX_PROCESS_SIZE)
            .long(MAX_PROCESS_SIZE)
            .takes_value(true)
            .value_name("SIZE")
            .help("process size at which cachelib will shrink, in GiB"),
    )
    .arg(
        Arg::with_name(MIN_PROCESS_SIZE)
            .long(MIN_PROCESS_SIZE)
            .takes_value(true)
            .value_name("SIZE")
            .help(&*MIN_PROCESS_SIZE_HELP),
    )
    .arg(
        Arg::with_name(WITH_CONTENT_SHA1_CACHE)
            .long(WITH_CONTENT_SHA1_CACHE)
            .help("[Mononoke API Server only] enable content SHA1 cache"),
    )
    .arg(
        Arg::with_name(SKIP_CACHING)
            .long(SKIP_CACHING)
            .help("do not init cachelib and disable caches (useful for tests)"),
    )
    .arg(
        Arg::with_name(CACHELIB_ONLY_BLOBSTORE)
            .long(CACHELIB_ONLY_BLOBSTORE)
            .help("do not init memcache for blobstore"),
    )
    .arg(
        Arg::with_name(READONLY_STORAGE)
            .long(READONLY_STORAGE)
            .help("Error on any attempts to write to storage"),
    )
    .args(&cache_args)
}

pub fn parse_caching<'a>(matches: &ArgMatches<'a>) -> Caching {
    if matches.is_present(SKIP_CACHING) {
        Caching::Disabled
    } else if matches.is_present(CACHELIB_ONLY_BLOBSTORE) {
        Caching::CachelibOnlyBlobstore
    } else {
        Caching::Enabled
    }
}

pub fn init_cachelib<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Caching {
    let caching = parse_caching(matches);

    if caching == Caching::Enabled || caching == Caching::CachelibOnlyBlobstore {
        let mut settings = CachelibSettings::default();
        if let Some(cache_size) = matches.value_of(CACHE_SIZE_GB) {
            settings.cache_size = cache_size.parse::<usize>().unwrap() * 1024 * 1024 * 1024;
        }
        if let Some(max_process_size) = matches.value_of(MAX_PROCESS_SIZE) {
            settings.max_process_size_gib = Some(max_process_size.parse().unwrap());
        }
        if let Some(min_process_size) = matches.value_of(MIN_PROCESS_SIZE) {
            settings.min_process_size_gib = Some(min_process_size.parse().unwrap());
        }
        settings.use_tupperware_shrinker = matches.is_present(USE_TUPPERWARE_SHRINKER);
        if let Some(presence_cache_size) = matches.value_of("presence-cache-size") {
            settings.presence_cache_size = Some(presence_cache_size.parse().unwrap());
        }
        if let Some(changesets_cache_size) = matches.value_of("changesets-cache-size") {
            settings.changesets_cache_size = Some(changesets_cache_size.parse().unwrap());
        }
        if let Some(filenodes_cache_size) = matches.value_of("filenodes-cache-size") {
            settings.filenodes_cache_size = Some(filenodes_cache_size.parse().unwrap());
        }
        if let Some(idmapping_cache_size) = matches.value_of("idmapping-cache-size") {
            settings.idmapping_cache_size = Some(idmapping_cache_size.parse().unwrap());
        }
        settings.with_content_sha1_cache = matches.is_present(WITH_CONTENT_SHA1_CACHE);
        if let Some(content_sha1_cache_size) = matches.value_of("content-sha1-cache-size") {
            settings.content_sha1_cache_size = Some(content_sha1_cache_size.parse().unwrap());
        }
        if let Some(blob_cache_size) = matches.value_of("blob-cache-size") {
            settings.blob_cache_size = Some(blob_cache_size.parse().unwrap());
        }

        init_cachelib_from_settings(fb, settings).unwrap();
    }

    caching
}

pub fn add_mysql_options_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(MYSQL_MYROUTER_PORT)
            .long(MYSQL_MYROUTER_PORT)
            .help("Use MyRouter at this port")
            .takes_value(true),
    )
    .arg(
        Arg::with_name(MYSQL_MASTER_ONLY)
            .long(MYSQL_MASTER_ONLY)
            .help("Connect to MySQL master only")
            .takes_value(false),
    )
}

pub fn add_blobstore_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(READ_QPS_ARG)
            .long(READ_QPS_ARG)
            .takes_value(true)
            .required(false)
            .help("Read QPS limit to ThrottledBlob"),
    )
    .arg(
        Arg::with_name(WRITE_QPS_ARG)
            .long(WRITE_QPS_ARG)
            .takes_value(true)
            .required(false)
            .help("Write QPS limit to ThrottledBlob"),
    )
}

pub fn add_mcrouter_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name(ENABLE_MCROUTER)
            .long(ENABLE_MCROUTER)
            .help("Use local McRouter for rate limits")
            .takes_value(false),
    )
}

pub fn add_fb303_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.args_from_usage(r"--fb303-thrift-port=[PORT]    'port for fb303 service'")
}

pub fn add_disabled_hooks_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("disabled-hooks")
            .long("disable-hook")
            .help("Disable a hook. Pass this argument multiple times to disable multiple hooks.")
            .multiple(true)
            .number_of_values(1)
            .takes_value(true),
    )
}

pub fn add_shutdown_timeout_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("shutdown-grace-period")
            .long("shutdown-grace-period")
            .help(
                "Number of seconds to wait after receiving a shutdown signal before shutting down.",
            )
            .takes_value(true)
            .required(false)
            .default_value("0"),
    )
    .arg(
        Arg::with_name("shutdown-timeout")
            .long("shutdown-timeout")
            .help("Number of seconds to wait for requests to complete during shutdown.")
            .takes_value(true)
            .required(false)
            .default_value("10"),
    )
}

pub fn get_shutdown_grace_period<'a>(matches: &ArgMatches<'a>) -> Result<Duration> {
    let seconds = matches
        .value_of("shutdown-grace-period")
        .ok_or(Error::msg("shutdown-grace-period must be specifier"))?
        .parse()
        .map_err(Error::from)?;
    Ok(Duration::from_secs(seconds))
}

pub fn get_shutdown_timeout<'a>(matches: &ArgMatches<'a>) -> Result<Duration> {
    let seconds = matches
        .value_of("shutdown-timeout")
        .ok_or(Error::msg("shutdown-timeout must be specifier"))?
        .parse()
        .map_err(Error::from)?;
    Ok(Duration::from_secs(seconds))
}

pub fn add_scuba_logging_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("scuba-dataset")
            .long("scuba-dataset")
            .takes_value(true)
            .help("The name of the scuba dataset to log to"),
    )
    .arg(
        Arg::with_name("scuba-log-file")
            .long("scuba-log-file")
            .takes_value(true)
            .help("A log file to write Scuba logs to (primarily useful in testing)"),
    )
}

pub fn get_scuba_sample_builder<'a>(
    fb: FacebookInit,
    matches: &ArgMatches<'a>,
) -> Result<ScubaSampleBuilder> {
    let mut scuba_logger = if let Some(scuba_dataset) = matches.value_of("scuba-dataset") {
        ScubaSampleBuilder::new(fb, scuba_dataset)
    } else {
        ScubaSampleBuilder::with_discard()
    };
    if let Some(scuba_log_file) = matches.value_of("scuba-log-file") {
        scuba_logger = scuba_logger.with_log_file(scuba_log_file)?;
    }
    Ok(scuba_logger)
}

pub fn read_configs<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<RepoConfigs> {
    let config_path = matches
        .value_of("mononoke-config-path")
        .ok_or(Error::msg("mononoke-config-path must be specified"))?;
    RepoConfigs::read_configs(fb, config_path)
}

pub fn read_common_config<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<CommonConfig> {
    let config_path = matches
        .value_of("mononoke-config-path")
        .ok_or(Error::msg("mononoke-config-path must be specified"))?;

    let config_path = Path::new(config_path);
    RepoConfigs::read_common_config(fb, &config_path.to_path_buf())
}

pub fn read_storage_configs<'a>(
    fb: FacebookInit,
    matches: &ArgMatches<'a>,
) -> Result<HashMap<String, StorageConfig>> {
    let config_path = matches
        .value_of("mononoke-config-path")
        .ok_or(Error::msg("mononoke-config-path must be specified"))?;
    RepoConfigs::read_storage_configs(fb, config_path)
}

pub fn get_config<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) -> Result<(String, RepoConfig)> {
    let repo_id = get_repo_id(fb, matches)?;
    get_config_by_repoid(fb, matches, repo_id)
}

pub fn get_config_by_repoid<'a>(
    fb: FacebookInit,
    matches: &ArgMatches<'a>,
    repo_id: RepositoryId,
) -> Result<(String, RepoConfig)> {
    let configs = read_configs(fb, matches)?;
    configs
        .get_repo_config(repo_id)
        .ok_or_else(|| format_err!("unknown repoid {:?}", repo_id))
        .map(|(name, config)| (name.clone(), config.clone()))
}

fn open_repo_internal<'a>(
    fb: FacebookInit,
    logger: &Logger,
    matches: &ArgMatches<'a>,
    create: bool,
    caching: Caching,
    scrub: Scrubbing,
    redaction_override: Option<Redaction>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    let repo_id = try_boxfuture!(get_repo_id(fb, matches));
    open_repo_internal_with_repo_id(
        fb,
        logger,
        repo_id,
        matches,
        create,
        caching,
        scrub,
        redaction_override,
    )
}

fn open_repo_internal_with_repo_id<'a>(
    fb: FacebookInit,
    logger: &Logger,
    repo_id: RepositoryId,
    matches: &ArgMatches<'a>,
    create: bool,
    caching: Caching,
    scrub: Scrubbing,
    redaction_override: Option<Redaction>,
) -> BoxFuture<BlobRepo, Error> {
    let common_config = try_boxfuture!(read_common_config(fb, &matches));

    let (reponame, config) = {
        let (reponame, mut config) = try_boxfuture!(get_config_by_repoid(fb, matches, repo_id));
        if let Scrubbing::Enabled = scrub {
            config
                .storage_config
                .blobstore
                .set_scrubbed(ScrubAction::ReportOnly);
        }
        (reponame, config)
    };
    info!(logger, "using repo \"{}\" repoid {:?}", reponame, repo_id);
    match &config.storage_config.blobstore {
        BlobConfig::Files { path } | BlobConfig::Rocks { path } | BlobConfig::Sqlite { path } => {
            let create = if create {
                // Many path repos can share one blobstore, so allow store to exist or create it.
                CreateStorage::ExistingOrCreate
            } else {
                CreateStorage::ExistingOnly
            };
            try_boxfuture!(setup_repo_dir(path, create));
        }
        _ => {}
    };

    let mysql_options = parse_mysql_options(matches);
    let blobstore_options = parse_blobstore_options(matches);
    let readonly_storage = parse_readonly_storage(matches);

    cloned!(logger);
    open_blobrepo(
        fb,
        config.storage_config,
        repo_id,
        mysql_options,
        caching,
        config.bookmarks_cache_ttl,
        redaction_override.unwrap_or(config.redaction),
        common_config.scuba_censored_table,
        config.filestore,
        readonly_storage,
        blobstore_options,
        logger,
    )
    .boxify()
}

pub fn open_repo_with_repo_id<'a>(
    fb: FacebookInit,
    logger: &Logger,
    repo_id: RepositoryId,
    matches: &ArgMatches<'a>,
) -> impl Future<Item = BlobRepo, Error = Error> {
    open_repo_internal_with_repo_id(
        fb,
        logger,
        repo_id,
        matches,
        false,
        parse_caching(matches),
        Scrubbing::Disabled,
        None,
    )
    .boxify()
}

pub fn parse_readonly_storage<'a>(matches: &ArgMatches<'a>) -> ReadOnlyStorage {
    ReadOnlyStorage(matches.is_present("readonly-storage"))
}

pub fn parse_mysql_options<'a>(matches: &ArgMatches<'a>) -> MysqlOptions {
    let myrouter_port = match matches.value_of(MYSQL_MYROUTER_PORT) {
        Some(port) => Some(
            port.parse::<u16>()
                .expect("Provided --myrouter-port is not u16"),
        ),
        None => None,
    };

    let master_only = matches.is_present(MYSQL_MASTER_ONLY);

    MysqlOptions {
        myrouter_port,
        master_only,
    }
}

pub fn parse_blobstore_options<'a>(matches: &ArgMatches<'a>) -> BlobstoreOptions {
    let read_qps: Option<NonZeroU32> = matches
        .value_of(READ_QPS_ARG)
        .map(|v| v.parse().expect("Provided qps is not u32"));

    let write_qps: Option<NonZeroU32> = matches
        .value_of(WRITE_QPS_ARG)
        .map(|v| v.parse().expect("Provided qps is not u32"));

    BlobstoreOptions::new(ThrottleOptions::new(read_qps, write_qps))
}

pub fn maybe_enable_mcrouter<'a>(fb: FacebookInit, matches: &ArgMatches<'a>) {
    if matches.is_present(ENABLE_MCROUTER) {
        ::ratelim::use_proxy_if_available(fb);
    }
}

pub fn get_usize_opt<'a>(matches: &ArgMatches<'a>, key: &str) -> Option<usize> {
    matches.value_of(key).map(|val| {
        val.parse::<usize>()
            .expect(&format!("{} must be integer", key))
    })
}

#[inline]
pub fn get_usize<'a>(matches: &ArgMatches<'a>, key: &str, default: usize) -> usize {
    get_usize_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_u64<'a>(matches: &ArgMatches<'a>, key: &str, default: u64) -> u64 {
    get_u64_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_u64_opt<'a>(matches: &ArgMatches<'a>, key: &str) -> Option<u64> {
    matches.value_of(key).map(|val| {
        val.parse::<u64>()
            .expect(&format!("{} must be integer", key))
    })
}

#[inline]
pub fn get_i32_opt<'a>(matches: &ArgMatches<'a>, key: &str) -> Option<i32> {
    matches.value_of(key).map(|val| {
        val.parse::<i32>()
            .expect(&format!("{} must be integer", key))
    })
}

#[inline]
pub fn get_i32<'a>(matches: &ArgMatches<'a>, key: &str, default: i32) -> i32 {
    get_i32_opt(matches, key).unwrap_or(default)
}

#[inline]
pub fn get_i64_opt<'a>(matches: &ArgMatches<'a>, key: &str) -> Option<i64> {
    matches.value_of(key).map(|val| {
        val.parse::<i64>()
            .expect(&format!("{} must be integer", key))
    })
}

pub fn parse_disabled_hooks_with_repo_prefix(
    matches: &ArgMatches,
    logger: &Logger,
) -> Result<HashMap<String, HashSet<String>>, Error> {
    let disabled_hooks = matches
        .values_of("disabled-hooks")
        .map(|m| m.collect())
        .unwrap_or(vec![]);

    let mut res = HashMap::new();
    for repohook in disabled_hooks {
        let repohook: Vec<_> = repohook.splitn(2, ":").collect();
        let repo = repohook.get(0);
        let hook = repohook.get(1);

        let (repo, hook) =
            repo.and_then(|repo| hook.map(|hook| (repo, hook)))
                .ok_or(format_err!(
                    "invalid format of disabled hook, should be 'REPONAME:HOOKNAME'"
                ))?;
        res.entry(repo.to_string())
            .or_insert(HashSet::new())
            .insert(hook.to_string());
    }
    if res.len() > 0 {
        warn!(logger, "The following Hooks were disabled: {:?}", res);
    }
    Ok(res)
}

pub fn parse_disabled_hooks_no_repo_prefix(
    matches: &ArgMatches,
    logger: &Logger,
) -> HashSet<String> {
    let disabled_hooks: HashSet<String> = matches
        .values_of("disabled-hooks")
        .map(|m| m.collect())
        .unwrap_or(vec![])
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    if disabled_hooks.len() > 0 {
        warn!(
            logger,
            "The following Hooks were disabled: {:?}", disabled_hooks
        );
    }

    disabled_hooks
}

/// Initialize a new `tokio_compat::runtime::Runtime` with thread number parsed from the CLI
pub fn init_runtime(matches: &ArgMatches) -> io::Result<tokio_compat::runtime::Runtime> {
    let core_threads = get_usize_opt(matches, RUNTIME_THREADS);
    create_runtime(None, core_threads)
}
