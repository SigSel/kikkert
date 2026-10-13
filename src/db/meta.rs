//! Catalog queries: what tables exist and what they are made of.
//!
//! These are the only statements that skip the read-only guard, because they
//! are fixed text written here. Names taken from the catalog are embedded as
//! quoted literals, and the statements still run inside a read-only session.

use super::{Engine, ResultSet};
use crate::query::{ColumnInfo, TableRef};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
}

/// A table or view that can be opened.
#[derive(Clone, Debug)]
pub struct DbObject {
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
    /// The catalog's own word for it, such as "materialized view".
    pub kind_label: String,
}

impl DbObject {
    pub fn table_ref(&self) -> TableRef {
        TableRef {
            schema: self.schema.clone(),
            name: self.name.clone(),
        }
    }

    pub fn display_name(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{schema}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// One tab of table structure: a title and the query that fills it.
#[derive(Clone, Debug)]
pub struct StructureTab {
    pub title: &'static str,
    pub sql: String,
}

/// Lists tables and views as (schema, name, kind).
pub fn objects_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => {
            "SELECT n.nspname, c.relname, \
                    CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
                                   WHEN 'f' THEN 'foreign table' ELSE 'table' END \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
               AND n.nspname <> 'information_schema' \
               AND n.nspname !~ '^pg_' \
               AND NOT c.relispartition \
               AND pg_catalog.has_schema_privilege(n.oid, 'USAGE') \
             ORDER BY n.nspname, c.relname"
        }
        Engine::Mysql => {
            "SELECT table_schema, table_name, \
                    CASE WHEN table_type LIKE '%VIEW%' THEN 'view' ELSE 'table' END \
             FROM information_schema.tables \
             WHERE table_schema NOT IN ('information_schema', 'performance_schema', 'mysql', 'sys') \
             ORDER BY table_schema, table_name"
        }
        Engine::Mssql => {
            "SELECT s.name, o.name, CASE o.type WHEN 'V' THEN 'view' ELSE 'table' END \
             FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
             ORDER BY s.name, o.name"
        }
        Engine::Sqlite => {
            "SELECT NULL, name, type FROM sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite!_%' ESCAPE '!' \
             ORDER BY name"
        }
    }
}

pub fn parse_objects(result: &ResultSet) -> Vec<DbObject> {
    result
        .rows
        .iter()
        .filter_map(|row| {
            let name = row.get(1)?.clone()?;
            let kind_label = row
                .get(2)
                .cloned()
                .flatten()
                .unwrap_or_else(|| "table".into());
            let kind = if kind_label.contains("view") {
                ObjectKind::View
            } else {
                ObjectKind::Table
            };
            Some(DbObject {
                schema: row.first().cloned().flatten().filter(|s| !s.is_empty()),
                name,
                kind,
                kind_label,
            })
        })
        .collect()
}

/// The structure tabs for one object. The first is always "Columns", and its
/// result always starts with: name, type, nullable (YES/NO), default, key (PK
/// for primary key columns).
pub fn structure_tabs(engine: Engine, object: &DbObject) -> Vec<StructureTab> {
    let schema = object.schema.as_deref();
    let name = object.name.as_str();
    let lit = |s: &str| engine.quote_literal(s);
    let schema_lit = lit(schema.unwrap_or(""));
    let name_lit = lit(name);
    let is_view = object.kind == ObjectKind::View;

    match engine {
        Engine::Postgres => {
            // 'schema.table'::regclass resolves the object once, by its quoted name.
            let regclass = format!("{}::regclass", lit(&engine.qualify(schema, name)));
            let mut tabs = vec![
                StructureTab {
                    title: "Columns",
                    sql: format!(
                        "SELECT a.attname AS name, \
                                pg_catalog.format_type(a.atttypid, a.atttypmod) AS type, \
                                CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS nullable, \
                                pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS \"default\", \
                                CASE WHEN EXISTS (SELECT 1 FROM pg_catalog.pg_index i \
                                                  WHERE i.indrelid = a.attrelid AND i.indisprimary \
                                                    AND a.attnum = ANY (i.indkey)) \
                                     THEN 'PK' ELSE '' END AS key, \
                                pg_catalog.col_description(a.attrelid, a.attnum) AS comment \
                         FROM pg_catalog.pg_attribute a \
                         LEFT JOIN pg_catalog.pg_attrdef d \
                                ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                         WHERE a.attrelid = {regclass} AND a.attnum > 0 AND NOT a.attisdropped \
                         ORDER BY a.attnum"
                    ),
                },
                StructureTab {
                    title: "Indexes",
                    sql: format!(
                        "SELECT indexname AS name, indexdef AS definition \
                         FROM pg_catalog.pg_indexes \
                         WHERE schemaname = {schema_lit} AND tablename = {name_lit} \
                         ORDER BY indexname"
                    ),
                },
                StructureTab {
                    title: "Keys",
                    sql: format!(
                        "SELECT conname AS name, \
                                CASE contype WHEN 'p' THEN 'primary key' WHEN 'f' THEN 'foreign key' \
                                             WHEN 'u' THEN 'unique' WHEN 'c' THEN 'check' \
                                             WHEN 'x' THEN 'exclusion' WHEN 'n' THEN 'not null' \
                                             ELSE contype::text END AS type, \
                                pg_catalog.pg_get_constraintdef(oid) AS definition \
                         FROM pg_catalog.pg_constraint \
                         WHERE conrelid = {regclass} \
                         ORDER BY contype, conname"
                    ),
                },
            ];
            if is_view {
                tabs.push(StructureTab {
                    title: "Definition",
                    sql: format!(
                        "SELECT pg_catalog.pg_get_viewdef({regclass}, true) AS definition"
                    ),
                });
            }
            tabs
        }
        Engine::Mysql => {
            let scope = format!("table_schema = {schema_lit} AND table_name = {name_lit}");
            vec![
                StructureTab {
                    title: "Columns",
                    sql: format!(
                        "SELECT column_name AS name, column_type AS type, is_nullable AS nullable, \
                                column_default AS `default`, \
                                CASE column_key WHEN 'PRI' THEN 'PK' ELSE column_key END AS `key`, \
                                extra, column_comment AS comment \
                         FROM information_schema.columns \
                         WHERE {scope} \
                         ORDER BY ordinal_position"
                    ),
                },
                StructureTab {
                    title: "Indexes",
                    sql: format!(
                        "SELECT index_name AS name, \
                                GROUP_CONCAT(column_name ORDER BY seq_in_index SEPARATOR ', ') AS columns, \
                                CASE non_unique WHEN 0 THEN 'YES' ELSE 'NO' END AS `unique`, \
                                index_type AS type \
                         FROM information_schema.statistics \
                         WHERE {scope} \
                         GROUP BY index_name, non_unique, index_type \
                         ORDER BY index_name"
                    ),
                },
                StructureTab {
                    title: "Keys",
                    sql: format!(
                        "SELECT k.constraint_name AS name, t.constraint_type AS type, \
                                k.column_name AS `column`, \
                                CONCAT_WS('.', k.referenced_table_schema, k.referenced_table_name, \
                                          k.referenced_column_name) AS `references` \
                         FROM information_schema.key_column_usage k \
                         JOIN information_schema.table_constraints t \
                           ON t.constraint_schema = k.constraint_schema \
                          AND t.constraint_name = k.constraint_name \
                          AND t.table_schema = k.table_schema \
                          AND t.table_name = k.table_name \
                         WHERE k.table_schema = {schema_lit} AND k.table_name = {name_lit} \
                         ORDER BY k.constraint_name, k.ordinal_position"
                    ),
                },
                StructureTab {
                    title: "Definition",
                    sql: if is_view {
                        format!(
                            "SELECT view_definition AS definition \
                             FROM information_schema.views WHERE {scope}"
                        )
                    } else {
                        format!("SHOW CREATE TABLE {}", engine.qualify(schema, name))
                    },
                },
            ]
        }
        Engine::Mssql => {
            // Always N'...' so names outside the code page resolve.
            let quoted = lit(&engine.qualify(schema, name));
            let object_id = format!("OBJECT_ID(N{})", quoted.trim_start_matches('N'));
            let mut tabs = vec![
                StructureTab {
                    title: "Columns",
                    sql: format!(
                        "SELECT c.name, \
                                TYPE_NAME(c.user_type_id) + CASE \
                                    WHEN TYPE_NAME(c.system_type_id) IN ('varchar', 'char', 'varbinary', 'binary') \
                                        THEN '(' + CASE c.max_length WHEN -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')' \
                                    WHEN TYPE_NAME(c.system_type_id) IN ('nvarchar', 'nchar') \
                                        THEN '(' + CASE c.max_length WHEN -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')' \
                                    WHEN TYPE_NAME(c.system_type_id) IN ('decimal', 'numeric') \
                                        THEN '(' + CAST(c.precision AS varchar(10)) + ',' + CAST(c.scale AS varchar(10)) + ')' \
                                    ELSE '' END AS [type], \
                                CASE c.is_nullable WHEN 1 THEN 'YES' ELSE 'NO' END AS nullable, \
                                dc.definition AS [default], \
                                CASE WHEN pk.column_id IS NOT NULL THEN 'PK' ELSE '' END AS [key], \
                                CASE WHEN c.is_identity = 1 THEN 'identity' \
                                     WHEN c.is_computed = 1 THEN 'computed' ELSE '' END AS extra \
                         FROM sys.columns c \
                         LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
                         LEFT JOIN (SELECT ic.object_id, ic.column_id \
                                    FROM sys.indexes i \
                                    JOIN sys.index_columns ic \
                                      ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                                    WHERE i.is_primary_key = 1) pk \
                                ON pk.object_id = c.object_id AND pk.column_id = c.column_id \
                         WHERE c.object_id = {object_id} \
                         ORDER BY c.column_id"
                    ),
                },
                StructureTab {
                    title: "Indexes",
                    sql: format!(
                        "SELECT i.name, i.type_desc AS [type], \
                                CASE i.is_unique WHEN 1 THEN 'YES' ELSE 'NO' END AS [unique], \
                                CASE i.is_primary_key WHEN 1 THEN 'YES' ELSE 'NO' END AS [primary], \
                                STUFF((SELECT ', ' + c.name \
                                       FROM sys.index_columns ic \
                                       JOIN sys.columns c \
                                         ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                                       WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id \
                                         AND ic.is_included_column = 0 \
                                       ORDER BY ic.key_ordinal \
                                       FOR XML PATH('')), 1, 2, '') AS columns \
                         FROM sys.indexes i \
                         WHERE i.object_id = {object_id} AND i.name IS NOT NULL \
                         ORDER BY i.name"
                    ),
                },
                StructureTab {
                    title: "Keys",
                    sql: format!(
                        "SELECT fk.name, pc.name AS [column], \
                                rs.name + '.' + rt.name + '.' + rc.name AS [references], \
                                fk.delete_referential_action_desc AS on_delete, \
                                fk.update_referential_action_desc AS on_update \
                         FROM sys.foreign_keys fk \
                         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                         JOIN sys.columns pc \
                           ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                         JOIN sys.columns rc \
                           ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                         JOIN sys.objects rt ON rt.object_id = fkc.referenced_object_id \
                         JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
                         WHERE fk.parent_object_id = {object_id} \
                         ORDER BY fk.name, fkc.constraint_column_id"
                    ),
                },
            ];
            if is_view {
                tabs.push(StructureTab {
                    title: "Definition",
                    sql: format!("SELECT OBJECT_DEFINITION({object_id}) AS definition"),
                });
            }
            tabs
        }
        Engine::Sqlite => vec![
            StructureTab {
                title: "Columns",
                sql: format!(
                    "SELECT name, type, CASE \"notnull\" WHEN 0 THEN 'YES' ELSE 'NO' END AS nullable, \
                            dflt_value AS \"default\", CASE WHEN pk > 0 THEN 'PK' ELSE '' END AS key \
                     FROM pragma_table_info({name_lit}) \
                     ORDER BY cid"
                ),
            },
            StructureTab {
                title: "Indexes",
                sql: format!(
                    "SELECT il.name, \
                            (SELECT group_concat(ii.name, ', ') FROM pragma_index_info(il.name) ii) AS columns, \
                            CASE il.\"unique\" WHEN 1 THEN 'YES' ELSE 'NO' END AS \"unique\", \
                            il.origin \
                     FROM pragma_index_list({name_lit}) il \
                     ORDER BY il.name"
                ),
            },
            StructureTab {
                title: "Keys",
                sql: format!(
                    "SELECT \"from\" AS \"column\", \
                            \"table\" || '.' || COALESCE(\"to\", '') AS \"references\", \
                            on_update, on_delete \
                     FROM pragma_foreign_key_list({name_lit}) \
                     ORDER BY id, seq"
                ),
            },
            StructureTab {
                title: "Definition",
                sql: format!(
                    "SELECT sql AS definition FROM sqlite_master \
                     WHERE name = {name_lit} AND type IN ('table', 'view')"
                ),
            },
        ],
    }
}

/// Reads the "Columns" tab back into the column list the query builder uses.
pub fn parse_columns(result: &ResultSet) -> Vec<ColumnInfo> {
    result
        .rows
        .iter()
        .filter_map(|row| {
            let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            let name = row.first()?.clone()?;
            Some(ColumnInfo {
                name,
                type_name: cell(1),
                primary_key: cell(4) == "PK",
            })
        })
        .collect()
}
