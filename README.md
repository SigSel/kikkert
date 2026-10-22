# kikkert

A read-only, keyboard-driven database viewer for the terminal. *Kikkert* is
Norwegian for binoculars: it gets you a close look at your data from a safe
distance, and never touches it.

- **Engines:** PostgreSQL, MySQL / MariaDB, SQL Server and SQLite.
- **Credentials from 1Password:** a connection points at a 1Password item, and
  the server, database, username and password are read from it when you
  connect. Nothing secret is written to disk.
- **Search without writing SQL:** pick a column, how to match and a value.
  kikkert writes the SQL, shows it, and lets you edit it.
- **Sort, page, find duplicates, inspect structure**, all from the keyboard.
- **Compare two tables' columns**, across connections and even across engines.
- **Read-only by construction:** see [How read-only is enforced](#how-read-only-is-enforced).

## Install

Cargo can build kikkert straight from GitHub, without a clone:

```sh
cargo install --locked --git https://github.com/SigSel/kikkert
```

Running the same command again updates to the latest commit on `main`; add
`--rev <commit>` to install a particular one. From a clone of the repository,
`cargo install --locked --path .` does the same.

This needs Rust 1.94 or later and a C compiler (SQLite is built in). 1Password
connections also need the [1Password CLI](https://developer.1password.com/docs/cli/get-started/)
(`op`) with **Settings → Developer → Integrate with 1Password CLI** switched on
in the desktop app, so that unlocking happens in the app and not in the terminal.

## Configure

Connections live in `~/.config/kikkert/config.toml` (or wherever
`$KIKKERT_CONFIG` or `--config` points). `kikkert --example-config` prints a
commented example.

```toml
# Everything comes from one 1Password item.
[[connection]]
name = "orders-prod"
item = "op://Engineering/Orders Prod DB"

# Values set here win over the item, so one item can serve several databases.
[[connection]]
name = "orders-reporting"
item = "op://Engineering/Orders Prod DB"
database = "reporting"

# Or give the details directly; any value may be an op:// reference.
[[connection]]
name = "staging"
engine = "postgres"
host = "staging-db.internal"
database = "app"
username = "op://Engineering/Staging DB/username"
password = "op://Engineering/Staging DB/password"

[[connection]]
name = "local-file"
engine = "sqlite"
path = "~/data/app.db"
```

### The 1Password item

The simplest item is 1Password's own **Database** type, which already has the
right fields: type, server, port, database, username, password. Any item works
as long as its fields have recognisable names:

| kikkert needs | Field names it looks for |
|---|---|
| engine | type, engine, driver |
| host | server, hostname, host, server url, url, address, endpoint (or the item's website) |
| port | port |
| database | database, database name, dbname, db, catalog |
| username | username, user, login |
| password | password, pwd |

Names are matched without regard to case, spaces or punctuation. The server
field can be a bare host, `host:port`, SQL Server's `host,port` or
`host\instance`, Azure's `tcp:host,1433`, or a full URL such as
`postgres://host:5432/db`. If the engine is not stated it is worked out from
the URL scheme or a standard port.

For an item with unusual labels, say which field is which:

```toml
[[connection]]
name = "legacy-erp"
item = "op://Engineering/ERP"
engine = "mssql"
fields = { host = "Primary Node", database = "Catalog" }
```

`kikkert --check orders-prod` resolves a connection and tries it without
opening the interface, which is the quickest way to debug a new entry. It
prints where it is connecting and as whom, never the password.

### Connection options

| Key | Meaning |
|---|---|
| `engine` | `postgres`, `mysql`, `mssql` or `sqlite` |
| `host`, `port`, `database`, `username`, `password` | literal values or `op://vault/item/field` references |
| `instance` | SQL Server named instance |
| `path` | SQLite database file |
| `tls` | `disable`, `prefer`, `require` (encrypt, do not check the certificate) or `verify`. Default: `prefer`, and `require` for SQL Server |
| `ca_file` | CA certificate to trust with `tls = "verify"` |
| `read_intent` | SQL Server: ask an availability group for a read-only replica |

### Settings

```toml
[settings]
page_size = 200            # rows per page
query_timeout_secs = 30    # a query is abandoned, and stopped on the server, after this
count_timeout_secs = 5     # budget for the row count in the status bar; 0 turns it off
max_rows = 5000            # cap for hand-written SQL that has no LIMIT
max_column_width = 40
```

## Use

```sh
kikkert                # choose a connection
kikkert orders-prod    # connect straight away
kikkert --list         # print the configured connections
```

Press `?` at any time for the full list of keys.

| Key | What it does |
|---|---|
| `Tab` | switch between the table list and the data |
| `t` | find a table by typing part of its name |
| `Enter` | open the table; on data, show the whole row |
| arrows or `h j k l`, `g G`, `0 $` | move around |
| `]` `[` | next and previous page |
| `/` | filter: column, how to match, value |
| `=` `!` | keep, or drop, rows with the value under the cursor |
| `F` `X` | list and remove filters; clear them all |
| `s` `S` | sort by this column; add it as a further sort key |
| `d` | values of this column that occur more than once |
| `D` | every value of this column with its count |
| `Space` | mark columns, to check a combination for duplicates |
| `Backspace` | go back |
| `1`–`5` | Data, Columns, Indexes, Keys, Definition |
| `e` | edit the SQL; `F5` or `Ctrl-R` runs it |
| `v` | hold this table's columns; on another table, compare the two |
| `V` | hold this table's columns in place of the ones held |
| `y` `Y` | copy the cell or the row |
| `Esc` | cancel a running query |
| `c` | open another connection alongside this one; `Esc` comes back |
| `C` | open another connection in place of this one, which closes once the new one is connected |
| `r` `R` | next and previous open connection |
| `F5` or `Ctrl-R` | run again |
| `q` | close this connection; the last one goes back to the connection list |
| `Q` | quit |

Each open connection gets a tab in the title bar. Queries keep running in a
connection you have switched away from, and its tab shows a spinner until
they finish.

`v` holds the columns of a table, the one open or the one picked in the table
list, and `v` on another table shows the two side by side: names, types,
whether they take NULL, defaults and keys, never the rows. The other table can
be in another connection, even on another engine. Types and defaults that two
engines spell differently, such as `integer` and `int` or `true` and `1`, are
marked as alike rather than different, and `d` narrows the list to the real
differences. The held table is named in the title bar until `V` holds another.

Filtering "contains" works on any column, not only text: numbers, dates and
ids are compared as text, so `2024-03` finds March and `42` finds order 1042.

kikkert draws in its own colours rather than the terminal theme's sixteen,
which themes remap too freely to keep dimmed text and the selection readable.
Terminals that say they take 24-bit colour (`COLORTERM=truecolor`, or kitty,
WezTerm, Ghostty, iTerm2, Windows Terminal) get the palette as designed, others
the nearest of the standard 256. It is made for a dark background.

## How read-only is enforced

There is no write path in kikkert: it has no feature that inserts, updates or
deletes. On top of that there are three independent layers.

1. **Statement guard.** Every statement that is not one of kikkert's own
   catalog queries is parsed before it is sent. Exactly one statement is
   allowed, and it must be a `SELECT` (or `EXPLAIN`, `SHOW`, `DESCRIBE` of
   one). Writes hidden inside a query are refused too: data-modifying CTEs,
   `SELECT ... INTO`, locking clauses, and functions known to change state.
   A statement the parser cannot understand is refused, not passed along.
2. **Read-only session**, enforced by the database itself:

   | Engine | Mechanism |
   |---|---|
   | PostgreSQL | every statement runs in `BEGIN TRANSACTION READ ONLY` |
   | MySQL / MariaDB | `SET SESSION TRANSACTION READ ONLY`; the connection is dropped if that fails |
   | SQLite | the file is opened read-only, with `query_only` on |
   | SQL Server | no read-only session exists; each statement runs in a transaction that is always rolled back |

3. **A read-only login.** This is the one guarantee that does not depend on
   kikkert being right, and for SQL Server it is the only server-side one.
   Give the account `SELECT` only (`db_datareader`, `pg_read_all_data`,
   `GRANT SELECT`).

The engine tests bypass layer 1 on purpose and confirm that `INSERT`, `UPDATE`,
`DELETE` and `CREATE TABLE` still change nothing on any of the four engines:
three refuse them outright, and on SQL Server they are rolled back.

Being read-only is not the same as being harmless: a heavy query still costs
the server. Queries have a timeout, `Esc` cancels on the server as well as in
the interface, and on SQL Server kikkert sets a lock timeout and low deadlock
priority so it loses to real workloads.

## Not there yet

- SQL Server logs in with a SQL username and password only: no Windows
  integrated authentication or Entra ID tokens.
- No SSH tunnels. Open one yourself and point the connection at the local port.
- No export to a file; `y` and `Y` copy to the clipboard.
- The filter builder works on tables and views. Hand-written SQL is run as
  written and is not filtered or paged further.

## Development

Tasks are in the `justfile`; `just --list` shows them all.

```sh
just              # clippy and unit tests
just fmt          # format
just test guard   # unit tests matching "guard"; no database needed
just deny         # advisories, licences and sources of the dependencies
just ci           # formatting, clippy, dependencies, unit tests and engine tests
```

GitHub Actions (`.github/workflows/ci.yml`) runs the same checks on every push to
`main` and on pull requests.

The engine tests run against real databases: Postgres, MySQL and SQL Server
in containers, plus a SQLite file.

```sh
just test-dbs     # starts the databases, runs the tests, removes them again
just dbs-up       # or keep them running while you work...
just test-dbs     # ...and the tests reuse them and leave them up
just dev          # kikkert pointed at the test databases, to look around in
just dbs-down
```

The containers need podman or docker (`CONTAINER_ENGINE` chooses), listen on
127.0.0.1 only, and starting the SQL Server one accepts Microsoft's developer
EULA. Without `just`, `scripts/test-dbs.sh up|status|test|down` does the same.

The 1Password integration has only been tested against a stand-in for `op`
that returns items in the documented JSON format, not against a live account.

## License

Apache License 2.0, see [LICENSE](LICENSE).
