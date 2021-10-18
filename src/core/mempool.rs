// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::cmp;
use std::collections::HashSet;
use std::fs;
use std::hash::Hasher;
use std::io::{Read, Write};
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};

use rusqlite::types::ToSql;
use rusqlite::Connection;
use rusqlite::Error as SqliteError;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::Transaction;
use rusqlite::NO_PARAMS;

use siphasher::sip::SipHasher; // this is SipHash-2-4

use burnchains::Txid;
use chainstate::burn::ConsensusHash;
use chainstate::stacks::TransactionPayload;
use chainstate::stacks::{
    db::blocks::MemPoolRejection, db::ClarityTx, db::StacksChainState, db::TxStreamData,
    index::Error as MarfError, Error as ChainstateError, StacksTransaction,
};
use core::FIRST_BURNCHAIN_CONSENSUS_HASH;
use core::FIRST_STACKS_BLOCK_HASH;
use monitoring::increment_stx_mempool_gc;
use util::db::query_int;
use util::db::query_row_columns;
use util::db::query_rows;
use util::db::tx_begin_immediate;
use util::db::tx_busy_handler;
use util::db::u64_to_sql;
use util::db::Error as db_error;
use util::db::FromColumn;
use util::db::{query_row, Error};
use util::db::{sql_pragma, DBConn, DBTx, FromRow};
use util::get_epoch_time_ms;
use util::get_epoch_time_secs;
use util::hash::to_hex;
use util::hash::Sha512Trunc256Sum;
use vm::types::PrincipalData;

use net::MemPoolSyncData;

use util::bloom::{BloomCounter, BloomFilter, BloomNodeHasher};

use clarity_vm::clarity::ClarityConnection;

use crate::codec::Error as codec_error;
use crate::codec::StacksMessageCodec;
use crate::monitoring;
use crate::types::chainstate::{BlockHeaderHash, StacksAddress, StacksBlockHeader};

// maximum number of confirmations a transaction can have before it's garbage-collected
pub const MEMPOOL_MAX_TRANSACTION_AGE: u64 = 256;
pub const MAXIMUM_MEMPOOL_TX_CHAINING: u64 = 25;

// name of table for storing the counting bloom filter
pub const BLOOM_COUNTER_TABLE: &'static str = "txid_bloom_counter";

// bloom filter error rate
pub const BLOOM_COUNTER_ERROR_RATE: f64 = 0.001;

// expected number of txs in the bloom filter
pub const MAX_BLOOM_COUNTER_TXS: u32 = 8192;

// how far back in time (in Stacks blocks) does the bloom counter maintain tx records?
pub const BLOOM_COUNTER_DEPTH: usize = 2;

// maximum many tx tags we'll send before sending a bloom filter instead.
// The parameter choice here is due to performance -- calculating a tag set can be slower than just
// loading the bloom filter, even though the bloom filter is larger.
const DEFAULT_MAX_TX_TAGS: u32 = 2048;

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct TxTag(pub [u8; 8]);

impl TxTag {
    pub fn from_seed_and_txid(seed: &[u8], txid: &Txid) -> TxTag {
        let mut hasher = SipHasher::new();
        hasher.write(seed);
        hasher.write(&txid.0);

        let result_64 = hasher.finish();
        TxTag(result_64.to_be_bytes())
    }
}

impl std::fmt::Display for TxTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{}", &to_hex(&self.0))
    }
}

impl StacksMessageCodec for TxTag {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), codec_error> {
        fd.write_all(&self.0).map_err(codec_error::WriteError)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<TxTag, codec_error> {
        let mut bytes = [0u8; 8];
        fd.read_exact(&mut bytes).map_err(codec_error::ReadError)?;
        Ok(TxTag(bytes))
    }
}

pub struct MemPoolAdmitter {
    cur_block: BlockHeaderHash,
    cur_consensus_hash: ConsensusHash,
}

enum MemPoolWalkResult {
    Chainstate(ConsensusHash, BlockHeaderHash, u64, u64),
    NoneAtHeight(ConsensusHash, BlockHeaderHash, u64),
    Done,
}

impl MemPoolAdmitter {
    pub fn new(cur_block: BlockHeaderHash, cur_consensus_hash: ConsensusHash) -> MemPoolAdmitter {
        MemPoolAdmitter {
            cur_block,
            cur_consensus_hash,
        }
    }

    pub fn set_block(&mut self, cur_block: &BlockHeaderHash, cur_consensus_hash: ConsensusHash) {
        self.cur_consensus_hash = cur_consensus_hash.clone();
        self.cur_block = cur_block.clone();
    }
    pub fn will_admit_tx(
        &mut self,
        chainstate: &mut StacksChainState,
        tx: &StacksTransaction,
        tx_size: u64,
    ) -> Result<(), MemPoolRejection> {
        chainstate.will_admit_mempool_tx(&self.cur_consensus_hash, &self.cur_block, tx, tx_size)
    }
}

pub enum MemPoolDropReason {
    REPLACE_ACROSS_FORK,
    REPLACE_BY_FEE,
    STALE_COLLECT,
    TOO_EXPENSIVE,
}

impl std::fmt::Display for MemPoolDropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemPoolDropReason::STALE_COLLECT => write!(f, "StaleGarbageCollect"),
            MemPoolDropReason::TOO_EXPENSIVE => write!(f, "TooExpensive"),
            MemPoolDropReason::REPLACE_ACROSS_FORK => write!(f, "ReplaceAcrossFork"),
            MemPoolDropReason::REPLACE_BY_FEE => write!(f, "ReplaceByFee"),
        }
    }
}

pub trait MemPoolEventDispatcher {
    fn mempool_txs_dropped(&self, txids: Vec<Txid>, reason: MemPoolDropReason);
}

#[derive(Debug, PartialEq, Clone)]
pub struct MemPoolTxInfo {
    pub tx: StacksTransaction,
    pub metadata: MemPoolTxMetadata,
}

#[derive(Debug, PartialEq, Clone)]
pub struct MemPoolTxMetadata {
    pub txid: Txid,
    pub len: u64,
    pub tx_fee: u64,
    pub consensus_hash: ConsensusHash,
    pub block_header_hash: BlockHeaderHash,
    pub block_height: u64,
    pub origin_address: StacksAddress,
    pub origin_nonce: u64,
    pub sponsor_address: StacksAddress,
    pub sponsor_nonce: u64,
    pub accept_time: u64,
}

#[derive(Debug, Clone)]
pub struct MemPoolWalkSettings {
    /// Minimum transaction fee that will be considered
    pub min_tx_fee: u64,
    /// Maximum amount of time a miner will spend walking through mempool transactions, in
    /// milliseconds.  This is a soft deadline.
    pub max_walk_time_ms: u64,
}

impl MemPoolWalkSettings {
    pub fn default() -> MemPoolWalkSettings {
        MemPoolWalkSettings {
            min_tx_fee: 1,
            max_walk_time_ms: u64::max_value(),
        }
    }
    pub fn zero() -> MemPoolWalkSettings {
        MemPoolWalkSettings {
            min_tx_fee: 0,
            max_walk_time_ms: u64::max_value(),
        }
    }
}

impl FromRow<Txid> for Txid {
    fn from_row<'a>(row: &'a Row) -> Result<Txid, db_error> {
        row.get(0).map_err(db_error::SqliteError)
    }
}

impl FromRow<MemPoolTxMetadata> for MemPoolTxMetadata {
    fn from_row<'a>(row: &'a Row) -> Result<MemPoolTxMetadata, db_error> {
        let txid = Txid::from_column(row, "txid")?;
        let consensus_hash = ConsensusHash::from_column(row, "consensus_hash")?;
        let block_header_hash = BlockHeaderHash::from_column(row, "block_header_hash")?;
        let tx_fee = u64::from_column(row, "tx_fee")?;
        let height = u64::from_column(row, "height")?;
        let len = u64::from_column(row, "length")?;
        let ts = u64::from_column(row, "accept_time")?;
        let origin_address = StacksAddress::from_column(row, "origin_address")?;
        let origin_nonce = u64::from_column(row, "origin_nonce")?;
        let sponsor_address = StacksAddress::from_column(row, "sponsor_address")?;
        let sponsor_nonce = u64::from_column(row, "sponsor_nonce")?;

        Ok(MemPoolTxMetadata {
            txid: txid,
            tx_fee: tx_fee,
            len: len,
            consensus_hash: consensus_hash,
            block_header_hash: block_header_hash,
            block_height: height,
            accept_time: ts,
            origin_address: origin_address,
            origin_nonce: origin_nonce,
            sponsor_address: sponsor_address,
            sponsor_nonce: sponsor_nonce,
        })
    }
}

impl FromRow<MemPoolTxInfo> for MemPoolTxInfo {
    fn from_row<'a>(row: &'a Row) -> Result<MemPoolTxInfo, db_error> {
        let md = MemPoolTxMetadata::from_row(row)?;
        let tx_bytes: Vec<u8> = row.get_unwrap("tx");
        let tx = StacksTransaction::consensus_deserialize(&mut &tx_bytes[..])
            .map_err(|_e| db_error::ParseError)?;

        if tx.txid() != md.txid {
            return Err(db_error::ParseError);
        }

        Ok(MemPoolTxInfo {
            tx: tx,
            metadata: md,
        })
    }
}

impl FromRow<(u64, u64)> for (u64, u64) {
    fn from_row<'a>(row: &'a Row) -> Result<(u64, u64), db_error> {
        let t1: i64 = row.get_unwrap(0);
        let t2: i64 = row.get_unwrap(1);
        if t1 < 0 || t2 < 0 {
            return Err(db_error::ParseError);
        }
        Ok((t1 as u64, t2 as u64))
    }
}

const MEMPOOL_INITIAL_SCHEMA: &'static [&'static str] = &[
    r#"
    CREATE TABLE mempool(
        txid TEXT NOT NULL,
        origin_address TEXT NOT NULL,
        origin_nonce INTEGER NOT NULL,
        sponsor_address TEXT NOT NULL,
        sponsor_nonce INTEGER NOT NULL,
        tx_fee INTEGER NOT NULL,
        length INTEGER NOT NULL,
        consensus_hash TEXT NOT NULL,
        block_header_hash TEXT NOT NULL,
        height INTEGER NOT NULL,    -- stacks block height
        accept_time INTEGER NOT NULL,
        tx BLOB NOT NULL,
        PRIMARY KEY (txid),
        UNIQUE (origin_address, origin_nonce),
        UNIQUE (sponsor_address,sponsor_nonce)
    );
    "#,
    "CREATE INDEX by_txid ON mempool(txid);",
    "CREATE INDEX by_txid_and_height ON mempool(txid,height);",
    "CREATE INDEX by_sponsor ON mempool(sponsor_address, sponsor_nonce);",
    "CREATE INDEX by_origin ON mempool(origin_address, origin_nonce);",
    "CREATE INDEX by_timestamp ON mempool(accept_time);",
    "CREATE INDEX by_chaintip ON mempool(consensus_hash,block_header_hash);",
];

const MEMPOOL_SCHEMA_BLOOM_STATE: &'static [&'static str] = &[
    r#"
    PRAGMA foreign_keys = 1;
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS removed_txids(
        txid TEXT PRIMARY KEY NOT NULL,
        FOREIGN KEY(txid) REFERENCES mempool(txid) ON DELETE CASCADE
    );
    "#,
    r#"
    -- mapping between hash(local-seed,txid) and txid, used for randomized but efficient
    -- paging when streaming transactions out of the mempool.
    CREATE TABLE IF NOT EXISTS randomized_txids(
        txid TEXT PRIMARY KEY NOT NULL,
        hashed_txid TEXT NOT NULL,
        FOREIGN KEY(txid) REFERENCES mempool(txid) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS by_hashed_txid ON randomized_txids(txid,hashed_txid);
    "#,
];

pub struct MemPoolDB {
    db: DBConn,
    path: String,
    admitter: MemPoolAdmitter,
    bloom_counter: BloomCounter<BloomNodeHasher>,
    max_tx_tags: u32,
}

pub struct MemPoolTx<'a> {
    tx: DBTx<'a>,
    admitter: &'a mut MemPoolAdmitter,
    bloom_counter: Option<&'a mut BloomCounter<BloomNodeHasher>>,
}

impl<'a> Deref for MemPoolTx<'a> {
    type Target = DBTx<'a>;
    fn deref(&self) -> &DBTx<'a> {
        &self.tx
    }
}

impl<'a> DerefMut for MemPoolTx<'a> {
    fn deref_mut(&mut self) -> &mut DBTx<'a> {
        &mut self.tx
    }
}

impl<'a> MemPoolTx<'a> {
    pub fn new(
        tx: DBTx<'a>,
        admitter: &'a mut MemPoolAdmitter,
        bloom_counter: &'a mut BloomCounter<BloomNodeHasher>,
    ) -> MemPoolTx<'a> {
        MemPoolTx {
            tx,
            admitter,
            bloom_counter: Some(bloom_counter),
        }
    }

    pub fn take_bloom_state(&mut self) -> &'a mut BloomCounter<BloomNodeHasher> {
        let bc = self
            .bloom_counter
            .take()
            .expect("BUG: did not replace bloom state");
        bc
    }

    pub fn replace_bloom_state(&mut self, bc: &'a mut BloomCounter<BloomNodeHasher>) {
        self.bloom_counter.replace(bc);
    }

    pub fn commit(self) -> Result<(), db_error> {
        self.tx.commit().map_err(db_error::SqliteError)
    }
}

impl MemPoolTxInfo {
    pub fn from_tx(
        tx: StacksTransaction,
        consensus_hash: ConsensusHash,
        block_header_hash: BlockHeaderHash,
        block_height: u64,
    ) -> MemPoolTxInfo {
        let txid = tx.txid();
        let mut tx_data = vec![];
        tx.consensus_serialize(&mut tx_data)
            .expect("BUG: failed to serialize to vector");

        let origin_address = tx.origin_address();
        let origin_nonce = tx.get_origin_nonce();
        let (sponsor_address, sponsor_nonce) =
            if let (Some(addr), Some(nonce)) = (tx.sponsor_address(), tx.get_sponsor_nonce()) {
                (addr, nonce)
            } else {
                (origin_address.clone(), origin_nonce)
            };

        let metadata = MemPoolTxMetadata {
            txid: txid,
            len: tx_data.len() as u64,
            tx_fee: tx.get_tx_fee(),
            consensus_hash: consensus_hash,
            block_header_hash: block_header_hash,
            block_height: block_height,
            origin_address: origin_address,
            origin_nonce: origin_nonce,
            sponsor_address: sponsor_address,
            sponsor_nonce: sponsor_nonce,
            accept_time: get_epoch_time_secs(),
        };
        MemPoolTxInfo {
            tx: tx,
            metadata: metadata,
        }
    }
}

impl MemPoolDB {
    fn instantiate_mempool_db(conn: &mut DBConn) -> Result<(), db_error> {
        sql_pragma(conn, "PRAGMA journal_mode = WAL;")?;

        let mut tx = tx_begin_immediate(conn)?;

        // create mempool tables
        for cmd in MEMPOOL_INITIAL_SCHEMA {
            tx.execute_batch(cmd).map_err(db_error::SqliteError)?;
        }

        MemPoolDB::instantiate_bloom_state(&mut tx)?;
        tx.commit().map_err(db_error::SqliteError)?;
        Ok(())
    }

    fn instantiate_bloom_state(tx: &mut DBTx) -> Result<(), db_error> {
        let node_hasher = BloomNodeHasher::new_random();
        let _ = BloomCounter::new(
            tx,
            BLOOM_COUNTER_TABLE,
            BLOOM_COUNTER_ERROR_RATE,
            MAX_BLOOM_COUNTER_TXS,
            node_hasher,
        )?;

        for cmd in MEMPOOL_SCHEMA_BLOOM_STATE {
            tx.execute_batch(cmd).map_err(db_error::SqliteError)?;
        }
        Ok(())
    }

    pub fn db_path(chainstate_root_path: &str) -> Result<String, db_error> {
        let mut path = PathBuf::from(chainstate_root_path);

        path.push("mempool.sqlite");
        path.to_str()
            .ok_or_else(|| db_error::ParseError)
            .map(String::from)
    }

    /// Open the mempool db within the chainstate directory.
    /// The chainstate must be instantiated already.
    pub fn open(
        mainnet: bool,
        chain_id: u32,
        chainstate_path: &str,
    ) -> Result<MemPoolDB, db_error> {
        match fs::metadata(chainstate_path) {
            Ok(md) => {
                if !md.is_dir() {
                    return Err(db_error::NotFoundError);
                }
            }
            Err(_e) => {
                return Err(db_error::NotFoundError);
            }
        }

        let (chainstate, _) = StacksChainState::open(mainnet, chain_id, chainstate_path)
            .map_err(|e| db_error::Other(format!("Failed to open chainstate: {:?}", &e)))?;

        let admitter = MemPoolAdmitter::new(BlockHeaderHash([0u8; 32]), ConsensusHash([0u8; 20]));

        let db_path = MemPoolDB::db_path(&chainstate.root_path)?;

        let mut create_flag = false;
        let open_flags = if fs::metadata(&db_path).is_err() {
            // need to create
            create_flag = true;
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            // can just open
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };

        let mut conn =
            DBConn::open_with_flags(&db_path, open_flags).map_err(db_error::SqliteError)?;
        conn.busy_handler(Some(tx_busy_handler))
            .map_err(db_error::SqliteError)?;

        if create_flag {
            // instantiate!
            MemPoolDB::instantiate_mempool_db(&mut conn)?;
        } else {
            // possibly migrating from a mempool without the bloom state
            if let Err(_) = BloomCounter::<BloomNodeHasher>::try_load(&conn, BLOOM_COUNTER_TABLE) {
                info!("Instantiating bloom counter");
                let mut tx = tx_begin_immediate(&mut conn)?;
                MemPoolDB::instantiate_bloom_state(&mut tx)?;
                tx.commit().map_err(db_error::SqliteError)?;
            }
        }

        let bloom_counter = BloomCounter::<BloomNodeHasher>::try_load(&conn, BLOOM_COUNTER_TABLE)?
            .ok_or(db_error::Other(format!("Failed to load bloom counter")))?;

        Ok(MemPoolDB {
            db: conn,
            path: db_path,
            admitter: admitter,
            bloom_counter,
            max_tx_tags: DEFAULT_MAX_TX_TAGS,
        })
    }

    /// Find the origin addresses who have sent the highest-fee transactions
    fn find_origin_addresses_by_descending_fees(
        &self,
        start_height: i64,
        end_height: i64,
        min_fees: u64,
        offset: u32,
        count: u32,
    ) -> Result<Vec<StacksAddress>, db_error> {
        let sql = "SELECT DISTINCT origin_address FROM mempool WHERE height > ?1 AND height <= ?2 AND tx_fee >= ?3 ORDER BY tx_fee DESC LIMIT ?4 OFFSET ?5";
        let args: &[&dyn ToSql] = &[
            &start_height,
            &end_height,
            &u64_to_sql(min_fees)?,
            &count,
            &offset,
        ];
        query_row_columns(self.conn(), sql, args, "origin_address")
    }

    ///
    /// Iterate over candidates in the mempool
    ///  `todo` will be called once for each transaction whose origin nonce is equal
    ///  to the origin account's nonce. At most one transaction per origin will be
    ///  considered by this method, and transactions will be considered in
    ///  highest-fee-first order.  This method is interruptable -- in the `settings` struct, the
    ///  caller may choose how long to spend iterating before this method stops.
    ///
    ///  Returns the number of transactions considered on success.
    pub fn iterate_candidates<F, E, C>(
        &self,
        clarity_tx: &mut C,
        tip_height: u64,
        settings: MemPoolWalkSettings,
        mut todo: F,
    ) -> Result<u64, E>
    where
        C: ClarityConnection,
        F: FnMut(&mut C, MemPoolTxInfo) -> Result<bool, E>,
        E: From<db_error> + From<ChainstateError>,
    {
        let min_height = (tip_height as i64)
            .checked_sub((MEMPOOL_MAX_TRANSACTION_AGE + 1) as i64)
            .unwrap_or(-1);
        let max_height = tip_height as i64;

        let page_size = 1000;
        let mut offset = 0;

        let min_tx_fee = settings.min_tx_fee;

        let deadline = get_epoch_time_ms() + (settings.max_walk_time_ms as u128);
        let mut total_considered = 0;
        let mut total_origins = 0;

        test_debug!(
            "Mempool walk for {}ms, min tx fee {}",
            settings.max_walk_time_ms,
            min_tx_fee,
        );

        loop {
            if deadline <= get_epoch_time_ms() {
                debug!("Mempool iteration deadline exceeded");
                break;
            }

            let origin_addresses = self.find_origin_addresses_by_descending_fees(
                min_height,
                max_height,
                min_tx_fee,
                offset * page_size,
                page_size,
            )?;
            debug!(
                "Consider {} origin addresses between {},{} with min_fee {}",
                origin_addresses.len(),
                min_height,
                max_height,
                min_tx_fee,
            );

            if origin_addresses.len() == 0 {
                debug!("No more origin addresses to consider");
                break;
            }

            for origin_address in origin_addresses.iter() {
                if deadline <= get_epoch_time_ms() {
                    debug!("Mempool iteration deadline exceeded");
                    break;
                }

                let min_origin_nonce = StacksChainState::get_account(
                    clarity_tx,
                    &PrincipalData::Standard(origin_address.to_owned().into()),
                )
                .nonce;

                total_origins += 1;

                debug!(
                    "Consider mempool transactions from origin address {} nonce {}",
                    &origin_address, min_origin_nonce
                );

                let sql = "SELECT * FROM mempool WHERE origin_address = ?1 AND height > ?2 AND height <= ?3 AND origin_nonce = ?4 AND tx_fee >= ?5 ORDER BY sponsor_nonce ASC LIMIT 1";
                let args: &[&dyn ToSql] = &[
                    &origin_address.to_string(),
                    &min_height,
                    &max_height,
                    &u64_to_sql(min_origin_nonce)?,
                    &u64_to_sql(min_tx_fee)?,
                ];

                let tx_opt = query_row::<MemPoolTxInfo, _>(self.conn(), sql, args)?;
                if let Some(tx) = tx_opt {
                    total_considered += 1;
                    debug!(
                        "Consider transaction {} from {} between heights {},{} with nonce = {} and tx_fee = {} and size = {}",
                        &tx.metadata.txid,
                        &origin_address,
                        min_height,
                        max_height,
                        min_origin_nonce,
                        tx.metadata.tx_fee,
                        tx.metadata.len
                    );

                    if !todo(clarity_tx, tx)? {
                        test_debug!("Mempool early return from iteration");
                        break;
                    }
                }
            }
            offset += 1;
        }
        debug!(
            "Mempool iteration finished; considered {} transactions across {} origin addresses",
            total_considered, total_origins
        );
        Ok(total_considered)
    }

    pub fn conn(&self) -> &DBConn {
        &self.db
    }

    pub fn tx_begin<'a>(&'a mut self) -> Result<MemPoolTx<'a>, db_error> {
        let tx = tx_begin_immediate(&mut self.db)?;
        Ok(MemPoolTx::new(
            tx,
            &mut self.admitter,
            &mut self.bloom_counter,
        ))
    }

    fn db_has_tx(conn: &DBConn, txid: &Txid) -> Result<bool, db_error> {
        query_row(
            conn,
            "SELECT 1 FROM mempool WHERE txid = ?1",
            &[txid as &dyn ToSql],
        )
        .and_then(|row_opt: Option<i64>| Ok(row_opt.is_some()))
    }

    pub fn get_tx(conn: &DBConn, txid: &Txid) -> Result<Option<MemPoolTxInfo>, db_error> {
        query_row(
            conn,
            "SELECT * FROM mempool WHERE txid = ?1",
            &[txid as &dyn ToSql],
        )
    }

    /// Get all transactions across all tips
    #[cfg(test)]
    pub fn get_all_txs(conn: &DBConn) -> Result<Vec<MemPoolTxInfo>, db_error> {
        let sql = "SELECT * FROM mempool";
        let rows = query_rows::<MemPoolTxInfo, _>(conn, &sql, NO_PARAMS)?;
        Ok(rows)
    }

    /// Get all transactions at a specific block
    #[cfg(test)]
    pub fn get_num_tx_at_block(
        conn: &DBConn,
        consensus_hash: &ConsensusHash,
        block_header_hash: &BlockHeaderHash,
    ) -> Result<usize, db_error> {
        let sql = "SELECT * FROM mempool WHERE consensus_hash = ?1 AND block_header_hash = ?2";
        let args: &[&dyn ToSql] = &[consensus_hash, block_header_hash];
        let rows = query_rows::<MemPoolTxInfo, _>(conn, &sql, args)?;
        Ok(rows.len())
    }

    /// Get all transactions at a particular timestamp on a given chain tip.
    /// Order them by origin nonce.
    pub fn get_txs_at(
        conn: &DBConn,
        consensus_hash: &ConsensusHash,
        block_header_hash: &BlockHeaderHash,
        timestamp: u64,
    ) -> Result<Vec<MemPoolTxInfo>, db_error> {
        let sql = "SELECT * FROM mempool WHERE accept_time = ?1 AND consensus_hash = ?2 AND block_header_hash = ?3 ORDER BY origin_nonce ASC";
        let args: &[&dyn ToSql] = &[&u64_to_sql(timestamp)?, consensus_hash, block_header_hash];
        let rows = query_rows::<MemPoolTxInfo, _>(conn, &sql, args)?;
        Ok(rows)
    }

    /// Given a chain tip, find the highest block-height from _before_ this tip
    pub fn get_previous_block_height(conn: &DBConn, height: u64) -> Result<Option<u64>, db_error> {
        let sql = "SELECT height FROM mempool WHERE height < ?1 ORDER BY height DESC LIMIT 1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(height)?];
        query_row(conn, sql, args)
    }

    /// Get a number of transactions after a given timestamp on a given chain tip.
    pub fn get_txs_after(
        conn: &DBConn,
        consensus_hash: &ConsensusHash,
        block_header_hash: &BlockHeaderHash,
        timestamp: u64,
        count: u64,
    ) -> Result<Vec<MemPoolTxInfo>, db_error> {
        let sql = "SELECT * FROM mempool WHERE accept_time >= ?1 AND consensus_hash = ?2 AND block_header_hash = ?3 ORDER BY tx_fee DESC LIMIT ?4";
        let args: &[&dyn ToSql] = &[
            &u64_to_sql(timestamp)?,
            consensus_hash,
            block_header_hash,
            &u64_to_sql(count)?,
        ];
        let rows = query_rows::<MemPoolTxInfo, _>(conn, &sql, args)?;
        Ok(rows)
    }

    /// Get a transaction's metadata, given address and nonce, and whether the address is used as a sponsor or an origin.
    /// Faster than getting the MemPoolTxInfo, since no deserialization will be needed.
    /// Used to see if there exists a transaction with this info, so as to implement replace-by-fee
    fn get_tx_metadata_by_address(
        conn: &DBConn,
        is_origin: bool,
        addr: &StacksAddress,
        nonce: u64,
    ) -> Result<Option<MemPoolTxMetadata>, db_error> {
        let sql = format!(
            "SELECT 
                          txid,
                          origin_address,
                          origin_nonce,
                          sponsor_address,
                          sponsor_nonce,
                          tx_fee,
                          length,
                          consensus_hash,
                          block_header_hash,
                          height,
                          accept_time
                          FROM mempool WHERE {0}_address = ?1 AND {0}_nonce = ?2",
            if is_origin { "origin" } else { "sponsor" }
        );
        let args: &[&dyn ToSql] = &[&addr.to_string(), &u64_to_sql(nonce)?];
        query_row(conn, &sql, args)
    }

    fn are_blocks_in_same_fork(
        chainstate: &mut StacksChainState,
        first_consensus_hash: &ConsensusHash,
        first_stacks_block: &BlockHeaderHash,
        second_consensus_hash: &ConsensusHash,
        second_stacks_block: &BlockHeaderHash,
    ) -> Result<bool, db_error> {
        let first_block =
            StacksBlockHeader::make_index_block_hash(first_consensus_hash, first_stacks_block);
        let second_block =
            StacksBlockHeader::make_index_block_hash(second_consensus_hash, second_stacks_block);
        // short circuit equality
        if second_block == first_block {
            return Ok(true);
        }

        let headers_conn = &chainstate
            .index_conn()
            .map_err(|_e| db_error::Other("ChainstateError".to_string()))?;
        let height_of_first_with_second_tip =
            headers_conn.get_ancestor_block_height(&second_block, &first_block)?;
        let height_of_second_with_first_tip =
            headers_conn.get_ancestor_block_height(&first_block, &second_block)?;

        match (
            height_of_first_with_second_tip,
            height_of_second_with_first_tip,
        ) {
            (None, None) => Ok(false),
            (_, _) => Ok(true),
        }
    }

    /// Remove all txids at the given height from the bloom counter.
    /// Used to clear out txids that are now outside the bloom counter's depth.
    fn prune_bloom_counter(tx: &mut MemPoolTx, target_height: u64) -> Result<(), MemPoolRejection> {
        let sql = "SELECT txid FROM mempool WHERE height = ?1 AND NOT EXISTS (SELECT 1 FROM removed_txids WHERE txid = mempool.txid)";
        let args: &[&dyn ToSql] = &[&u64_to_sql(target_height)?];
        let txids: Vec<Txid> = query_rows(tx, sql, args)?;
        let num_txs = txids.len();

        debug!("Prune bloom counter from height {}", target_height);

        // keep borrow-checker happy
        let bloom_counter = tx.take_bloom_state();
        for txid in txids.into_iter() {
            bloom_counter.remove_raw(&mut tx.tx, &txid.0)?;

            let sql = "INSERT OR REPLACE INTO removed_txids (txid) VALUES (?1)";
            let args: &[&dyn ToSql] = &[&txid];
            tx.execute(sql, args).map_err(db_error::SqliteError)?;
        }

        debug!(
            "Pruned bloom filter at height {}: removed {} txs",
            target_height, num_txs
        );
        tx.replace_bloom_state(bloom_counter);
        Ok(())
    }

    /// Add the txid to the bloom counter in the mempool DB.
    /// If this is the first txid at this block height, then also garbage-collect the bloom counter to remove no-longer-recent transactions.
    /// If the bloom counter is saturated -- i.e. it represents more than MAX_BLOOM_COUNTER_TXS
    /// transactions -- then pick another transaction to evict from the bloom filter and return its txid.
    /// (Note that no transactions are ever removed from the mempool; we just don't prioritize them
    /// in the bloom filter).
    fn update_bloom_counter(
        tx: &mut MemPoolTx,
        height: u64,
        txid: &Txid,
        prior_txid: Option<Txid>,
    ) -> Result<Option<Txid>, MemPoolRejection> {
        // is this the first-ever txid at this height?
        let sql = "SELECT 1 FROM mempool WHERE height = ?1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(height)?];
        let present: Option<i64> = query_row(tx, sql, args)?;
        if present.is_none() && height > (BLOOM_COUNTER_DEPTH as u64) {
            // this is the first-ever tx at this height.
            // which means, the bloom filter window has advanced.
            // which means, we need to remove all the txs that are now out of the window.
            MemPoolDB::prune_bloom_counter(tx, height - (BLOOM_COUNTER_DEPTH as u64))?;
        }

        // keep borrow-checker happy
        let bloom_counter = tx.take_bloom_state();

        // remove replaced transaction
        if let Some(prior_txid) = prior_txid {
            bloom_counter.remove_raw(&mut tx.tx, &prior_txid.0)?;
        }

        // keep the bloom counter un-saturated -- remove at most one transaction from it to keep
        // the error rate at or below the target error rate
        let evict_txid = {
            let num_recents = MemPoolDB::get_num_recent_txs(&tx.tx)?;
            if num_recents >= MAX_BLOOM_COUNTER_TXS.into() {
                // for now, remove lowest-fee tx in the recent tx set.
                // TODO: In the future, do it by lowest fee rate
                let sql = "SELECT txid FROM mempool WHERE height > ?1 AND NOT EXISTS (SELECT 1 FROM removed_txids WHERE txid = mempool.txid) ORDER BY tx_fee ASC LIMIT 1";
                let args: &[&dyn ToSql] = &[&u64_to_sql(
                    height.saturating_sub(BLOOM_COUNTER_DEPTH as u64),
                )?];
                let evict_txid: Option<Txid> = query_row(&tx.tx, sql, args)?;
                if let Some(evict_txid) = evict_txid {
                    bloom_counter.remove_raw(&mut tx.tx, &evict_txid.0)?;

                    let sql = "INSERT OR REPLACE INTO removed_txids (txid) VALUES (?1)";
                    let args: &[&dyn ToSql] = &[&evict_txid];
                    tx.execute(sql, args).map_err(db_error::SqliteError)?;

                    Some(evict_txid)
                } else {
                    None
                }
            } else {
                None
            }
        };

        // finally add the new transaction
        bloom_counter.insert_raw(&mut tx.tx, &txid.0)?;
        tx.replace_bloom_state(bloom_counter);
        Ok(evict_txid)
    }

    /// Add the txid to our randomized page order
    fn update_mempool_pager(tx: &mut MemPoolTx, txid: &Txid) -> Result<(), MemPoolRejection> {
        let mut randomized_buff = vec![];

        let bloom_counter = tx.take_bloom_state();
        randomized_buff.extend_from_slice(bloom_counter.get_seed());
        tx.replace_bloom_state(bloom_counter);

        randomized_buff.extend_from_slice(&txid.0);
        let hashed_txid = Txid(Sha512Trunc256Sum::from_data(&randomized_buff).0);

        let sql = "INSERT OR REPLACE INTO randomized_txids (txid,hashed_txid) VALUES (?1,?2)";
        let args: &[&dyn ToSql] = &[txid, &hashed_txid];

        tx.execute(sql, args).map_err(db_error::SqliteError)?;

        Ok(())
    }

    /// Add a transaction to the mempool.  If it already exists, then replace it if the given fee
    /// is higher than the one that's already there.
    /// Carry out the mempool admission test before adding.
    /// Don't call directly; use submit().
    /// This is `pub` only for testing.
    pub fn try_add_tx(
        tx: &mut MemPoolTx,
        chainstate: &mut StacksChainState,
        consensus_hash: &ConsensusHash,
        block_header_hash: &BlockHeaderHash,
        txid: Txid,
        tx_bytes: Vec<u8>,
        tx_fee: u64,
        height: u64,
        origin_address: &StacksAddress,
        origin_nonce: u64,
        sponsor_address: &StacksAddress,
        sponsor_nonce: u64,
        event_observer: Option<&dyn MemPoolEventDispatcher>,
    ) -> Result<(), MemPoolRejection> {
        let length = tx_bytes.len() as u64;

        // do we already have txs with either the same origin nonce or sponsor nonce ?
        let prior_tx = {
            match MemPoolDB::get_tx_metadata_by_address(tx, true, origin_address, origin_nonce)? {
                Some(prior_tx) => Some(prior_tx),
                None => MemPoolDB::get_tx_metadata_by_address(
                    tx,
                    false,
                    sponsor_address,
                    sponsor_nonce,
                )?,
            }
        };

        let mut replace_reason = MemPoolDropReason::REPLACE_BY_FEE;

        // if so, is this a replace-by-fee? or a replace-in-chain-tip?
        let add_tx = if let Some(ref prior_tx) = prior_tx {
            if tx_fee > prior_tx.tx_fee {
                // is this a replace-by-fee ?
                debug!(
                    "Can replace {} with {} for {},{} by fee ({} < {})",
                    &prior_tx.txid, &txid, origin_address, origin_nonce, &prior_tx.tx_fee, &tx_fee
                );
                replace_reason = MemPoolDropReason::REPLACE_BY_FEE;
                true
            } else if !MemPoolDB::are_blocks_in_same_fork(
                chainstate,
                &prior_tx.consensus_hash,
                &prior_tx.block_header_hash,
                consensus_hash,
                block_header_hash,
            )? {
                // is this a replace-across-fork ?
                debug!(
                    "Can replace {} with {} for {},{} across fork",
                    &prior_tx.txid, &txid, origin_address, origin_nonce
                );
                replace_reason = MemPoolDropReason::REPLACE_ACROSS_FORK;
                true
            } else {
                // there's a >= fee tx in this fork, cannot add
                info!("TX conflicts with sponsor/origin nonce in same fork with >= fee";
                      "new_txid" => %txid, 
                      "old_txid" => %prior_tx.txid,
                      "origin_addr" => %origin_address,
                      "origin_nonce" => origin_nonce,
                      "sponsor_addr" => %sponsor_address,
                      "sponsor_nonce" => sponsor_nonce,
                      "new_fee" => tx_fee,
                      "old_fee" => prior_tx.tx_fee);
                false
            }
        } else {
            // no conflicting TX with this origin/sponsor, go ahead and add
            true
        };

        if !add_tx {
            return Err(MemPoolRejection::ConflictingNonceInMempool);
        }

        MemPoolDB::update_bloom_counter(
            tx,
            height,
            &txid,
            prior_tx.as_ref().map(|tx| tx.txid.clone()),
        )?;

        let sql = "INSERT OR REPLACE INTO mempool (
            txid,
            origin_address,
            origin_nonce,
            sponsor_address,
            sponsor_nonce,
            tx_fee,
            length,
            consensus_hash,
            block_header_hash,
            height,
            accept_time,
            tx)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

        let args: &[&dyn ToSql] = &[
            &txid,
            &origin_address.to_string(),
            &u64_to_sql(origin_nonce)?,
            &sponsor_address.to_string(),
            &u64_to_sql(sponsor_nonce)?,
            &u64_to_sql(tx_fee)?,
            &u64_to_sql(length)?,
            consensus_hash,
            block_header_hash,
            &u64_to_sql(height)?,
            &u64_to_sql(get_epoch_time_secs())?,
            &tx_bytes,
        ];

        tx.execute(sql, args)
            .map_err(|e| MemPoolRejection::DBError(db_error::SqliteError(e)))?;

        MemPoolDB::update_mempool_pager(tx, &txid)?;

        // broadcast drop event if a tx is being replaced
        if let (Some(prior_tx), Some(event_observer)) = (prior_tx, event_observer) {
            event_observer.mempool_txs_dropped(vec![prior_tx.txid], replace_reason);
        };

        Ok(())
    }

    /// Garbage-collect the mempool.  Remove transactions that have a given number of
    /// confirmations.
    pub fn garbage_collect(
        tx: &mut MemPoolTx,
        min_height: u64,
        event_observer: Option<&dyn MemPoolEventDispatcher>,
    ) -> Result<(), db_error> {
        let args: &[&dyn ToSql] = &[&u64_to_sql(min_height)?];

        if let Some(event_observer) = event_observer {
            let sql = "SELECT txid FROM mempool WHERE height < ?1";
            let txids = query_rows(tx, sql, args)?;
            event_observer.mempool_txs_dropped(txids, MemPoolDropReason::STALE_COLLECT);
        }

        let sql = "DELETE FROM mempool WHERE height < ?1";

        tx.execute(sql, args)?;
        increment_stx_mempool_gc();
        Ok(())
    }

    #[cfg(test)]
    pub fn clear_before_height(&mut self, min_height: u64) -> Result<(), db_error> {
        let mut tx = self.tx_begin()?;
        MemPoolDB::garbage_collect(&mut tx, min_height, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Scan the chain tip for all available transactions (but do not remove them!)
    pub fn poll(
        &mut self,
        consensus_hash: &ConsensusHash,
        block_hash: &BlockHeaderHash,
    ) -> Vec<StacksTransaction> {
        test_debug!("Mempool poll at {}/{}", consensus_hash, block_hash);
        MemPoolDB::get_txs_after(
            &self.db,
            consensus_hash,
            block_hash,
            0,
            (i64::MAX - 1) as u64,
        )
        .unwrap_or(vec![])
        .into_iter()
        .map(|tx_info| {
            test_debug!(
                "Mempool poll {} at {}/{}",
                &tx_info.tx.txid(),
                consensus_hash,
                block_hash
            );
            tx_info.tx
        })
        .collect()
    }

    /// Submit a transaction to the mempool at a particular chain tip.
    fn tx_submit(
        mempool_tx: &mut MemPoolTx,
        chainstate: &mut StacksChainState,
        consensus_hash: &ConsensusHash,
        block_hash: &BlockHeaderHash,
        tx: &StacksTransaction,
        do_admission_checks: bool,
        event_observer: Option<&dyn MemPoolEventDispatcher>,
    ) -> Result<(), MemPoolRejection> {
        test_debug!(
            "Mempool submit {} at {}/{}",
            tx.txid(),
            consensus_hash,
            block_hash
        );

        let height = match chainstate.get_stacks_block_height(consensus_hash, block_hash) {
            Ok(Some(h)) => h,
            Ok(None) => {
                if *consensus_hash == FIRST_BURNCHAIN_CONSENSUS_HASH {
                    0
                } else {
                    return Err(MemPoolRejection::NoSuchChainTip(
                        consensus_hash.clone(),
                        block_hash.clone(),
                    ));
                }
            }
            Err(e) => {
                return Err(MemPoolRejection::Other(format!(
                    "Failed to load chain tip: {:?}",
                    &e
                )));
            }
        };

        let txid = tx.txid();
        let mut tx_data = vec![];
        tx.consensus_serialize(&mut tx_data)
            .map_err(MemPoolRejection::SerializationFailure)?;

        let len = tx_data.len() as u64;
        let tx_fee = tx.get_tx_fee();
        let origin_address = tx.origin_address();
        let origin_nonce = tx.get_origin_nonce();
        let (sponsor_address, sponsor_nonce) =
            if let (Some(addr), Some(nonce)) = (tx.sponsor_address(), tx.get_sponsor_nonce()) {
                (addr, nonce)
            } else {
                (origin_address.clone(), origin_nonce)
            };

        if do_admission_checks {
            mempool_tx
                .admitter
                .set_block(&block_hash, (*consensus_hash).clone());
            mempool_tx.admitter.will_admit_tx(chainstate, tx, len)?;
        }

        MemPoolDB::try_add_tx(
            mempool_tx,
            chainstate,
            &consensus_hash,
            &block_hash,
            txid.clone(),
            tx_data,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            event_observer,
        )?;

        if let Err(e) = monitoring::mempool_accepted(&txid, &chainstate.root_path) {
            warn!("Failed to monitor TX receive: {:?}", e; "txid" => %txid);
        }

        Ok(())
    }

    /// One-shot submit
    pub fn submit(
        &mut self,
        chainstate: &mut StacksChainState,
        consensus_hash: &ConsensusHash,
        block_hash: &BlockHeaderHash,
        tx: &StacksTransaction,
        event_observer: Option<&dyn MemPoolEventDispatcher>,
    ) -> Result<(), MemPoolRejection> {
        let mut mempool_tx = self.tx_begin().map_err(MemPoolRejection::DBError)?;
        MemPoolDB::tx_submit(
            &mut mempool_tx,
            chainstate,
            consensus_hash,
            block_hash,
            tx,
            true,
            event_observer,
        )?;
        mempool_tx.commit().map_err(MemPoolRejection::DBError)?;
        Ok(())
    }

    /// Directly submit to the mempool, and don't do any admissions checks.
    pub fn submit_raw(
        &mut self,
        chainstate: &mut StacksChainState,
        consensus_hash: &ConsensusHash,
        block_hash: &BlockHeaderHash,
        tx_bytes: Vec<u8>,
    ) -> Result<(), MemPoolRejection> {
        let tx = StacksTransaction::consensus_deserialize(&mut &tx_bytes[..])
            .map_err(MemPoolRejection::DeserializationFailure)?;

        let mut mempool_tx = self.tx_begin().map_err(MemPoolRejection::DBError)?;
        MemPoolDB::tx_submit(
            &mut mempool_tx,
            chainstate,
            consensus_hash,
            block_hash,
            &tx,
            false,
            None,
        )?;
        mempool_tx.commit().map_err(MemPoolRejection::DBError)?;
        Ok(())
    }

    /// Drop transactions from the mempool
    pub fn drop_txs(&mut self, txids: &[Txid]) -> Result<(), db_error> {
        let mempool_tx = self.tx_begin()?;
        let sql = "DELETE FROM mempool WHERE txid = ?";
        for txid in txids.iter() {
            mempool_tx.execute(sql, &[txid])?;
        }
        mempool_tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn dump_txs(&self) {
        let sql = "SELECT * FROM mempool";
        let txs: Vec<MemPoolTxMetadata> = query_rows(&self.db, sql, NO_PARAMS).unwrap();

        eprintln!("{:#?}", txs);
    }

    /// Do we have a transaction?
    pub fn has_tx(&self, txid: &Txid) -> bool {
        match MemPoolDB::db_has_tx(self.conn(), txid) {
            Ok(b) => {
                if b {
                    test_debug!("Mempool tx already present: {}", txid);
                }
                b
            }
            Err(e) => {
                warn!("Failed to query txid: {:?}", &e);
                false
            }
        }
    }

    /// Get the bloom filter that represents the set of recent transactions we have
    pub fn get_txid_bloom_filter(&self) -> Result<BloomFilter<BloomNodeHasher>, db_error> {
        self.bloom_counter.to_bloom_filter(&self.conn())
    }

    /// Find maximum height represented in the mempool
    fn get_max_height(conn: &DBConn) -> Result<Option<u64>, db_error> {
        let sql = "SELECT 1 FROM mempool WHERE height >= 0";
        let count = query_rows::<i64, _>(conn, sql, NO_PARAMS)?.len();
        if count == 0 {
            Ok(None)
        } else {
            let sql = "SELECT MAX(height) FROM mempool";
            Ok(Some(query_int(conn, sql, NO_PARAMS)? as u64))
        }
    }

    /// Get the transaction ID list that represents the set of transactions that are represented in
    /// the bloom counter.
    pub fn get_bloom_txids(&self) -> Result<Vec<Txid>, db_error> {
        let max_height = match MemPoolDB::get_max_height(&self.conn())? {
            Some(h) => h,
            None => {
                // mempool is empty
                return Ok(vec![]);
            }
        };
        let min_height = max_height.saturating_sub(BLOOM_COUNTER_DEPTH as u64);
        let sql = "SELECT mempool.txid FROM mempool WHERE height > ?1 AND height <= ?2 AND NOT EXISTS (SELECT 1 FROM removed_txids WHERE txid = mempool.txid)";
        let args: &[&dyn ToSql] = &[&u64_to_sql(min_height)?, &u64_to_sql(max_height)?];
        query_rows(&self.conn(), sql, args)
    }

    /// Get the transaction tag list that represents the set of recent transactions we have.
    /// Generate them with our node-local seed so that our txtag list is different from anyone
    /// else's, w.h.p.
    pub fn get_txtags(&self, seed: &[u8]) -> Result<Vec<TxTag>, db_error> {
        self.get_bloom_txids().map(|txid_list| {
            txid_list
                .iter()
                .map(|txid| TxTag::from_seed_and_txid(seed, txid))
                .collect()
        })
    }

    /// How many recent transactions are there -- i.e. within BLOOM_COUNTER_DEPTH block heights of
    /// the chain tip?
    pub fn get_num_recent_txs(conn: &DBConn) -> Result<u64, db_error> {
        let max_height = match MemPoolDB::get_max_height(conn)? {
            Some(h) => h,
            None => {
                // mempool is empty
                return Ok(0);
            }
        };
        let min_height = max_height.saturating_sub(BLOOM_COUNTER_DEPTH as u64);
        let sql = "SELECT COUNT(txid) FROM mempool WHERE height > ?1 AND height <= ?2";
        let args: &[&dyn ToSql] = &[&u64_to_sql(min_height)?, &u64_to_sql(max_height)?];
        query_int(conn, sql, args).map(|cnt| cnt as u64)
    }

    /// Make a mempool sync request.
    /// If sufficiently sparse, use a MemPoolSyncData::TxTags variant
    /// Otherwise, use a MemPoolSyncData::BloomFilter variant
    /// If force_bloom_filter is true, then always make a bloom filter.  The reason for doin this
    /// is that it's faster to do this than making a txtag list, even though the bloom filter is a
    /// larger data structure.
    pub fn make_mempool_sync_data(&self) -> Result<MemPoolSyncData, db_error> {
        let num_tags = MemPoolDB::get_num_recent_txs(self.conn())?;
        if num_tags < self.max_tx_tags.into() {
            let seed = self.bloom_counter.get_seed().clone();
            let tags = self.get_txtags(&seed)?;
            Ok(MemPoolSyncData::TxTags(seed, tags))
        } else {
            Ok(MemPoolSyncData::BloomFilter(self.get_txid_bloom_filter()?))
        }
    }

    /// Get the next batch of transactions from our mempool that are *not* represented in the given
    /// MemPoolSyncData.  Transactions are ordered lexicographically by randomized_txids.hashed_txid, since this allows us
    /// to use the txid as a cursor while ensuring that each node returns txids in a deterministic random order
    /// (so if some nodes are configured to return fewer than MAX_BLOOM_COUNTER_TXS transactions,
    /// a requesting node will still have a good chance of getting something useful).
    pub fn find_next_missing_transactions(
        &self,
        data: &MemPoolSyncData,
        height: u64,
        last_txid: &Txid,
        max_txs: u64,
        max_run: u64,
    ) -> Result<Vec<StacksTransaction>, db_error> {
        let mut ret = vec![];
        let sql = "SELECT mempool.txid as txid, mempool.tx as tx \
                   FROM mempool JOIN randomized_txids \
                   ON mempool.txid = randomized_txids.txid \
                   WHERE randomized_txids.hashed_txid > ?1 \
                   AND mempool.height > ?2 \
                   AND NOT EXISTS \
                        (SELECT 1 FROM removed_txids WHERE txid = mempool.txid) \
                   ORDER BY randomized_txids.hashed_txid ASC LIMIT ?3";

        let args: &[&dyn ToSql] = &[
            &last_txid,
            &u64_to_sql(height.saturating_sub(BLOOM_COUNTER_DEPTH as u64))?,
            &u64_to_sql(max_run)?,
        ];

        let mut tags_table = HashSet::new();
        if let MemPoolSyncData::TxTags(_, ref tags) = data {
            for tag in tags.iter() {
                tags_table.insert(tag.clone());
            }
        }

        let mut stmt = self.conn().prepare(sql)?;
        let mut rows = stmt.query(args)?;
        while let Some(row) = rows.next()? {
            let txid = Txid::from_column(row, "txid")?;
            test_debug!("Consider txid {}", &txid);
            let contains = match data {
                MemPoolSyncData::BloomFilter(ref bf) => bf.contains_raw(&txid.0),
                MemPoolSyncData::TxTags(ref seed, ..) => {
                    tags_table.contains(&TxTag::from_seed_and_txid(seed, &txid))
                }
            };
            if contains {
                // remote peer already has this one
                continue;
            }

            let tx_bytes: Vec<u8> = row.get_unwrap("tx");
            let tx = StacksTransaction::consensus_deserialize(&mut &tx_bytes[..])
                .map_err(|_e| db_error::ParseError)?;

            test_debug!("Returning txid {}", &txid);
            ret.push(tx);
            if (ret.len() as u64) >= max_txs {
                break;
            }
        }

        Ok(ret)
    }

    /// Stream transaction data
    pub fn stream_txs<W: Write>(
        &self,
        fd: &mut W,
        query: &mut TxStreamData,
        count: u64,
    ) -> Result<u64, ChainstateError> {
        let mut num_written = 0;
        while num_written < count {
            if query.num_txs >= query.max_txs {
                // don't serve more than this many txs
                break;
            }

            // write out bufferred tx
            while query.tx_buf.len() > 0 && query.tx_buf_ptr < query.tx_buf.len() {
                let start = query.tx_buf_ptr;
                let end = cmp::min(query.tx_buf.len(), ((start as u64) + count) as usize);
                fd.write_all(&query.tx_buf[start..end])
                    .map_err(ChainstateError::WriteError)?;

                let nw = end.saturating_sub(start) as u64;
                if nw == 0 {
                    break;
                }
                query.tx_buf_ptr = end;
                num_written += nw;
            }

            // load next
            let mut next_txs = self.find_next_missing_transactions(
                &query.tx_query,
                query.height,
                &query.last_txid,
                1,
                MAX_BLOOM_COUNTER_TXS.into(),
            )?;
            if let Some(next_tx) = next_txs.pop() {
                query.tx_buf_ptr = 0;
                query.tx_buf.clear();
                query.num_txs += 1;

                next_tx
                    .consensus_serialize(&mut query.tx_buf)
                    .map_err(ChainstateError::CodecError)?;

                // find next page
                let sql = "SELECT hashed_txid FROM randomized_txids WHERE txid = ?1 LIMIT 1";
                let args: &[&dyn ToSql] = &[&next_tx.txid()];
                let last_txid = match query_row(&self.conn(), sql, args)? {
                    Some(txid) => txid,
                    None => {
                        // done!
                        break;
                    }
                };
                query.last_txid = last_txid;
            } else {
                // no more
                break;
            }
        }
        Ok(num_written)
    }
}

#[cfg(test)]
mod tests {
    use std::cmp;
    use std::collections::HashSet;
    use std::io;

    use address::AddressHashMode;
    use burnchains::Address;
    use burnchains::Txid;
    use chainstate::burn::ConsensusHash;
    use chainstate::stacks::db::test::chainstate_path;
    use chainstate::stacks::db::test::instantiate_chainstate;
    use chainstate::stacks::db::test::instantiate_chainstate_with_balances;
    use chainstate::stacks::db::BlockStreamData;
    use chainstate::stacks::test::codec_all_transactions;
    use chainstate::stacks::{
        db::blocks::MemPoolRejection, db::StacksChainState, index::MarfTrieId, CoinbasePayload,
        Error as ChainstateError, SinglesigHashMode, SinglesigSpendingCondition, StacksPrivateKey,
        StacksPublicKey, StacksTransaction, StacksTransactionSigner, TokenTransferMemo,
        TransactionAnchorMode, TransactionAuth, TransactionContractCall, TransactionPayload,
        TransactionPostConditionMode, TransactionPublicKeyEncoding, TransactionSmartContract,
        TransactionSpendingCondition, TransactionVersion,
    };
    use chainstate::stacks::{
        C32_ADDRESS_VERSION_MAINNET_SINGLESIG, C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
    };
    use core::mempool::MemPoolWalkSettings;
    use core::mempool::TxTag;
    use core::mempool::{BLOOM_COUNTER_DEPTH, BLOOM_COUNTER_ERROR_RATE, MAX_BLOOM_COUNTER_TXS};
    use core::FIRST_BURNCHAIN_CONSENSUS_HASH;
    use core::FIRST_STACKS_BLOCK_HASH;
    use net::Error as NetError;
    use net::MemPoolSyncData;
    use util::bloom::test::setup_bloom_counter;
    use util::bloom::*;
    use util::db::{tx_begin_immediate, DBConn, FromRow};
    use util::get_epoch_time_ms;
    use util::hash::Hash160;
    use util::secp256k1::MessageSignature;
    use util::{hash::hex_bytes, hash::to_hex, hash::*, log, secp256k1::*, strings::StacksString};
    use vm::{
        database::HeadersDB,
        database::NULL_BURN_STATE_DB,
        errors::Error as ClarityError,
        errors::RuntimeErrorType,
        types::{PrincipalData, QualifiedContractIdentifier},
        ClarityName, ContractName, Value,
    };

    use crate::codec::StacksMessageCodec;
    use crate::types::chainstate::{BlockHeaderHash, BurnchainHeaderHash};
    use crate::types::chainstate::{
        StacksAddress, StacksBlockHeader, StacksBlockId, StacksMicroblockHeader, StacksWorkScore,
        VRFSeed,
    };
    use crate::types::proof::TrieHash;
    use crate::{
        chainstate::stacks::db::StacksHeaderInfo, util::vrf::VRFProof, vm::costs::ExecutionCost,
    };

    use super::MemPoolDB;

    use rand::prelude::*;
    use rand::thread_rng;

    use codec::read_next;
    use codec::Error as codec_error;

    const FOO_CONTRACT: &'static str = "(define-public (foo) (ok 1))
                                        (define-public (bar (x uint)) (ok x))";
    const SK_1: &'static str = "a1289f6438855da7decf9b61b852c882c398cff1446b2a0f823538aa2ebef92e01";
    const SK_2: &'static str = "4ce9a8f7539ea93753a36405b16e8b57e15a552430410709c2b6d65dca5c02e201";
    const SK_3: &'static str = "cb95ddd0fe18ec57f4f3533b95ae564b3f1ae063dbf75b46334bd86245aef78501";

    #[test]
    fn mempool_db_init() {
        let _chainstate = instantiate_chainstate(false, 0x80000000, "mempool_db_init");
        let chainstate_path = chainstate_path("mempool_db_init");
        let _mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();
    }

    fn make_block(
        chainstate: &mut StacksChainState,
        block_consensus: ConsensusHash,
        parent: &(ConsensusHash, BlockHeaderHash),
        burn_height: u64,
        block_height: u64,
    ) -> (ConsensusHash, BlockHeaderHash) {
        let (mut chainstate_tx, clar_tx) = chainstate.chainstate_tx_begin().unwrap();

        let anchored_header = StacksBlockHeader {
            version: 1,
            total_work: StacksWorkScore {
                work: block_height,
                burn: 1,
            },
            proof: VRFProof::empty(),
            parent_block: parent.1.clone(),
            parent_microblock: BlockHeaderHash([0; 32]),
            parent_microblock_sequence: 0,
            tx_merkle_root: Sha512Trunc256Sum::empty(),
            state_index_root: TrieHash::from_empty_data(),
            microblock_pubkey_hash: Hash160([0; 20]),
        };

        let block_hash = anchored_header.block_hash();

        let c_tx = StacksChainState::chainstate_block_begin(
            &chainstate_tx,
            clar_tx,
            &NULL_BURN_STATE_DB,
            &parent.0,
            &parent.1,
            &block_consensus,
            &block_hash,
        );

        let new_tip_info = StacksHeaderInfo {
            anchored_header,
            microblock_tail: None,
            index_root: TrieHash::from_empty_data(),
            block_height,
            consensus_hash: block_consensus.clone(),
            burn_header_hash: BurnchainHeaderHash([0; 32]),
            burn_header_height: burn_height as u32,
            burn_header_timestamp: 0,
            anchored_block_size: 1,
        };

        c_tx.commit_block();

        let new_index_hash = StacksBlockId::new(&block_consensus, &block_hash);

        chainstate_tx
            .put_indexed_begin(&StacksBlockId::new(&parent.0, &parent.1), &new_index_hash)
            .unwrap();

        StacksChainState::insert_stacks_block_header(
            &mut chainstate_tx,
            &new_index_hash,
            &new_tip_info,
            &ExecutionCost::zero(),
        )
        .unwrap();

        chainstate_tx.commit().unwrap();

        (block_consensus, block_hash)
    }

    #[test]
    fn mempool_walk_over_fork() {
        let mut chainstate = instantiate_chainstate_with_balances(
            false,
            0x80000000,
            "mempool_walk_over_fork",
            vec![],
        );

        // genesis -> b_1* -> b_2*
        //               \-> b_3 -> b_4
        //
        // *'d blocks accept transactions,
        //   try to walk at b_4, we should be able to find
        //   the transaction at b_1

        let b_1 = make_block(
            &mut chainstate,
            ConsensusHash([0x1; 20]),
            &(
                FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
                FIRST_STACKS_BLOCK_HASH.clone(),
            ),
            1,
            1,
        );
        let b_2 = make_block(&mut chainstate, ConsensusHash([0x2; 20]), &b_1, 2, 2);
        let b_5 = make_block(&mut chainstate, ConsensusHash([0x5; 20]), &b_2, 5, 3);
        let b_3 = make_block(&mut chainstate, ConsensusHash([0x3; 20]), &b_1, 3, 2);
        let b_4 = make_block(&mut chainstate, ConsensusHash([0x4; 20]), &b_3, 4, 3);

        let chainstate_path = chainstate_path("mempool_walk_over_fork");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let mut all_txs = codec_all_transactions(
            &TransactionVersion::Testnet,
            0x80000000,
            &TransactionAnchorMode::Any,
            &TransactionPostConditionMode::Allow,
        );

        let blocks_to_broadcast_in = [&b_1, &b_2, &b_4];
        let mut txs = [
            all_txs.pop().unwrap(),
            all_txs.pop().unwrap(),
            all_txs.pop().unwrap(),
        ];
        for tx in txs.iter_mut() {
            tx.set_tx_fee(123);
        }

        let mut underfunded_txs = [
            all_txs.pop().unwrap(),
            all_txs.pop().unwrap(),
            all_txs.pop().unwrap(),
        ];
        for tx in underfunded_txs.iter_mut() {
            tx.set_tx_fee(0);
        }

        for ix in 0..3 {
            let mut mempool_tx = mempool.tx_begin().unwrap();

            let block = &blocks_to_broadcast_in[ix];
            let good_tx = &txs[ix];
            let underfunded_tx = &underfunded_txs[ix];

            let origin_address = StacksAddress {
                version: 22,
                bytes: Hash160::from_data(&[ix as u8; 32]),
            };
            let underfunded_origin_address = StacksAddress {
                version: 26,
                bytes: Hash160::from_data(&[ix as u8; 32]),
            };
            let sponsor_address = StacksAddress {
                version: 22,
                bytes: Hash160::from_data(&[0x80 | (ix as u8); 32]),
            };
            let underfunded_sponsor_address = StacksAddress {
                version: 26,
                bytes: Hash160::from_data(&[0x80 | (ix as u8); 32]),
            };

            for (i, (tx, (origin, sponsor))) in [good_tx, underfunded_tx]
                .iter()
                .zip(
                    [
                        (origin_address, sponsor_address),
                        (underfunded_origin_address, underfunded_sponsor_address),
                    ]
                    .iter(),
                )
                .enumerate()
            {
                let txid = tx.txid();
                let tx_bytes = tx.serialize_to_vec();
                let tx_fee = tx.get_tx_fee();

                let height = 1 + ix as u64;

                let origin_nonce = 0; // (2 * ix + i) as u64;
                let sponsor_nonce = 0; // (2 * ix + i) as u64;

                assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

                MemPoolDB::try_add_tx(
                    &mut mempool_tx,
                    &mut chainstate,
                    &block.0,
                    &block.1,
                    txid,
                    tx_bytes,
                    tx_fee,
                    height,
                    &origin,
                    origin_nonce,
                    &sponsor,
                    sponsor_nonce,
                    None,
                )
                .unwrap();

                assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());
            }

            mempool_tx.commit().unwrap();
        }

        // genesis -> b_1* -> b_2* -> b_5
        //               \-> b_3 -> b_4
        //
        // *'d blocks accept transactions,
        //   try to walk at b_4, we should be able to find
        //   the transaction at b_1, and we should skip all underfunded transactions.

        let mut mempool_settings = MemPoolWalkSettings::default();
        mempool_settings.min_tx_fee = 10;

        chainstate.with_read_only_clarity_tx(
            &NULL_BURN_STATE_DB,
            &StacksBlockHeader::make_index_block_hash(&b_2.0, &b_2.1),
            |clarity_conn| {
                let mut count_txs = 0;
                mempool
                    .iterate_candidates::<_, ChainstateError, _>(
                        clarity_conn,
                        2,
                        mempool_settings.clone(),
                        |_, available_tx| {
                            count_txs += 1;
                            Ok(true)
                        },
                    )
                    .unwrap();
                assert_eq!(
                    count_txs, 2,
                    "Mempool should find two transactions from b_2"
                );
            },
        );

        chainstate.with_read_only_clarity_tx(
            &NULL_BURN_STATE_DB,
            &StacksBlockHeader::make_index_block_hash(&b_5.0, &b_5.1),
            |clarity_conn| {
                let mut count_txs = 0;
                mempool
                    .iterate_candidates::<_, ChainstateError, _>(
                        clarity_conn,
                        3,
                        mempool_settings.clone(),
                        |_, available_tx| {
                            count_txs += 1;
                            Ok(true)
                        },
                    )
                    .unwrap();
                assert_eq!(
                    count_txs, 3,
                    "Mempool should find three transactions from b_5"
                );
            },
        );

        chainstate.with_read_only_clarity_tx(
            &NULL_BURN_STATE_DB,
            &StacksBlockHeader::make_index_block_hash(&b_3.0, &b_3.1),
            |clarity_conn| {
                let mut count_txs = 0;
                mempool
                    .iterate_candidates::<_, ChainstateError, _>(
                        clarity_conn,
                        2,
                        mempool_settings.clone(),
                        |_, available_tx| {
                            count_txs += 1;
                            Ok(true)
                        },
                    )
                    .unwrap();
                assert_eq!(
                    count_txs, 2,
                    "Mempool should find two transactions from b_3"
                );
            },
        );

        chainstate.with_read_only_clarity_tx(
            &NULL_BURN_STATE_DB,
            &StacksBlockHeader::make_index_block_hash(&b_4.0, &b_4.1),
            |clarity_conn| {
                let mut count_txs = 0;
                mempool
                    .iterate_candidates::<_, ChainstateError, _>(
                        clarity_conn,
                        3,
                        mempool_settings.clone(),
                        |_, available_tx| {
                            count_txs += 1;
                            Ok(true)
                        },
                    )
                    .unwrap();
                assert_eq!(
                    count_txs, 3,
                    "Mempool should find three transactions from b_4"
                );
            },
        );

        // let's test replace-across-fork while we're here.
        // first try to replace a tx in b_2 in b_1 - should fail because they are in the same fork
        let mut mempool_tx = mempool.tx_begin().unwrap();
        let block = &b_1;
        let tx = &txs[1];
        let origin_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&[1; 32]),
        };
        let sponsor_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&[0x81; 32]),
        };

        let txid = tx.txid();
        let tx_bytes = tx.serialize_to_vec();
        let tx_fee = tx.get_tx_fee();

        let height = 3;
        let origin_nonce = 0;
        let sponsor_nonce = 0;

        // make sure that we already have the transaction we're testing for replace-across-fork
        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        assert!(MemPoolDB::try_add_tx(
            &mut mempool_tx,
            &mut chainstate,
            &block.0,
            &block.1,
            txid,
            tx_bytes,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .is_err());

        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());
        mempool_tx.commit().unwrap();

        // now try replace-across-fork from b_2 to b_4
        // check that the number of transactions at b_2 and b_4 starts at 2 each
        assert_eq!(
            MemPoolDB::get_num_tx_at_block(&mempool.db, &b_4.0, &b_4.1).unwrap(),
            2
        );
        assert_eq!(
            MemPoolDB::get_num_tx_at_block(&mempool.db, &b_2.0, &b_2.1).unwrap(),
            2
        );
        let mut mempool_tx = mempool.tx_begin().unwrap();
        let block = &b_4;
        let tx = &txs[1];
        let origin_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&[0; 32]),
        };
        let sponsor_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&[1; 32]),
        };

        let txid = tx.txid();
        let tx_bytes = tx.serialize_to_vec();
        let tx_fee = tx.get_tx_fee();

        let height = 3;
        let origin_nonce = 1;
        let sponsor_nonce = 1;

        // make sure that we already have the transaction we're testing for replace-across-fork
        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        MemPoolDB::try_add_tx(
            &mut mempool_tx,
            &mut chainstate,
            &block.0,
            &block.1,
            txid,
            tx_bytes,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .unwrap();

        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        mempool_tx.commit().unwrap();

        // after replace-across-fork, tx[1] should have moved from the b_2->b_5 fork to b_4
        assert_eq!(
            MemPoolDB::get_num_tx_at_block(&mempool.db, &b_4.0, &b_4.1).unwrap(),
            3
        );
        assert_eq!(
            MemPoolDB::get_num_tx_at_block(&mempool.db, &b_2.0, &b_2.1).unwrap(),
            1
        );
    }

    #[test]
    fn mempool_do_not_replace_tx() {
        let mut chainstate = instantiate_chainstate_with_balances(
            false,
            0x80000000,
            "mempool_do_not_replace_tx",
            vec![],
        );

        // genesis -> b_1 -> b_2
        //      \-> b_3
        //
        let b_1 = make_block(
            &mut chainstate,
            ConsensusHash([0x1; 20]),
            &(
                FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
                FIRST_STACKS_BLOCK_HASH.clone(),
            ),
            1,
            1,
        );
        let b_2 = make_block(&mut chainstate, ConsensusHash([0x2; 20]), &b_1, 2, 2);
        let b_3 = make_block(&mut chainstate, ConsensusHash([0x3; 20]), &b_1, 1, 1);

        let chainstate_path = chainstate_path("mempool_do_not_replace_tx");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let mut txs = codec_all_transactions(
            &TransactionVersion::Testnet,
            0x80000000,
            &TransactionAnchorMode::Any,
            &TransactionPostConditionMode::Allow,
        );
        let mut tx = txs.pop().unwrap();

        let mut mempool_tx = mempool.tx_begin().unwrap();

        // do an initial insert
        let origin_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&[0; 32]),
        };
        let sponsor_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&[1; 32]),
        };

        tx.set_tx_fee(123);

        // test insert
        let txid = tx.txid();
        let tx_bytes = tx.serialize_to_vec();

        let tx_fee = tx.get_tx_fee();
        let height = 100;

        let origin_nonce = tx.get_origin_nonce();
        let sponsor_nonce = match tx.get_sponsor_nonce() {
            Some(n) => n,
            None => origin_nonce,
        };

        assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        MemPoolDB::try_add_tx(
            &mut mempool_tx,
            &mut chainstate,
            &b_1.0,
            &b_1.1,
            txid,
            tx_bytes,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .unwrap();

        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        let prior_txid = txid.clone();

        // now, let's try inserting again, with a lower fee, but at a different block hash
        tx.set_tx_fee(100);
        let txid = tx.txid();
        let tx_bytes = tx.serialize_to_vec();
        let tx_fee = tx.get_tx_fee();
        let height = 100;

        let err_resp = MemPoolDB::try_add_tx(
            &mut mempool_tx,
            &mut chainstate,
            &b_2.0,
            &b_2.1,
            txid,
            tx_bytes,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .unwrap_err();
        assert!(match err_resp {
            MemPoolRejection::ConflictingNonceInMempool => true,
            _ => false,
        });

        assert!(MemPoolDB::db_has_tx(&mempool_tx, &prior_txid).unwrap());
        assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());
    }

    #[test]
    fn mempool_db_load_store_replace_tx() {
        let mut chainstate =
            instantiate_chainstate(false, 0x80000000, "mempool_db_load_store_replace_tx");
        let chainstate_path = chainstate_path("mempool_db_load_store_replace_tx");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let mut txs = codec_all_transactions(
            &TransactionVersion::Testnet,
            0x80000000,
            &TransactionAnchorMode::Any,
            &TransactionPostConditionMode::Allow,
        );
        let num_txs = txs.len() as u64;

        let mut mempool_tx = mempool.tx_begin().unwrap();

        eprintln!("add all txs");
        for (i, mut tx) in txs.drain(..).enumerate() {
            // make sure each address is unique per tx (not the case in codec_all_transactions)
            let origin_address = StacksAddress {
                version: 22,
                bytes: Hash160::from_data(&i.to_be_bytes()),
            };
            let sponsor_address = StacksAddress {
                version: 22,
                bytes: Hash160::from_data(&(i + 1).to_be_bytes()),
            };

            tx.set_tx_fee(123);

            // test insert

            let txid = tx.txid();
            let mut tx_bytes = vec![];
            tx.consensus_serialize(&mut tx_bytes).unwrap();
            let expected_tx = tx.clone();

            let tx_fee = tx.get_tx_fee();
            let height = 100;
            let origin_nonce = tx.get_origin_nonce();
            let sponsor_nonce = match tx.get_sponsor_nonce() {
                Some(n) => n,
                None => origin_nonce,
            };
            let len = tx_bytes.len() as u64;

            assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

            MemPoolDB::try_add_tx(
                &mut mempool_tx,
                &mut chainstate,
                &ConsensusHash([0x1; 20]),
                &BlockHeaderHash([0x2; 32]),
                txid,
                tx_bytes,
                tx_fee,
                height,
                &origin_address,
                origin_nonce,
                &sponsor_address,
                sponsor_nonce,
                None,
            )
            .unwrap();

            assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

            // test retrieval
            let tx_info_opt = MemPoolDB::get_tx(&mempool_tx, &txid).unwrap();
            let tx_info = tx_info_opt.unwrap();

            assert_eq!(tx_info.tx, expected_tx);
            assert_eq!(tx_info.metadata.len, len);
            assert_eq!(tx_info.metadata.tx_fee, 123);
            assert_eq!(tx_info.metadata.origin_address, origin_address);
            assert_eq!(tx_info.metadata.origin_nonce, origin_nonce);
            assert_eq!(tx_info.metadata.sponsor_address, sponsor_address);
            assert_eq!(tx_info.metadata.sponsor_nonce, sponsor_nonce);
            assert_eq!(tx_info.metadata.consensus_hash, ConsensusHash([0x1; 20]));
            assert_eq!(
                tx_info.metadata.block_header_hash,
                BlockHeaderHash([0x2; 32])
            );
            assert_eq!(tx_info.metadata.block_height, height);

            // test replace-by-fee with a higher fee
            let old_txid = txid;

            tx.set_tx_fee(124);
            assert!(txid != tx.txid());

            let txid = tx.txid();
            let mut tx_bytes = vec![];
            tx.consensus_serialize(&mut tx_bytes).unwrap();
            let expected_tx = tx.clone();
            let tx_fee = tx.get_tx_fee();

            assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

            let tx_info_before = MemPoolDB::get_tx_metadata_by_address(
                &mempool_tx,
                true,
                &origin_address,
                origin_nonce,
            )
            .unwrap()
            .unwrap();
            assert_eq!(tx_info_before, tx_info.metadata);

            MemPoolDB::try_add_tx(
                &mut mempool_tx,
                &mut chainstate,
                &ConsensusHash([0x1; 20]),
                &BlockHeaderHash([0x2; 32]),
                txid,
                tx_bytes,
                tx_fee,
                height,
                &origin_address,
                origin_nonce,
                &sponsor_address,
                sponsor_nonce,
                None,
            )
            .unwrap();

            // was replaced
            assert!(!MemPoolDB::db_has_tx(&mempool_tx, &old_txid).unwrap());
            assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

            let tx_info_after = MemPoolDB::get_tx_metadata_by_address(
                &mempool_tx,
                true,
                &origin_address,
                origin_nonce,
            )
            .unwrap()
            .unwrap();
            assert!(tx_info_after != tx_info.metadata);

            // test retrieval -- transaction should have been replaced because it has a higher
            // estimated fee
            let tx_info_opt = MemPoolDB::get_tx(&mempool_tx, &txid).unwrap();

            let tx_info = tx_info_opt.unwrap();
            assert_eq!(tx_info.metadata, tx_info_after);

            assert_eq!(tx_info.tx, expected_tx);
            assert_eq!(tx_info.metadata.len, len);
            assert_eq!(tx_info.metadata.tx_fee, 124);
            assert_eq!(tx_info.metadata.origin_address, origin_address);
            assert_eq!(tx_info.metadata.origin_nonce, origin_nonce);
            assert_eq!(tx_info.metadata.sponsor_address, sponsor_address);
            assert_eq!(tx_info.metadata.sponsor_nonce, sponsor_nonce);
            assert_eq!(tx_info.metadata.consensus_hash, ConsensusHash([0x1; 20]));
            assert_eq!(
                tx_info.metadata.block_header_hash,
                BlockHeaderHash([0x2; 32])
            );
            assert_eq!(tx_info.metadata.block_height, height);

            // test replace-by-fee with a lower fee
            let old_txid = txid;

            tx.set_tx_fee(122);
            assert!(txid != tx.txid());

            let txid = tx.txid();
            let mut tx_bytes = vec![];
            tx.consensus_serialize(&mut tx_bytes).unwrap();
            let _expected_tx = tx.clone();
            let tx_fee = tx.get_tx_fee();

            assert!(match MemPoolDB::try_add_tx(
                &mut mempool_tx,
                &mut chainstate,
                &ConsensusHash([0x1; 20]),
                &BlockHeaderHash([0x2; 32]),
                txid,
                tx_bytes,
                tx_fee,
                height,
                &origin_address,
                origin_nonce,
                &sponsor_address,
                sponsor_nonce,
                None,
            )
            .unwrap_err()
            {
                MemPoolRejection::ConflictingNonceInMempool => true,
                _ => false,
            });

            // was NOT replaced
            assert!(MemPoolDB::db_has_tx(&mempool_tx, &old_txid).unwrap());
            assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());
        }
        mempool_tx.commit().unwrap();

        eprintln!("get all txs");
        let txs = MemPoolDB::get_txs_after(
            &mempool.db,
            &ConsensusHash([0x1; 20]),
            &BlockHeaderHash([0x2; 32]),
            0,
            num_txs,
        )
        .unwrap();
        assert_eq!(txs.len() as u64, num_txs);

        eprintln!("get empty txs");
        let txs = MemPoolDB::get_txs_after(
            &mempool.db,
            &ConsensusHash([0x1; 20]),
            &BlockHeaderHash([0x3; 32]),
            0,
            num_txs,
        )
        .unwrap();
        assert_eq!(txs.len(), 0);

        eprintln!("get empty txs");
        let txs = MemPoolDB::get_txs_after(
            &mempool.db,
            &ConsensusHash([0x2; 20]),
            &BlockHeaderHash([0x2; 32]),
            0,
            num_txs,
        )
        .unwrap();
        assert_eq!(txs.len(), 0);

        eprintln!("garbage-collect");
        let mut mempool_tx = mempool.tx_begin().unwrap();
        MemPoolDB::garbage_collect(&mut mempool_tx, 101, None).unwrap();
        mempool_tx.commit().unwrap();

        let txs = MemPoolDB::get_txs_after(
            &mempool.db,
            &ConsensusHash([0x1; 20]),
            &BlockHeaderHash([0x2; 32]),
            0,
            num_txs,
        )
        .unwrap();
        assert_eq!(txs.len(), 0);
    }

    #[test]
    fn mempool_db_test_rbf() {
        let mut chainstate = instantiate_chainstate(false, 0x80000000, "mempool_db_test_rbf");
        let chainstate_path = chainstate_path("mempool_db_test_rbf");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        // create initial transaction
        let mut mempool_tx = mempool.tx_begin().unwrap();
        let spending_condition =
            TransactionSpendingCondition::Singlesig(SinglesigSpendingCondition {
                signer: Hash160([0x11; 20]),
                hash_mode: SinglesigHashMode::P2PKH,
                key_encoding: TransactionPublicKeyEncoding::Uncompressed,
                nonce: 123,
                tx_fee: 456,
                signature: MessageSignature::from_raw(&vec![0xff; 65]),
            });
        let stx_address = StacksAddress {
            version: 1,
            bytes: Hash160([0xff; 20]),
        };
        let payload = TransactionPayload::TokenTransfer(
            PrincipalData::from(QualifiedContractIdentifier {
                issuer: stx_address.into(),
                name: "hello-contract-name".into(),
            }),
            123,
            TokenTransferMemo([0u8; 34]),
        );
        let mut tx = StacksTransaction {
            version: TransactionVersion::Testnet,
            chain_id: 0x80000000,
            auth: TransactionAuth::Standard(spending_condition.clone()),
            anchor_mode: TransactionAnchorMode::Any,
            post_condition_mode: TransactionPostConditionMode::Allow,
            post_conditions: Vec::new(),
            payload,
        };

        let i: usize = 0;
        let origin_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&i.to_be_bytes()),
        };
        let sponsor_address = StacksAddress {
            version: 22,
            bytes: Hash160::from_data(&(i + 1).to_be_bytes()),
        };

        tx.set_tx_fee(123);
        let txid = tx.txid();
        let mut tx_bytes = vec![];
        tx.consensus_serialize(&mut tx_bytes).unwrap();
        let expected_tx = tx.clone();
        let tx_fee = tx.get_tx_fee();
        let height = 100;
        let origin_nonce = tx.get_origin_nonce();
        let sponsor_nonce = match tx.get_sponsor_nonce() {
            Some(n) => n,
            None => origin_nonce,
        };
        let first_len = tx_bytes.len() as u64;

        assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());
        MemPoolDB::try_add_tx(
            &mut mempool_tx,
            &mut chainstate,
            &ConsensusHash([0x1; 20]),
            &BlockHeaderHash([0x2; 32]),
            txid,
            tx_bytes,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .unwrap();
        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        // test retrieval of initial transaction
        let tx_info_opt = MemPoolDB::get_tx(&mempool_tx, &txid).unwrap();
        let tx_info = tx_info_opt.unwrap();

        // test replace-by-fee with a higher fee, where the payload is smaller
        let old_txid = txid;
        let old_tx_fee = tx_fee;

        tx.set_tx_fee(124);
        tx.payload = TransactionPayload::TokenTransfer(
            stx_address.into(),
            123,
            TokenTransferMemo([0u8; 34]),
        );
        assert!(txid != tx.txid());
        let txid = tx.txid();
        let mut tx_bytes = vec![];
        tx.consensus_serialize(&mut tx_bytes).unwrap();
        let expected_tx = tx.clone();
        let tx_fee = tx.get_tx_fee();
        let second_len = tx_bytes.len() as u64;

        // these asserts are to ensure we are using the fee directly, not the fee rate
        assert!(second_len < first_len);
        assert!(second_len * tx_fee < first_len * old_tx_fee);
        assert!(tx_fee > old_tx_fee);
        assert!(!MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        let tx_info_before =
            MemPoolDB::get_tx_metadata_by_address(&mempool_tx, true, &origin_address, origin_nonce)
                .unwrap()
                .unwrap();
        assert_eq!(tx_info_before, tx_info.metadata);

        MemPoolDB::try_add_tx(
            &mut mempool_tx,
            &mut chainstate,
            &ConsensusHash([0x1; 20]),
            &BlockHeaderHash([0x2; 32]),
            txid,
            tx_bytes,
            tx_fee,
            height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .unwrap();

        // check that the transaction was replaced
        assert!(!MemPoolDB::db_has_tx(&mempool_tx, &old_txid).unwrap());
        assert!(MemPoolDB::db_has_tx(&mempool_tx, &txid).unwrap());

        let tx_info_after =
            MemPoolDB::get_tx_metadata_by_address(&mempool_tx, true, &origin_address, origin_nonce)
                .unwrap()
                .unwrap();
        assert!(tx_info_after != tx_info.metadata);

        // test retrieval -- transaction should have been replaced because it has a higher fee
        let tx_info_opt = MemPoolDB::get_tx(&mempool_tx, &txid).unwrap();
        let tx_info = tx_info_opt.unwrap();
        assert_eq!(tx_info.metadata, tx_info_after);
        assert_eq!(tx_info.metadata.len, second_len);
        assert_eq!(tx_info.metadata.tx_fee, 124);
    }

    #[test]
    fn test_add_txs_bloom_filter() {
        let mut chainstate =
            instantiate_chainstate(false, 0x80000000, "mempool_add_txs_bloom_filter");
        let chainstate_path = chainstate_path("mempool_add_txs_bloom_filter");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let addr = StacksAddress {
            version: 1,
            bytes: Hash160([0xff; 20]),
        };

        let mut all_txids: Vec<Vec<Txid>> = vec![];

        // none conflict
        for block_height in 10..(10 + 10 * BLOOM_COUNTER_DEPTH) {
            let mut txids: Vec<Txid> = vec![];
            let mut fp_count = 0;

            let bf = mempool.get_txid_bloom_filter().unwrap();
            let mut mempool_tx = mempool.tx_begin().unwrap();
            for i in 0..128 {
                let pk = StacksPrivateKey::new();
                let mut tx = StacksTransaction {
                    version: TransactionVersion::Testnet,
                    chain_id: 0x80000000,
                    auth: TransactionAuth::from_p2pkh(&pk).unwrap(),
                    anchor_mode: TransactionAnchorMode::Any,
                    post_condition_mode: TransactionPostConditionMode::Allow,
                    post_conditions: vec![],
                    payload: TransactionPayload::TokenTransfer(
                        addr.to_account_principal(),
                        (block_height + i * 128) as u64,
                        TokenTransferMemo([0u8; 34]),
                    ),
                };
                tx.set_tx_fee(1000);
                tx.set_origin_nonce(0);

                let txid = tx.txid();
                let tx_bytes = tx.serialize_to_vec();
                let origin_addr = tx.origin_address();
                let origin_nonce = tx.get_origin_nonce();
                let sponsor_addr = tx.sponsor_address().unwrap_or(origin_addr.clone());
                let sponsor_nonce = tx.get_sponsor_nonce().unwrap_or(origin_nonce);
                let tx_fee = tx.get_tx_fee();

                // should succeed
                MemPoolDB::try_add_tx(
                    &mut mempool_tx,
                    &mut chainstate,
                    &ConsensusHash([0x1 + (block_height as u8); 20]),
                    &BlockHeaderHash([0x2 + (block_height as u8); 32]),
                    txid,
                    tx_bytes,
                    tx_fee,
                    block_height as u64,
                    &origin_addr,
                    origin_nonce,
                    &sponsor_addr,
                    sponsor_nonce,
                    None,
                )
                .unwrap();

                if bf.contains_raw(&tx.txid().0) {
                    fp_count += 1;
                }

                txids.push(txid);
            }

            mempool_tx.commit().unwrap();

            // nearly all txs should be new
            assert!((fp_count as f64) / (MAX_BLOOM_COUNTER_TXS as f64) <= BLOOM_COUNTER_ERROR_RATE);

            let bf = mempool.get_txid_bloom_filter().unwrap();
            for txid in txids.iter() {
                assert!(
                    bf.contains_raw(&txid.0),
                    "Bloom filter does not contain {}",
                    &txid
                );
            }

            all_txids.push(txids);

            if block_height > 10 + BLOOM_COUNTER_DEPTH {
                let expired_block_height = block_height - BLOOM_COUNTER_DEPTH;
                let bf = mempool.get_txid_bloom_filter().unwrap();
                for i in 0..(block_height - 10 - BLOOM_COUNTER_DEPTH) {
                    let txids = &all_txids[i];
                    let mut fp_count = 0;
                    for txid in txids {
                        if bf.contains_raw(&txid.0) {
                            fp_count += 1;
                        }
                    }

                    // these expired txids should mostly be absent
                    assert!(
                        (fp_count as f64) / (MAX_BLOOM_COUNTER_TXS as f64)
                            <= BLOOM_COUNTER_ERROR_RATE
                    );
                }
            }
        }
    }

    #[test]
    fn test_txtags() {
        let mut chainstate = instantiate_chainstate(false, 0x80000000, "mempool_txtags");
        let chainstate_path = chainstate_path("mempool_txtags");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let addr = StacksAddress {
            version: 1,
            bytes: Hash160([0xff; 20]),
        };

        let mut seed = [0u8; 32];
        thread_rng().fill_bytes(&mut seed);

        let mut all_txtags: Vec<Vec<TxTag>> = vec![];

        for block_height in 10..(10 + 10 * BLOOM_COUNTER_DEPTH) {
            let mut txtags: Vec<TxTag> = vec![];

            let mut mempool_tx = mempool.tx_begin().unwrap();
            for i in 0..128 {
                let pk = StacksPrivateKey::new();
                let mut tx = StacksTransaction {
                    version: TransactionVersion::Testnet,
                    chain_id: 0x80000000,
                    auth: TransactionAuth::from_p2pkh(&pk).unwrap(),
                    anchor_mode: TransactionAnchorMode::Any,
                    post_condition_mode: TransactionPostConditionMode::Allow,
                    post_conditions: vec![],
                    payload: TransactionPayload::TokenTransfer(
                        addr.to_account_principal(),
                        (block_height + i * 128) as u64,
                        TokenTransferMemo([0u8; 34]),
                    ),
                };
                tx.set_tx_fee(1000);
                tx.set_origin_nonce(0);

                let txid = tx.txid();
                let tx_bytes = tx.serialize_to_vec();
                let origin_addr = tx.origin_address();
                let origin_nonce = tx.get_origin_nonce();
                let sponsor_addr = tx.sponsor_address().unwrap_or(origin_addr.clone());
                let sponsor_nonce = tx.get_sponsor_nonce().unwrap_or(origin_nonce);
                let tx_fee = tx.get_tx_fee();

                let txtag = TxTag::from_seed_and_txid(&seed, &txid);

                // should succeed
                MemPoolDB::try_add_tx(
                    &mut mempool_tx,
                    &mut chainstate,
                    &ConsensusHash([0x1 + (block_height as u8); 20]),
                    &BlockHeaderHash([0x2 + (block_height as u8); 32]),
                    txid,
                    tx_bytes,
                    tx_fee,
                    block_height as u64,
                    &origin_addr,
                    origin_nonce,
                    &sponsor_addr,
                    sponsor_nonce,
                    None,
                )
                .unwrap();

                txtags.push(txtag);
            }

            mempool_tx.commit().unwrap();
            all_txtags.push(txtags);

            if block_height - 10 >= BLOOM_COUNTER_DEPTH {
                assert_eq!(
                    MemPoolDB::get_num_recent_txs(mempool.conn()).unwrap(),
                    (BLOOM_COUNTER_DEPTH * 128) as u64
                );
            }

            let txtags = mempool.get_txtags(&seed).unwrap();
            let len_txtags = all_txtags.len();
            let last_txtags =
                &all_txtags[len_txtags.saturating_sub(BLOOM_COUNTER_DEPTH as usize)..len_txtags];

            let mut expected_txtag_set = HashSet::new();
            for txtags in last_txtags.iter() {
                for txtag in txtags.iter() {
                    expected_txtag_set.insert(txtag.clone());
                }
            }

            assert_eq!(expected_txtag_set.len(), txtags.len());
            for txtag in txtags.into_iter() {
                assert!(expected_txtag_set.contains(&txtag));
            }
        }
    }

    #[test]
    #[ignored]
    fn test_make_mempool_sync_data() {
        let mut chainstate = instantiate_chainstate(false, 0x80000000, "make_mempool_sync_data");
        let chainstate_path = chainstate_path("make_mempool_sync_data");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let addr = StacksAddress {
            version: 1,
            bytes: Hash160([0xff; 20]),
        };

        let mut txids = vec![];
        let mut nonrecent_fp_rates = vec![];
        for block_height in 10..(10 + BLOOM_COUNTER_DEPTH + 1) {
            for i in 0..((MAX_BLOOM_COUNTER_TXS + 128) as usize) {
                let mut mempool_tx = mempool.tx_begin().unwrap();
                for j in 0..128 {
                    let pk = StacksPrivateKey::new();
                    let mut tx = StacksTransaction {
                        version: TransactionVersion::Testnet,
                        chain_id: 0x80000000,
                        auth: TransactionAuth::from_p2pkh(&pk).unwrap(),
                        anchor_mode: TransactionAnchorMode::Any,
                        post_condition_mode: TransactionPostConditionMode::Allow,
                        post_conditions: vec![],
                        payload: TransactionPayload::TokenTransfer(
                            addr.to_account_principal(),
                            123,
                            TokenTransferMemo([0u8; 34]),
                        ),
                    };
                    tx.set_tx_fee(1000);
                    tx.set_origin_nonce(0);

                    let txid = tx.txid();
                    let tx_bytes = tx.serialize_to_vec();
                    let origin_addr = tx.origin_address();
                    let origin_nonce = tx.get_origin_nonce();
                    let sponsor_addr = tx.sponsor_address().unwrap_or(origin_addr.clone());
                    let sponsor_nonce = tx.get_sponsor_nonce().unwrap_or(origin_nonce);
                    let tx_fee = tx.get_tx_fee();

                    // should succeed
                    MemPoolDB::try_add_tx(
                        &mut mempool_tx,
                        &mut chainstate,
                        &ConsensusHash([0x1 + (block_height as u8); 20]),
                        &BlockHeaderHash([0x2 + (block_height as u8); 32]),
                        txid.clone(),
                        tx_bytes,
                        tx_fee,
                        block_height as u64,
                        &origin_addr,
                        origin_nonce,
                        &sponsor_addr,
                        sponsor_nonce,
                        None,
                    )
                    .unwrap();

                    txids.push(txid);
                }
                mempool_tx.commit().unwrap();

                let ts_1 = get_epoch_time_ms();
                let ms = mempool.make_mempool_sync_data().unwrap();
                let ts_2 = get_epoch_time_ms();
                eprintln!(
                    "make_mempool_sync_data({}): {} ms",
                    txids.len(),
                    ts_2.saturating_sub(ts_1)
                );

                let mut present_count: u32 = 0;
                let mut absent_count: u32 = 0;
                let mut fp_count: u32 = 0;
                match ms {
                    MemPoolSyncData::BloomFilter(ref bf) => {
                        eprintln!(
                            "bloomfilter({}); txids.len() == {}",
                            block_height,
                            txids.len()
                        );
                        let recent_txids = mempool.get_bloom_txids().unwrap();
                        assert!(recent_txids.len() <= MAX_BLOOM_COUNTER_TXS as usize);

                        let max_height = MemPoolDB::get_max_height(mempool.conn())
                            .unwrap()
                            .unwrap_or(0);
                        eprintln!(
                            "bloomfilter({}): recent_txids.len() == {}, max height is {}",
                            block_height,
                            recent_txids.len(),
                            max_height
                        );

                        let mut recent_set = HashSet::new();
                        let mut in_bf = 0;
                        for txid in recent_txids.iter() {
                            if bf.contains_raw(&txid.0) {
                                in_bf += 1;
                            }
                            recent_set.insert(txid.clone());
                        }

                        eprintln!("in bloom filter: {}", in_bf);
                        assert!(in_bf >= recent_txids.len());

                        for txid in txids.iter() {
                            if !recent_set.contains(&txid) && bf.contains_raw(&txid.0) {
                                fp_count += 1;
                            }
                            if bf.contains_raw(&txid.0) {
                                present_count += 1;
                            } else {
                                absent_count += 1;
                            }
                        }

                        // all recent transactions should be present
                        assert!(
                            present_count
                                >= cmp::min(MAX_BLOOM_COUNTER_TXS.into(), txids.len() as u32)
                        );
                    }
                    MemPoolSyncData::TxTags(ref seed, ref tags) => {
                        eprintln!("txtags({}); txids.len() == {}", block_height, txids.len());
                        let recent_txids = mempool.get_bloom_txids().unwrap();

                        // all tags are present in the recent set
                        let mut recent_set = HashSet::new();
                        for txid in recent_txids {
                            recent_set.insert(TxTag::from_seed_and_txid(seed, &txid));
                        }

                        for tag in tags.iter() {
                            assert!(recent_set.contains(tag));
                        }
                    }
                }

                let mut nonrecent_fp_rate = 0.0f64;
                let recent_txids = mempool.get_bloom_txids().unwrap();
                if recent_txids.len() < (present_count + absent_count) as usize {
                    nonrecent_fp_rate = (fp_count as f64)
                        / ((present_count + absent_count - (recent_txids.len() as u32)) as f64);
                    eprintln!(
                        "Nonrecent false positive rate: {} / ({} + {} - {} = {}) = {}",
                        fp_count,
                        present_count,
                        absent_count,
                        recent_txids.len(),
                        present_count + absent_count - (recent_txids.len() as u32),
                        nonrecent_fp_rate
                    );
                }

                let total_count = MemPoolDB::get_num_recent_txs(&mempool.conn()).unwrap();
                eprintln!(
                    "present_count: {}, absent count: {}, total sent: {}, total recent: {}",
                    present_count,
                    absent_count,
                    txids.len(),
                    total_count
                );

                nonrecent_fp_rates.push(nonrecent_fp_rate);
            }
        }

        // average false positive rate for non-recent transactions should be around the bloom
        // counter false positive rate
        let num_nonrecent_fp_samples = nonrecent_fp_rates.len() as f64;
        let avg_nonrecent_fp_rate =
            nonrecent_fp_rates.iter().fold(0.0f64, |acc, x| acc + x) / num_nonrecent_fp_samples;

        assert!((avg_nonrecent_fp_rate - BLOOM_COUNTER_ERROR_RATE).abs() < 0.001);
    }

    #[test]
    fn test_find_next_missing_transactions() {
        let mut chainstate =
            instantiate_chainstate(false, 0x80000000, "find_next_missing_transactions");
        let chainstate_path = chainstate_path("find_next_missing_transactions");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let addr = StacksAddress {
            version: 1,
            bytes: Hash160([0xff; 20]),
        };

        let block_height = 10;
        let mut txids = vec![];

        let mut mempool_tx = mempool.tx_begin().unwrap();
        for i in 0..(2 * MAX_BLOOM_COUNTER_TXS) {
            let pk = StacksPrivateKey::new();
            let mut tx = StacksTransaction {
                version: TransactionVersion::Testnet,
                chain_id: 0x80000000,
                auth: TransactionAuth::from_p2pkh(&pk).unwrap(),
                anchor_mode: TransactionAnchorMode::Any,
                post_condition_mode: TransactionPostConditionMode::Allow,
                post_conditions: vec![],
                payload: TransactionPayload::TokenTransfer(
                    addr.to_account_principal(),
                    123,
                    TokenTransferMemo([0u8; 34]),
                ),
            };
            tx.set_tx_fee(1000);
            tx.set_origin_nonce(0);

            let txid = tx.txid();
            let tx_bytes = tx.serialize_to_vec();
            let origin_addr = tx.origin_address();
            let origin_nonce = tx.get_origin_nonce();
            let sponsor_addr = tx.sponsor_address().unwrap_or(origin_addr.clone());
            let sponsor_nonce = tx.get_sponsor_nonce().unwrap_or(origin_nonce);
            let tx_fee = tx.get_tx_fee();

            // should succeed
            MemPoolDB::try_add_tx(
                &mut mempool_tx,
                &mut chainstate,
                &ConsensusHash([0x1 + (block_height as u8); 20]),
                &BlockHeaderHash([0x2 + (block_height as u8); 32]),
                txid.clone(),
                tx_bytes,
                tx_fee,
                block_height as u64,
                &origin_addr,
                origin_nonce,
                &sponsor_addr,
                sponsor_nonce,
                None,
            )
            .unwrap();

            eprintln!("Added {} {}", i, &txid);
            txids.push(txid);
        }
        mempool_tx.commit().unwrap();

        let mut txid_set = HashSet::new();
        for txid in txids.iter() {
            txid_set.insert(txid.clone());
        }

        eprintln!("Find next missing transactions");

        let txtags = mempool.get_txtags(&[0u8; 32]).unwrap();

        // no txs returned for a full txtag set
        let txs = mempool
            .find_next_missing_transactions(
                &MemPoolSyncData::TxTags([0u8; 32], txtags.clone()),
                block_height,
                &Txid([0u8; 32]),
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                MAX_BLOOM_COUNTER_TXS as u64,
            )
            .unwrap();
        assert_eq!(txs.len(), 0);

        // all txs returned for an empty txtag set
        let txs = mempool
            .find_next_missing_transactions(
                &MemPoolSyncData::TxTags([0u8; 32], vec![]),
                block_height,
                &Txid([0u8; 32]),
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                MAX_BLOOM_COUNTER_TXS as u64,
            )
            .unwrap();
        for tx in txs {
            assert!(txid_set.contains(&tx.txid()));
        }

        // all bloom-filter-absent txids should be returned
        let txid_bloom = mempool.get_txid_bloom_filter().unwrap();
        let txs = mempool
            .find_next_missing_transactions(
                &MemPoolSyncData::BloomFilter(txid_bloom),
                block_height,
                &Txid([0u8; 32]),
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
            )
            .unwrap();
        assert_eq!(txs.len(), 0);

        let mut empty_bloom_conn = setup_bloom_counter("find_next_missing_txs_empty");
        let mut empty_tx = tx_begin_immediate(&mut empty_bloom_conn).unwrap();
        let hasher = BloomNodeHasher::new(&[0u8; 32]);
        let empty_bloom = BloomCounter::new(
            &mut empty_tx,
            "bloom_counter",
            BLOOM_COUNTER_ERROR_RATE,
            MAX_BLOOM_COUNTER_TXS,
            hasher,
        )
        .unwrap();
        empty_tx.commit().unwrap();

        let txs = mempool
            .find_next_missing_transactions(
                &MemPoolSyncData::BloomFilter(
                    empty_bloom.to_bloom_filter(&empty_bloom_conn).unwrap(),
                ),
                block_height,
                &Txid([0u8; 32]),
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
            )
            .unwrap();
        for tx in txs {
            assert!(txid_set.contains(&tx.txid()));
        }

        // paginated access works too
        let mut last_txid = Txid([0u8; 32]);
        let page_size = 10;
        let mut all_txs = vec![];
        for i in 0..(txtags.len() / (page_size as usize)) + 1 {
            let mut txs = mempool
                .find_next_missing_transactions(
                    &MemPoolSyncData::TxTags([0u8; 32], vec![]),
                    block_height,
                    &last_txid,
                    (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                    page_size,
                )
                .unwrap();
            assert!(txs.len() <= page_size as usize);

            if txs.len() == 0 {
                break;
            }

            last_txid = txs.last().clone().unwrap().txid();
            all_txs.append(&mut txs);
        }

        for tx in all_txs {
            assert!(txid_set.contains(&tx.txid()));
        }

        last_txid = Txid([0u8; 32]);
        all_txs = vec![];
        for i in 0..(txtags.len() / (page_size as usize)) + 1 {
            let mut txs = mempool
                .find_next_missing_transactions(
                    &MemPoolSyncData::BloomFilter(
                        empty_bloom.to_bloom_filter(&empty_bloom_conn).unwrap(),
                    ),
                    block_height,
                    &last_txid,
                    (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                    page_size,
                )
                .unwrap();
            assert!(txs.len() <= page_size as usize);

            if txs.len() == 0 {
                break;
            }

            last_txid = txs.last().clone().unwrap().txid();
            all_txs.append(&mut txs);
        }

        for tx in all_txs {
            assert!(txid_set.contains(&tx.txid()));
        }

        // old transactions are ignored
        let old_txs = mempool
            .find_next_missing_transactions(
                &MemPoolSyncData::TxTags([0u8; 32], vec![]),
                block_height + (BLOOM_COUNTER_DEPTH as u64) + 1,
                &last_txid,
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                page_size,
            )
            .unwrap();
        assert_eq!(old_txs.len(), 0);

        let old_txs = mempool
            .find_next_missing_transactions(
                &MemPoolSyncData::BloomFilter(
                    empty_bloom.to_bloom_filter(&empty_bloom_conn).unwrap(),
                ),
                block_height + (BLOOM_COUNTER_DEPTH as u64) + 1,
                &last_txid,
                (2 * MAX_BLOOM_COUNTER_TXS) as u64,
                page_size,
            )
            .unwrap();
        assert_eq!(old_txs.len(), 0);
    }

    #[test]
    fn test_stream_txs() {
        let mut chainstate = instantiate_chainstate(false, 0x80000000, "test_stream_txs");
        let chainstate_path = chainstate_path("test_stream_txs");
        let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

        let addr = StacksAddress {
            version: 1,
            bytes: Hash160([0xff; 20]),
        };
        let mut txs = vec![];
        let block_height = 10;
        let mut total_len = 0;

        let mut mempool_tx = mempool.tx_begin().unwrap();
        for i in 0..10 {
            let pk = StacksPrivateKey::new();
            let mut tx = StacksTransaction {
                version: TransactionVersion::Testnet,
                chain_id: 0x80000000,
                auth: TransactionAuth::from_p2pkh(&pk).unwrap(),
                anchor_mode: TransactionAnchorMode::Any,
                post_condition_mode: TransactionPostConditionMode::Allow,
                post_conditions: vec![],
                payload: TransactionPayload::TokenTransfer(
                    addr.to_account_principal(),
                    123,
                    TokenTransferMemo([0u8; 34]),
                ),
            };
            tx.set_tx_fee(1000);
            tx.set_origin_nonce(0);

            let txid = tx.txid();
            let tx_bytes = tx.serialize_to_vec();
            let origin_addr = tx.origin_address();
            let origin_nonce = tx.get_origin_nonce();
            let sponsor_addr = tx.sponsor_address().unwrap_or(origin_addr.clone());
            let sponsor_nonce = tx.get_sponsor_nonce().unwrap_or(origin_nonce);
            let tx_fee = tx.get_tx_fee();

            total_len += tx_bytes.len();

            // should succeed
            MemPoolDB::try_add_tx(
                &mut mempool_tx,
                &mut chainstate,
                &ConsensusHash([0x1 + (block_height as u8); 20]),
                &BlockHeaderHash([0x2 + (block_height as u8); 32]),
                txid.clone(),
                tx_bytes,
                tx_fee,
                block_height as u64,
                &origin_addr,
                origin_nonce,
                &sponsor_addr,
                sponsor_nonce,
                None,
            )
            .unwrap();

            eprintln!("Added {} {}", i, &txid);
            txs.push(tx);
        }
        mempool_tx.commit().unwrap();

        let mut buf = vec![];
        let mut stream = BlockStreamData::new_tx_stream(
            MemPoolSyncData::TxTags([0u8; 32], vec![]),
            MAX_BLOOM_COUNTER_TXS.into(),
            block_height,
        );
        let mut tx_stream_data = stream.take_tx_stream().unwrap();

        loop {
            let nw = mempool
                .stream_txs(&mut buf, &mut tx_stream_data, 10)
                .unwrap();
            if nw == 0 {
                break;
            }
        }

        // buf decodes to the list of txs we have
        let mut decoded_txs = vec![];
        let mut ptr = &buf[..];
        loop {
            let tx: StacksTransaction = match read_next::<StacksTransaction, _>(&mut ptr) {
                Ok(tx) => tx,
                Err(e) => match e {
                    codec_error::ReadError(ref ioe) => match ioe.kind() {
                        io::ErrorKind::UnexpectedEof => {
                            eprintln!("out of transactions");
                            break;
                        }
                        _ => {
                            panic!("IO error: {:?}", &e);
                        }
                    },
                    _ => {
                        panic!("other error: {:?}", &e);
                    }
                },
            };
            decoded_txs.push(tx);
        }

        let mut tx_set = HashSet::new();
        for tx in txs.iter() {
            tx_set.insert(tx.txid());
        }

        // the order won't be preserved
        assert_eq!(tx_set.len(), decoded_txs.len());
        for tx in decoded_txs {
            assert!(tx_set.contains(&tx.txid()));
        }
    }
}
