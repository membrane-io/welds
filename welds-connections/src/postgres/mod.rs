use super::transaction::{TransT, Transaction};
use super::Row;
use super::TransactStart;
use super::{Client, Param};
use crate::errors::Result;
use crate::ExecuteResult;
use async_trait::async_trait;
use sqlx::postgres::{PgArguments, PgPoolOptions};
use sqlx::query::Query;
use sqlx::{PgPool, Postgres};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PostgresClient {
    pool: Arc<PgPool>,
}

#[async_trait]
impl TransactStart for PostgresClient {
    async fn begin(&self) -> Result<Transaction> {
        let t = self.pool.begin().await?;
        let t = TransT::Postgres(t);
        Ok(Transaction::new(t))
    }
}

pub async fn connect(
    url: &str,
    timeout: Option<Duration>,
    max_connections: Option<usize>,
) -> Result<PostgresClient> {
    let mut pool = PgPoolOptions::new();
    if let Some(timeout) = timeout {
        pool = pool.acquire_timeout(timeout);
    }
    if let Some(max_connections) = max_connections {
        pool = pool.max_connections(max_connections as _);
    }
    let pool = pool.connect(url).await?;
    Ok(PostgresClient {
        pool: Arc::new(pool),
    })
}

impl From<sqlx::PgPool> for PostgresClient {
    fn from(pool: sqlx::PgPool) -> PostgresClient {
        PostgresClient {
            pool: Arc::new(pool),
        }
    }
}

impl PostgresClient {
    pub fn as_sqlx_pool(&self) -> &PgPool {
        &self.pool
    }
}

use sqlx::encode::Encode;
use sqlx::types::Type;

#[async_trait]
impl Client for PostgresClient {
    async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn Param + Sync + Send)],
    ) -> Result<ExecuteResult> {
        let mut query = sqlx::query::<Postgres>(sql).persistent(false);
        for param in params {
            query = PostgresParam::add_param(*param, query);
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
        let mut query = sqlx::query::<Postgres>(sql).persistent(false);
        for param in params {
            query = PostgresParam::add_param(*param, query);
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
            let mut query = sqlx::query::<Postgres>(sql).persistent(false);
            for param in params {
                query = PostgresParam::add_param(*param, query);
            }
            let mut raw_rows = query.fetch_all(&mut *conn).await?;
            let rows: Vec<Row> = raw_rows.drain(..).map(Row::from).collect();
            datasets.push(rows);
        }
        Ok(datasets)
    }

    fn syntax(&self) -> crate::Syntax {
        crate::Syntax::Postgres
    }
}

pub trait PostgresParam {
    fn add_param<'q>(
        &'q self,
        query: Query<'q, Postgres, PgArguments>,
    ) -> Query<'q, Postgres, PgArguments>;
}

impl<T> PostgresParam for T
where
    for<'a> T: 'a + Send + Encode<'a, Postgres> + Type<Postgres>,
    for<'a> &'a T: Send,
{
    fn add_param<'q>(
        &'q self,
        query: Query<'q, Postgres, PgArguments>,
    ) -> Query<'q, Postgres, PgArguments> {
        query.bind(self)
    }
}
