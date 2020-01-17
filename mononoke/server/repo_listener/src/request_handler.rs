/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Error;
use configerator_cached::ConfigHandle;
use context::{generate_session_id, SessionId};
use failure_ext::SlogKVError;
use fbinit::FacebookInit;
use fbwhoami::FbWhoAmI;
use futures::{Future, Sink, Stream};
use futures_stats::Timed;
use lazy_static::lazy_static;
use limits::types::{MononokeThrottleLimit, MononokeThrottleLimits, RateLimits};
use maplit::{hashmap, hashset};
use pushredirect_enable::types::MononokePushRedirectEnable;
use slog::{self, error, o, Drain, Level, Logger};
use slog_ext::SimpleFormatWithError;
use slog_kvfilter::KVFilter;
use stats::prelude::*;
use time_ext::DurationExt;
use tracing::{trace_args, TraceContext, TraceId, Traced};

use hgproto::{sshproto, HgProtoHandler};
use repo_client::RepoClient;
use scuba_ext::ScubaSampleBuilderExt;
use sshrelay::{SenderBytesWrite, SshEnvVars, Stdio};

use crate::repo_handlers::RepoHandler;

use context::{is_quicksand, LoggingContainer, Metric, SessionContainer};
use hooks::HookManager;

lazy_static! {
    static ref DATACENTER_REGION_PREFIX: String = {
        FbWhoAmI::new()
            .expect("failed to init fbwhoami")
            .get_region_data_center_prefix()
            .expect("failed to get region from fbwhoami")
            .to_string()
    };
}

const DEFAULT_PERCENTAGE: f64 = 100.0;

define_stats! {
    prefix = "mononoke.request_handler";
    wireproto_ms:
        histogram(500, 0, 100_000, Average, Sum, Count; P 5; P 25; P 50; P 75; P 95; P 97; P 99),
}

pub fn request_handler(
    fb: FacebookInit,
    RepoHandler {
        logger,
        mut scuba,
        wireproto_logging,
        repo,
        hash_validation_percentage,
        preserve_raw_bundle2,
        pure_push_allowed,
        support_bundle2_listkeys,
        maybe_push_redirector,
    }: RepoHandler,
    stdio: Stdio,
    hook_manager: Arc<HookManager>,
    load_limiting_config: Option<(ConfigHandle<MononokeThrottleLimits>, String)>,
    pushredirect_config: Option<ConfigHandle<MononokePushRedirectEnable>>,
) -> impl Future<Item = (), Error = ()> {
    let Stdio {
        stdin,
        stdout,
        stderr,
        mut preamble,
    } = stdio;

    let session_id = match preamble
        .misc
        .get("session_uuid")
        .map(SessionId::from_string)
    {
        Some(session_id) => session_id,
        None => {
            let session_id = generate_session_id();
            preamble
                .misc
                .insert("session_uuid".to_owned(), session_id.to_string());
            session_id
        }
    };

    // Info per wireproto command within this session
    let wireproto_calls = Arc::new(Mutex::new(Vec::new()));
    let trace = TraceContext::new(TraceId::from_string(session_id.to_string()), Instant::now());

    // Per-connection logging drain that forks output to normal log and back to client stderr
    let conn_log = {
        let stderr_write = SenderBytesWrite {
            chan: stderr.wait(),
        };
        let client_drain = slog_term::PlainSyncDecorator::new(stderr_write);
        let client_drain = SimpleFormatWithError::new(client_drain);
        let client_drain = KVFilter::new(client_drain, Level::Critical).only_pass_any_on_all_keys(
            (hashmap! {
                "remote".into() => hashset!["true".into(), "remote_only".into()],
            })
            .into(),
        );

        let server_drain = KVFilter::new(logger, Level::Critical).always_suppress_any(
            (hashmap! {
                "remote".into() => hashset!["remote_only".into()],
            })
            .into(),
        );

        // Don't fail logging if the client goes away
        let drain = slog::Duplicate::new(client_drain, server_drain).ignore_res();
        Logger::root(drain, o!("session_uuid" => format!("{}", session_id)))
    };

    scuba.log_with_msg("Connection established", None);
    let client_hostname = preamble
        .misc
        .get("source_hostname")
        .cloned()
        .unwrap_or("".to_string());

    let ssh_env_vars = SshEnvVars::from_map(&preamble.misc);

    let load_limiting_config = load_limiting_config.map(|(config, category)| {
        let (throttle_limits, rate_limits) =
            loadlimiting_configs(config, client_hostname, &ssh_env_vars);
        (throttle_limits, rate_limits, category)
    });
    let session = SessionContainer::new(
        fb,
        session_id,
        trace.clone(),
        preamble.misc.get("unix_username").cloned(),
        preamble.misc.get("source_hostname").cloned(),
        None,
        ssh_env_vars,
        load_limiting_config,
    );

    let logging = LoggingContainer::new(conn_log.clone(), scuba.clone());

    // Construct a hg protocol handler
    let proto_handler = HgProtoHandler::new(
        conn_log.clone(),
        stdin,
        RepoClient::new(
            repo.clone(),
            session.clone(),
            logging,
            hash_validation_percentage,
            preserve_raw_bundle2,
            pure_push_allowed,
            hook_manager,
            support_bundle2_listkeys,
            wireproto_logging,
            maybe_push_redirector,
            pushredirect_config,
        ),
        sshproto::HgSshCommandDecode,
        sshproto::HgSshCommandEncode,
        wireproto_calls.clone(),
    );

    // send responses back
    let endres = proto_handler
        .inspect(move |bytes| session.bump_load(Metric::EgressBytes, bytes.len() as f64))
        .map_err(Error::from)
        .forward(stdout)
        .map(|_| ());

    // If we got an error at this point, then catch it and print a message
    endres
        .traced(&trace, "wireproto request", trace_args!())
        .timed(move |stats, result| {
            let mut wireproto_calls = wireproto_calls.lock().expect("lock poisoned");
            let wireproto_calls = mem::replace(&mut *wireproto_calls, Vec::new());

            STATS::wireproto_ms.add_value(stats.completion_time.as_millis_unchecked() as i64);

            let mut scuba = scuba.clone();

            scuba
                .add_future_stats(&stats)
                .add("wireproto_commands", wireproto_calls);

            match result {
                Ok(_) => scuba.log_with_msg("Request finished - Success", None),
                Err(err) => {
                    scuba.log_with_msg("Request finished - Failure", format!("{:#?}", err));
                }
            }
            scuba.log_with_trace(fb, &trace)
        })
        .map_err(move |err| {
            error!(&conn_log, "Command failed";
                SlogKVError(err),
                "remote" => "true"
            );
        })
}

fn loadlimiting_configs(
    config: ConfigHandle<MononokeThrottleLimits>,
    client_hostname: String,
    ssh_env_vars: &SshEnvVars,
) -> (MononokeThrottleLimit, RateLimits) {
    let is_quicksand = is_quicksand(&ssh_env_vars);

    let config = config.get();
    let region_percentage = config
        .datacenter_prefix_capacity
        .get(&*DATACENTER_REGION_PREFIX)
        .copied()
        .unwrap_or(DEFAULT_PERCENTAGE);
    let host_scheme = hostname_scheme(client_hostname);
    let limit = config
        .hostprefixes
        .get(&host_scheme)
        .unwrap_or(&config.defaults);

    let multiplier = if is_quicksand {
        region_percentage / 100.0 * config.quicksand_multiplier
    } else {
        region_percentage / 100.0
    };

    let throttle_limits = MononokeThrottleLimit {
        egress_bytes: limit.egress_bytes * multiplier,
        ingress_blobstore_bytes: limit.ingress_blobstore_bytes * multiplier,
        total_manifests: limit.total_manifests * multiplier,
        quicksand_manifests: limit.quicksand_manifests * multiplier,
        getfiles_files: limit.getfiles_files * multiplier,
        getpack_files: limit.getpack_files * multiplier,
        commits: limit.commits * multiplier,
    };

    (throttle_limits, config.rate_limits.clone())
}

/// Translates a hostname in to a host scheme:
///   devvm001.lla1.facebook.com -> devvm
///   hg001.lla1.facebook.com -> hg
fn hostname_scheme(hostname: String) -> String {
    let mut hostprefix = hostname.clone();
    let index = hostprefix.find(|c: char| !c.is_ascii_alphabetic());
    match index {
        Some(index) => hostprefix.truncate(index),
        None => {}
    }
    hostprefix
}
