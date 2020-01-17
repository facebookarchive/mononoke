/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use crate::{ErrorKind, FastlogParent};
use anyhow::Error;
use blobstore::{Blobstore, BlobstoreBytes};
use cloned::cloned;
use context::CoreContext;
use futures::{future, Future};
use futures_ext::{BoxFuture, FutureExt};
use manifest::Entry;
use maplit::hashset;
use mononoke_types::{
    fastlog_batch::{FastlogBatch, ParentOffset},
    ChangesetId, FileUnodeId, ManifestUnodeId,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

pub(crate) fn create_new_batch(
    ctx: CoreContext,
    blobstore: Arc<dyn Blobstore>,
    unode_parents: Vec<Entry<ManifestUnodeId, FileUnodeId>>,
    linknode: ChangesetId,
) -> impl Future<Item = FastlogBatch, Error = Error> {
    let f = future::join_all(unode_parents.clone().into_iter().map({
        cloned!(ctx, blobstore);
        move |entry| {
            fetch_fastlog_batch_by_unode_id(ctx.clone(), blobstore.clone(), entry)
                .and_then(move |maybe_batch| maybe_batch.ok_or(ErrorKind::NotFound(entry).into()))
        }
    }));

    f.and_then(move |parent_batches| {
        if parent_batches.len() < 2 {
            match parent_batches.get(0) {
                Some(parent_batch) => parent_batch
                    .prepend_child_with_single_parent(ctx, blobstore, linknode)
                    .boxify(),
                None => {
                    let mut d = VecDeque::new();
                    d.push_back((linknode, vec![]));
                    FastlogBatch::new_from_raw_list(ctx, blobstore, d).boxify()
                }
            }
        } else {
            future::join_all(parent_batches.into_iter().map({
                cloned!(ctx, blobstore);
                move |batch| fetch_flattened(&batch, ctx.clone(), blobstore.clone())
            }))
            .map(move |parents_flattened| create_merged_list(linknode, parents_flattened))
            // Converts FastlogParent to ParentOffset
            .map(convert_to_raw_list)
            .and_then(move |raw_list| {
                FastlogBatch::new_from_raw_list(ctx, blobstore, raw_list)
            })
            .boxify()
        }
    })
}

// This function creates a FastlogBatch list for a merge unode.
// It does so by taking a merge_cs_id (i.e. a linknode of this merge unode) and
// FastlogBatches for it's parents and merges them together in BFS order
//
// For example, let's say we have a unode whose history graph is the following:
//
//             o <- commit A
//            / \
// commit B  o   \
//           \   o <- commit C
//            \ /
//             o <- commit D
//
// create_merged_list() accepts commit A as merge_cs_id, [B, D] as a first parent's list
// and [C, D] as the second parent's list. The expected output is [A, B, C, D].
fn create_merged_list(
    merge_cs_id: ChangesetId,
    parents_lists: Vec<Vec<(ChangesetId, Vec<FastlogParent>)>>,
) -> Vec<(ChangesetId, Vec<FastlogParent>)> {
    // parents_of_merge_commits preserve the order of `parents_lists`
    let mut parents_of_merge_commit = vec![];
    for list in parents_lists.iter() {
        if let Some((p, _)) = list.get(0) {
            parents_of_merge_commit.push(FastlogParent::Known(*p));
        }
    }
    {
        // Make sure we have unique parents
        let mut used = HashSet::new();
        parents_of_merge_commit.retain(move |p| used.insert(p.clone()));
    }

    let mut cs_id_to_parents: HashMap<_, _> = parents_lists
        .into_iter()
        .map(|list| list.into_iter())
        .flatten()
        .collect();
    cs_id_to_parents.insert(merge_cs_id, parents_of_merge_commit.clone());

    let mut q = VecDeque::new();
    q.push_back((merge_cs_id, parents_of_merge_commit));

    let mut res = vec![];
    let mut used = hashset! {merge_cs_id};
    while let Some((cs_id, parents)) = q.pop_front() {
        res.push((cs_id, parents.clone()));

        for p in parents {
            if let FastlogParent::Known(p) = p {
                if let Some(parents) = cs_id_to_parents.get(&p) {
                    if !used.contains(&p) {
                        used.insert(p);
                        q.push_back((p, parents.clone()));
                    }
                }
            }
        }
    }

    res
}

// Converts from an "external" representation (i.e. the one used by users of this library)
// to an "internal" representation (i.e. the one that we store in the blobstore).
fn convert_to_raw_list(
    list: Vec<(ChangesetId, Vec<FastlogParent>)>,
) -> Vec<(ChangesetId, Vec<ParentOffset>)> {
    let cs_to_idx: HashMap<_, _> = list
        .iter()
        .enumerate()
        .map(|(idx, (cs_id, _))| (*cs_id, idx as i32))
        .collect();

    // Special offset that points outside of the list.
    // It's used for unknown parents
    let max_idx = (list.len() + 1) as i32;
    let mut res = vec![];
    for (current_idx, (cs_id, fastlog_parents)) in list.into_iter().enumerate() {
        let current_idx = current_idx as i32;
        let mut parent_offsets = vec![];
        for p in fastlog_parents {
            let maybe_idx = match p {
                FastlogParent::Known(cs_id) => {
                    cs_to_idx.get(&cs_id).cloned().map(|idx| idx - current_idx)
                }
                FastlogParent::Unknown => None,
            };

            parent_offsets.push(ParentOffset::new(maybe_idx.unwrap_or(max_idx)))
        }
        res.push((cs_id, parent_offsets));
    }

    res
}

pub(crate) fn fetch_fastlog_batch_by_unode_id(
    ctx: CoreContext,
    blobstore: Arc<dyn Blobstore>,
    unode_entry: Entry<ManifestUnodeId, FileUnodeId>,
) -> impl Future<Item = Option<FastlogBatch>, Error = Error> {
    let fastlog_batch_key = generate_fastlog_batch_key(unode_entry);

    blobstore
        .get(ctx, fastlog_batch_key.clone())
        .and_then(move |maybe_bytes| match maybe_bytes {
            Some(serialized) => FastlogBatch::from_bytes(serialized.as_bytes()).map(Some),
            None => Ok(None),
        })
}

pub(crate) fn save_fastlog_batch_by_unode_id(
    ctx: CoreContext,
    blobstore: Arc<dyn Blobstore>,
    unode_entry: Entry<ManifestUnodeId, FileUnodeId>,
    batch: FastlogBatch,
) -> BoxFuture<(), Error> {
    let fastlog_batch_key = generate_fastlog_batch_key(unode_entry);
    let serialized = batch.into_bytes();

    blobstore.put(
        ctx,
        fastlog_batch_key,
        BlobstoreBytes::from_bytes(serialized),
    )
}

fn generate_fastlog_batch_key(unode_entry: Entry<ManifestUnodeId, FileUnodeId>) -> String {
    let key_part = match unode_entry {
        Entry::Leaf(file_unode_id) => format!("fileunode.{}", file_unode_id),
        Entry::Tree(mf_unode_id) => format!("manifestunode.{}", mf_unode_id),
    };
    format!("fastlogbatch.{}", key_part)
}

pub(crate) fn fetch_flattened(
    batch: &FastlogBatch,
    ctx: CoreContext,
    blobstore: Arc<dyn Blobstore>,
) -> impl Future<Item = Vec<(ChangesetId, Vec<FastlogParent>)>, Error = Error> {
    batch.fetch_raw_list(ctx, blobstore).map(flatten_raw_list)
}

fn flatten_raw_list(
    raw_list: Vec<(ChangesetId, Vec<ParentOffset>)>,
) -> Vec<(ChangesetId, Vec<FastlogParent>)> {
    let mut res = vec![];
    for (index, (cs_id, parent_offsets)) in raw_list.iter().enumerate() {
        let mut batch_parents = vec![];
        for offset in parent_offsets {
            // NOTE: Offset can be negative!
            let parent_index = index as i32 + offset.num();
            let batch_parent = if parent_index >= 0 {
                match raw_list.get(parent_index as usize) {
                    Some((p_cs_id, _)) => FastlogParent::Known(*p_cs_id),
                    None => FastlogParent::Unknown,
                }
            } else {
                FastlogParent::Unknown
            };
            batch_parents.push(batch_parent);
        }

        res.push((*cs_id, batch_parents));
    }

    res
}

#[cfg(test)]
mod test {
    use super::*;
    use fbinit::FacebookInit;
    use fixtures::linear;
    use mononoke_types_mocks::changesetid::{ONES_CSID, THREES_CSID, TWOS_CSID};
    use tokio_compat::runtime::Runtime;

    #[fbinit::test]
    fn fetch_flattened_simple(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = linear::getrepo(fb);
        let mut rt = Runtime::new().unwrap();
        let mut d = VecDeque::new();
        d.push_back((ONES_CSID, vec![]));
        let blobstore = Arc::new(repo.get_blobstore());
        let batch = rt.block_on(FastlogBatch::new_from_raw_list(
            ctx.clone(),
            blobstore.clone(),
            d,
        ))?;

        assert_eq!(
            vec![(ONES_CSID, vec![])],
            rt.block_on(fetch_flattened(&batch, ctx, blobstore))
                .unwrap()
        );
        Ok(())
    }

    #[fbinit::test]
    fn fetch_flattened_prepend(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = linear::getrepo(fb);
        let mut rt = Runtime::new().unwrap();
        let mut d = VecDeque::new();
        d.push_back((ONES_CSID, vec![]));
        let blobstore = Arc::new(repo.get_blobstore());
        let batch = rt.block_on(FastlogBatch::new_from_raw_list(
            ctx.clone(),
            blobstore.clone(),
            d,
        ))?;

        assert_eq!(
            vec![(ONES_CSID, vec![])],
            rt.block_on(fetch_flattened(&batch, ctx.clone(), blobstore.clone()))
                .unwrap()
        );

        let prepended = rt
            .block_on(batch.prepend_child_with_single_parent(
                ctx.clone(),
                blobstore.clone(),
                TWOS_CSID,
            ))
            .unwrap();
        assert_eq!(
            vec![
                (TWOS_CSID, vec![FastlogParent::Known(ONES_CSID)]),
                (ONES_CSID, vec![])
            ],
            rt.block_on(fetch_flattened(&prepended, ctx.clone(), blobstore.clone()))
                .unwrap()
        );

        let prepended = rt
            .block_on(prepended.prepend_child_with_single_parent(
                ctx.clone(),
                blobstore.clone(),
                THREES_CSID,
            ))
            .unwrap();
        assert_eq!(
            vec![
                (THREES_CSID, vec![FastlogParent::Known(TWOS_CSID)]),
                (TWOS_CSID, vec![FastlogParent::Known(ONES_CSID)]),
                (ONES_CSID, vec![])
            ],
            rt.block_on(fetch_flattened(&prepended, ctx, blobstore))
                .unwrap()
        );

        Ok(())
    }

    #[test]
    fn test_create_merged_list() -> Result<(), Error> {
        assert_eq!(
            create_merged_list(ONES_CSID, vec![]),
            vec![(ONES_CSID, vec![])]
        );

        let first_parent = vec![(TWOS_CSID, vec![])];
        let second_parent = vec![(THREES_CSID, vec![])];
        assert_eq!(
            create_merged_list(ONES_CSID, vec![first_parent, second_parent]),
            vec![
                (
                    ONES_CSID,
                    vec![
                        FastlogParent::Known(TWOS_CSID),
                        FastlogParent::Known(THREES_CSID)
                    ]
                ),
                (TWOS_CSID, vec![]),
                (THREES_CSID, vec![]),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_create_merged_list_same_commit() -> Result<(), Error> {
        assert_eq!(
            create_merged_list(ONES_CSID, vec![]),
            vec![(ONES_CSID, vec![])]
        );

        let first_parent = vec![(TWOS_CSID, vec![])];
        let second_parent = vec![(TWOS_CSID, vec![])];
        assert_eq!(
            create_merged_list(ONES_CSID, vec![first_parent, second_parent]),
            vec![
                (ONES_CSID, vec![FastlogParent::Known(TWOS_CSID),]),
                (TWOS_CSID, vec![]),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_convert_to_raw_list_simple() -> Result<(), Error> {
        let list = vec![
            (
                ONES_CSID,
                vec![
                    FastlogParent::Known(TWOS_CSID),
                    FastlogParent::Known(THREES_CSID),
                ],
            ),
            (TWOS_CSID, vec![]),
            (THREES_CSID, vec![]),
        ];

        let raw_list = Vec::from(convert_to_raw_list(list.clone()));
        let expected = vec![
            (ONES_CSID, vec![ParentOffset::new(1), ParentOffset::new(2)]),
            (TWOS_CSID, vec![]),
            (THREES_CSID, vec![]),
        ];
        assert_eq!(raw_list, expected);
        assert_eq!(flatten_raw_list(raw_list), list);

        let list = vec![
            (ONES_CSID, vec![FastlogParent::Known(TWOS_CSID)]),
            (TWOS_CSID, vec![FastlogParent::Known(THREES_CSID)]),
            (THREES_CSID, vec![]),
        ];

        let raw_list = Vec::from(convert_to_raw_list(list.clone()));
        let expected = vec![
            (ONES_CSID, vec![ParentOffset::new(1)]),
            (TWOS_CSID, vec![ParentOffset::new(1)]),
            (THREES_CSID, vec![]),
        ];
        assert_eq!(raw_list, expected);
        assert_eq!(flatten_raw_list(raw_list), list);

        Ok(())
    }
}
