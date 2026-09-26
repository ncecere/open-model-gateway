use sqlx::{PgPool, migrate::Migrator};

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Fail readiness for missing, dirty, changed, or unexpected migrations.
    pub async fn is_ready(&self) -> bool {
        let applied = sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
            "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&self.pool)
        .await;
        let Ok(applied) = applied else {
            return false;
        };
        applied.len() == MIGRATOR.iter().count()
            && applied.iter().zip(MIGRATOR.iter()).all(|(row, expected)| {
                row.0 == expected.version && row.1 == expected.checksum.as_ref() && row.2
            })
    }
}
