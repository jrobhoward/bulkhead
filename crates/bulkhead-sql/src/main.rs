//! `bulkhead-sql`: a datasource's Postgres client as a driver app. The
//! node runs one per datasource; it is not started by hand.

bulkhead_sdk::app!(bulkhead_sql::Sql);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
