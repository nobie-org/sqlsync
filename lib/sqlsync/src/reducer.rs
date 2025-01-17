use std::collections::BTreeMap;

use rusqlite::{
    params_from_iter,
    types::{Value, ValueRef},
    Transaction,
};
use sqlsync_reducer::{
    host_ffi::{register_log_handler, WasmFFI, WasmFFIError},
    types::{
        ErrorResponse, ExecResponse, QueryResponse, Request, Row, SqliteValue,
    },
};
use thiserror::Error;
use wasmi::{errors::LinkerError, Engine, Linker, Module, Store};

use crate::unixtime::unix_timestamp_milliseconds;

#[derive(Error, Debug)]
pub enum ReducerError {
    #[error(transparent)]
    Link(#[from] LinkerError),

    #[error(transparent)]
    Runtime(#[from] wasmi::Error),

    #[error(transparent)]
    Interface(#[from] WasmFFIError),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

type Result<T> = std::result::Result<T, ReducerError>;
type SqlResult<T> = std::result::Result<T, ErrorResponse>;

pub struct Reducer {
    store: Store<WasmFFI>,
}

impl Reducer {
    pub fn new(wasm_bytes: impl std::io::Read) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::new(&engine, wasm_bytes)?;

        let mut linker = Linker::new(&engine);
        register_log_handler(&mut linker)?;

        let mut store = Store::new(&engine, WasmFFI::uninitialized());
        let instance =
            linker.instantiate(&mut store, &module)?.start(&mut store)?;

        // initialize the FFI
        let ffi = WasmFFI::initialized(&store, &instance)?;
        (*store.data_mut()) = ffi.clone();

        // initialize the reducer
        ffi.init_reducer(&mut store)?;

        Ok(Self { store })
    }

    pub fn apply(
        &mut self,
        tx: &mut Transaction,
        mutation: &[u8],
    ) -> Result<()> {
        let ffi = self.store.data().to_owned();

        // start the reducer
        let mut requests = ffi.reduce(&mut self.store, mutation)?;

        while let Some(requests_inner) = requests {
            // process requests
            let mut responses = BTreeMap::new();
            for (id, req) in requests_inner {
                match req {
                    Request::Query { sql, params } => {
                        let response = self.run_query(tx, &sql, params);
                        let ptr = ffi.encode(&mut self.store, &response)?;
                        responses.insert(id, ptr);
                    }
                    Request::Exec { sql, params } => {
                        let response = self.run_exec(tx, &sql, params);
                        let ptr = ffi.encode(&mut self.store, &response)?;
                        responses.insert(id, ptr);
                    }
                }
            }

            // step the reactor forward
            requests = ffi.reactor_step(&mut self.store, Some(responses))?;
        }

        Ok(())
    }

    fn run_query(
        &mut self,
        tx: &mut Transaction,
        sql: &str,
        params: Vec<SqliteValue>,
    ) -> SqlResult<QueryResponse> {
        log::info!("received query req: {}, {:?}", sql, params);
        let params =
            params_from_iter(params.into_iter().map(from_sqlite_value));
        let mut stmt =
            tx.prepare(&sql).map_err(rusqlite_err_to_response_err)?;

        let columns: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let num_columns = columns.len();

        let start = unix_timestamp_milliseconds();

        let rows = stmt
            .query_and_then(params, move |row| {
                (0..num_columns)
                    .map(|i| Ok(to_sqlite_value(row.get_ref(i)?)))
                    .collect::<std::result::Result<Row, rusqlite::Error>>()
            })
            .map_err(rusqlite_err_to_response_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(rusqlite_err_to_response_err)?;

        let end = unix_timestamp_milliseconds();
        log::info!("query took {}ms", end - start);

        Ok(QueryResponse { columns, rows })
    }

    fn run_exec(
        &mut self,
        tx: &mut Transaction,
        sql: &str,
        params: Vec<SqliteValue>,
    ) -> SqlResult<ExecResponse> {
        log::info!("received exec req: {}, {:?}", sql, params);
        let params =
            params_from_iter(params.into_iter().map(from_sqlite_value));

        let start = unix_timestamp_milliseconds();

        let changes = tx
            .execute(&sql, params)
            .map_err(rusqlite_err_to_response_err)?;

        let end = unix_timestamp_milliseconds();
        log::info!("exec took {}ms", end - start);

        Ok(ExecResponse { changes })
    }
}

#[inline]
fn from_sqlite_value(v: SqliteValue) -> Value {
    match v {
        SqliteValue::Null => Value::Null,
        SqliteValue::Integer(i) => Value::Integer(i),
        SqliteValue::Real(f) => Value::Real(f),
        SqliteValue::Text(s) => Value::Text(s),
        SqliteValue::Blob(b) => Value::Blob(b),
    }
}

#[inline]
fn to_sqlite_value(v: ValueRef) -> SqliteValue {
    match v {
        ValueRef::Null => SqliteValue::Null,
        ValueRef::Integer(i) => SqliteValue::Integer(i),
        ValueRef::Real(f) => SqliteValue::Real(f),
        r @ ValueRef::Text(_) => {
            SqliteValue::Text(r.as_str().unwrap().to_owned())
        }
        ValueRef::Blob(b) => SqliteValue::Blob(b.to_vec()),
    }
}

fn rusqlite_err_to_response_err(e: rusqlite::Error) -> ErrorResponse {
    match e {
        rusqlite::Error::SqliteFailure(e, _) => ErrorResponse::SqliteError {
            code: e.extended_code,
            message: format!("{}", e),
        },
        other => ErrorResponse::Unknown(format!("{}", other)),
    }
}
