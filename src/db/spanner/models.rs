use futures::future;
use futures::lazy;

use chrono::DateTime;

use diesel::r2d2::PooledConnection;

use std::cell::RefCell;
use std::cmp;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use super::manager::SpannerConnectionManager;
use super::pool::CollectionCache;
use super::support::{SpannerType, SyncResultSet};

use crate::db::{
    error::{DbError, DbErrorKind},
    params, results,
    util::{to_rfc3339, SyncTimestamp},
    Db, DbFuture, Sorting,
};

use crate::web::extractors::BsoQueryParams;

use super::{
    batch,
    support::{as_value, ExecuteSqlRequestBuilder},
};

use google_spanner1::{
    BeginTransactionRequest, CommitRequest, ExecuteSqlRequest, ReadOnly, ReadWrite, ResultSet,
    RollbackRequest, TransactionOptions, TransactionSelector,
};

#[derive(Debug)]
pub enum CollectionLock {
    Read,
    Write,
}

pub(super) type Conn = PooledConnection<SpannerConnectionManager>;
pub type Result<T> = std::result::Result<T, DbError>;

/// The ttl to use for rows that are never supposed to expire (in seconds)
pub const DEFAULT_BSO_TTL: i64 = 2_100_000_000;

/// Per session Db metadata
#[derive(Debug, Default)]
struct SpannerDbSession {
    /// The "current time" on the server used for this session's operations
    timestamp: SyncTimestamp,
    /// Cache of collection modified timestamps per (user_id, collection_id)
    coll_modified_cache: HashMap<(u32, i32), SyncTimestamp>,
    /// Currently locked collections
    coll_locks: HashMap<(u32, i32), CollectionLock>,
    transaction: Option<TransactionSelector>,
    execute_sql_count: u64,
}

#[derive(Clone, Debug)]
pub struct SpannerDb {
    pub(super) inner: Arc<SpannerDbInner>,

    /// Pool level cache of collection_ids and their names
    coll_cache: Arc<CollectionCache>,
}

pub struct SpannerDbInner {
    pub(super) conn: Conn,

    thread_pool: Arc<::tokio_threadpool::ThreadPool>,
    session: RefCell<SpannerDbSession>,
}

impl fmt::Debug for SpannerDbInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpannerDbInner")
    }
}

impl Deref for SpannerDb {
    type Target = SpannerDbInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

macro_rules! batch_db_method {
    ($name:ident, $batch_name:ident, $type:ident) => {
        pub fn $name(&self, params: params::$type) -> Result<results::$type> {
            batch::$batch_name(self, params)
        }
    }
}

impl SpannerDb {
    pub fn new(
        conn: Conn,
        thread_pool: Arc<::tokio_threadpool::ThreadPool>,
        coll_cache: Arc<CollectionCache>,
    ) -> Self {
        let inner = SpannerDbInner {
            conn,
            thread_pool,
            session: RefCell::new(Default::default()),
        };
        SpannerDb {
            inner: Arc::new(inner),
            coll_cache,
        }
    }

    pub(super) fn get_collection_id(&self, name: &str) -> Result<i32> {
        if let Some(id) = self.coll_cache.get_id(name)? {
            return Ok(id);
        }

        let result = self
            .sql("SELECT collectionid FROM collections WHERE name = @name")?
            .params(params! {"name" => name.to_string()})
            .execute(&self.conn)?
            .one_or_none()?
            .ok_or(DbErrorKind::CollectionNotFound)?;
        //let rows = result.1.rows.ok_or(DbErrorKind::CollectionNotFound)?;
        //let id = rows[0][0]
        let id = result[0]
            .get_string_value()
            .parse::<i32>()
            .map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
        self.coll_cache.put(id, name.to_owned())?;
        Ok(id)
    }

    pub(super) fn create_collection(&self, name: &str) -> Result<i32> {
        // XXX: handle concurrent attempts at inserts
        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();

        let sql = self.sql_request("SELECT COALESCE(MAX(collectionid), 1) from collections")?;
        let result = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit()?;
        let id = if let Some(rows) = result.1.rows {
            let max = rows[0][0]
                .parse::<i32>()
                .map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
            max + 1
        } else {
            // XXX: should never happen but defaulting to id of 1 is bad
            1
        };
         */
        let result = self
            .sql("SELECT COALESCE(MAX(collectionid), 1) from collections")?
            .execute(&self.conn)?
            .one()?;
        let max = result[0]
            .get_string_value()
            .parse::<i32>()
            .map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
        let id = max + 1;

        self.sql("INSERT INTO collections (collectionid, name) VALUES (@collectionid, @name)")?
            .params(params! {
                "name" => name.to_string(),
                "collectionid" => cmp::max(id, 100).to_string(),
            })
            .execute(&self.conn)?;
        self.coll_cache.put(id, name.to_owned())?;
        Ok(id)
        /*
        let mut sql = self.sql_request(
            "INSERT INTO collections (collectionid, name) VALUES (@collectionid, @name)",
        )?;
        let params = params! {
            "name" => name.to_string(),
            "collectionid" => cmp::max(id, 100).to_string(),
        };
        sql.params = Some(params);

        spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit()?;
        self.coll_cache.put(id, name.to_owned())?;
        Ok(id)
        */
    }

    fn get_or_create_collection_id(&self, name: &str) -> Result<i32> {
        self.get_collection_id(name).or_else(|e| match e.kind() {
            DbErrorKind::CollectionNotFound => self.create_collection(name),
            _ => Err(e),
        })
    }

    pub fn lock_for_read_sync(&self, params: params::LockCollection) -> Result<()> {
        let user_id = params.user_id.legacy_id as u32;
        let collection_id =
            self.get_collection_id(&params.collection)
                .or_else(|e| match e.kind() {
                    // If the collection doesn't exist, we still want to start a
                    // transaction so it will continue to not exist.
                    DbErrorKind::CollectionNotFound => Ok(0),
                    _ => Err(e),
                })?;
        // If we already have a read or write lock then it's safe to
        // use it as-is.
        if self
            .inner
            .session
            .borrow()
            .coll_locks
            .get(&(user_id, collection_id))
            .is_some()
        {
            return Ok(());
        }

        // Lock the db
        self.begin(false)?;
        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT CURRENT_TIMESTAMP() as now, last_modified FROM user_collections WHERE userid=@userid AND collection=@collectionid")?;
        let params = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
        };
        sql.params = Some(params);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        if let Ok(results) = results {
            if let Some(rows) = results.1.rows {
                let modified = SyncTimestamp::from_rfc3339(&rows[0][0])?;
                self.session
                    .borrow_mut()
                    .coll_modified_cache
                    .insert((user_id, collection_id), modified);
            }
        }
         */
        // XXX: lock_for_read shouldn't need to query for a timestamp?
        let result = self
            .sql("SELECT CURRENT_TIMESTAMP() as now, last_modified FROM user_collections WHERE userid=@userid AND collection=@collectionid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
            })
            .execute(&self.conn)?
            .one_or_none()?;
        // XXX: python code does a "SELECT CURRENT_TIMESTAMP()" when None here
        if let Some(result) = result {
            let modified = SyncTimestamp::from_rfc3339(result[0].get_string_value())?;
            self.session
                .borrow_mut()
                .coll_modified_cache
                .insert((user_id, collection_id), modified);
        }
        self.session
            .borrow_mut()
            .coll_locks
            .insert((user_id, collection_id), CollectionLock::Read);

        Ok(())
    }

    pub fn lock_for_write_sync(&self, params: params::LockCollection) -> Result<()> {
        let user_id = params.user_id.legacy_id as u32;
        let collection_id = self.get_or_create_collection_id(&params.collection)?;
        if let Some(CollectionLock::Read) = self
            .inner
            .session
            .borrow()
            .coll_locks
            .get(&(user_id, collection_id))
        {
            Err(DbError::internal("Can't escalate read-lock to write-lock"))?
        }

        // Lock the db
        self.begin(true)?;
        /*
                let spanner = &self.conn;
                let session = spanner.session.name.as_ref().unwrap();
                let mut sql = self.sql_request("SELECT CURRENT_TIMESTAMP() as now, last_modified FROM user_collections WHERE userid=@userid AND collection=@collectionid")?;
                let params = params! {
                    "userid" => user_id.to_string(),
                    "collectionid" => collection_id.to_string(),
                };
                sql.params = Some(params);

                let results = spanner
                    .hub
                    .projects()
                    .instances_databases_sessions_execute_sql(sql, session)
                    .doit();
                if let Ok(results) = results {
                    if let Some(rows) = results.1.rows {
                        let modified = SyncTimestamp::from_rfc3339(&rows[0][0])?;
                        self.session
                            .borrow_mut()
                            .coll_modified_cache
                            .insert((user_id, collection_id), modified);
                    }
                }
        */
        let result = self
            .sql("SELECT CURRENT_TIMESTAMP() as now, last_modified FROM user_collections WHERE userid=@userid AND collection=@collectionid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
            })
            .execute(&self.conn)?
            .one_or_none()?;
        // XXX: python code does a "SELECT CURRENT_TIMESTAMP()" when None here
        if let Some(result) = result {
            let modified = SyncTimestamp::from_rfc3339(result[0].get_string_value())?;
            self.session
                .borrow_mut()
                .coll_modified_cache
                .insert((user_id, collection_id), modified);
        }
        self.session
            .borrow_mut()
            .coll_locks
            .insert((user_id, collection_id), CollectionLock::Write);

        Ok(())
    }

    pub(super) fn begin(&self, for_write: bool) -> Result<()> {
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut options = TransactionOptions::default();
        if for_write {
            options.read_write = Some(ReadWrite::default());
        } else {
            options.read_only = Some(ReadOnly::default());
        }
        let req = BeginTransactionRequest {
            options: Some(options),
        };
        let (_, transaction) = spanner
            .hub
            .projects()
            .instances_databases_sessions_begin_transaction(req, session)
            .doit()?;
        self.session.borrow_mut().transaction = Some(google_spanner1::TransactionSelector {
            id: transaction.id,
            ..Default::default()
        });
        Ok(())
    }

    /// Return the current transaction metadata (TransactionSelector) if one is active.
    fn get_transaction(&self) -> Result<Option<TransactionSelector>> {
        Ok(if self.session.borrow().transaction.is_some() {
            self.session.borrow().transaction.clone()
        } else {
            self.begin(true)?;
            self.session.borrow().transaction.clone()
        })
    }

    fn sql_request(&self, sql: &str) -> Result<ExecuteSqlRequest> {
        let mut sqlr = ExecuteSqlRequest::default();
        sqlr.sql = Some(sql.to_owned());
        sqlr.transaction = self.get_transaction()?;
        let mut session = self.session.borrow_mut();
        // XXX: include seqno if no transaction?
        sqlr.seqno = Some(session.execute_sql_count.to_string());
        session.execute_sql_count += 1;
        Ok(sqlr)
    }

    fn sql(&self, sql: &str) -> Result<ExecuteSqlRequestBuilder> {
        Ok(ExecuteSqlRequestBuilder::new(self.sql_request(sql)?))
    }

    pub fn commit_sync(&self) -> Result<()> {
        let spanner = &self.conn;

        if cfg!(any(test, feature = "db_test")) && spanner.use_test_transactions {
            // don't commit test transactions
            return Ok(());
        }

        if let Some(transaction) = self.get_transaction()? {
            let session = spanner.session.name.as_ref().unwrap();
            spanner
                .hub
                .projects()
                .instances_databases_sessions_commit(
                    CommitRequest {
                        transaction_id: transaction.id,
                        ..Default::default()
                    },
                    session,
                )
                .doit()?;
            Ok(())
        } else {
            Err(DbError::internal("No transaction to commit"))?
        }
    }

    pub fn rollback_sync(&self) -> Result<()> {
        if let Some(transaction) = self.get_transaction()? {
            let spanner = &self.conn;
            let session = spanner.session.name.as_ref().unwrap();
            spanner
                .hub
                .projects()
                .instances_databases_sessions_rollback(
                    RollbackRequest {
                        transaction_id: transaction.id,
                    },
                    session,
                )
                .doit()?;
            Ok(())
        } else {
            Err(DbError::internal("No transaction to rollback"))?
        }
    }

    pub fn get_collection_timestamp_sync(
        &self,
        params: params::GetCollectionTimestamp,
    ) -> Result<SyncTimestamp> {
        let user_id = params.user_id.legacy_id as u32;
        dbg!("!!QQQ get_collection_timestamp_sync", &params.collection);

        let collection_id = self.get_collection_id(&params.collection)?;
        if let Some(modified) = self
            .session
            .borrow()
            .coll_modified_cache
            .get(&(user_id, collection_id))
        {
            return Ok(*modified);
        }

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT last_modified FROM user_collections WHERE userid=@userid AND collection=@collectionid")?;
        let params = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
        };
        sql.params = Some(params);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let modified = SyncTimestamp::from_rfc3339(&rows[0][0])?;
                    Ok(modified)
                }
                None => Err(DbErrorKind::CollectionNotFound.into()),
            },
            // TODO Return the correct error
            Err(_e) => Err(DbErrorKind::CollectionNotFound.into()),
        }
        */
        let result = self
            .sql("SELECT last_modified FROM user_collections WHERE userid=@userid AND collection=@collectionid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
            })
            .execute(&self.conn)?
            .one_or_none()?
            .ok_or_else(|| DbErrorKind::CollectionNotFound)?;
        let modified = SyncTimestamp::from_rfc3339(&result[0].get_string_value())?;
        Ok(modified)
    }

    pub fn get_collection_timestamps_sync(
        &self,
        user_id: params::GetCollectionTimestamps,
    ) -> Result<results::GetCollectionTimestamps> {
        let user_id = user_id.legacy_id as u32;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request(
            "SELECT collection, last_modified FROM user_collections WHERE userid=@userid",
        )?;
        let params = params! {"userid" => user_id.to_string()};
        sql.params = Some(params);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let mut timestamps = HashMap::new();
                    rows.iter().for_each(|row| {
                        if let Ok(timestamp) = SyncTimestamp::from_rfc3339(&row[1]) {
                            timestamps.insert(row[0].parse::<i32>().unwrap(), timestamp);
                        }
                    });
                    self.map_collection_names(timestamps)
                }
                None => Err(DbErrorKind::CollectionNotFound.into()),
            },
            // TODO Return the correct error
            Err(_e) => Err(DbErrorKind::CollectionNotFound.into()),
        }
         */
        let modifieds = self
            .sql("SELECT collection, last_modified FROM user_collections WHERE userid=@userid")?
            .params(params! {"userid" => user_id.to_string()})
            .execute(&self.conn)?
            .into_iter()
            .map(|row| {
                let collection_id = row[0]
                    .get_string_value()
                    .parse::<i32>()
                    .map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
                let ts = SyncTimestamp::from_rfc3339(&row[1].get_string_value())?;
                Ok((collection_id, ts))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        self.map_collection_names(modifieds)
    }

    fn map_collection_names<T>(&self, by_id: HashMap<i32, T>) -> Result<HashMap<String, T>> {
        let mut names = self.load_collection_names(by_id.keys())?;
        by_id
            .into_iter()
            .map(|(id, value)| {
                names
                    .remove(&id)
                    .map(|name| (name, value))
                    .ok_or_else(|| DbError::internal("load_collection_names get"))
            })
            .collect()
    }

    fn load_collection_names<'a>(
        &self,
        collection_ids: impl Iterator<Item = &'a i32>,
    ) -> Result<HashMap<i32, String>> {
        let mut names = HashMap::new();
        let mut uncached = Vec::new();
        for &id in collection_ids {
            if let Some(name) = self.coll_cache.get_name(id)? {
                names.insert(id, name);
            } else {
                uncached.push(id);
            }
        }

        if !uncached.is_empty() {
            // TODO only select names that are in uncached.
            /*
            let sql = self.sql_request("SELECT collectionid, name FROM collections")?;
            let spanner = &self.conn;
            let session = spanner.session.name.as_ref().unwrap();
            let results = spanner
                .hub
                .projects()
                .instances_databases_sessions_execute_sql(sql, session)
                .doit();
            match results {
                Ok(results) => match results.1.rows {
                    Some(rows) => {
                        rows.iter().for_each(|row| {
                            let id = row[0].parse::<i32>().unwrap();
                            let name = row[1].clone();
                            if uncached.contains(&id) {
                                names.insert(id, name.clone());
                                self.coll_cache.put(id, name.clone()).unwrap();
                            }
                        });
                    }
                    None => return Err(DbErrorKind::CollectionNotFound.into()),
                },
                // TODO Return the correct error
                Err(_e) => return Err(DbErrorKind::CollectionNotFound.into()),
            }
             */
            let result = self
                .sql("SELECT collectionid, name FROM collections")?
                .execute(&self.conn)?;
            for row in result {
                let id = row[0].get_string_value().parse::<i32>().unwrap();
                let name = row[1].get_string_value().to_owned();
                // XXX: shouldn't this always insert?
                if uncached.contains(&id) {
                    names.insert(id, name.clone());
                    self.coll_cache.put(id, name).unwrap();
                }
            }
        }

        Ok(names)
    }

    pub fn get_collection_counts_sync(
        &self,
        user_id: params::GetCollectionCounts,
    ) -> Result<results::GetCollectionCounts> {
        let user_id = user_id.legacy_id as u32;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT collection, COUNT(collection) FROM bso WHERE userid=@userid AND ttl > CURRENT_TIMESTAMP() GROUP BY collection")?;
        let params = params! {"userid" => user_id.to_string()};
        sql.params = Some(params);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let mut counts = HashMap::new();
                    rows.iter().for_each(|row| {
                        counts.insert(row[0].parse::<i32>().unwrap(), row[1].parse().unwrap());
                    });
                    self.map_collection_names(counts)
                }
                None => Err(DbErrorKind::CollectionNotFound.into()),
            },
            // TODO Return the correct error
            Err(_e) => Err(DbErrorKind::CollectionNotFound.into()),
        }
         */
        let counts = self
            .sql("SELECT collection, COUNT(collection) FROM bso WHERE userid=@userid AND ttl > CURRENT_TIMESTAMP() GROUP BY collection")?
            .params(params! {"userid" => user_id.to_string()})
            .execute(&self.conn)?
            .into_iter()
            .map(|row| {
                let collection = row[0].get_string_value().parse::<i32>().map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
                // XXX: i64 enough here?
                let count = row[1].get_string_value().parse::<i64>().map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
                Ok((collection, count))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        self.map_collection_names(counts)
    }

    pub fn get_collection_usage_sync(
        &self,
        user_id: params::GetCollectionUsage,
    ) -> Result<results::GetCollectionUsage> {
        let user_id = user_id.legacy_id as u32;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT collection, SUM(LENGTH(payload)) FROM bso WHERE userid=@userid AND ttl > CURRENT_TIMESTAMP() GROUP BY collection")?;
        let params = params! {"userid" => user_id.to_string()};
        sql.params = Some(params);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let mut usage = HashMap::new();
                    rows.iter().for_each(|row| {
                        usage.insert(row[0].parse::<i32>().unwrap(), row[1].parse().unwrap());
                    });
                    self.map_collection_names(usage)
                }
                None => Err(DbErrorKind::CollectionNotFound.into()),
            },
            // TODO Return the correct error
            Err(_e) => Err(DbErrorKind::CollectionNotFound.into()),
        }
         */
        let usages = self
            .sql("SELECT collection, SUM(LENGTH(payload)) FROM bso WHERE userid=@userid AND ttl > CURRENT_TIMESTAMP() GROUP BY collection")?
            .params(params! {"userid" => user_id.to_string()})
            .execute(&self.conn)?
            .into_iter()
            .map(|row| {
                let collection = row[0].get_string_value().parse::<i32>().map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
                // XXX: i64 enough here?
                let usage = row[1].get_string_value().parse::<i64>().map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
                Ok((collection, usage))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        self.map_collection_names(usages)
    }

    pub fn get_storage_timestamp_sync(
        &self,
        user_id: params::GetStorageTimestamp,
    ) -> Result<SyncTimestamp> {
        let user_id = user_id.legacy_id as u32;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let ts0 = "0001-01-01T00:00:00Z";
        let mut sql = self
            .sql_request(&format!("SELECT COALESCE(MAX(last_modified), TIMESTAMP '{}') FROM user_collections WHERE userid=@userid", ts0))?;
        let params = params! {"userid" => user_id.to_string()};
        sql.params = Some(params);

        let result = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit()?;
        if let Some(rows) = result.1.rows {
            // XXX: detect not max last_modified found via ts0 to workaround
            // google-apis-rs barfing on a null result
            if rows[0][0] == ts0 {
                SyncTimestamp::from_i64(0)
            } else {
                SyncTimestamp::from_rfc3339(&rows[0][0])
            }
        } else {
            SyncTimestamp::from_i64(0)
        }
        */
        let ts0 = "0001-01-01T00:00:00Z";
        let result = self
            .sql(&format!("SELECT COALESCE(MAX(last_modified), TIMESTAMP '{}') FROM user_collections WHERE userid=@userid", ts0))?
            .params(params! {"userid" => user_id.to_string()})
            .execute(&self.conn)?
            .one_or_none()?;
        if let Some(result) = result {
            let val = result[0].get_string_value();
            // XXX: detect not max last_modified found via ts0 to workaround
            // google-apis-rs barfing on a null result
            if val == ts0 {
                SyncTimestamp::from_i64(0)
            } else {
                SyncTimestamp::from_rfc3339(val)
            }
        } else {
            SyncTimestamp::from_i64(0)
        }
    }

    pub fn get_storage_usage_sync(
        &self,
        user_id: params::GetStorageUsage,
    ) -> Result<results::GetStorageUsage> {
        let user_id = user_id.legacy_id as u32;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT SUM(LENGTH(payload)) FROM bso WHERE userid=@userid AND ttl > CURRENT_TIMESTAMP() GROUP BY userid")?;
        let params = params! {"userid" => user_id.to_string()};
        sql.params = Some(params);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let usage = rows[0][0].parse().unwrap();
                    Ok(usage)
                }
                None => Err(DbErrorKind::CollectionNotFound.into()),
            },
            // TODO Return the correct error
            Err(_e) => Err(DbErrorKind::CollectionNotFound.into()),
        }
         */
        let result = self
            .sql("SELECT SUM(LENGTH(payload)) FROM bso WHERE userid=@userid AND ttl > CURRENT_TIMESTAMP() GROUP BY userid")?
            .params(params! {"userid" => user_id.to_string()})
            .execute(&self.conn)?
            .one_or_none()?;
        if let Some(result) = result {
            // XXX: i64?
            let usage = result[0]
                .get_string_value()
                .parse::<i64>()
                .map_err(|e| DbErrorKind::Integrity(e.to_string()))?;
            Ok(usage as u64)
        } else {
            Ok(0)
        }
    }

    pub fn delete_storage_sync(&self, user_id: params::DeleteStorage) -> Result<()> {
        // XXX: should delete from bso table too
        let user_id = user_id.legacy_id as u32;
        self.sql("DELETE FROM user_collections WHERE userid=@userid")?
            .params(params! {"userid" => user_id.to_string()})
            .execute(&self.conn)?;
        Ok(())
    }

    pub fn timestamp(&self) -> SyncTimestamp {
        self.session.borrow().timestamp
    }

    pub fn delete_collection_sync(
        &self,
        params: params::DeleteCollection,
    ) -> Result<results::DeleteCollection> {
        let user_id = params.user_id.legacy_id as u32;
        let collection_id = self.get_collection_id(&params.collection)?;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql =
            self.sql_request("DELETE FROM bso WHERE userid=@userid AND collection=@collectionid")?;
        let sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
        };
        sql.params = Some(sqlparams);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(_results) => {}
            // TODO Return the correct error
            Err(_e) => return Err(DbErrorKind::CollectionNotFound.into()),
        }
         */
        self.sql("DELETE FROM bso WHERE userid=@userid AND collection=@collectionid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
            })
            .execute(&self.conn)?;

        /*
        let mut sql = self.sql_request(
            "DELETE FROM user_collections WHERE userid=@userid AND collection=@collectionid",
        )?;
        let sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
        };
        sql.params = Some(sqlparams);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(_results) => {}
            // TODO Return the correct error
            Err(_e) => return Err(DbErrorKind::CollectionNotFound.into()),
        }
        */
        self.sql("DELETE FROM user_collections WHERE userid=@userid AND collection=@collectionid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
            })
            .execute(&self.conn)?;
        self.get_storage_timestamp_sync(params.user_id)
    }

    pub(super) fn touch_collection(
        &self,
        user_id: u32,
        collection_id: i32,
    ) -> Result<SyncTimestamp> {
        // XXX: We should be able to use Spanner's insert_or_update here (unlike w/ put_bsos)
        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT 1 as count FROM user_collections WHERE userid = @userid AND collection = @collectionid;")?;
        let sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
        };
        sql.params = Some(sqlparams);
        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        let exists = match results {
            Ok(results) => results.1.rows.is_some(),
            _ => return Err(DbErrorKind::CollectionNotFound.into()),
        };
         */
        let result = self
            .sql("SELECT 1 as count FROM user_collections WHERE userid = @userid AND collection = @collectionid;")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
            })
            .execute(&self.conn)?
            .one_or_none()?;
        let exists = result.is_some();

        if exists {
            /*
            let mut sql = self.sql_request("UPDATE user_collections SET last_modified=@last_modified WHERE userid=@userid AND collection=@collectionid")?;
            let timestamp = self.timestamp().as_i64();
            let modifiedstring = to_rfc3339(timestamp)?;
            let sqlparams = params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "last_modified" => modifiedstring,
            };
            let sqltypes = param_types! {
                "last_modified" => SpannerType::Timestamp,
            };

            sql.params = Some(sqlparams);
            sql.param_types = Some(sqltypes);

            spanner
                .hub
                .projects()
                .instances_databases_sessions_execute_sql(sql, session)
                .doit()?;
             */
            let timestamp = self.timestamp().as_i64();
            self
                .sql("UPDATE user_collections SET last_modified=@last_modified WHERE userid=@userid AND collection=@collectionid")?
                .params(params! {
                    "userid" => user_id.to_string(),
                    "collectionid" => collection_id.to_string(),
                    "last_modified" => to_rfc3339(timestamp)?,
                })
                .param_types(param_types! {
                    "last_modified" => SpannerType::Timestamp,
                })
                .execute(&self.conn)?;
            Ok(self.timestamp())
        } else {
            /*
            let mut sql = self.sql_request("INSERT INTO user_collections (userid, collection, last_modified) VALUES (@userid, @collectionid, @modified)")?;
            let timestamp = self.timestamp().as_i64();
            let modifiedstring = to_rfc3339(timestamp)?;
            let sqlparams = params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "modified" => modifiedstring,
            };
            let sqltypes = param_types! {
                "modified" => SpannerType::Timestamp,
            };
            sql.params = Some(sqlparams);
            sql.param_types = Some(sqltypes);

            spanner
                .hub
                .projects()
                .instances_databases_sessions_execute_sql(sql, session)
                .doit()?;
            */
            let timestamp = self.timestamp().as_i64();
            self
                .sql("INSERT INTO user_collections (userid, collection, last_modified) VALUES (@userid, @collectionid, @modified)")?
                .params(params! {
                    "userid" => user_id.to_string(),
                    "collectionid" => collection_id.to_string(),
                    "modified" => to_rfc3339(timestamp)?,
                })
                .param_types(param_types! {
                    "modified" => SpannerType::Timestamp,
                })
                .execute(&self.conn)?;
            Ok(self.timestamp())
        }
    }

    pub fn delete_bso_sync(&self, params: params::DeleteBso) -> Result<results::DeleteBso> {
        let user_id = params.user_id.legacy_id as u32;
        let collection_id = self.get_collection_id(&params.collection)?;
        let touch = self.touch_collection(user_id as u32, collection_id)?;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request(
            "DELETE FROM bso WHERE userid=@userid AND collection=@collectionid AND id=@bsoid",
        )?;
        let sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
            "bsoid" => params.id.to_string(),
        };
        sql.params = Some(sqlparams);
        */

        //fn affected_rows(result_set: &ResultSet) -> Result<i64> {
        fn affected_rows(result_set: &SyncResultSet) -> Result<i64> {
            let stats = result_set
                .stats()
                //                .as_ref()
                .ok_or_else(|| DbError::internal("Expected result_set stats"))?;
            let row_count_exact = stats
                .row_count_exact
                .as_ref()
                .ok_or_else(|| DbError::internal("Expected result_set stats row_count_exact"))?;
            Ok(row_count_exact.parse().map_err(|e| {
                DbError::internal(&format!("Invalid row_count_exact i64 value {}", e))
            })?)
        }

        /*
        let result = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit()?;
        // XXX: requires stats
        */
        let result = self
            .sql("DELETE FROM bso WHERE userid=@userid AND collection=@collectionid AND id=@bsoid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "bsoid" => params.id.to_string(),
            })
            .execute(&self.conn)?;
        //if affected_rows(&result.1)? == 0 {
        if affected_rows(&result)? == 0 {
            Err(DbErrorKind::BsoNotFound)?
        } else {
            Ok(touch)
        }
    }

    pub fn delete_bsos_sync(&self, params: params::DeleteBsos) -> Result<results::DeleteBsos> {
        let user_id = params.user_id.legacy_id as u32;
        let collection_id = self.get_collection_id(&params.collection)?;

        let mut deleted = 0;
        // TODO figure out how spanner specifies an "IN" query
        for id in params.ids {
            /*
            let spanner = &self.conn;
            let session = spanner.session.name.as_ref().unwrap();
            let mut sql = self.sql_request(
                "SELECT 1 FROM bso WHERE userid=@userid AND collection=@collectionid AND id=@bsoid",
            )?;
            let sqlparams = params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "bsoid" => id.to_string(),
            };
            sql.params = Some(sqlparams);

            spanner
                .hub
                .projects()
                .instances_databases_sessions_execute_sql(sql, session)
                .doit()?;
             */
            self.sql(
                "SELECT 1 FROM bso WHERE userid=@userid AND collection=@collectionid AND id=@bsoid",
            )?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "bsoid" => id.to_string(),
            })
            .execute(&self.conn)?;
            deleted += 1;
            self.delete_bso_sync(params::DeleteBso {
                user_id: params.user_id.clone(),
                collection: params.collection.clone(),
                id: id.to_string(),
            })?;
        }
        if deleted > 0 {
            self.touch_collection(user_id, collection_id)
        } else {
            Err(DbErrorKind::BsoNotFound.into())
        }
    }

    pub fn get_bsos_sync(&self, params: params::GetBsos) -> Result<results::GetBsos> {
        let user_id = params.user_id.legacy_id as i32;
        let collection_id = self.get_collection_id(&params.collection)?;
        let BsoQueryParams {
            newer,
            older,
            sort,
            limit,
            offset,
            ids,
            ..
        } = params.params;

        let mut query = "SELECT id, modified, payload, COALESCE(sortindex, 0), ttl FROM bso WHERE userid = @userid AND collection = @collectionid AND ttl > @timestamp".to_string();
        let timestamp = self.timestamp().as_i64();
        let modifiedstring = to_rfc3339(timestamp)?;
        let mut sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
            "timestamp" => modifiedstring,
        };
        let mut sqltypes = param_types! {
            "timestamp" => SpannerType::Timestamp,
        };

        if let Some(older) = older {
            query = format!("{} AND modified < @older", query).to_string();
            sqlparams.insert(
                "older".to_string(),
                as_value(to_rfc3339(older.as_i64()).unwrap()),
            );
            sqltypes.insert("older".to_string(), SpannerType::Timestamp.into());
        }
        if let Some(newer) = newer {
            query = format!("{} AND modified > @newer", query).to_string();
            sqlparams.insert(
                "newer".to_string(),
                as_value(to_rfc3339(newer.as_i64()).unwrap()),
            );
            sqltypes.insert("newer".to_string(), SpannerType::Timestamp.into());
        }

        let idlen = ids.len();
        if !ids.is_empty() {
            // TODO use UNNEST and pass a vec later
            let mut i = 0;
            query = format!("{} AND id IN (", query).to_string();
            while i < idlen {
                if i == 0 {
                    query = format!("{}@arg{}", query, i.to_string()).to_string();
                } else {
                    query = format!("{}, @arg{}", query, i.to_string()).to_string();
                }
                sqlparams.insert(
                    format!("arg{}", i.to_string()).to_string(),
                    as_value(ids[i].to_string()),
                );
                i += 1;
            }
            query = format!("{})", query).to_string();
        }

        query = match sort {
            Sorting::Index => format!("{} ORDER BY sortindex DESC", query).to_string(),
            Sorting::Newest => format!("{} ORDER BY modified DESC", query).to_string(),
            Sorting::Oldest => format!("{} ORDER BY modified ASC", query).to_string(),
            _ => query,
        };

        let limit = limit.map(i64::from).unwrap_or(-1);
        // fetch an extra row to detect if there are more rows that
        // match the query conditions

        //query = query.limit(if limit >= 0 { limit + 1 } else { limit });
        if limit >= 0 {
            query = format!("{} LIMIT {}", query, limit + 1).to_string();
        } else {
            query = format!("{} LIMIT {}", query, limit).to_string();
        }

        let offset = offset.unwrap_or(0) as i64;
        if offset != 0 {
            // XXX: copy over this optimization:
            // https://github.com/mozilla-services/server-syncstorage/blob/a0f8117/syncstorage/storage/sql/__init__.py#L404
            query = format!("{} OFFSET {}", query, offset).to_string();
        }

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request(&query)?;
        sql.params = Some(sqlparams);
        sql.param_types = Some(sqltypes);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        dbg!("!!RESULTS", &results);
        let mut bsos = match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let mut vec = Vec::new();
                    rows.iter().for_each(|row| {
                        vec.push(results::GetBso {
                            id: row[0].parse().unwrap(),
                            modified: SyncTimestamp::from_rfc3339(&row[1]).unwrap(),
                            payload: row[2].parse().unwrap(),
                            sortindex: Some(row[3].parse().unwrap()),
                            expiry: SyncTimestamp::from_rfc3339(&row[4]).unwrap().as_i64(),
                        });
                    });
                    vec
                }
                None => Vec::new(),
            },
            // TODO Return the correct error
            Err(_e) => Vec::new(),
        };
         */
        let result = self
            .sql(&query)?
            .params(sqlparams)
            .param_types(sqltypes)
            .execute(&self.conn)?;
        let mut bsos = vec![];
        for row in result {
            bsos.push(results::GetBso {
                id: row[0].get_string_value().parse().unwrap(),
                modified: SyncTimestamp::from_rfc3339(&row[1].get_string_value()).unwrap(),
                payload: row[2].get_string_value().parse().unwrap(),
                sortindex: Some(row[3].get_string_value().parse().unwrap()),
                expiry: SyncTimestamp::from_rfc3339(&row[4].get_string_value())
                    .unwrap()
                    .as_i64(),
            });
        }

        // XXX: an additional get_collection_timestamp is done here in
        // python to trigger potential CollectionNotFoundErrors
        //if bsos.len() == 0 {
        //}

        let next_offset = if limit >= 0 && bsos.len() > limit as usize {
            bsos.pop();
            Some(limit + offset)
        } else {
            None
        };

        Ok(results::GetBsos {
            items: bsos,
            offset: next_offset,
        })
    }

    pub fn get_bso_ids_sync(&self, params: params::GetBsos) -> Result<results::GetBsoIds> {
        // XXX: should be a more efficient select of only the id column
        let result = self.get_bsos_sync(params)?;
        Ok(results::GetBsoIds {
            items: result.items.into_iter().map(|bso| bso.id).collect(),
            offset: result.offset,
        })
    }

    pub fn get_bso_sync(&self, params: params::GetBso) -> Result<Option<results::GetBso>> {
        let user_id = params.user_id.legacy_id;
        let collection_id = self.get_collection_id(&params.collection)?;

        let result = self.
            sql("SELECT id, modified, payload, coalesce(sortindex, 0), ttl FROM bso WHERE userid=@userid AND collection=@collectionid AND id=@bsoid AND ttl > @timestamp")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "bsoid" => params.id.to_string(),
                "timestamp" => to_rfc3339(self.timestamp().as_i64())?
            })
            .param_types(param_types! {
                "timestamp" => SpannerType::Timestamp,
            })
            .execute(&self.conn)?
        .one_or_none()?;
        dbg!("RRRRRRRRRR", &result);
        // XXX: map() now
        //Ok(if let Some(rows) = result.1.rows {
        Ok(if let Some(row) = result {
            let modified = SyncTimestamp::from_rfc3339(&row[1].get_string_value())?;
            let expiry_dt =
                DateTime::parse_from_rfc3339(&row[4].get_string_value()).map_err(|e| {
                    DbErrorKind::Integrity(format!("Invalid TIMESTAMP {}", e.to_string()))
                })?;
            // XXX: expiry is i64?
            let expiry = expiry_dt.timestamp_millis();
            dbg!(
                "!!!! GET expiry",
                &row[4].get_string_value(),
                expiry_dt,
                expiry
            );
            Some(results::GetBso {
                id: row[0].get_string_value().to_owned(),
                modified,
                payload: row[2].get_string_value().to_owned(),
                sortindex: Some(row[3].get_string_value().parse().unwrap()),
                expiry,
            })
        } else {
            None
        })
    }

    pub fn get_bso_timestamp_sync(&self, params: params::GetBsoTimestamp) -> Result<SyncTimestamp> {
        let user_id = params.user_id.legacy_id as u32;
        dbg!("!!QQQ get_bso_timestamp_sync", &params.collection);
        let collection_id = self.get_collection_id(&params.collection)?;

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT modified FROM bso WHERE collection=@collectionid AND userid=@userid AND id=@bsoid AND ttl>@ttl")?;
        let timestamp = self.timestamp().as_i64();
        let expirystring = to_rfc3339(timestamp)?;

        let sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
            "bsoid" => params.id.to_string(),
            "ttl" => expirystring,
        };
        let sqltypes = param_types! {
            "ttl" => SpannerType::Timestamp,
        };
        sql.params = Some(sqlparams);
        sql.param_types = Some(sqltypes);

        let results = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match results {
            Ok(results) => match results.1.rows {
                Some(rows) => {
                    let modified = SyncTimestamp::from_rfc3339(&rows[0][0])?;
                    Ok(modified)
                }
                None => Err(DbErrorKind::CollectionNotFound.into()),
            },
            // TODO Return the correct error
            Err(_e) => Err(DbErrorKind::CollectionNotFound.into()),
        }
         */
        let result = self
            .sql("SELECT modified FROM bso WHERE collection=@collectionid AND userid=@userid AND id=@bsoid AND ttl>@ttl")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "bsoid" => params.id.to_string(),
                "ttl" => to_rfc3339(self.timestamp().as_i64())?,
            })
            .param_types(param_types! {
                "ttl" => SpannerType::Timestamp,
            })
            .execute(&self.conn)?
            .one_or_none()?;
        if let Some(result) = result {
            SyncTimestamp::from_rfc3339(&result[0].get_string_value())
        } else {
            SyncTimestamp::from_i64(0)
        }
    }

    pub fn put_bso_sync(&self, bso: params::PutBso) -> Result<results::PutBso> {
        let collection_id = self.get_or_create_collection_id(&bso.collection)?;
        let user_id: u64 = bso.user_id.legacy_id;
        let touch = self.touch_collection(user_id as u32, collection_id)?;
        let timestamp = self.timestamp().as_i64();

        /*
        let spanner = &self.conn;
        let session = spanner.session.name.as_ref().unwrap();
        let mut sql = self.sql_request("SELECT 1 as count FROM bso WHERE userid = @userid AND collection = @collectionid AND id = @bsoid")?;
        let sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
            "bsoid" => bso.id.to_string(),
        };
        sql.params = Some(sqlparams);
        #[derive(Default)]
        pub struct Dlg;

        impl google_spanner1::Delegate for Dlg {
            fn http_failure(
                &mut self,
                _: &hyper::client::Response,
                a: Option<google_spanner1::JsonServerError>,
                b: Option<google_spanner1::ServerError>,
            ) -> yup_oauth2::Retry {
                if let Some(a) = a {
                    dbg!("DDDDDDDDDDDDDDDDDDDD1", a.error, a.error_description);
                }
                dbg!("DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD", b);
                yup_oauth2::Retry::Abort
            }
        }
        let result = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .delegate(&mut Dlg {})
            .doit()?;
        // XXX: should we rows.len() == 1?
        let exists = result.1.rows.is_some();
         */
        let result = self
            .sql("SELECT 1 as count FROM bso WHERE userid = @userid AND collection = @collectionid AND id = @bsoid")?
            .params(params! {
                "userid" => user_id.to_string(),
                "collectionid" => collection_id.to_string(),
                "bsoid" => bso.id.to_string(),
            })
            .execute(&self.conn)?
            .one_or_none()?;
        let exists = result.is_some();

        let mut sqlparams = params! {
            "userid" => user_id.to_string(),
            "collectionid" => collection_id.to_string(),
            "bsoid" => bso.id.to_string(),
        };
        let mut sqltypes = HashMap::new();

        let sql = if exists {
            // XXX: the "ttl" column is more aptly named "expiry": our mysql
            // schema names it this. the current spanner schema prefers "ttl"
            // to more closely match the python code

            //let mut sqltypes = HashMap::new();

            let mut q = "".to_string();
            let comma = |q: &String| if q.is_empty() { "" } else { ", " };

            q = format!(
                "{}{}",
                q,
                if let Some(sortindex) = bso.sortindex {
                    sqlparams.insert("sortindex".to_string(), as_value(sortindex.to_string()));
                    sqltypes.insert("sortindex".to_string(), SpannerType::Int64.into());

                    format!("{}{}", comma(&q), "sortindex = @sortindex")
                } else {
                    "".to_string()
                }
            )
            .to_string();
            q = format!(
                "{}{}",
                q,
                if let Some(ttl) = bso.ttl {
                    let expiry = timestamp + (i64::from(ttl) * 1000);
                    sqlparams.insert("expiry".to_string(), as_value(to_rfc3339(expiry)?));
                    sqltypes.insert("expiry".to_string(), SpannerType::Timestamp.into());
                    format!("{}{}", comma(&q), "ttl = @expiry")
                } else {
                    "".to_string()
                }
            )
            .to_string();
            q = format!(
                "{}{}",
                q,
                if bso.payload.is_some() || bso.sortindex.is_some() {
                    sqlparams.insert(
                        "modified".to_string(),
                        as_value(self.timestamp().as_rfc3339()?),
                    );
                    sqltypes.insert("modified".to_string(), SpannerType::Timestamp.into());
                    format!("{}{}", comma(&q), "modified = @modified")
                } else {
                    "".to_string()
                }
            )
            .to_string();
            q = format!(
                "{}{}",
                q,
                if let Some(payload) = bso.payload {
                    sqlparams.insert("payload".to_string(), as_value(payload));
                    format!("{}{}", comma(&q), "payload = @payload")
                } else {
                    "".to_string()
                }
            )
            .to_string();

            if q.is_empty() {
                // Nothing to update
                return Ok(touch);
            }

            q = format!(
                "UPDATE bso SET {}{}",
                q, " WHERE userid = @userid AND collection = @collectionid AND id = @bsoid"
            );

            /*
            let mut sql = self.sql_request(&q)?;
            sql.params = Some(sqlparams);
            sql.param_types = Some(sqltypes);
            sql
             */
            q
        } else {
            let use_sortindex = bso
                .sortindex
                .map(|sortindex| sortindex.to_string())
                .unwrap_or_else(|| "NULL".to_owned())
                != "NULL";
            /*
            let mut sql = if use_sortindex {
                self.sql_request("INSERT INTO bso (userid, collection, id, sortindex, payload, modified, ttl) VALUES (@userid, @collectionid, @bsoid, @sortindex, @payload, @modified, @expiry)")?
            } else {
                self.sql_request("INSERT INTO bso (userid, collection, id, payload, modified, ttl) VALUES (@userid, @collectionid, @bsoid,  @payload, @modified, @expiry)")?
            };
            */
            let sql = if use_sortindex {
                "INSERT INTO bso (userid, collection, id, sortindex, payload, modified, ttl) VALUES (@userid, @collectionid, @bsoid, @sortindex, @payload, @modified, @expiry)"
            } else {
                "INSERT INTO bso (userid, collection, id, payload, modified, ttl) VALUES (@userid, @collectionid, @bsoid,  @payload, @modified, @expiry)"
            };

            if use_sortindex {
                // XXX: special handling for google_grpc (null)
                sqlparams.insert(
                    "sortindex".to_string(),
                    bso.sortindex
                        .map(|sortindex| sortindex.to_string())
                        .unwrap_or_else(|| "NULL".to_owned()),
                );
                sqltypes.insert("sortindex".to_string(), SpannerType::Int64.into());
            }
            sqlparams.insert(
                "payload".to_string(),
                as_value(bso.payload.unwrap_or_else(|| "DEFAULT".to_owned())),
            );
            let now_millis = self.timestamp().as_i64();
            let ttl = bso
                .ttl
                .map_or(DEFAULT_BSO_TTL, |ttl| ttl.try_into().unwrap())
                * 1000;
            let expirystring = to_rfc3339(now_millis + ttl)?;
            dbg!("!!!!! INSERT", &expirystring, timestamp, ttl);
            sqlparams.insert("expiry".to_string(), as_value(expirystring));
            sqltypes.insert("expiry".to_string(), SpannerType::Timestamp.into());

            sqlparams.insert(
                "modified".to_string(),
                as_value(self.timestamp().as_rfc3339()?),
            );
            sqltypes.insert("modified".to_string(), SpannerType::Timestamp.into());
            /*
            sql.params = Some(sqlparams);
            sql.param_types = Some(sqltypes);
            */
            sql.to_owned()
        };

        /*
        let result = spanner
            .hub
            .projects()
            .instances_databases_sessions_execute_sql(sql, session)
            .doit();
        match result {
            Ok(_) => {
                dbg!("OK!!!");
            }
            Err(e) => {
                dbg!("ERR!!!", &e);
                Err(e)?;
            }
        }
         */
        self.sql(&sql)?
            .params(sqlparams)
            .param_types(sqltypes)
            .execute(&self.conn)?;
        Ok(touch)
    }

    pub fn post_bsos_sync(&self, input: params::PostBsos) -> Result<results::PostBsos> {
        let collection_id = self.get_or_create_collection_id(&input.collection)?;
        let mut result = results::PostBsos {
            modified: self.timestamp(),
            success: Default::default(),
            failed: input.failed,
        };

        for pbso in input.bsos {
            let id = pbso.id;
            let put_result = self.put_bso_sync(params::PutBso {
                user_id: input.user_id.clone(),
                collection: input.collection.clone(),
                id: id.clone(),
                payload: pbso.payload,
                sortindex: pbso.sortindex,
                ttl: pbso.ttl,
            });
            // XXX: python version doesn't report failures from db layer..
            // XXX: sanitize to.to_string()?
            match put_result {
                Ok(_) => result.success.push(id),
                Err(e) => {
                    result.failed.insert(id, e.to_string());
                }
            }
        }
        self.touch_collection(input.user_id.legacy_id as u32, collection_id)?;
        Ok(result)
    }

    batch_db_method!(create_batch_sync, create, CreateBatch);
    batch_db_method!(validate_batch_sync, validate, ValidateBatch);
    batch_db_method!(append_to_batch_sync, append, AppendToBatch);
    batch_db_method!(commit_batch_sync, commit, CommitBatch);

    pub fn get_batch_sync(&self, params: params::GetBatch) -> Result<Option<results::GetBatch>> {
        batch::get(&self, params)
    }
}

unsafe impl Send for SpannerDb {}

macro_rules! sync_db_method {
    ($name:ident, $sync_name:ident, $type:ident) => {
        sync_db_method!($name, $sync_name, $type, results::$type);
    };
    ($name:ident, $sync_name:ident, $type:ident, $result:ty) => {
        fn $name(&self, params: params::$type) -> DbFuture<$result> {
            let db = self.clone();
            Box::new(self.thread_pool.spawn_handle(lazy(move || {
                future::result(db.$sync_name(params).map_err(Into::into))
            })))
        }
    };
}

impl Db for SpannerDb {
    fn commit(&self) -> DbFuture<()> {
        let db = self.clone();
        Box::new(self.thread_pool.spawn_handle(lazy(move || {
            future::result(db.commit_sync().map_err(Into::into))
        })))
    }

    fn rollback(&self) -> DbFuture<()> {
        let db = self.clone();
        Box::new(self.thread_pool.spawn_handle(lazy(move || {
            future::result(db.rollback_sync().map_err(Into::into))
        })))
    }

    fn box_clone(&self) -> Box<dyn Db> {
        Box::new(self.clone())
    }

    sync_db_method!(lock_for_read, lock_for_read_sync, LockCollection);
    sync_db_method!(lock_for_write, lock_for_write_sync, LockCollection);
    sync_db_method!(
        get_collection_timestamp,
        get_collection_timestamp_sync,
        GetCollectionTimestamp
    );
    sync_db_method!(
        get_collection_timestamps,
        get_collection_timestamps_sync,
        GetCollectionTimestamps
    );
    sync_db_method!(
        get_collection_counts,
        get_collection_counts_sync,
        GetCollectionCounts
    );
    sync_db_method!(
        get_collection_usage,
        get_collection_usage_sync,
        GetCollectionUsage
    );
    sync_db_method!(
        get_storage_timestamp,
        get_storage_timestamp_sync,
        GetStorageTimestamp
    );
    sync_db_method!(get_storage_usage, get_storage_usage_sync, GetStorageUsage);
    sync_db_method!(delete_storage, delete_storage_sync, DeleteStorage);
    sync_db_method!(delete_collection, delete_collection_sync, DeleteCollection);
    sync_db_method!(delete_bso, delete_bso_sync, DeleteBso);
    sync_db_method!(delete_bsos, delete_bsos_sync, DeleteBsos);
    sync_db_method!(get_bsos, get_bsos_sync, GetBsos);
    sync_db_method!(get_bso_ids, get_bso_ids_sync, GetBsoIds);
    sync_db_method!(get_bso, get_bso_sync, GetBso, Option<results::GetBso>);
    sync_db_method!(
        get_bso_timestamp,
        get_bso_timestamp_sync,
        GetBsoTimestamp,
        results::GetBsoTimestamp
    );
    sync_db_method!(put_bso, put_bso_sync, PutBso);
    sync_db_method!(post_bsos, post_bsos_sync, PostBsos);
    sync_db_method!(create_batch, create_batch_sync, CreateBatch);
    sync_db_method!(validate_batch, validate_batch_sync, ValidateBatch);
    sync_db_method!(append_to_batch, append_to_batch_sync, AppendToBatch);
    sync_db_method!(
        get_batch,
        get_batch_sync,
        GetBatch,
        Option<results::GetBatch>
    );
    sync_db_method!(commit_batch, commit_batch_sync, CommitBatch);

    #[cfg(any(test, feature = "db_test"))]
    fn get_collection_id(&self, name: String) -> DbFuture<i32> {
        let db = self.clone();
        Box::new(self.thread_pool.spawn_handle(lazy(move || {
            future::result(db.get_collection_id(&name).map_err(Into::into))
        })))
    }

    #[cfg(any(test, feature = "db_test"))]
    fn create_collection(&self, name: String) -> DbFuture<i32> {
        let db = self.clone();
        Box::new(self.thread_pool.spawn_handle(lazy(move || {
            future::result(db.create_collection(&name).map_err(Into::into))
        })))
    }

    #[cfg(any(test, feature = "db_test"))]
    fn touch_collection(&self, param: params::TouchCollection) -> DbFuture<SyncTimestamp> {
        let db = self.clone();
        Box::new(self.thread_pool.spawn_handle(lazy(move || {
            future::result(
                db.touch_collection(param.user_id.legacy_id as u32, param.collection_id)
                    .map_err(Into::into),
            )
        })))
    }

    #[cfg(any(test, feature = "db_test"))]
    fn timestamp(&self) -> SyncTimestamp {
        self.timestamp()
    }

    #[cfg(any(test, feature = "db_test"))]
    fn set_timestamp(&self, timestamp: SyncTimestamp) {
        self.session.borrow_mut().timestamp = timestamp;
    }
}