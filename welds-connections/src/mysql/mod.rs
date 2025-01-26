use super::transaction::{TransT, Transaction};
use super::Row;
use super::TransactStart;
use super::{Client, Param};
use crate::errors::Result;
use crate::ExecuteResult;
use async_trait::async_trait;
use sqlx::mysql::{MySqlArguments, MySqlPoolOptions};
use sqlx::query::Query;
use sqlx::{MySql, MySqlPool};
use std::sync::Arc;
use std::time::Duration;

pub struct MysqlClient {
    pool: Arc<MySqlPool>,
}

#[async_trait]
impl TransactStart for MysqlClient {
    async fn begin(&self) -> Result<Transaction> {
        let t = self.pool.begin().await?;
        let t = TransT::Mysql(t);
        Ok(Transaction::new(t))
    }
}

pub async fn connect(url: &str, timeout: Option<Duration>) -> Result<MysqlClient> {
    let mut pool = MySqlPoolOptions::new();
    if let Some(timeout) = timeout {
        pool = pool.acquire_timeout(timeout);
    }
    let pool = pool.connect(url).await?;
    Ok(MysqlClient {
        pool: Arc::new(pool),
    })
}

impl From<sqlx::MySqlPool> for MysqlClient {
    fn from(pool: sqlx::MySqlPool) -> MysqlClient {
        MysqlClient {
            pool: Arc::new(pool),
        }
    }
}

impl MysqlClient {
    pub fn as_sqlx_pool(&self) -> &MySqlPool {
        &self.pool
    }
}

use sqlx::encode::Encode;
use sqlx::types::Type;

#[async_trait]
impl Client for MysqlClient {
    async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn Param + Sync + Send)],
    ) -> Result<ExecuteResult> {
        let mut query = sqlx::query::<MySql>(sql);
        for param in params {
            query = MysqlParam::add_param(*param, query);
        }
        let r = query.execute(&*self.pool).await?;
        Ok(ExecuteResult {
            rows_affected: r.rows_affected(),
        })
    }

    async fn fetch_rows(
        &self,
        sql: &str,
        params: &[&(dyn Param + Sync + Send)],
    ) -> Result<Vec<Row>> {
        let mut query = sqlx::query::<MySql>(sql);
        for param in params {
            query = MysqlParam::add_param(*param, query);
        }
        let mut raw_rows = query.fetch_all(&*self.pool).await?;
        let rows: Vec<Row> = raw_rows.drain(..).map(Row::from).collect();
        Ok(rows)
    }

    async fn fetch_many<'s, 'args, 't>(
        &self,
        fetches: &[crate::Fetch<'s, 'args, 't>],
    ) -> Result<Vec<Vec<Row>>> {
        let mut datasets = Vec::default();
        let mut conn = self.pool.acquire().await?;
        for fetch in fetches {
            let sql = fetch.sql;
            let params = fetch.params;
            let mut query = sqlx::query::<MySql>(sql);
            for param in params {
                query = MysqlParam::add_param(*param, query);
            }
            let mut raw_rows = query.fetch_all(&mut *conn).await?;
            let rows: Vec<Row> = raw_rows.drain(..).map(Row::from).collect();
            datasets.push(rows);
        }
        Ok(datasets)
    }

    fn syntax(&self) -> crate::Syntax {
        crate::Syntax::Mysql
    }
}

pub trait MysqlParam {
    fn add_param<'q>(
        &'q self,
        query: Query<'q, MySql, MySqlArguments>,
    ) -> Query<'q, MySql, MySqlArguments>;
}

impl<T> MysqlParam for T
where
    for<'a> T: 'a + Send + Encode<'a, MySql> + Type<MySql>,
    for<'a> &'a T: Send,
{
    fn add_param<'q>(
        &'q self,
        query: Query<'q, MySql, MySqlArguments>,
    ) -> Query<'q, MySql, MySqlArguments> {
        query.bind(self)
    }
}
