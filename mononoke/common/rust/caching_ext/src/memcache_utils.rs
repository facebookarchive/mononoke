/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

use crate::mock_store::MockStore;
use bytes::Bytes;
use futures::{future::ok, Future};
use futures_ext::FutureExt;
use iobuf::IOBuf;
use memcache::MemcacheClient;
use std::sync::atomic::Ordering;

#[derive(Clone)]
pub enum MemcacheHandler {
    Real(MemcacheClient),
    #[allow(dead_code)]
    Mock(MockStore<Bytes>),
}

impl From<MemcacheClient> for MemcacheHandler {
    fn from(client: MemcacheClient) -> Self {
        MemcacheHandler::Real(client)
    }
}

impl MemcacheHandler {
    pub fn get(&self, key: String) -> impl Future<Item = Option<IOBuf>, Error = ()> {
        match self {
            MemcacheHandler::Real(ref client) => client.get(key).left_future(),
            MemcacheHandler::Mock(store) => {
                ok(store.get(&key).map(|value| value.clone().into())).right_future()
            }
        }
    }

    pub fn set(&self, key: String, value: Bytes) -> impl Future<Item = (), Error = ()> {
        match self {
            MemcacheHandler::Real(ref client) => client.set(key, value).left_future(),
            MemcacheHandler::Mock(store) => {
                store.set(&key, &value);
                ok(()).right_future()
            }
        }
    }

    #[allow(dead_code)]
    pub fn create_mock() -> Self {
        MemcacheHandler::Mock(MockStore::new())
    }

    #[allow(dead_code)]
    pub(crate) fn gets_count(&self) -> usize {
        match self {
            MemcacheHandler::Real(_) => unimplemented!(),
            MemcacheHandler::Mock(MockStore { ref get_count, .. }) => {
                get_count.load(Ordering::SeqCst)
            }
        }
    }
}
