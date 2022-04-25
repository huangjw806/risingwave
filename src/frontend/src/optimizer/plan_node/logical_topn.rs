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

use std::fmt;

use fixedbitset::FixedBitSet;

use super::{ColPrunable, PlanBase, PlanRef, PlanTreeNodeUnary, ToBatch, ToStream};
use crate::optimizer::plan_node::{BatchTopN, LogicalProject, StreamTopN};
use crate::optimizer::property::{Distribution, FieldOrder, Order};
use crate::utils::ColIndexMapping;

/// `LogicalTopN` sorts the input data and fetches up to `limit` rows from `offset`
#[derive(Debug, Clone)]
pub struct LogicalTopN {
    pub base: PlanBase,
    input: PlanRef,
    limit: usize,
    offset: usize,
    order: Order,
}

impl LogicalTopN {
    fn new(input: PlanRef, limit: usize, offset: usize, order: Order) -> Self {
        let ctx = input.ctx();
        let schema = input.schema().clone();
        let pk_indices = input.pk_indices().to_vec();
        let base = PlanBase::new_logical(ctx, schema, pk_indices);
        LogicalTopN {
            base,
            input,
            limit,
            offset,
            order,
        }
    }

    /// the function will check if the cond is bool expression
    pub fn create(input: PlanRef, limit: usize, offset: usize, order: Order) -> PlanRef {
        Self::new(input, limit, offset, order).into()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// `topn_order` returns the order of the Top-N operator. This naming is because `order()`
    /// already exists and it was designed to return the operator's physical property order.
    ///
    /// Note that `order()` and `topn_order()` may differ. For streaming query, `order()` which
    /// implies the output ordering of an operator, is never guaranteed; while `topn_order()` must
    /// be non-null because it's a critical information for Top-N operators to work
    pub fn topn_order(&self) -> &Order {
        &self.order
    }
}

impl PlanTreeNodeUnary for LogicalTopN {
    fn input(&self) -> PlanRef {
        self.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        Self::new(input, self.limit, self.offset, self.order.clone())
    }

    #[must_use]
    fn rewrite_with_input(
        &self,
        input: PlanRef,
        input_col_change: ColIndexMapping,
    ) -> (Self, ColIndexMapping) {
        (
            Self::new(
                input,
                self.limit,
                self.offset,
                input_col_change
                    .rewrite_required_order(&self.order)
                    .unwrap(),
            ),
            input_col_change,
        )
    }
}
impl_plan_tree_node_for_unary! {LogicalTopN}
impl fmt::Display for LogicalTopN {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "LogicalTopN {{ order: {}, limit: {}, offset: {} }}",
            &self.order, &self.limit, &self.offset,
        )
    }
}

impl ColPrunable for LogicalTopN {
    fn prune_col(&self, required_cols: &[usize]) -> PlanRef {
        let mut input_required_cols = FixedBitSet::from_iter(required_cols.iter().copied());
        self.order
            .field_order
            .iter()
            .for_each(|fo| input_required_cols.insert(fo.index));

        let mapping = ColIndexMapping::with_remaining_columns(&input_required_cols);
        let new_order = Order {
            field_order: self
                .order
                .field_order
                .iter()
                .map(|fo| FieldOrder {
                    index: mapping.map(fo.index),
                    direct: fo.direct,
                })
                .collect(),
        };
        let new_input = self.input.prune_col(required_cols);
        let top_n = Self::new(new_input, self.limit, self.offset, new_order).into();
        let input_required_cols: Vec<_> = input_required_cols.ones().collect();

        if required_cols == input_required_cols {
            top_n
        } else {
            let mut remaining_columns = FixedBitSet::with_capacity(top_n.schema().fields().len());
            remaining_columns.extend(required_cols.iter().map(|i| mapping.map(*i)));
            LogicalProject::with_mapping(
                top_n,
                ColIndexMapping::with_remaining_columns(&remaining_columns),
            )
        }
    }
}

impl ToBatch for LogicalTopN {
    fn to_batch(&self) -> PlanRef {
        self.to_batch_with_order_required(Order::any())
    }

    fn to_batch_with_order_required(&self, required_order: &Order) -> PlanRef {
        let new_input = self.input().to_batch();
        let new_logical = self.clone_with_input(new_input);
        let ret = BatchTopN::new(new_logical).into();

        if self.topn_order().satisfies(required_order) {
            ret
        } else {
            required_order.enforce(ret)
        }
    }
}

impl ToStream for LogicalTopN {
    fn to_stream(&self) -> PlanRef {
        // Unlike `BatchTopN`, `StreamTopN` cannot guarantee the output order
        let input = self
            .input()
            .to_stream_with_dist_required(&Distribution::Single);
        StreamTopN::new(self.clone_with_input(input)).into()
    }

    fn logical_rewrite_for_stream(&self) -> (PlanRef, ColIndexMapping) {
        let (input, input_col_change) = self.input.logical_rewrite_for_stream();
        let (top_n, out_col_change) = self.rewrite_with_input(input, input_col_change);
        (top_n.into(), out_col_change)
    }
}
