/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use clap::ArgMatches;
use cmdlib::args;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::prelude::*;
use futures_ext::{BoxFuture, FutureExt};
use mononoke_types::{BonsaiChangeset, ChangesetId, DateTime, FileChange};
use serde_derive::Serialize;
use slog::Logger;
use std::collections::BTreeMap;

use crate::common::fetch_bonsai_changeset;
use crate::error::SubcommandError;

pub fn subcommand_bonsai_fetch(
    fb: FacebookInit,
    logger: Logger,
    matches: &ArgMatches<'_>,
    sub_m: &ArgMatches<'_>,
) -> BoxFuture<(), SubcommandError> {
    let rev = sub_m.value_of("CHANGESET_ID").unwrap().to_string();

    args::init_cachelib(fb, &matches);

    let ctx = CoreContext::new_with_logger(fb, logger.clone());
    let json_flag = sub_m.is_present("json");

    args::open_repo(fb, &logger, &matches)
        .and_then(move |blobrepo| fetch_bonsai_changeset(ctx, &rev, &blobrepo))
        .map(move |bcs| {
            if json_flag {
                match serde_json::to_string(&SerializableBonsaiChangeset::from(bcs)) {
                    Ok(json) => println!("{}", json),
                    Err(e) => println!("{}", e),
                }
            } else {
                println!(
                    "BonsaiChangesetId: {} \n\
                     Author: {} \n\
                     Message: {} \n\
                     FileChanges:",
                    bcs.get_changeset_id(),
                    bcs.author(),
                    bcs.message().lines().next().unwrap_or("")
                );

                for (path, file_change) in bcs.file_changes() {
                    match file_change {
                        Some(file_change) => match file_change.copy_from() {
                            Some(_) => {
                                println!("\t COPY/MOVE: {} {}", path, file_change.content_id())
                            }
                            None => {
                                println!("\t ADDED/MODIFIED: {} {}", path, file_change.content_id())
                            }
                        },
                        None => println!("\t REMOVED: {}", path),
                    }
                }
            }
        })
        .from_err()
        .boxify()
}

#[derive(Serialize)]
pub struct SerializableBonsaiChangeset {
    pub parents: Vec<ChangesetId>,
    pub author: String,
    pub author_date: DateTime,
    pub committer: Option<String>,
    // XXX should committer date always be recorded? If so, it should probably be a
    // monotonically increasing value:
    // max(author date, max(committer date of parents) + epsilon)
    pub committer_date: Option<DateTime>,
    pub message: String,
    pub extra: BTreeMap<String, Vec<u8>>,
    pub file_changes: BTreeMap<String, Option<FileChange>>,
}

impl From<BonsaiChangeset> for SerializableBonsaiChangeset {
    fn from(bonsai: BonsaiChangeset) -> Self {
        let mut parents = Vec::new();
        parents.extend(bonsai.parents());

        let author = bonsai.author().to_string();
        let author_date = bonsai.author_date().clone();

        let committer = bonsai.committer().map(|s| s.to_string());
        let committer_date = bonsai.committer_date().cloned();

        let message = bonsai.message().to_string();

        let extra = bonsai
            .extra()
            .map(|(k, v)| (k.to_string(), v.to_vec()))
            .collect();

        let file_changes = bonsai
            .file_changes()
            .map(|(k, v)| {
                (
                    String::from_utf8(k.to_vec()).expect("Found invalid UTF-8"),
                    v.cloned(),
                )
            })
            .collect();

        SerializableBonsaiChangeset {
            parents,
            author,
            author_date,
            committer,
            committer_date,
            message,
            extra,
            file_changes,
        }
    }
}
