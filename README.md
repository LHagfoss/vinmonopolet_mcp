# Vinmonopolet Database MCP

Rust MCP server for querying the existing Vinmonopolet SQLite database. It
implements the Python server's `sql` tool and communicates over MCP stdio, so
it can be launched directly by Claude Desktop, Claude Code, Codex, or another
MCP client.

## Build and run

```bash
cargo build --release
VINMONOPOLET_DB_PATH=/Users/lagos/code/vinmonopolet-database-mcp/vinmonopolet.db \
  ./target/release/vinmonopolet_mcp
```

`VINMONOPOLET_DB_PATH` is recommended. If it is omitted, the server looks for
`vinmonopolet.db` in the current directory and then in the sibling
`vinmonopolet-database-mcp` directory.

The database is opened read-only. The only exposed tool is `sql`, which accepts
one `SELECT` statement and returns `{ "columns": [...], "rows": [...] }`.
The `Products` table has these columns:

| Column | Type | Meaning |
| --- | --- | --- |
| `Id` | INTEGER | Internal database ID |
| `ProductId` | TEXT | Vinmonopolet product identifier |
| `ProductShortName` | TEXT | Product name |
| `Percentage` | REAL | Alcohol percentage |
| `CostPerLiter` | REAL | Price per liter |

Use `CostPerLiter` for price comparisons; there is no `price` column. Results
are limited to 1,000 rows by default; change that with
`VINMONOPOLET_MAX_ROWS`.

## MCP client configuration

```json
{
  "mcpServers": {
    "vinmonopolet": {
      "command": "/Users/lagos/code/vinmonopolet_mcp/target/release/vinmonopolet_mcp",
      "env": {
        "VINMONOPOLET_DB_PATH": "/Users/lagos/code/vinmonopolet-database-mcp/vinmonopolet.db"
      }
    }
  }
}
```

Example tool query:

```sql
SELECT ProductShortName, Percentage, CostPerLiter
FROM Products
WHERE Percentage > 10
ORDER BY CostPerLiter DESC
LIMIT 5
```
