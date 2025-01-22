use super::DbConn;
use super::MssqlParam;
use crate::errors::Result;
use crate::Client;
use crate::ExecuteResult;
use crate::Param;
use crate::Row;
use async_trait::async_trait;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use tiberius::ToSql;
use tokio::sync::oneshot::Sender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Rolledback,
    Commited,
}

pub(crate) struct MssqlTransaction<'t> {
    done: Option<Sender<bool>>,
    conn: Arc<Mutex<Option<DbConn>>>,
    state: State,
    _phantom: PhantomData<&'t ()>,
    pub(crate) trans_name: String,
}

impl<'t> MssqlTransaction<'t> {
    pub async fn new(done: Sender<bool>, conn: Arc<Mutex<Option<DbConn>>>) -> Result<Self> {
        let this = Self {
            done: Some(done),
            conn,
            state: State::Open,
            _phantom: Default::default(),
            trans_name: format!("t_{}", get_trans_count()),
        };

        let mut conn = this.take_conn();
        let sql = format!("BEGIN TRANSACTION {}", this.trans_name);
        conn.simple_query(sql).await?;
        this.return_conn(conn);

        Ok(this)
    }

    pub async fn commit(mut self) -> Result<()> {
        assert_eq!(self.state, State::Open);
        self.state = State::Commited;
        let mut conn = self.take_conn();
        let sql = format!("COMMIT TRANSACTION {}", self.trans_name);
        conn.simple_query(sql).await?;
        self.return_conn(conn);
        Ok(())
    }

    pub async fn rollback(mut self) -> Result<()> {
        if self.state == State::Rolledback {
            return Ok(());
        }
        assert_eq!(self.state, State::Open);
        self.state = State::Rolledback;
        let mut conn = self.take_conn();
        let sql = format!("ROLLBACK TRANSACTION {}", self.trans_name);
        let _ = conn.simple_query(sql).await;
        self.return_conn(conn);
        Ok(())
    }

    pub(crate) async fn rollback_internal(&mut self) -> Result<()> {
        if self.state == State::Rolledback {
            return Ok(());
        }
        assert_eq!(self.state, State::Open);
        self.state = State::Rolledback;
        let mut conn = self.take_conn();
        let sql = format!("ROLLBACK TRANSACTION {}", self.trans_name);
        let _ = conn.simple_query(sql).await;
        self.return_conn(conn);
        Ok(())
    }
}

impl<'t> MssqlTransaction<'t> {
    // HACK - CODE SMELL:
    // we need a &mut conn for the connection pool
    // this (take_conn/return_conn) acts like a CellRef
    // It will panic if you try to the conn more one at at time
    //
    fn take_conn(&self) -> DbConn {
        let mut placeholder = None;
        let mut m = self.conn.lock().unwrap();
        let inner: &mut Option<_> = &mut m;
        // Panic if the conn is already taken
        assert!(inner.is_some(), "Pool was already taken");
        std::mem::swap(&mut placeholder, inner);
        placeholder.unwrap()
    }
    fn return_conn(&self, conn: DbConn) {
        let mut placeholder = Some(conn);
        let mut m = self.conn.lock().unwrap();
        let inner: &mut Option<_> = &mut m;
        // Panic if we already have a the conn
        assert!(inner.is_none(), "Overriding existing pool");
        std::mem::swap(&mut placeholder, inner);
    }
}

#[async_trait]
impl<'t> Client for MssqlTransaction<'t> {
    async fn execute(&self, sql: &str, params: &[&(dyn Param + Sync + Send)]) -> Result<ExecuteResult> {
        assert_eq!(self.state, State::Open);
        let mut conn = self.take_conn();
        let mut args: Vec<&dyn ToSql> = Vec::new();
        for &p in params {
            args = MssqlParam::add_param(p, args);
        }
        log::debug!("MSSQL_TRANS_EXEC: {}", sql);
        let r = conn.execute(sql, &args).await;
        self.return_conn(conn);
        let r = r?;

        Ok(ExecuteResult {
            rows_affected: r.rows_affected().iter().sum(),
        })
    }

    async fn fetch_rows(&self, sql: &str, params: &[&(dyn Param + Sync + Send)]) -> Result<Vec<Row>> {
        assert_eq!(self.state, State::Open);
        let mut conn = self.take_conn();
        let results = fetch_rows_inner(&mut conn, sql, params).await;
        self.return_conn(conn);
        let rows = results?;
        Ok(rows)
    }

    async fn fetch_many<'s, 'args, 'i>(
        &self,
        fetches: &[crate::Fetch<'s, 'args, 'i>],
    ) -> Result<Vec<Vec<Row>>> {
        assert_eq!(self.state, State::Open);
        let mut conn = self.take_conn();
        let mut results = Vec::default();
        for fetch in fetches {
            let sql = fetch.sql;
            let params = fetch.params;
            let r = fetch_rows_inner(&mut conn, sql, params).await;
            let is_err = r.is_err();
            results.push(r);
            if is_err {
                break;
            }
        }
        self.return_conn(conn);
        results.drain(..).collect()
    }

    fn syntax(&self) -> crate::Syntax {
        crate::Syntax::Mssql
    }
}

async fn fetch_rows_inner<'t>(
    conn: &mut DbConn,
    sql: &str,
    params: &[&(dyn Param + Sync + Send)],
) -> Result<Vec<Row>> {
    let mut args: Vec<&dyn ToSql> = Vec::new();
    for &p in params {
        args = MssqlParam::add_param(p, args);
    }
    log::debug!("MSSQL_TRANS_QUERY: {}", sql);

    let stream = conn.query(sql, &args).await;
    let stream = stream?;

    let mssql_rows = stream.into_results().await?;
    let mut all = Vec::default();
    for batch in mssql_rows {
        for r in batch {
            all.push(Row::from(r))
        }
    }
    Ok(all)
}

impl<'t> Drop for MssqlTransaction<'t> {
    fn drop(&mut self) {
        let mut done = None;
        std::mem::swap(&mut done, &mut self.done);
        let done = done.unwrap();

        if self.state != State::Open {
            done.send(false).unwrap();
            return;
        }

        done.send(true).unwrap();

        //// Last resort, Make sure the transaction is rolled back if just dropped
        //futures::executor::block_on(async {
        //    log::warn!("WARNING: transaction was dropped without a commit or rollback. auto-rollback of transaction occurred",);
        //    let mut conn = self.take_conn();
        //    conn.simple_query("ROLLBACK").await.unwrap();
        //})
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

fn get_trans_count() -> usize {
    CALL_COUNT.fetch_add(1, Ordering::SeqCst) + 1
}
