//! The database's schemas, their relations and columns, as the schema
//! tree shows them.

use crate::api::{DbColumn, DbSchema, DbTable, DbTableKind};

use super::{Handle, Result, describe};

impl Handle {
    pub async fn schema(&self) -> Result<Vec<DbSchema>> {
        let _busy = self.busy.lock().await;
        let rows = |sql: &'static str| async move {
            self.client.query(sql, &[]).await.map_err(|e| describe(&e))
        };
        let mut schemas: Vec<DbSchema> = rows(SCHEMAS)
            .await?
            .iter()
            .map(|row| DbSchema {
                name: row.get(0),
                tables: Vec::new(),
            })
            .collect();
        for row in rows(RELATIONS).await? {
            let schema: String = row.get(0);
            let Some(schema) = schemas.iter_mut().find(|s| s.name == schema) else {
                continue;
            };
            let table: String = row.get(1);
            if schema.tables.last().is_none_or(|t| t.name != table) {
                let kind = match row.get::<_, String>(2).as_str() {
                    "v" => DbTableKind::View,
                    "m" => DbTableKind::MaterializedView,
                    "f" => DbTableKind::ForeignTable,
                    "p" => DbTableKind::PartitionedTable,
                    _ => DbTableKind::Table,
                };
                schema.tables.push(DbTable {
                    name: table,
                    kind,
                    columns: Vec::new(),
                });
            }
            if let Some(name) = row.get::<_, Option<String>>(3) {
                let table = schema.tables.last_mut().expect("pushed above");
                table.columns.push(DbColumn {
                    name,
                    data_type: row.get(4),
                    nullable: row.get(5),
                });
            }
        }
        Ok(schemas)
    }
}

/// Schemas the user made, leaving out the system's own.
const SCHEMAS: &str = "\
    SELECT nspname::text FROM pg_catalog.pg_namespace \
    WHERE nspname <> 'information_schema' AND nspname !~ '^pg_' \
    ORDER BY 1";

/// Their tables, views and other relations, with columns in order. A
/// relation without columns still has a row, with NULL for the column.
const RELATIONS: &str = "\
    SELECT n.nspname::text, c.relname::text, c.relkind::text, a.attname::text, \
           pg_catalog.format_type(a.atttypid, a.atttypmod), NOT a.attnotnull \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    LEFT JOIN pg_catalog.pg_attribute a \
      ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
    WHERE c.relkind IN ('r', 'v', 'm', 'f', 'p') \
      AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' \
    ORDER BY 1, 2, a.attnum";
