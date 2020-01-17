/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use crate::setup::{RepoWalkDatasources, RepoWalkParams};
use crate::walk::{walk_exact, WalkVisitor};

use anyhow::Error;
use cloned::cloned;
use context::CoreContext;
use futures::{
    stream::{repeat, Stream},
    Future,
};
use futures_ext::{BoxStream, FutureExt};
use std::time::{Duration, Instant};
use tokio_timer::Delay;

pub fn walk_exact_tail<SinkFac, SinkOut, WS, VOut>(
    ctx: CoreContext,
    datasources: RepoWalkDatasources,
    walk_params: RepoWalkParams,
    walk_state: WS,
    make_sink: SinkFac,
) -> impl Future<Item = (), Error = Error>
where
    SinkFac: 'static + Fn(BoxStream<VOut, Error>) -> SinkOut,
    SinkOut: Future<Item = (), Error = Error>,
    WS: 'static + Clone + WalkVisitor<VOut> + Send,
    VOut: 'static + Send,
{
    let scuba_builder = datasources.scuba_builder;
    let datasources = datasources.blobrepo.join(datasources.phases_store);
    let traversal_fut = datasources.and_then(move |(repo, phases_store)| {
        cloned!(walk_params.tail_secs);
        let stream = repeat(()).and_then({
            move |()| {
                cloned!(ctx, repo, phases_store, walk_state,);
                let walk_output = walk_exact(
                    ctx,
                    repo,
                    phases_store,
                    walk_params.enable_derive,
                    walk_params.walk_roots.clone(),
                    walk_state,
                    walk_params.scheduled_max,
                    walk_params.error_as_data_node_types.clone(),
                    walk_params.error_as_data_edge_types.clone(),
                    scuba_builder.clone(),
                );
                make_sink(walk_output)
            }
        });
        match tail_secs {
            // NOTE: This would be a lot nicer with async / await since could just .next().await
            None => stream
                .into_future()
                .map(|_| ())
                .map_err(|(e, _)| e)
                .left_future(),
            Some(interval) => stream
                .for_each(move |_| {
                    let start = Instant::now();
                    let next_iter_deadline = start + Duration::from_secs(interval);
                    Delay::new(next_iter_deadline).from_err()
                })
                .right_future(),
        }
    });
    traversal_fut
}
