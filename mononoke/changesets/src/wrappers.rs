/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

//! Implementations for wrappers that enable dynamic dispatch. Add more as necessary.

use std::sync::Arc;

use anyhow::Error;
use context::CoreContext;
use futures_ext::BoxFuture;
use mononoke_types::{ChangesetId, RepositoryId};

use crate::{ChangesetEntry, ChangesetInsert, Changesets};

impl Changesets for Arc<dyn Changesets> {
    fn add(&self, ctx: CoreContext, cs: ChangesetInsert) -> BoxFuture<bool, Error> {
        (**self).add(ctx, cs)
    }

    fn get(
        &self,
        ctx: CoreContext,
        repo_id: RepositoryId,
        cs_id: ChangesetId,
    ) -> BoxFuture<Option<ChangesetEntry>, Error> {
        (**self).get(ctx, repo_id, cs_id)
    }

    fn get_many(
        &self,
        ctx: CoreContext,
        repo_id: RepositoryId,
        cs_ids: Vec<ChangesetId>,
    ) -> BoxFuture<Vec<ChangesetEntry>, Error> {
        (**self).get_many(ctx, repo_id, cs_ids)
    }
}
