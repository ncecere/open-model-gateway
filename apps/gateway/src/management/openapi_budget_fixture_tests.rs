//! Temporary day-window corruption for the executed OpenAPI fixtures.
use crate::governance::{BudgetPeriod, totals::Scope};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// These throwaway scopes have exactly one day row. Preserve its original
/// bucket and figures, but corrupt the bucket the reports will actually read,
/// even when settlement happened on the preceding UTC day (#434).
pub(super) struct CorruptedDayBudget {
    scope: Scope,
    original: (String, String, DateTime<Utc>),
    pinned_start: DateTime<Utc>,
}

impl CorruptedDayBudget {
    pub(super) async fn new(pool: &PgPool, scope: Scope) -> Self {
        let (kind, id) = scope.key();
        let mut rows: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT settled_microusd::text,held_microusd::text,period_start FROM budget_totals WHERE scope_kind=$1 AND scope_id=$2 AND period='day'",
        )
        .bind(kind)
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap();
        assert!(
            rows.len() == 1,
            "fixture scope has exactly one day totals row"
        );
        let original = rows.pop().unwrap();
        let pinned_start = BudgetPeriod::Day.bucket_start(Utc::now());
        let changed = sqlx::query(
            "UPDATE budget_totals SET settled_microusd=-1,held_microusd=0,period_start=$4 WHERE scope_kind=$1 AND scope_id=$2 AND period='day' AND period_start=$3",
        )
        .bind(kind)
        .bind(id)
        .bind(original.2)
        .bind(pinned_start)
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();
        assert!(changed == 1, "fixture day totals row is corrupted");
        Self {
            scope,
            original,
            pinned_start,
        }
    }

    pub(super) async fn restore(self, pool: &PgPool) {
        let (kind, id) = self.scope.key();
        let changed = sqlx::query(
            "UPDATE budget_totals SET settled_microusd=$4::numeric,held_microusd=$5::numeric,period_start=$6 WHERE scope_kind=$1 AND scope_id=$2 AND period='day' AND period_start=$3",
        )
        .bind(kind)
        .bind(id)
        .bind(self.pinned_start)
        .bind(self.original.0)
        .bind(self.original.1)
        .bind(self.original.2)
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();
        assert!(changed == 1, "fixture day totals row is restored");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn day_budget_corruption_pins_current_bucket_and_restores_original(pool: PgPool) {
    for scope in [
        Scope::Workspace(uuid::Uuid::new_v4()),
        Scope::KeyLineage(uuid::Uuid::new_v4()),
    ] {
        let (kind, id) = scope.key();
        let seeded_at = Utc::now();
        let yesterday = BudgetPeriod::Day.bucket_start(seeded_at) - chrono::Duration::days(1);
        let month_start = BudgetPeriod::Month.bucket_start(seeded_at);
        sqlx::query("INSERT INTO budget_totals(scope_kind,scope_id,period,period_start,settled_microusd,held_microusd) VALUES($1,$2,'day',$3,7,3),($1,$2,'month',$4,11,5)")
            .bind(kind).bind(id).bind(yesterday)
            .bind(month_start)
            .execute(&pool).await.unwrap();
        let before = BudgetPeriod::Day.bucket_start(Utc::now());
        let corruption = CorruptedDayBudget::new(&pool, scope).await;
        let after = BudgetPeriod::Day.bucket_start(Utc::now());
        let read = || {
            sqlx::query_as::<_, (String, String, DateTime<Utc>)>(
            "SELECT settled_microusd::text,held_microusd::text,period_start FROM budget_totals WHERE scope_kind=$1 AND scope_id=$2 AND period='day'",
        ).bind(kind).bind(id)
        };
        let corrupted = read().fetch_one(&pool).await.unwrap();
        assert!(
            corrupted.0 == "-1" && corrupted.1 == "0" && corrupted.2 == corruption.pinned_start,
            "day corruption uses the current bucket"
        );
        assert!(
            corruption.pinned_start >= before && corruption.pinned_start <= after,
            "day bucket is current"
        );
        corruption.restore(&pool).await;
        let restored = read().fetch_one(&pool).await.unwrap();
        assert!(
            restored.0 == "7" && restored.1 == "3" && restored.2 == yesterday,
            "day bucket and figures are restored"
        );
        let neighbour: bool = sqlx::query_scalar(
            "SELECT settled_microusd=11 AND held_microusd=5 AND period_start=$3 FROM budget_totals WHERE scope_kind=$1 AND scope_id=$2 AND period='month'",
        ).bind(kind).bind(id).bind(month_start)
            .fetch_one(&pool).await.unwrap();
        assert!(neighbour, "neighbouring month window is unchanged");
    }
}
