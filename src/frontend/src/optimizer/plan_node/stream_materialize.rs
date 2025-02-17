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

use std::collections::HashMap;
use std::fmt;

use fixedbitset::FixedBitSet;
use itertools::Itertools;
use risingwave_common::catalog::{ColumnDesc, OrderedColumnDesc, TableId};
use risingwave_common::error::ErrorCode::InternalError;
use risingwave_common::error::Result;
use risingwave_common::util::sort_util::OrderType;
use risingwave_pb::expr::InputRefExpr;
use risingwave_pb::plan_common::ColumnOrder;
use risingwave_pb::stream_plan::stream_node::NodeBody as ProstStreamNode;

use super::{PlanRef, PlanTreeNodeUnary, ToStreamProst};
use crate::catalog::column_catalog::ColumnCatalog;
use crate::catalog::table_catalog::TableCatalog;
use crate::catalog::ColumnId;
use crate::optimizer::plan_node::{PlanBase, PlanNode};
use crate::optimizer::property::{Distribution, Order};

/// Materializes a stream.
#[derive(Debug, Clone)]
pub struct StreamMaterialize {
    pub base: PlanBase,
    /// Child of Materialize plan
    input: PlanRef,
    table: TableCatalog,
}

impl StreamMaterialize {
    fn derive_plan_base(input: &PlanRef) -> Result<PlanBase> {
        let ctx = input.ctx();

        let schema = input.schema().clone();
        let pk_indices = input.pk_indices();

        // Materialize executor won't change the append-only behavior of the stream, so it depends
        // on input's `append_only`.
        Ok(PlanBase::new_stream(
            ctx,
            schema,
            pk_indices.to_vec(),
            input.distribution().clone(),
            input.append_only(),
        ))
    }

    #[must_use]
    pub fn new(input: PlanRef, table: TableCatalog) -> Self {
        let base = Self::derive_plan_base(&input).unwrap();
        Self { base, input, table }
    }

    /// Create a materialize node.
    ///
    /// When creating index, `is_index` should be true. Then, materialize will distribute keys
    /// using order by columns, instead of pk.
    pub fn create(
        input: PlanRef,
        mv_name: String,
        user_order_by: Order,
        user_cols: FixedBitSet,
        out_names: Vec<String>,
        is_index_on: Option<TableId>,
    ) -> Result<Self> {
        // ensure the same pk will not shuffle to different node
        let input = match input.distribution() {
            Distribution::Single => input,
            _ => Distribution::HashShard(if is_index_on.is_some() {
                user_order_by.field_order.iter().map(|x| x.index).collect()
            } else {
                input.pk_indices().to_vec()
            })
            .enforce_if_not_satisfies(input, Order::any())?,
        };

        let base = Self::derive_plan_base(&input)?;
        let schema = &base.schema;
        let pk_indices = &base.pk_indices;

        let mut col_names = HashMap::new();
        for name in &out_names {
            if col_names.try_insert(name.clone(), 0).is_err() {
                return Err(
                    InternalError(format!("column {} specified more than once", name)).into(),
                );
            }
        }
        let mut out_name_iter = out_names.into_iter();
        let mut columns = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let mut c = ColumnCatalog {
                    column_desc: ColumnDesc::from_field_without_column_id(field),
                    is_hidden: !user_cols.contains(i),
                };
                c.column_desc.name = if !c.is_hidden {
                    out_name_iter.next().unwrap()
                } else {
                    match col_names.try_insert(field.name.clone(), 0) {
                        Ok(_) => field.name.clone(),
                        Err(mut err) => {
                            let cnt = err.entry.get_mut();
                            *cnt += 1;
                            field.name.clone() + "#" + &cnt.to_string()
                        }
                    }
                };
                c
            })
            .collect_vec();

        // Since the `field.into()` only generate same ColumnId,
        // so rewrite ColumnId for each `column_desc` and `column_desc.field_desc`.
        ColumnCatalog::generate_increment_id(&mut columns);

        let mut in_order = FixedBitSet::with_capacity(schema.len());
        let mut order_desc = vec![];

        for field in &user_order_by.field_order {
            let idx = field.index;
            order_desc.push(OrderedColumnDesc {
                column_desc: columns[idx].column_desc.clone(),
                order: field.direct.into(),
            });
            in_order.insert(idx);
        }

        for &idx in pk_indices {
            if in_order.contains(idx) {
                continue;
            }
            order_desc.push(OrderedColumnDesc {
                column_desc: columns[idx].column_desc.clone(),
                order: OrderType::Ascending,
            });
            in_order.insert(idx);
        }

        let table = TableCatalog {
            id: TableId::placeholder(),
            associated_source_id: None,
            name: mv_name,
            columns,
            order_desc,
            pks: pk_indices.clone(),
            is_index_on,
            distribution_keys: base.dist.dist_column_indices().to_vec(),
        };

        Ok(Self { base, input, table })
    }

    /// Get a reference to the stream materialize's table.
    #[must_use]
    pub fn table(&self) -> &TableCatalog {
        &self.table
    }

    pub fn name(&self) -> &str {
        self.table.name()
    }

    /// XXX(st1page): this function is used for potential DDL demand in future, and please try your
    /// best not convert `ColumnId` to `usize(col_index`)
    fn col_id_to_idx(&self, id: ColumnId) -> usize {
        id.get_id() as usize
    }
}

impl fmt::Display for StreamMaterialize {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let table = self.table();

        let column_names = table
            .columns()
            .iter()
            .map(|c| c.name_with_hidden())
            .join(", ");

        let pk_column_names = table
            .pks
            .iter()
            .map(|&pk| &table.columns[pk].column_desc.name)
            .join(", ");

        let order_descs = table
            .order_desc
            .iter()
            .map(|order| &order.column_desc.name)
            .join(", ");

        if pk_column_names != order_descs {
            write!(
                f,
                "StreamMaterialize {{ columns: [{}], pk_columns: [{}], order_descs: [{}] }}",
                column_names, pk_column_names, order_descs
            )
        } else {
            write!(
                f,
                "StreamMaterialize {{ columns: [{}], pk_columns: [{}] }}",
                column_names, pk_column_names
            )
        }
    }
}

impl PlanTreeNodeUnary for StreamMaterialize {
    fn input(&self) -> PlanRef {
        self.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        let new = Self::new(input, self.table().clone());
        assert_eq!(new.plan_base().schema, self.plan_base().schema);
        assert_eq!(new.plan_base().pk_indices, self.plan_base().pk_indices);
        new
    }
}

impl_plan_tree_node_for_unary! { StreamMaterialize }

impl ToStreamProst for StreamMaterialize {
    fn to_stream_prost_body(&self) -> ProstStreamNode {
        use risingwave_pb::stream_plan::*;

        ProstStreamNode::Materialize(MaterializeNode {
            // We don't need table id for materialize node in frontend. The id will be generated on
            // meta catalog service.
            table_ref_id: None,
            associated_table_ref_id: None,
            column_ids: self
                .table()
                .columns()
                .iter()
                .map(|col| ColumnId::get_id(&col.column_desc.column_id))
                .collect(),
            column_orders: self
                .table()
                .order_desc()
                .iter()
                .map(|col| {
                    let idx = self.col_id_to_idx(col.column_desc.column_id);
                    ColumnOrder {
                        order_type: col.order.to_prost() as i32,
                        input_ref: Some(InputRefExpr {
                            column_idx: idx as i32,
                        }),
                        return_type: Some(col.column_desc.data_type.to_protobuf()),
                    }
                })
                .collect(),
            distribution_keys: self
                .base
                .dist
                .dist_column_indices()
                .iter()
                .map(|idx| *idx as i32)
                .collect_vec(),
        })
    }
}
