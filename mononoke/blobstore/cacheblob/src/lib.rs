/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

#![deny(warnings)]

mod cachelib_cache;
pub use crate::cachelib_cache::{new_cachelib_blobstore, new_cachelib_blobstore_no_lease};

pub mod dummy;

mod in_process_lease;
pub use in_process_lease::InProcessLease;

mod locking_cache;
pub use crate::locking_cache::{
    CacheBlobstore, CacheBlobstoreExt, CacheOps, CacheOpsUtil, LeaseOps,
};

mod memcache_cache_lease;
pub use crate::memcache_cache_lease::{
    new_memcache_blobstore, new_memcache_blobstore_no_lease, MemcacheOps,
};

mod mem_writes;
pub use crate::mem_writes::MemWritesBlobstore;
