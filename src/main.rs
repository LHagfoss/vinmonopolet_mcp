use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
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
    /// One SQLite SELECT (or WITH ... SELECT) statement. Add LIMIT for large results.
    statement: String,
}

#[derive(Clone)]
struct VinmonopoletMcp {
    cfg: Config,
    instructions: String,
}

/// Successful results are JSON text; failures (SQL syntax, unknown column,
/// rejected statement) become `isError: true` results so the model can read
/// the message and correct its query.
fn tool_result(result: Result<serde_json::Value>) -> CallToolResult {
    match result.and_then(|value| Ok(serde_json::to_string(&value)?)) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!("error: {error:#}"))]),
    }
}

/// Describe the user tables as `Table(col TYPE, ...)` lines for the server
/// instructions, so the schema is available without bloating the tool description.
fn describe_schema(path: &PathBuf) -> Result<String> {
    let connection = open_database(path)?;
    let mut tables = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') \
         AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut lines = Vec::new();
    for name in names {
        let mut info = connection.prepare("SELECT name, type FROM pragma_table_info(?1)")?;
        let columns = info
            .query_map([&name], |row| {
                let column: String = row.get(0)?;
                let ty: String = row.get(1)?;
                Ok(if ty.is_empty() {
                    column
                } else {
                    format!("{column} {ty}")
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        lines.push(format!("{name}({})", columns.join(", ")));
    }
    Ok(lines.join("\n"))
}

fn build_instructions(cfg: &Config) -> String {
    let schema = describe_schema(&cfg.db_path).unwrap_or_else(|_| {
        "Products(Id INTEGER, ProductId TEXT, ProductShortName TEXT, Percentage REAL, CostPerLiter REAL)"
            .into()
    });
    format!(
        "Read-only SQLite database of Vinmonopolet (Norwegian alcohol retailer) products, \
         queried with the sql tool.\nSchema:\n{schema}\nNotes: Percentage is alcohol by volume (%). \
         CostPerLiter is NOK per liter; 0 means unknown, so filter CostPerLiter > 0 when comparing \
         prices. Use LIKE for name matching. At most {} rows are returned per query.",
        cfg.max_rows
    )
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
    let trimmed = sql.trim().trim_end_matches(';').trim_end();
    if trimmed.is_empty() {
        anyhow::bail!("statement must not be empty");
    }
    if !trimmed
        .split_ascii_whitespace()
        .next()
        .is_some_and(|keyword| {
            keyword.eq_ignore_ascii_case("select") || keyword.eq_ignore_ascii_case("with")
        })
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
    let mut truncated = false;

    while let Some(row) = rows.next()? {
        if result.len() >= cfg.max_rows {
            truncated = true;
            break;
        }

        let values = (0..columns.len())
            .map(|index| row.get_ref(index).map(value_to_json))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result.push(serde_json::Value::Array(values));
    }

    let mut value = serde_json::json!({
        "columns": columns,
        "rows": result,
    });
    if truncated {
        value["truncated"] = serde_json::json!(format!(
            "stopped after {} rows; add LIMIT or narrow the query",
            cfg.max_rows
        ));
    }
    Ok(value)
}

#[tool_router]
impl VinmonopoletMcp {
    fn new(cfg: Config) -> Self {
        let instructions = build_instructions(&cfg);
        Self { cfg, instructions }
    }

    #[tool(
        description = "Run one read-only SELECT on the Vinmonopolet SQLite DB (main table Products(ProductShortName, Percentage, CostPerLiter, ...); full schema in server instructions). Returns {columns, rows}.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sql(
        &self,
        Parameters(params): Parameters<SqlParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let result = tokio::task::spawn_blocking(move || execute_select(cfg, params.statement))
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(tool_result(result))
    }
}

#[tool_handler]
impl ServerHandler for VinmonopoletMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "vinmonopolet",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions.clone())
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

    fn temp_db(tag: &str) -> Result<PathBuf> {
        let path =
            env::temp_dir().join(format!("vinmonopolet-mcp-{tag}-{}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE Products (Id INTEGER, ProductShortName TEXT, CostPerLiter REAL);
             INSERT INTO Products VALUES (1, 'A', 10.0), (2, 'B', 20.0), (3, 'C', 0.0);",
        )?;
        Ok(path)
    }

    #[test]
    fn truncates_instead_of_failing_and_accepts_cte_and_trailing_semicolon() -> Result<()> {
        let path = temp_db("trunc")?;
        let cfg = Config {
            db_path: path.clone(),
            max_rows: 2,
        };
        let result = execute_select(cfg.clone(), "SELECT Id FROM Products;".into())?;
        assert_eq!(result["rows"], serde_json::json!([[1], [2]]));
        assert!(result["truncated"].is_string());
        let result = execute_select(
            cfg.clone(),
            "WITH p AS (SELECT * FROM Products WHERE CostPerLiter > 0) SELECT count(*) FROM p"
                .into(),
        )?;
        assert_eq!(result["rows"], serde_json::json!([[2]]));
        assert!(result.get("truncated").is_none());
        assert!(execute_select(cfg, "WITH p AS (SELECT 1) DELETE FROM Products".into()).is_err());
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn sql_errors_become_error_results() -> Result<()> {
        let path = temp_db("err")?;
        let cfg = Config {
            db_path: path.clone(),
            max_rows: 10,
        };
        let result = tool_result(execute_select(cfg, "SELECT Nope FROM Products".into()));
        assert_eq!(result.is_error, Some(true));
        let text = serde_json::to_string(&result.content)?;
        assert!(text.contains("no such column"), "{text}");
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn instructions_include_live_schema() -> Result<()> {
        let path = temp_db("schema")?;
        let cfg = Config {
            db_path: path.clone(),
            max_rows: 10,
        };
        let text = build_instructions(&cfg);
        assert!(
            text.contains("Products(Id INTEGER, ProductShortName TEXT, CostPerLiter REAL)"),
            "{text}"
        );
        fs::remove_file(path)?;
        Ok(())
    }
}
