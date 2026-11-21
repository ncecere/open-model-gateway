use std::borrow::Cow;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::{Connection, Executor, PgConnection, PgPool, query, query_scalar};

use super::{ENTERPRISE_MIGRATIONS, Templates, quote};

/// A template family of its own, so these tests never touch the one the
/// database tests of this process use. Its test databases are named
/// `<prefix>x<n>`: `<prefix>_` is the family's collectable namespace.
struct Family {
    prefix: String,
}

impl Family {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        Self {
            prefix: format!("_omgut_{}", &id[..16]),
        }
    }

    fn templates(&self, migrations: &[&str]) -> Templates {
        let migrations = migrations
            .iter()
            .enumerate()
            .map(|(index, sql)| {
                Migration::new(
                    i64::try_from(index).unwrap() + 1,
                    format!("step {index}").into(),
                    MigrationType::Simple,
                    Cow::Owned((*sql).to_owned()),
                    false,
                )
            })
            .collect::<Vec<_>>();
        let migrator = Box::leak(Box::new(Migrator {
            migrations: Cow::Owned(migrations),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        }));
        Templates::from_env(&self.prefix, migrator)
    }

    /// A test path of the family; its database is named from it by sqlx's rule.
    fn path(&self, label: &str) -> &'static str {
        Box::leak(format!("{}x{label}", self.prefix).into_boxed_str())
    }

    /// Remove everything the family created, whatever state it is in.
    async fn cleanup(&self, templates: &Templates) {
        let mut conn = templates.maintenance_connection().await.unwrap();
        let names: Vec<String> =
            query_scalar("select datname::text from pg_database where starts_with(datname, $1)")
                .bind(&self.prefix)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        for name in names {
            conn.execute(&*format!(
                "alter database {} with is_template false allow_connections true",
                quote(&name)
            ))
            .await
            .unwrap();
            conn.execute(&*format!("drop database {} with (force)", quote(&name)))
                .await
                .unwrap();
            query("delete from _sqlx_test.databases where db_name = $1")
                .bind(&name)
                .execute(&mut conn)
                .await
                .unwrap();
        }
    }
}

async fn database_exists(conn: &mut PgConnection, name: &str) -> bool {
    query_scalar("select exists(select 1 from pg_database where datname = $1)")
        .bind(name)
        .fetch_one(conn)
        .await
        .unwrap()
}

async fn recorded(conn: &mut PgConnection, name: &str) -> bool {
    query_scalar("select exists(select 1 from _sqlx_test.databases where db_name = $1)")
        .bind(name)
        .fetch_one(conn)
        .await
        .unwrap()
}

const SEEDED: &str = "create table seeded (id int primary key); insert into seeded values (1);";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn template_is_built_once_under_contention() {
    let family = Family::new();
    let templates = family.templates(&[SEEDED]);
    let tasks = (0..8)
        .map(|n| {
            let templates = templates.clone();
            let path = family.path(&n.to_string());
            tokio::spawn(async move {
                let template = templates.ensure().await.unwrap();
                let database = templates
                    .create_test_database(&template.name, path)
                    .await
                    .unwrap();
                let mut conn = PgConnection::connect_with(&templates.database_options(&database))
                    .await
                    .unwrap();
                let rows: i64 = query_scalar("select count(*) from seeded")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
                let migrations: i64 =
                    query_scalar("select count(*) from _sqlx_migrations where success")
                        .fetch_one(&mut conn)
                        .await
                        .unwrap();
                conn.close().await.unwrap();
                templates.drop_test_database(&database).await.unwrap();
                (template.name.clone(), template.built, rows, migrations)
            })
        })
        .collect::<Vec<_>>();
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(task.await.unwrap());
    }
    family.cleanup(&templates).await;
    assert!(outcomes.iter().all(|o| o.0 == outcomes[0].0));
    assert_eq!(outcomes.iter().filter(|o| o.1).count(), 1, "built once");
    assert!(outcomes.iter().all(|o| o.2 == 1 && o.3 == 1));
}

#[tokio::test]
async fn template_stays_clonable_with_no_session_connected() {
    let family = Family::new();
    let templates = family.templates(&[SEEDED]);
    let template = templates.ensure().await.unwrap();
    let mut conn = templates.maintenance_connection().await.unwrap();
    let (is_template, connects, sessions): (bool, bool, i64) = sqlx::query_as(
        "select d.datistemplate, d.datallowconn, \
         (select count(*) from pg_stat_activity a where a.datname = d.datname) \
         from pg_database d where d.datname = $1",
    )
    .bind(&template.name)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    drop(template);
    family.cleanup(&templates).await;
    assert!(is_template && !connects, "marked and closed to connections");
    assert_eq!(sessions, 0);
}

#[tokio::test]
async fn changed_migration_set_gets_another_template_and_leased_ones_survive() {
    let family = Family::new();
    let first = family.templates(&[SEEDED]);
    let second = family.templates(&[SEEDED, "create table later (id int);"]);

    let old = first.ensure().await.unwrap();
    let old_name = old.name.clone();
    drop(old);

    let current = second.ensure().await.unwrap();
    assert_ne!(
        current.name, old_name,
        "another migration set, another name"
    );
    let mut conn = first.maintenance_connection().await.unwrap();
    assert!(
        !database_exists(&mut conn, &old_name).await,
        "a template nobody leases is collected"
    );

    let again = first.ensure().await.unwrap();
    assert!(again.built, "the collected template is rebuilt");
    assert_eq!(again.name, old_name);
    assert!(
        database_exists(&mut conn, &current.name).await,
        "a template a live session leases is never dropped"
    );
    let second_again = second.ensure().await.unwrap();
    assert!(!second_again.built, "the leased template is reused");
    drop((current, again, second_again));
    family.cleanup(&first).await;
}

#[tokio::test]
async fn a_leftover_another_process_is_using_is_not_dropped() {
    let family = Family::new();
    let templates = family.templates(&[SEEDED]);
    let template = templates.ensure().await.unwrap();
    let path = family.path("collision");

    let first = templates
        .create_test_database(&template.name, path)
        .await
        .unwrap();
    let busy = PgConnection::connect_with(&templates.database_options(&first))
        .await
        .unwrap();
    let second = templates
        .create_test_database(&template.name, path)
        .await
        .unwrap();
    let mut conn = templates.maintenance_connection().await.unwrap();
    let outcome = (
        first == second,
        database_exists(&mut conn, &first).await,
        database_exists(&mut conn, &second).await,
        recorded(&mut conn, &second).await,
    );
    busy.close().await.unwrap();
    templates.drop_test_database(&first).await.unwrap();
    templates.drop_test_database(&second).await.unwrap();
    drop(template);
    family.cleanup(&templates).await;
    assert_eq!(outcome, (false, true, true, true));
}

#[tokio::test]
async fn half_built_templates_are_rebuilt_never_cloned() {
    for leftover in ["scratch", "unmarked", "connectable"] {
        let family = Family::new();
        let templates = family.templates(&[SEEDED]);
        let mut conn = templates.maintenance_connection().await.unwrap();
        let name = templates.template_name(&mut conn).await.unwrap();
        let (database, mark) = match leftover {
            // A builder killed before the rename: only the scratch name exists.
            "scratch" => (format!("{name}_b"), ""),
            // A database under the final name that was never marked.
            "unmarked" => (name.clone(), ""),
            // Marked, but never closed to connections.
            _ => (name.clone(), " with is_template true"),
        };
        conn.execute(&*format!("create database {}{mark}", quote(&database)))
            .await
            .unwrap();
        let mut partial = PgConnection::connect_with(&templates.database_options(&database))
            .await
            .unwrap();
        partial
            .execute("create table half_built (id int)")
            .await
            .unwrap();
        partial.close().await.unwrap();

        let template = templates.ensure().await.unwrap();
        assert!(template.built, "{leftover}: rebuilt");
        let clone = templates
            .create_test_database(&template.name, family.path("half_built"))
            .await
            .unwrap();
        let mut copy = PgConnection::connect_with(&templates.database_options(&clone))
            .await
            .unwrap();
        let (half_built, seeded): (Option<String>, Option<String>) =
            sqlx::query_as("select to_regclass('half_built')::text, to_regclass('seeded')::text")
                .fetch_one(&mut copy)
                .await
                .unwrap();
        copy.close().await.unwrap();
        templates.drop_test_database(&clone).await.unwrap();
        drop(template);
        family.cleanup(&templates).await;
        assert_eq!(half_built, None, "{leftover}: leftover content is gone");
        assert_eq!(seeded.as_deref(), Some("seeded"), "{leftover}");
    }
}

async fn passes(pool: PgPool) -> Result<(), sqlx::Error> {
    query("select 1").execute(&pool).await.map(|_| ())
}

async fn fails(pool: PgPool) -> Result<(), String> {
    query("select 1").execute(&pool).await.unwrap();
    Err("deliberate failure".into())
}

async fn panics(pool: PgPool) {
    query("select 1").execute(&pool).await.unwrap();
    panic!("deliberate panic");
}

#[tokio::test]
async fn passed_database_is_dropped_failed_and_panicked_ones_are_kept() {
    let family = Family::new();
    let templates = family.templates(&[SEEDED]);
    let template = templates.ensure().await.unwrap();
    let path = |label: &str| -> &'static str {
        Box::leak(format!("dbtest_runtime::tests::{}::{label}", family.prefix).into_boxed_str())
    };
    let name = |path: &'static str| {
        <sqlx::Postgres as sqlx::testing::TestSupport>::db_name(&sqlx::testing::TestArgs::new(path))
    };
    let (pass, fail, panic) = (path("pass"), path("fail"), path("panic"));

    templates
        .run_test(&template.name, pass, passes)
        .await
        .unwrap();
    assert_eq!(
        templates
            .run_test(&template.name, fail, fails)
            .await
            .unwrap_err(),
        "deliberate failure"
    );
    let caught = AssertUnwindSafe(templates.run_test(&template.name, panic, panics))
        .catch_unwind()
        .await;
    assert!(caught.is_err());

    let mut conn = templates.maintenance_connection().await.unwrap();
    let outcome = (
        database_exists(&mut conn, &name(pass)).await,
        recorded(&mut conn, &name(pass)).await,
        database_exists(&mut conn, &name(fail)).await,
        recorded(&mut conn, &name(fail)).await,
        database_exists(&mut conn, &name(panic)).await,
        recorded(&mut conn, &name(panic)).await,
    );
    // The kept databases are named from the test path, not the family: remove them here.
    for kept in [name(fail), name(panic)] {
        templates.drop_test_database(&kept).await.unwrap();
    }
    drop(template);
    family.cleanup(&templates).await;
    assert_eq!(
        outcome,
        (false, false, true, true, true, true),
        "(pass db, pass row, fail db, fail row, panic db, panic row)"
    );
}

#[tokio::test]
async fn a_rerun_replaces_the_database_a_failed_run_kept() {
    let family = Family::new();
    let templates = family.templates(&[SEEDED]);
    let template = templates.ensure().await.unwrap();
    let path: &'static str =
        Box::leak(format!("dbtest_runtime::tests::{}::rerun", family.prefix).into_boxed_str());
    let db_name = <sqlx::Postgres as sqlx::testing::TestSupport>::db_name(
        &sqlx::testing::TestArgs::new(path),
    );

    templates
        .run_test(&template.name, path, fails)
        .await
        .unwrap_err();
    let mut conn = templates.maintenance_connection().await.unwrap();
    assert!(database_exists(&mut conn, &db_name).await);
    templates
        .run_test(&template.name, path, passes)
        .await
        .unwrap();
    let gone = !database_exists(&mut conn, &db_name).await && !recorded(&mut conn, &db_name).await;
    drop(template);
    family.cleanup(&templates).await;
    assert!(gone);
}

/// A description of everything the migrations create and seed in a database
/// (catalog definitions, privileges, comments, sequence settings, the rows of
/// every table), plus the migration records without their two timing columns.
async fn snapshot(pool: &PgPool) -> Vec<String> {
    let user_schema = "n.nspname not in ('pg_catalog', 'information_schema') \
                       and n.nspname not like 'pg_toast%' and n.nspname not like 'pg_temp%'";
    let catalog = [
        format!(
            "select format('rel %s.%s kind=%s owner=%s acl=%s opts=%s part=%s rls=%s/%s bound=%s', \
             n.nspname, c.relname, c.relkind, pg_get_userbyid(c.relowner), c.relacl, c.reloptions, \
             c.relispartition, c.relrowsecurity, c.relforcerowsecurity, pg_get_expr(c.relpartbound, c.oid)) \
             from pg_class c join pg_namespace n on n.oid = c.relnamespace where {user_schema}"
        ),
        format!(
            "select format('col %s.%s.%s %s %s notnull=%s default=%s identity=%s generated=%s acl=%s', \
             n.nspname, c.relname, a.attnum, a.attname, format_type(a.atttypid, a.atttypmod), \
             a.attnotnull, pg_get_expr(d.adbin, d.adrelid), a.attidentity, a.attgenerated, a.attacl) \
             from pg_attribute a join pg_class c on c.oid = a.attrelid \
             join pg_namespace n on n.oid = c.relnamespace \
             left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
             where a.attnum > 0 and not a.attisdropped and c.relkind in ('r', 'p', 'v', 'm', 'f') and {user_schema}"
        ),
        format!(
            "select format('con %s.%s %s %s', n.nspname, c.conname, c.conrelid::regclass, pg_get_constraintdef(c.oid)) \
             from pg_constraint c join pg_namespace n on n.oid = c.connamespace where {user_schema}"
        ),
        format!(
            "select 'idx ' || pg_get_indexdef(i.indexrelid) from pg_index i \
             join pg_class c on c.oid = i.indexrelid join pg_namespace n on n.oid = c.relnamespace where {user_schema}"
        ),
        format!(
            "select 'trg ' || pg_get_triggerdef(t.oid) from pg_trigger t \
             join pg_class c on c.oid = t.tgrelid join pg_namespace n on n.oid = c.relnamespace \
             where not t.tgisinternal and {user_schema}"
        ),
        format!(
            "select format('fn %s acl=%s owner=%s', pg_get_functiondef(p.oid), p.proacl, pg_get_userbyid(p.proowner)) \
             from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
             where p.prokind in ('f', 'p') and {user_schema}"
        ),
        format!(
            "select 'view ' || n.nspname || '.' || c.relname || ' ' || pg_get_viewdef(c.oid) \
             from pg_class c join pg_namespace n on n.oid = c.relnamespace \
             where c.relkind in ('v', 'm') and {user_schema}"
        ),
        "select format('seq %s.%s %s %s %s %s %s %s', schemaname, sequencename, data_type, start_value, \
         min_value, max_value, increment_by, cycle) from pg_sequences \
         where schemaname not in ('pg_catalog', 'information_schema')"
            .to_owned(),
        format!(
            "select format('type %s.%s kind=%s labels=%s', n.nspname, t.typname, t.typtype, \
             (select string_agg(enumlabel, ',' order by enumsortorder) from pg_enum e where e.enumtypid = t.oid)) \
             from pg_type t join pg_namespace n on n.oid = t.typnamespace \
             where t.typtype in ('d', 'e', 'r', 'm') and {user_schema}"
        ),
        "select format('policy %s.%s %s %s %s %s %s %s', schemaname, tablename, policyname, permissive, \
         roles, cmd, qual, with_check) from pg_policies"
            .to_owned(),
        "select format('ext %s %s', extname, extversion) from pg_extension".to_owned(),
        "select format('schema %s owner=%s acl=%s', nspname, pg_get_userbyid(nspowner), nspacl) \
         from pg_namespace where nspname not like 'pg_%' and nspname <> 'information_schema'"
            .to_owned(),
        "select format('defacl %s %s %s %s', defaclrole::regrole, defaclnamespace, defaclobjtype, defaclacl) \
         from pg_default_acl"
            .to_owned(),
        "select format('comment %s: %s', pg_describe_object(classoid, objoid, objsubid), description) \
         from pg_description where objoid >= 16384"
            .to_owned(),
        "select format('db enc=%s collate=%s ctype=%s', pg_encoding_to_char(encoding), datcollate, datctype) \
         from pg_database where datname = current_database()"
            .to_owned(),
        "select format('mig %s %s %s %s %s', version, description, success, encode(checksum, 'hex'), no_tx) \
         from (select version, description, success, checksum, false as no_tx from _sqlx_migrations) m"
            .to_owned(),
    ];
    let mut lines = Vec::new();
    for sql in &catalog {
        let mut rows: Vec<String> = query_scalar(sql).fetch_all(pool).await.unwrap();
        rows.sort();
        lines.append(&mut rows);
    }
    let tables: Vec<(String, String)> = user_tables(pool).await;
    for (schema, table) in tables {
        // The rows these tables get from a migration carry the moment it ran
        // (and the installation's random id), so any two migrated databases
        // differ in exactly those columns.
        let volatile: Vec<String> = if [
            "installation",
            "installation_settings",
            "history_partitions",
            "storage_usage_progress",
            "usage_rollup_progress",
        ]
        .contains(&table.as_str())
        {
            query_scalar(
                "select a.attname::text from pg_attribute a \
                 where a.attrelid = format('%I.%I', $1::text, $2::text)::regclass \
                 and a.attnum > 0 and not a.attisdropped \
                 and (a.atttypid = 'timestamptz'::regtype or (a.attname = 'id' and $2::text = 'installation'))",
            )
            .bind(&schema)
            .bind(&table)
            .fetch_all(pool)
            .await
            .unwrap()
        } else {
            Vec::new()
        };
        let (count, digest): (i64, String) = sqlx::query_as(&format!(
            "select count(*), md5(coalesce(string_agg((to_jsonb(t) - $1::text[])::text, E'\\n' \
             order by (to_jsonb(t) - $1::text[])::text), '')) from {}.{} t",
            quote(&schema),
            quote(&table)
        ))
        .bind(&volatile)
        .fetch_one(pool)
        .await
        .unwrap();
        lines.push(format!("data {schema}.{table} rows={count} md5={digest}"));
    }
    lines
}

async fn user_tables(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "select n.nspname::text, c.relname::text from pg_class c \
         join pg_namespace n on n.oid = c.relnamespace \
         where c.relkind in ('r', 'p') and n.nspname not in ('pg_catalog', 'information_schema') \
         and n.nspname not like 'pg_toast%' and c.relname <> '_sqlx_migrations' \
         order by 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The copy is what `#[dbtest::test]`
/// gave: the schema, privileges, seeded rows and migration records of an empty
/// database the real migrator ran on. Only the two timing columns of
/// `_sqlx_migrations` (`installed_on`, `execution_time`) may differ, because
/// they record when and how fast the one build ran.
#[dbtest::test]
async fn cloned_database_equals_a_freshly_migrated_one(pool: PgPool) {
    let url = dotenvy::var("DATABASE_URL").unwrap();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let fresh_name = format!("_omgut_fresh_{}", uuid::Uuid::new_v4().simple());
    let mut admin = PgConnection::connect_with(&options).await.unwrap();
    admin
        .execute(&*format!("create database {}", quote(&fresh_name)))
        .await
        .unwrap();
    let mut conn = PgConnection::connect_with(&options.clone().database(&fresh_name))
        .await
        .unwrap();
    ENTERPRISE_MIGRATIONS.run_direct(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    let fresh = PgPool::connect_with(options.clone().database(&fresh_name))
        .await
        .unwrap();

    let cloned = snapshot(&pool).await;
    let migrated = snapshot(&fresh).await;
    let records: i64 = query_scalar("select count(*) from _sqlx_migrations where success")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Running the migrator again on the copy changes nothing.
    ENTERPRISE_MIGRATIONS.run(&pool).await.unwrap();
    let rerun = snapshot(&pool).await;

    fresh.close().await;
    admin
        .execute(&*format!(
            "drop database {} with (force)",
            quote(&fresh_name)
        ))
        .await
        .unwrap();
    assert!(
        cloned.len() > 1000,
        "the snapshot sees the schema ({})",
        cloned.len()
    );
    assert_eq!(
        usize::try_from(records).unwrap(),
        ENTERPRISE_MIGRATIONS.migrations.len()
    );
    let differing: Vec<_> = cloned
        .iter()
        .filter(|line| !migrated.contains(line))
        .chain(migrated.iter().filter(|line| !cloned.contains(line)))
        .collect();
    assert!(differing.is_empty(), "differences: {differing:#?}");
    assert_eq!(cloned, migrated);
    assert_eq!(cloned, rerun);
}
