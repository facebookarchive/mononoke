/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{format_err, Error};
use ascii::AsciiString;
use cloned::cloned;
use futures::{prelude::*, stream};
use futures_ext::{try_boxfuture, BoxFuture, FutureExt};
use slog::{info, Logger};

use blobrepo::BlobRepo;
use bookmarks::{BookmarkName, BookmarkUpdateReason};
use context::CoreContext;
use mercurial_revlog::RevlogRepo;
use mercurial_types::HgChangesetId;
use mononoke_types::ChangesetId;

pub fn read_bookmarks(revlogrepo: RevlogRepo) -> BoxFuture<Vec<(Vec<u8>, HgChangesetId)>, Error> {
    let bookmarks = Arc::new(try_boxfuture!(revlogrepo.get_bookmarks()));

    (*bookmarks)
        .keys()
        .and_then({
            let bookmarks = bookmarks.clone();
            move |key| {
                (*bookmarks).get(&key).and_then(move |cs_id| {
                    cs_id
                        .ok_or_else(|| format_err!("Bookmark value missing: {:?}", key))
                        .map(move |cs_id| (key, cs_id))
                })
            }
        })
        .collect()
        .boxify()
}

pub type BookmarkNameTransformer =
    Box<dyn Fn(BookmarkName) -> BookmarkName + Send + Sync + 'static>;

pub fn get_bookmark_prefixer(prefix: AsciiString) -> BookmarkNameTransformer {
    Box::new(move |bookmark| {
        let mut name = prefix.clone();
        name.push_str(bookmark.as_ascii());
        BookmarkName::new_ascii(name)
    })
}

pub fn upload_bookmarks(
    ctx: CoreContext,
    logger: &Logger,
    revlogrepo: RevlogRepo,
    blobrepo: BlobRepo,
    stale_bookmarks: Vec<(Vec<u8>, HgChangesetId)>,
    mononoke_bookmarks: Vec<(BookmarkName, ChangesetId)>,
    bookmark_name_transformer: BookmarkNameTransformer,
) -> BoxFuture<(), Error> {
    let logger = logger.clone();
    let stale_bookmarks = Arc::new(stale_bookmarks.into_iter().collect::<HashMap<_, _>>());

    read_bookmarks(revlogrepo)
        .map({
            cloned!(ctx, logger, blobrepo, stale_bookmarks);
            move |bookmarks| {
                stream::futures_unordered(bookmarks.into_iter().map(|(key, cs_id)| {
                    blobrepo
                        .changeset_exists(ctx.clone(), cs_id)
                        .and_then({
                            cloned!(ctx, logger, key, blobrepo, stale_bookmarks);
                            move |exists| {
                                match (exists, stale_bookmarks.get(&key).cloned()) {
                                    (false, Some(stale_cs_id)) => {
                                        info!(
                                            logger,
                                            "current version of bookmark {:?} couldn't be \
                                            imported, because cs {:?} was not present in blobrepo \
                                            yet; using stale version instead {:?}",
                                            key,
                                            cs_id,
                                            stale_cs_id,
                                        );

                                        blobrepo
                                            .changeset_exists(ctx, stale_cs_id)
                                            .map(move |exists| (key, stale_cs_id, exists))
                                            .boxify()
                                    }
                                    _ => Ok((key, cs_id, exists)).into_future().boxify(),
                                }
                            }})
                        .and_then({
                            cloned!(ctx, blobrepo, logger);
                            move |(key, cs_id, exists)| {
                                if exists {
                                    blobrepo.get_bonsai_from_hg(ctx, cs_id)
                                        .and_then(move |bcs_id| bcs_id.ok_or(
                                            format_err!("failed to resolve hg to bonsai: {}", cs_id),
                                        ))
                                        .map(move |bcs_id| Some((key, bcs_id)))
                                        .left_future()
                                } else {
                                    info!(
                                        logger,
                                        "did not update bookmark {:?}, because cs {:?} was not imported yet",
                                        key,
                                        cs_id,
                                    );
                                    Ok(None).into_future().right_future()
                                }
                            }
                        })
                }))
            }
        })
        .flatten_stream()
        .filter_map(|key_cs_id| key_cs_id)
        .chunks(100) // send 100 bookmarks in a single transaction
        .and_then({
            let blobrepo = blobrepo.clone();
            let mononoke_bookmarks: HashMap<_, _> = mononoke_bookmarks.into_iter().collect();
            move |vec| {
                let mut transaction = blobrepo.update_bookmark_transaction(ctx.clone());

                let mut count = 0;
                for (key, value) in vec {
                    let bookmark_name = BookmarkName::new_ascii(try_boxfuture!(AsciiString::from_ascii(key)));
                    let bookmark_name = bookmark_name_transformer(bookmark_name);
                    if mononoke_bookmarks.get(&bookmark_name) != Some(&value) {
                        count += 1;
                        try_boxfuture!(transaction.force_set(&bookmark_name, value, BookmarkUpdateReason::Blobimport))
                    }
                }

                if count > 0 {
                    transaction.commit()
                        .and_then(move |ok| {
                            if ok {
                                Ok(count)
                            } else {
                                Err(format_err!("Bookmark transaction failed"))
                            }
                        })
                        .boxify()
                } else {
                    Ok(0).into_future().boxify()
                }
            }
        }).for_each(move |count| {
            info!(logger, "uploaded chunk of {:?} bookmarks", count);
            Ok(())
        }).boxify()
}
