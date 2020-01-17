/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use anyhow::Error;
use blobstore::{Blobstore, Loadable, LoadableError, Storable};
use cloned::cloned;
use context::CoreContext;
use futures::{Future, IntoFuture};
use futures_ext::FutureExt;
use mononoke_types::{BlobstoreValue, ContentId, ContentMetadata, ContentMetadataId};
use thiserror::Error;

use crate::alias::alias_stream;
use crate::expected_size::ExpectedSize;
use crate::fetch;

#[derive(Debug, Error)]
pub enum RebuildBackmappingError {
    #[error("Not found: {0:?}")]
    NotFound(ContentId),

    #[error("Error computing metadata for {0:?}: {1:?}")]
    InternalError(ContentId, Error),
}

/// Finds the metadata for a ContentId. Returns None if the content does not exist, and returns
/// the metadata otherwise. This might recompute the metadata on the fly if it is found to
/// be missing but the content exists.
pub fn get_metadata<B: Blobstore + Clone>(
    blobstore: B,
    ctx: CoreContext,
    content_id: ContentId,
) -> impl Future<Item = Option<ContentMetadata>, Error = Error> {
    get_metadata_readonly(&blobstore, ctx.clone(), content_id).and_then({
        move |maybe_metadata| match maybe_metadata {
            // We found the metadata. Return it.
            Some(metadata) => Ok(Some(metadata)).into_future().left_future(),

            // We didn't find the metadata. Try to recompute it. This might fail if the
            // content doesn't exist, or due to an internal error.
            None => rebuild_metadata(blobstore, ctx, content_id)
                .map(Some)
                .or_else({
                    use RebuildBackmappingError::*;
                    |e| match e {
                        // If we didn't find the ContentId we're rebuilding the metadata for,
                        // then there is nothing else to do but indicate this metadata does not
                        // exist.
                        NotFound(_) => Ok(None),
                        // If we ran into some error rebuilding the metadata that isn't not
                        // having found the content, then we pass it up.
                        e @ InternalError(..) => Err(e.into()),
                    }
                })
                .right_future(),
        }
    })
}

/// Finds the metadata for a ContentId. Returns None if the content metadata does not exist
/// and returns Some(metadata) if it already exists. Does not recompute it on the fly.
pub fn get_metadata_readonly<B: Blobstore + Clone>(
    blobstore: &B,
    ctx: CoreContext,
    content_id: ContentId,
) -> impl Future<Item = Option<ContentMetadata>, Error = Error> {
    ContentMetadataId::from(content_id)
        .load(ctx.clone(), blobstore)
        .map(Some)
        .or_else(|err| match err {
            LoadableError::Error(err) => Err(err),
            LoadableError::Missing(_) => Ok(None),
        })
}

/// If the metadata is missing, we can rebuild it on the fly, since all that's needed to do so
/// is the file contents. This can happen if we successfully stored a file, but failed to store
/// its metadata. To rebuild the metadata, we peek at the content in the blobstore to get
/// its size, then produce a stream of its contents and compute aliases over it. Finally, store
/// the metadata, and return it.
fn rebuild_metadata<B: Blobstore + Clone>(
    blobstore: B,
    ctx: CoreContext,
    content_id: ContentId,
) -> impl Future<Item = ContentMetadata, Error = RebuildBackmappingError> {
    use RebuildBackmappingError::*;

    content_id
        .load(ctx.clone(), &blobstore)
        .or_else(move |err| match err {
            LoadableError::Error(err) => Err(InternalError(content_id, err)),
            LoadableError::Missing(_) => Err(NotFound(content_id)),
        })
        .and_then({
            cloned!(blobstore, ctx);
            move |file_contents| {
                // NOTE: We implicitly trust data from the Filestore here. We do not validate
                // the size, nor the ContentId.
                let total_size = file_contents.size();
                let content_stream =
                    fetch::stream_file_bytes(blobstore, ctx, file_contents, fetch::Range::All);

                alias_stream(ExpectedSize::new(total_size), content_stream)
                    .from_err()
                    .and_then(move |redeemable| Ok((redeemable.redeem(total_size)?, total_size)))
                    .map_err(move |e| InternalError(content_id, e))
            }
        })
        .and_then({
            cloned!(blobstore, ctx);
            move |(aliases, total_size)| {
                let (sha1, sha256, git_sha1) = aliases;

                let metadata = ContentMetadata {
                    total_size,
                    content_id,
                    sha1,
                    sha256,
                    git_sha1,
                };

                let blob = metadata.clone().into_blob();

                blob.store(ctx, &blobstore)
                    .map_err(move |e| InternalError(content_id, e))
                    .map(|_| metadata)
            }
        })
}
