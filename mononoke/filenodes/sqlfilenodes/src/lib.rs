/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

#![deny(warnings)]
#![feature(wait_until)]

mod errors;

use anyhow::{Error, Result};
use cloned::cloned;
use context::{CoreContext, PerfCounterType};
use failure_ext::chain::ChainExt;
use fbinit::FacebookInit;
use futures::{future::join_all, Future, IntoFuture, Stream};
use futures_ext::{BoxFuture, BoxStream, FutureExt};
use sql::{rusqlite::Connection as SqliteConnection, Connection};
use sql_facebook::{myrouter, raw};
use stats::prelude::*;

use filenodes::{FilenodeInfo, Filenodes};
use mercurial_types::{HgChangesetId, HgFileNodeId, RepoPath};
use mononoke_types::{hash, RepositoryId};
use sql::queries;
pub use sql_ext::SqlConstructors;
use sql_ext::{
    create_myrouter_connections, create_raw_xdb_connections, MysqlOptions, PoolSizeConfig,
    SqlConnections,
};

use crate::errors::ErrorKind;

use std::collections::HashSet;
use std::sync::Arc;

const MYSQL_INSERT_CHUNK_SIZE: usize = 1000;
const SQLITE_INSERT_CHUNK_SIZE: usize = 100;

pub struct SqlFilenodes {
    chunk_size: usize,
    write_connection: Arc<Vec<Connection>>,
    read_connection: Arc<Vec<Connection>>,
    read_master_connection: Arc<Vec<Connection>>,
}

define_stats! {
    prefix = "mononoke.filenodes";
    gets: timeseries(Rate, Sum),
    gets_master: timeseries(Rate, Sum),
    range_gets: timeseries(Rate, Sum),
    adds: timeseries(Rate, Sum),
}

queries! {
    write InsertPaths(values: (repo_id: RepositoryId, path: Vec<u8>, path_hash: Vec<u8>)) {
        insert_or_ignore,
        "{insert_or_ignore} INTO paths (repo_id, path, path_hash) VALUES {values}"
    }

    write InsertFilenodes(values: (
        repo_id: RepositoryId,
        path_hash: Vec<u8>,
        is_tree: i8,
        filenode: HgFileNodeId,
        linknode: HgChangesetId,
        p1: Option<HgFileNodeId>,
        p2: Option<HgFileNodeId>,
        has_copyinfo: i8,
    )) {
        insert_or_ignore,
        "{insert_or_ignore} INTO filenodes (
            repo_id
            , path_hash
            , is_tree
            , filenode
            , linknode
            , p1
            , p2
            , has_copyinfo
        ) VALUES {values}"
    }

    write ReplaceFilenodes(values: (
        repo_id: RepositoryId,
        path_hash: Vec<u8>,
        is_tree: i8,
        filenode: HgFileNodeId,
        linknode: HgChangesetId,
        p1: Option<HgFileNodeId>,
        p2: Option<HgFileNodeId>,
        has_copyinfo: i8,
    )) {
        none,
        "REPLACE INTO filenodes (
            repo_id
            , path_hash
            , is_tree
            , filenode
            , linknode
            , p1
            , p2
            , has_copyinfo
        ) VALUES {values}"
    }

    write InsertFixedcopyinfo(values: (
        repo_id: RepositoryId,
        topath_hash: Vec<u8>,
        tonode: HgFileNodeId,
        is_tree: i8,
        frompath_hash: Vec<u8>,
        fromnode: HgFileNodeId,
    )) {
        insert_or_ignore,
        "{insert_or_ignore} INTO fixedcopyinfo (
            repo_id
            , topath_hash
            , tonode
            , is_tree
            , frompath_hash
            , fromnode
        ) VALUES {values}"
    }

    read SelectFilenode(
        repo_id: RepositoryId,
        path_hash: Vec<u8>,
        is_tree: i8,
        filenode: HgFileNodeId
    ) -> (HgChangesetId, Option<HgFileNodeId>, Option<HgFileNodeId>, i8) {
        "SELECT linknode, p1, p2, has_copyinfo
         FROM filenodes
         WHERE repo_id = {repo_id}
           AND path_hash = {path_hash}
           AND is_tree = {is_tree}
           AND filenode = {filenode}
         LIMIT 1"
    }

    read SelectAllFilenodes(
        repo_id: RepositoryId,
        path_hash: Vec<u8>,
        is_tree: i8
    ) -> (HgFileNodeId, HgChangesetId, Option<HgFileNodeId>, Option<HgFileNodeId>, i8) {
        "SELECT filenode, linknode, p1, p2, has_copyinfo
         FROM filenodes
         WHERE repo_id = {repo_id}
           AND path_hash = {path_hash}
           AND is_tree = {is_tree}"
    }

    read SelectCopyinfo(
        repo_id: RepositoryId,
        topath_hash: Vec<u8>,
        tonode: HgFileNodeId,
        is_tree: i8,
    ) -> (Vec<u8>, HgFileNodeId) {
        "SELECT frompath_hash, fromnode
         FROM fixedcopyinfo
         WHERE fixedcopyinfo.repo_id = {repo_id}
           AND fixedcopyinfo.topath_hash = {topath_hash}
           AND fixedcopyinfo.tonode = {tonode}
           AND fixedcopyinfo.is_tree = {is_tree}
         LIMIT 1"
    }

    read SelectPath(
        repo_id: RepositoryId,
        path_hash: Vec<u8>,
    ) -> (Vec<u8>) {
        "SELECT path
         FROM paths
         WHERE paths.repo_id = {repo_id}
           AND paths.path_hash = {path_hash}
         LIMIT 1"
    }

    read SelectAllPaths(repo_id: RepositoryId, >list path_hashes: Vec<u8>) -> (Vec<u8>) {
        "SELECT path
         FROM paths
         WHERE paths.repo_id = {repo_id}
           AND paths.path_hash in {path_hashes}"
    }
}

impl SqlConstructors for SqlFilenodes {
    const LABEL: &'static str = "filenodes";

    fn from_connections(
        write_connection: Connection,
        read_connection: Connection,
        read_master_connection: Connection,
    ) -> Self {
        let chunk_size = match read_connection {
            Connection::Sqlite(_) => SQLITE_INSERT_CHUNK_SIZE,
            Connection::Mysql(_) => MYSQL_INSERT_CHUNK_SIZE,
        };

        Self {
            chunk_size,
            write_connection: Arc::new(vec![write_connection]),
            read_connection: Arc::new(vec![read_connection]),
            read_master_connection: Arc::new(vec![read_master_connection]),
        }
    }

    fn get_up_query() -> &'static str {
        include_str!("../schemas/sqlite-filenodes.sql")
    }
}

impl SqlFilenodes {
    pub fn with_sharded_xdb(
        fb: FacebookInit,
        tier: String,
        options: MysqlOptions,
        shard_count: usize,
        readonly: bool,
    ) -> BoxFuture<Self, Error> {
        match options.myrouter_port {
            Some(myrouter_port) => Self::with_sharded_myrouter(
                tier,
                myrouter_port,
                options.myrouter_read_service_type(),
                shard_count,
                readonly,
            ),
            None => Self::with_sharded_raw_xdb(
                fb,
                tier,
                options.db_locator_read_instance_requirement(),
                shard_count,
                readonly,
            ),
        }
    }

    fn with_sharded_myrouter(
        tier: String,
        port: u16,
        read_service_type: myrouter::ServiceType,
        shard_count: usize,
        readonly: bool,
    ) -> BoxFuture<Self, Error> {
        Self::with_sharded_factory(
            shard_count,
            move |shard_id| {
                Ok(create_myrouter_connections(
                    tier.clone(),
                    Some(shard_id),
                    port,
                    read_service_type,
                    PoolSizeConfig::for_sharded_connection(),
                    "shardedfilenodes".into(),
                    readonly,
                ))
                .into_future()
                .boxify()
            },
            MYSQL_INSERT_CHUNK_SIZE,
        )
    }

    pub fn with_sharded_raw_xdb(
        fb: FacebookInit,
        tier: String,
        read_instance_requirement: raw::InstanceRequirement,
        shard_count: usize,
        readonly: bool,
    ) -> BoxFuture<Self, Error> {
        Self::with_sharded_factory(
            shard_count,
            move |shard_id| {
                create_raw_xdb_connections(
                    fb,
                    format!("{}.{}", tier, shard_id),
                    read_instance_requirement,
                    readonly,
                )
                .boxify()
            },
            MYSQL_INSERT_CHUNK_SIZE,
        )
    }

    fn with_sharded_factory(
        shard_count: usize,
        factory: impl Fn(usize) -> BoxFuture<SqlConnections, Error>,
        chunk_size: usize,
    ) -> BoxFuture<Self, Error> {
        let futs: Vec<_> = (1..=shard_count)
            .into_iter()
            .map(|shard| factory(shard))
            .collect();

        join_all(futs)
            .map(move |shard_connections| {
                let mut write_connections = vec![];
                let mut read_connections = vec![];
                let mut read_master_connections = vec![];

                for conn in shard_connections {
                    let SqlConnections {
                        write_connection,
                        read_connection,
                        read_master_connection,
                    } = conn;

                    write_connections.push(write_connection);
                    read_connections.push(read_connection);
                    read_master_connections.push(read_master_connection);
                }

                Self {
                    chunk_size,
                    write_connection: Arc::new(write_connections),
                    read_connection: Arc::new(read_connections),
                    read_master_connection: Arc::new(read_master_connections),
                }
            })
            .boxify()
    }

    pub fn with_sharded_sqlite(shard_count: usize) -> Result<Self> {
        let mut read_connection = vec![];
        let mut read_master_connection = vec![];
        let mut write_connection = vec![];

        for _ in 0..shard_count {
            let con = SqliteConnection::open_in_memory()?;
            con.execute_batch(Self::get_up_query())?;
            let con = Connection::with_sqlite(con);

            read_connection.push(con.clone());
            read_master_connection.push(con.clone());
            write_connection.push(con);
        }

        Ok(Self {
            chunk_size: SQLITE_INSERT_CHUNK_SIZE,
            write_connection: Arc::new(write_connection),
            read_connection: Arc::new(read_connection),
            read_master_connection: Arc::new(read_master_connection),
        })
    }

    fn do_insert(
        &self,
        filenodes: BoxStream<FilenodeInfo, Error>,
        repo_id: RepositoryId,
        replace: bool,
    ) -> BoxFuture<(), Error> {
        cloned!(self.write_connection);
        cloned!(self.read_connection);

        filenodes
            .chunks(self.chunk_size)
            .and_then(move |filenodes| {
                STATS::adds.add_value(filenodes.len() as i64);

                let filenodes: Vec<_> = filenodes
                    .into_iter()
                    .map(|filenode| {
                        let pwh = PathWithHash::from_repo_path(&filenode.path);
                        (filenode, pwh)
                    })
                    .collect();

                ensure_paths_exists(
                    &read_connection,
                    write_connection.clone(),
                    repo_id,
                    filenodes.clone(),
                )
                .and_then({
                    cloned!(write_connection);
                    move |()| insert_filenodes(&write_connection, repo_id, &filenodes, replace)
                })
            })
            .for_each(|()| Ok(()))
            .boxify()
    }
}

impl Filenodes for SqlFilenodes {
    fn add_filenodes(
        &self,
        ctx: CoreContext,
        filenodes: BoxStream<FilenodeInfo, Error>,
        repo_id: RepositoryId,
    ) -> BoxFuture<(), Error> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        self.do_insert(filenodes, repo_id, false)
    }

    fn add_or_replace_filenodes(
        &self,
        ctx: CoreContext,
        filenodes: BoxStream<FilenodeInfo, Error>,
        repo_id: RepositoryId,
    ) -> BoxFuture<(), Error> {
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlWrites);
        self.do_insert(filenodes, repo_id, true)
    }

    fn get_filenode(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        filenode: HgFileNodeId,
        repo_id: RepositoryId,
    ) -> BoxFuture<Option<FilenodeInfo>, Error> {
        STATS::gets.add_value(1);
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        cloned!(self.read_master_connection, path, filenode, repo_id);
        let pwh = PathWithHash::from_repo_path(&path);

        select_filenode(self.read_connection.clone(), &path, filenode, &pwh, repo_id)
            .and_then(move |maybe_filenode_info| match maybe_filenode_info {
                Some(filenode_info) => Ok(Some(filenode_info)).into_future().boxify(),
                None => {
                    STATS::gets_master.add_value(1);
                    ctx.perf_counters()
                        .increment_counter(PerfCounterType::SqlReadsMaster);
                    select_filenode(
                        read_master_connection.clone(),
                        &path,
                        filenode,
                        &pwh,
                        repo_id,
                    )
                }
            })
            .boxify()
    }

    fn get_all_filenodes_maybe_stale(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        repo_id: RepositoryId,
    ) -> BoxFuture<Vec<FilenodeInfo>, Error> {
        STATS::range_gets.add_value(1);
        ctx.perf_counters()
            .increment_counter(PerfCounterType::SqlReadsReplica);
        cloned!(self.read_connection, path, repo_id);
        let pwh = PathWithHash::from_repo_path(&path);

        SelectAllFilenodes::query(
            &read_connection[pwh.shard_number(read_connection.len())],
            &repo_id,
            &pwh.hash,
            &pwh.is_tree,
        )
        .chain_err(ErrorKind::FailRangeFetch(path.clone()))
        .from_err()
        .and_then(move |filenode_rows| {
            let mut futs = vec![];
            for (filenode, linknode, p1, p2, has_copyinfo) in filenode_rows {
                futs.push(convert_to_filenode_info(
                    read_connection.clone(),
                    path.clone(),
                    filenode,
                    &pwh,
                    repo_id,
                    linknode,
                    p1,
                    p2,
                    has_copyinfo,
                ))
            }

            join_all(futs)
        })
        .boxify()
    }
}

fn ensure_paths_exists(
    read_connections: &Vec<Connection>,
    write_connections: Arc<Vec<Connection>>,
    repo_id: RepositoryId,
    filenodes: Vec<(FilenodeInfo, PathWithHash)>,
) -> impl Future<Item = (), Error = Error> {
    let mut path_rows: Vec<Vec<_>> = read_connections.iter().map(|_| Vec::new()).collect();
    for &(_, ref pwh) in filenodes.iter() {
        path_rows[pwh.shard_number(read_connections.len())].push(pwh.hash.clone());
    }

    let read_futures: Vec<_> = read_connections
        .iter()
        .enumerate()
        .filter_map(|(shard, connection)| {
            if path_rows[shard].len() != 0 {
                Some(SelectAllPaths::query(
                    &connection.clone(),
                    &repo_id,
                    &path_rows[shard],
                ))
            } else {
                None
            }
        })
        .collect();

    join_all(read_futures)
        .map(|fetched_paths| {
            let mut v: HashSet<Vec<_>> = HashSet::new();
            for paths in fetched_paths {
                v.extend(paths.into_iter().map(|p| p.0));
            }
            v
        })
        .and_then(move |mut existing_paths| {
            let mut path_rows: Vec<Vec<_>> = write_connections.iter().map(|_| Vec::new()).collect();
            for &(_, ref pwh) in filenodes.iter() {
                if existing_paths.insert(pwh.path_bytes.clone()) {
                    path_rows[pwh.shard_number(write_connections.len())].push((
                        &repo_id,
                        &pwh.path_bytes,
                        &pwh.hash,
                    ));
                }
            }

            // If you have two concurrent INSERT OR IGNORE queries happening with the same rows,
            // but in different order, they will deadlock. Sorting the rows in each of our INSERT
            // OR IGNORE queries solves that. So we do it here.
            for shard in path_rows.iter_mut() {
                shard.sort();
            }

            let futures: Vec<_> = write_connections
                .iter()
                .enumerate()
                .filter_map(|(shard, connection)| {
                    if path_rows[shard].len() != 0 {
                        Some(InsertPaths::query(&connection.clone(), &path_rows[shard]))
                    } else {
                        None
                    }
                })
                .collect();
            join_all(futures).map(|_| ())
        })
}

fn insert_filenodes(
    connections: &Vec<Connection>,
    repo_id: RepositoryId,
    filenodes: &Vec<(FilenodeInfo, PathWithHash)>,
    replace: bool,
) -> impl Future<Item = (), Error = Error> {
    let mut filenode_rows: Vec<Vec<_>> = connections.iter().map(|_| Vec::new()).collect();
    let mut copydata_rows: Vec<Vec<_>> = connections.iter().map(|_| Vec::new()).collect();
    for &(ref filenode, ref pwh) in filenodes {
        filenode_rows[pwh.shard_number(connections.len())].push((
            &repo_id,
            &pwh.hash,
            &pwh.is_tree,
            &filenode.filenode,
            &filenode.linknode,
            &filenode.p1,
            &filenode.p2,
            if filenode.copyfrom.is_some() {
                &1i8
            } else {
                &0i8
            },
        ));

        if let Some(ref copyinfo) = filenode.copyfrom {
            let (ref frompath, ref fromnode) = copyinfo;
            let from_pwh = PathWithHash::from_repo_path(frompath);
            if from_pwh.is_tree != pwh.is_tree {
                return Err(ErrorKind::InvalidCopy(filenode.path.clone(), frompath.clone()).into())
                    .into_future()
                    .left_future();
            }
            copydata_rows[pwh.shard_number(connections.len())].push((
                &repo_id,
                &pwh.hash,
                &filenode.filenode,
                &pwh.is_tree,
                from_pwh.hash,
                fromnode,
            ));
        }
    }

    let copydata_rows: Vec<Vec<_>> = copydata_rows
        .iter()
        .map(|shard| {
            shard
                .iter()
                .map(
                    |&(repo_id, tohash, tonode, is_tree, ref fromhash, fromnode)| {
                        (repo_id, tohash, tonode, is_tree, fromhash, fromnode)
                    },
                )
                .collect()
        })
        .collect();

    let copyinfo_futures: Vec<_> = connections
        .iter()
        .enumerate()
        .filter_map(|(shard, connection)| {
            if copydata_rows[shard].len() != 0 {
                Some(InsertFixedcopyinfo::query(
                    &connection.clone(),
                    &copydata_rows[shard],
                ))
            } else {
                None
            }
        })
        .collect();
    let filenode_futures: Vec<_> = connections
        .iter()
        .enumerate()
        .filter_map(|(shard, connection)| {
            if filenode_rows[shard].len() != 0 {
                Some(if replace {
                    ReplaceFilenodes::query(&connection.clone(), &filenode_rows[shard])
                        .left_future()
                } else {
                    InsertFilenodes::query(&connection.clone(), &filenode_rows[shard])
                        .right_future()
                })
            } else {
                None
            }
        })
        .collect();

    join_all(filenode_futures)
        .join(join_all(copyinfo_futures))
        .map(|_| ())
        .right_future()
}

fn select_filenode(
    connections: Arc<Vec<Connection>>,
    path: &RepoPath,
    filenode: HgFileNodeId,
    pwh: &PathWithHash,
    repo_id: RepositoryId,
) -> BoxFuture<Option<FilenodeInfo>, Error> {
    let connection = &connections[pwh.shard_number(connections.len())];
    cloned!(connections, path, filenode, pwh, repo_id);

    SelectFilenode::query(connection, &repo_id, &pwh.hash, &pwh.is_tree, &filenode)
        .chain_err(ErrorKind::FailFetchFilenode(filenode.clone(), path.clone()))
        .from_err()
        .and_then({
            move |rows| match rows.into_iter().next() {
                Some((linknode, p1, p2, has_copyinfo)) => convert_to_filenode_info(
                    connections,
                    path,
                    filenode,
                    &pwh,
                    repo_id,
                    linknode,
                    p1,
                    p2,
                    has_copyinfo,
                )
                .map(Some)
                .boxify(),
                None => Ok(None).into_future().boxify(),
            }
        })
        .boxify()
}

fn select_copydata(
    connections: Arc<Vec<Connection>>,
    path: &RepoPath,
    filenode: HgFileNodeId,
    pwh: &PathWithHash,
    repo_id: RepositoryId,
) -> BoxFuture<(RepoPath, HgFileNodeId), Error> {
    let shard_number = connections.len();
    let cloned_connections = connections.clone();
    let connection = &connections[pwh.shard_number(shard_number)];
    SelectCopyinfo::query(connection, &repo_id, &pwh.hash, &filenode, &pwh.is_tree)
        .and_then({
            cloned!(path, filenode);
            move |maybe_copyinfo_row| {
                maybe_copyinfo_row
                    .into_iter()
                    .next()
                    .ok_or(ErrorKind::CopydataNotFound(filenode, path).into())
            }
        })
        .and_then({
            cloned!(path, repo_id);
            move |(frompathhash, fromnode)| {
                let shard_num = PathWithHash::shard_number_by_hash(&frompathhash, shard_number);
                let another_shard_connection = &cloned_connections[shard_num];
                SelectPath::query(another_shard_connection, &repo_id, &frompathhash).and_then(
                    move |maybe_path| {
                        maybe_path
                            .into_iter()
                            .next()
                            .ok_or(ErrorKind::FromPathNotFound(path.clone()).into())
                            .map(|path| (path.0, fromnode))
                    },
                )
            }
        })
        .and_then({
            cloned!(pwh.is_tree);
            move |(path, fromnode)| Ok((convert_to_repo_path(&path, is_tree)?, fromnode))
        })
        .chain_err(ErrorKind::FailFetchCopydata(filenode.clone(), path.clone()))
        .from_err()
        .boxify()
}

fn convert_to_filenode_info(
    connections: Arc<Vec<Connection>>,
    path: RepoPath,
    filenode: HgFileNodeId,
    pwh: &PathWithHash,
    repo_id: RepositoryId,
    linknode: HgChangesetId,
    p1: Option<HgFileNodeId>,
    p2: Option<HgFileNodeId>,
    has_copyinfo: i8,
) -> impl Future<Item = FilenodeInfo, Error = Error> {
    let copydata = if has_copyinfo != 0 {
        select_copydata(connections, &path, filenode, &pwh, repo_id)
            .map(Some)
            .boxify()
    } else {
        Ok(None).into_future().boxify()
    };

    copydata.map(move |copydata| FilenodeInfo {
        path,
        filenode,
        p1,
        p2,
        copyfrom: copydata,
        linknode,
    })
}

fn convert_from_repo_path(path: &RepoPath) -> (Vec<u8>, i8) {
    match path {
        &RepoPath::RootPath => (vec![], 1),
        &RepoPath::DirectoryPath(ref dir) => (dir.to_vec(), 1),
        &RepoPath::FilePath(ref file) => (file.to_vec(), 0),
    }
}

fn convert_to_repo_path<B: AsRef<[u8]>>(path_bytes: B, is_tree: i8) -> Result<RepoPath> {
    if is_tree != 0 {
        RepoPath::dir(path_bytes.as_ref())
    } else {
        RepoPath::file(path_bytes.as_ref())
    }
}

#[derive(Clone)]
struct PathWithHash {
    path_bytes: Vec<u8>,
    is_tree: i8,
    hash: Vec<u8>,
}

impl PathWithHash {
    fn from_repo_path(path: &RepoPath) -> Self {
        let (path_bytes, is_tree) = convert_from_repo_path(path);

        let hash = {
            let mut hash_content = hash::Context::new("path".as_bytes());
            hash_content.update(&path_bytes);
            Vec::from(hash_content.finish().as_ref())
        };

        Self {
            path_bytes,
            is_tree,
            hash,
        }
    }

    fn shard_number(&self, shard_count: usize) -> usize {
        Self::shard_number_by_hash(&self.hash, shard_count)
    }

    fn shard_number_by_hash(hash: &Vec<u8>, shard_count: usize) -> usize {
        // We don't need crypto strength here - we're just turning a potentially large hash into
        // a shard number.
        let raw_shard_number = hash
            .iter()
            .fold(0usize, |hash, byte| hash.rotate_left(8) ^ (*byte as usize));

        raw_shard_number % shard_count
    }
}
