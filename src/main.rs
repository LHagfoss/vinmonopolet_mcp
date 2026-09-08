use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
    transport::stdio,
};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Deserialize;

const DEFAULT_MAX_ROWS: usize = 1_000;

#[derive(Clone, Debug)]
struct Config {
    db_path: PathBuf,
    max_rows: usize,
}

impl Config {
    fn from_env() -> Result<Self> {
        let db_path = env::var_os("VINMONOPOLET_DB_PATH")
            .map(PathBuf::from)
            .or_else(find_default_database)
            .context("database not found; set VINMONOPOLET_DB_PATH to vinmonopolet.db")?;

        let max_rows = env::var("VINMONOPOLET_MAX_ROWS")
            .ok()
            .map(|value| {
                value
                    .parse::<usize>()
                    .context("VINMONOPOLET_MAX_ROWS must be a positive integer")
            })
            .transpose()?
            .unwrap_or(DEFAULT_MAX_ROWS);

        if max_rows == 0 {
            anyhow::bail!("VINMONOPOLET_MAX_ROWS must be greater than zero");
        }

        Ok(Self { db_path, max_rows })
    }
}

fn find_default_database() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("vinmonopolet.db"),
        PathBuf::from("../vinmonopolet-database-mcp/vinmonopolet.db"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SqlParams {
    /// A read-only SELECT statement. Include LIMIT for large result sets.
    statement: String,
}

#[derive(Clone)]
struct VinmonopoletMcp {
    cfg: Config,
}

fn mcp_error(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

fn text_result(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&value).map_err(mcp_error)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn open_database(path: &PathBuf) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    Connection::open_with_flags(path, flags)
        .with_context(|| format!("failed to open database {}", path.display()))
}

fn value_to_json(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(value) => value.into(),
        ValueRef::Real(value) => value.into(),
        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned().into(),
        ValueRef::Blob(value) => serde_json::json!({
            "type": "blob",
            "base64": base64::engine::general_purpose::STANDARD.encode(value),
        }),
    }
}

fn execute_select(cfg: Config, sql: String) -> Result<serde_json::Value> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        anyhow::bail!("statement must not be empty");
    }
    if !trimmed
        .split_ascii_whitespace()
        .next()
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case("select"))
    {
        anyhow::bail!("only SELECT statements are allowed");
    }
    if trimmed.contains(';') {
        anyhow::bail!("multiple SQL statements are not allowed");
    }

    let connection = open_database(&cfg.db_path)?;
    let mut statement = connection.prepare(trimmed)?;
    if !statement.readonly() {
        anyhow::bail!("only read-only SELECT statements are allowed");
    }

    let columns = (0..statement.column_count())
        .map(|index| statement.column_name(index).unwrap_or("column").to_owned())
        .collect::<Vec<_>>();
    let mut rows = statement.query([])?;
    let mut result = Vec::new();

    while let Some(row) = rows.next()? {
        if result.len() >= cfg.max_rows {
            anyhow::bail!(
                "query returned more than {} rows; add a LIMIT or raise VINMONOPOLET_MAX_ROWS",
                cfg.max_rows
            );
        }

        let values = (0..columns.len())
            .map(|index| row.get_ref(index).map(value_to_json))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result.push(serde_json::Value::Array(values));
    }

    Ok(serde_json::json!({
        "columns": columns,
        "rows": result,
    }))
}

#[tool_router(server_handler)]
impl VinmonopoletMcp {
    fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    #[tool(
        description = "Execute a read-only SELECT query against the Vinmonopolet Products SQLite database. Returns columns and rows. Only SELECT statements are accepted. Schema: Products(Id INTEGER, ProductId TEXT, ProductShortName TEXT, Percentage REAL, CostPerLiter REAL). Use CostPerLiter for price comparisons, not price. Example: SELECT ProductShortName, Percentage, CostPerLiter FROM Products WHERE CostPerLiter > 0 ORDER BY CostPerLiter DESC LIMIT 5."
    )]
    async fn sql(
        &self,
        Parameters(params): Parameters<SqlParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let result = tokio::task::spawn_blocking(move || execute_select(cfg, params.statement))
            .await
            .map_err(mcp_error)?
            .map_err(mcp_error)?;
        text_result(result)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    open_database(&cfg.db_path)?;
    eprintln!(
        "vinmonopolet_mcp: serving {} over stdio (max rows: {})",
        cfg.db_path.display(),
        cfg.max_rows
    );

    let service = VinmonopoletMcp::new(cfg).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rejects_non_select_queries() {
        let cfg = Config {
            db_path: PathBuf::from("does-not-matter.db"),
            max_rows: 10,
        };
        let error = execute_select(cfg, "DELETE FROM Products".into()).unwrap_err();
        assert!(error.to_string().contains("only SELECT"));
    }

    #[test]
    fn returns_columns_and_rows() -> Result<()> {
        let path = env::temp_dir().join(format!("vinmonopolet-mcp-test-{}.db", std::process::id()));
        let connection = Connection::open(&path)?;
        connection.execute(
            "CREATE TABLE Products (Id INTEGER, ProductShortName TEXT)",
            [],
        )?;
        connection.execute("INSERT INTO Products VALUES (1, 'Test wine')", [])?;
        drop(connection);

        let result = execute_select(
            Config {
                db_path: path.clone(),
                max_rows: 10,
            },
            "SELECT Id, ProductShortName FROM Products".into(),
        )?;
        assert_eq!(
            result["columns"],
            serde_json::json!(["Id", "ProductShortName"])
        );
        assert_eq!(result["rows"], serde_json::json!([[1, "Test wine"]]));
        fs::remove_file(path)?;
        Ok(())
    }
}
