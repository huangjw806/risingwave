// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use pgwire::pg_response::{PgResponse, StatementType};
use risingwave_common::error::{ErrorCode, Result, RwError};
use risingwave_pb::stream_plan::source_node::SourceType;
use risingwave_sqlparser::ast::ObjectName;

use crate::binder::Binder;
use crate::session::OptimizerContext;

pub async fn handle_drop_table(
    context: OptimizerContext,
    table_name: ObjectName,
) -> Result<PgResponse> {
    let session = context.session_ctx;
    let (schema_name, table_name) = Binder::resolve_table_name(table_name)?;

    let catalog_reader = session.env().catalog_reader();

    {
        let reader = catalog_reader.read_guard();
        if let Ok(s) = reader.get_source_by_name(session.database(), &schema_name, &table_name) {
            if s.source_type == SourceType::Source {
                return Err(RwError::from(ErrorCode::InvalidInputSyntax(
                    "Use `DROP SOURCE` to drop a source.".to_owned(),
                )));
            }
        }
    }

    let (source_id, table_id) = {
        let reader = catalog_reader.read_guard();
        let table = reader.get_table_by_name(session.database(), &schema_name, &table_name)?;

        // If associated source is `None`, then it is a normal mview.
        match table.associated_source_id() {
            Some(source_id) => (source_id, table.id()),
            None => {
                return Err(RwError::from(ErrorCode::InvalidInputSyntax(
                    "Use `DROP MATERIALIZED VIEW` to drop a materialized view.".to_owned(),
                )))
            }
        }
    };

    let catalog_writer = session.env().catalog_writer();
    catalog_writer
        .drop_materialized_source(source_id.table_id(), table_id)
        .await?;

    Ok(PgResponse::empty_result(StatementType::DROP_TABLE))
}

#[cfg(test)]
mod tests {
    use risingwave_common::catalog::{DEFAULT_DATABASE_NAME, DEFAULT_SCHEMA_NAME};

    use crate::test_utils::LocalFrontend;

    #[tokio::test]
    async fn test_drop_table_handler() {
        let sql_create_table = "create table t (v1 smallint);";
        let sql_drop_table = "drop table t;";
        let frontend = LocalFrontend::new(Default::default()).await;
        frontend.run_sql(sql_create_table).await.unwrap();
        frontend.run_sql(sql_drop_table).await.unwrap();

        let session = frontend.session_ref();
        let catalog_reader = session.env().catalog_reader();

        let source = catalog_reader
            .read_guard()
            .get_source_by_name(DEFAULT_DATABASE_NAME, DEFAULT_SCHEMA_NAME, "t")
            .ok()
            .cloned();
        assert!(source.is_none());

        let table = catalog_reader
            .read_guard()
            .get_table_by_name(DEFAULT_DATABASE_NAME, DEFAULT_SCHEMA_NAME, "t")
            .ok()
            .cloned();
        assert!(table.is_none());
    }
}
