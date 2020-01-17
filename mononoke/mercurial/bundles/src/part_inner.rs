/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

//! Type definitions for inner streams.
#![deny(warnings)]

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::str;

use anyhow::{bail, ensure, Error, Result};
use bytes::{Bytes, BytesMut};
use futures::{future, Future, Stream};
use futures_ext::{BoxFuture, FutureExt};
use lazy_static::lazy_static;
use maplit::hashset;
use slog::{o, warn, Logger};
use tokio_io::codec::Decoder;
use tokio_io::AsyncRead;

use crate::capabilities;
use crate::changegroup;
use crate::errors::ErrorKind;
use crate::infinitepush;
use crate::part_header::{PartHeader, PartHeaderType};
use crate::part_outer::{OuterFrame, OuterStream};
use crate::pushrebase;
use crate::wirepack;
use crate::Bundle2Item;
use futures_ext::{StreamExt, StreamLayeredExt};

// --- Part parameters

lazy_static! {
    static ref KNOWN_PARAMS: HashMap<PartHeaderType, HashSet<&'static str>> = {
        let mut m: HashMap<PartHeaderType, HashSet<&'static str>> = HashMap::new();
        m.insert(PartHeaderType::Changegroup, hashset!{"version", "nbchanges", "treemanifest"});
        // TODO(stash): currently ignore all the parameters. Later we'll
        // support 'bookmark' parameter, and maybe 'create' and 'force' (although 'force' will
        // probably) be renamed T26385545. 'bookprevnode' and 'pushbackbookmarks' will be
        // removed T26384190.
        m.insert(PartHeaderType::B2xInfinitepush, hashset!{
            "pushbackbookmarks", "cgversion", "bookmark", "bookprevnode", "create", "force"});
        m.insert(PartHeaderType::B2xInfinitepushBookmarks, hashset!{});
        m.insert(PartHeaderType::B2xCommonHeads, hashset!{});
        m.insert(PartHeaderType::B2xRebase, hashset!{"onto", "newhead", "cgversion", "obsmarkerversions"});
        m.insert(PartHeaderType::B2xRebasePack, hashset!{"version", "cache", "category"});
        m.insert(PartHeaderType::B2xTreegroup2, hashset!{"version", "cache", "category"});
        m.insert(PartHeaderType::Replycaps, hashset!{});
        m.insert(PartHeaderType::Pushkey, hashset!{ "namespace", "key", "old", "new" });
        m.insert(PartHeaderType::Pushvars, hashset!{});
        m
    };
}

pub fn validate_header(header: PartHeader) -> Result<Option<PartHeader>> {
    match KNOWN_PARAMS.get(header.part_type()) {
        Some(ref known_params) => {
            // Make sure all the mandatory params are recognized.
            let unknown_params: Vec<_> = header
                .mparams()
                .keys()
                .filter(|param| !known_params.contains(param.as_str()))
                .map(|param| param.clone())
                .collect();
            if !unknown_params.is_empty() {
                bail!(ErrorKind::BundleUnknownPartParams(
                    *header.part_type(),
                    unknown_params,
                ));
            }
            Ok(Some(header))
        }
        None => {
            if header.mandatory() {
                bail!(ErrorKind::BundleUnknownPart(header));
            }
            Ok(None)
        }
    }
}

pub fn get_cg_version(header: PartHeader, field: &str) -> Result<changegroup::unpacker::CgVersion> {
    let version = header.mparams().get(field).or(header.aparams().get(field));
    let err = ErrorKind::CgDecode(format!(
        "No changegroup version in Part Header in field {}",
        field
    ))
    .into();

    version
        .ok_or(err)
        .and_then(|version_bytes| {
            str::from_utf8(version_bytes)
                .map_err(|e| ErrorKind::CgDecode(format!("{:?}", e)).into())
        })
        .and_then(|version_str| {
            version_str
                .parse::<changegroup::unpacker::CgVersion>()
                .map_err(|e| ErrorKind::CgDecode(format!("{:?}", e)).into())
        })
}

pub fn get_cg_unpacker(
    logger: Logger,
    header: PartHeader,
    field: &str,
) -> changegroup::unpacker::CgUnpacker {
    // TODO(anastasiyaz): T34812941 return Result here, no default packer (version should be specified)
    get_cg_version(header, field)
    .map(|version| changegroup::unpacker::CgUnpacker::new(logger.clone(), version))
    // ChangeGroup2 by default
    .unwrap_or_else(|e| {
        warn!(logger, "{:?}", e);
        let default_version = changegroup::unpacker::CgVersion::Cg2Version;
        changegroup::unpacker::CgUnpacker::new(logger, default_version)
    })
}

/// Convert an OuterStream into an InnerStream using the part header.
pub fn inner_stream<R: AsyncRead + BufRead + 'static + Send>(
    logger: Logger,
    header: PartHeader,
    stream: OuterStream<R>,
) -> (Bundle2Item, BoxFuture<OuterStream<R>, Error>) {
    let wrapped_stream = stream
        .take_while(|frame| future::ok(frame.is_payload()))
        .map(OuterFrame::get_payload as fn(OuterFrame) -> Bytes);
    let (wrapped_stream, remainder) = wrapped_stream.return_remainder();

    let bundle2item = match header.part_type() {
        &PartHeaderType::Changegroup => {
            let cg2_stream = wrapped_stream.decode(get_cg_unpacker(
                logger.new(o!("stream" => "changegroup")),
                header.clone(),
                "version",
            ));
            Bundle2Item::Changegroup(header, cg2_stream.boxify())
        }
        &PartHeaderType::B2xCommonHeads => {
            let heads_stream = wrapped_stream.decode(pushrebase::CommonHeadsUnpacker::new());
            Bundle2Item::B2xCommonHeads(header, heads_stream.boxify())
        }
        &PartHeaderType::B2xInfinitepush => {
            let cg2_stream = wrapped_stream.decode(get_cg_unpacker(
                logger.new(o!("stream" => "b2xinfinitepush")),
                header.clone(),
                "cgversion",
            ));
            Bundle2Item::B2xInfinitepush(header, cg2_stream.boxify())
        }
        &PartHeaderType::B2xInfinitepushBookmarks => {
            let bookmarks_stream =
                wrapped_stream.decode(infinitepush::InfinitepushBookmarksUnpacker::new());
            Bundle2Item::B2xInfinitepushBookmarks(header, bookmarks_stream.boxify())
        }
        &PartHeaderType::B2xTreegroup2 => {
            let wirepack_stream = wrapped_stream.decode(wirepack::unpacker::new(
                logger.new(o!("stream" => "wirepack")),
                // Mercurial only knows how to send trees at the moment.
                // TODO: add support for file wirepacks once that's a thing
                wirepack::Kind::Tree,
            ));
            Bundle2Item::B2xTreegroup2(header, wirepack_stream.boxify())
        }
        &PartHeaderType::Replycaps => {
            let caps = wrapped_stream
                .decode(capabilities::CapabilitiesUnpacker)
                .collect()
                .and_then(|caps| {
                    ensure!(caps.len() == 1, "Unexpected Replycaps payload: {:?}", caps);
                    Ok(caps.into_iter().next().unwrap())
                });
            Bundle2Item::Replycaps(header, caps.boxify())
        }
        &PartHeaderType::B2xRebasePack => {
            let wirepack_stream = wrapped_stream.decode(wirepack::unpacker::new(
                logger.new(o!("stream" => "wirepack")),
                // Mercurial only knows how to send trees at the moment.
                // TODO: add support for file wirepacks once that's a thing
                wirepack::Kind::Tree,
            ));
            Bundle2Item::B2xRebasePack(header, wirepack_stream.boxify())
        }
        &PartHeaderType::B2xRebase => {
            let cg2_stream = wrapped_stream.decode(get_cg_unpacker(
                logger.new(o!("stream" => "bx2rebase")),
                header.clone(),
                "cgversion",
            ));
            Bundle2Item::B2xRebase(header, cg2_stream.boxify())
        }
        &PartHeaderType::Pushkey => {
            // Pushkey part has an empty part payload, but we still need to "parse" it
            // Otherwise polling remainder stream may fail.
            let empty = wrapped_stream.decode(EmptyUnpacker).for_each(|_| Ok(()));
            Bundle2Item::Pushkey(header, Box::new(empty))
        }
        &PartHeaderType::Pushvars => {
            // Pushvars part has an empty part payload, but we still need to "parse" it
            // Otherwise polling remainder stream may fail.
            let empty = wrapped_stream.decode(EmptyUnpacker).for_each(|_| Ok(()));
            Bundle2Item::Pushvars(header, Box::new(empty))
        }
        _ => panic!("TODO: make this an error"),
    };

    (
        bundle2item,
        remainder
            .map(|s| s.into_inner().into_inner())
            .from_err()
            .boxify(),
    )
}

// Decoder for an empty part (for example, pushkey)
pub struct EmptyUnpacker;

impl Decoder for EmptyUnpacker {
    type Item = ();
    type Error = Error;

    fn decode(&mut self, _buf: &mut BytesMut) -> Result<Option<Self::Item>> {
        Ok(None)
    }
}

#[cfg(test)]
mod test {

    use crate::changegroup::unpacker::CgVersion;
    use crate::part_header::{PartHeaderBuilder, PartHeaderType};
    use crate::part_inner::*;

    #[test]
    fn test_cg_unpacker_v3() {
        let mut header_builder =
            PartHeaderBuilder::new(PartHeaderType::Changegroup, false).unwrap();
        header_builder.add_aparam("version", "03").unwrap();
        let header = header_builder.build(1);

        assert_eq!(
            get_cg_version(header, "version").unwrap(),
            CgVersion::Cg3Version
        );
    }

    #[test]
    fn test_cg_unpacker_v2() {
        let mut header_builder =
            PartHeaderBuilder::new(PartHeaderType::Changegroup, false).unwrap();
        header_builder.add_aparam("version", "02").unwrap();
        let header = header_builder.build(1);

        assert_eq!(
            get_cg_version(header, "version").unwrap(),
            CgVersion::Cg2Version
        );
    }

    #[test]
    fn test_cg_unpacker_default_v2() {
        let header_builder = PartHeaderBuilder::new(PartHeaderType::Changegroup, false).unwrap();
        let h = header_builder.build(1);

        assert_eq!(get_cg_version(h, "version").is_err(), true);
    }

}
