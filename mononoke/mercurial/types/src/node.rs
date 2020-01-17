/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use anyhow::Error;
use futures::{Future, Stream};

/// A general source control Node
///
/// A `Node` has some content, and some number of `HgParents` (immediate ancestors).
/// For Mercurial this is constrained to [0, 2] parents, but other scms (ie Git) can have
/// arbitrary numbers of parents.
///
/// NOTE: Unless you're writing code that should be general across multiple source control
/// systems, don't use Node. For example, use HgBlobNode or manifest::Entry for Mercurial-specific
/// code.
pub trait Node: Sized {
    type Content;

    type GetParents: Stream<Item = Self, Error = Error>;
    type GetContent: Future<Item = Self::Content, Error = Error>;

    fn get_parents(&self) -> Self::GetParents;
    fn get_content(&self) -> Self::GetContent;
}
