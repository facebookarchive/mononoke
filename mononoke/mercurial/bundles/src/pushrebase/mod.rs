/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

// Pushrebase codecs

use anyhow::{Error, Result};
use bytes::BytesMut;
use mercurial_types::{HgChangesetId, HgNodeHash};
use tokio_codec::Decoder;

#[derive(Debug)]
pub struct CommonHeadsUnpacker {}

impl CommonHeadsUnpacker {
    pub fn new() -> Self {
        Self {}
    }
}

impl Decoder for CommonHeadsUnpacker {
    type Item = HgChangesetId;
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>> {
        if buf.len() >= 20 {
            let newcsid = buf.split_to(20).freeze();
            let nodehash = HgNodeHash::from_bytes(&newcsid)?;
            Ok(Some(HgChangesetId::new(nodehash)))
        } else {
            Ok(None)
        }
    }
}
