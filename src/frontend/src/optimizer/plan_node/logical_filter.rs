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

use super::{
    ColPrunable, CollectInputRef, LogicalProject, PlanBase, PlanRef, PlanTreeNodeUnary, ToBatch,
    ToStream,
};
use crate::expr::{assert_input_ref, ExprImpl};
use crate::optimizer::plan_node::{BatchFilter, StreamFilter};
use crate::utils::{ColIndexMapping, Condition};

/// `LogicalFilter` iterates over its input and returns elements for which `predicate` evaluates to
/// true, filtering out the others.
///
/// If the condition allows nulls, then a null value is treated the same as false.
#[derive(Debug, Clone)]
pub struct LogicalFilter {
    pub base: PlanBase,
    predicate: Condition,
    input: PlanRef,
}

impl LogicalFilter {
    pub fn new(input: PlanRef, predicate: Condition) -> Self {
        let ctx = input.ctx();
        for cond in &predicate.conjunctions {
            assert_input_ref!(cond, input.schema().fields().len());
        }
        let schema = input.schema().clone();
        let pk_indices = input.pk_indices().to_vec();
        let base = PlanBase::new_logical(ctx, schema, pk_indices);
        LogicalFilter {
            base,
            predicate,
            input,
        }
    }

    /// Create a `LogicalFilter` unless the predicate is always true
    pub fn create(input: PlanRef, predicate: Condition) -> PlanRef {
        if predicate.always_true() {
            input
        } else {
            LogicalFilter::new(input, predicate).into()
        }
    }

    /// the function will check if the predicate is bool expression
    pub fn create_with_expr(input: PlanRef, predicate: ExprImpl) -> PlanRef {
        let predicate = Condition::with_expr(predicate);
        Self::new(input, predicate).into()
    }

    /// Get the predicate of the logical join.
    pub fn predicate(&self) -> &Condition {
        &self.predicate
    }
}

impl PlanTreeNodeUnary for LogicalFilter {
    fn input(&self) -> PlanRef {
        self.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        Self::new(input, self.predicate.clone())
    }

    #[must_use]
    fn rewrite_with_input(
        &self,
        input: PlanRef,
        mut input_col_change: ColIndexMapping,
    ) -> (Self, ColIndexMapping) {
        let predicate = self.predicate().clone().rewrite_expr(&mut input_col_change);
        (Self::new(input, predicate), input_col_change)
    }
}

impl_plan_tree_node_for_unary! {LogicalFilter}

impl fmt::Display for LogicalFilter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LogicalFilter {{ predicate: {} }}", self.predicate)
    }
}

impl ColPrunable for LogicalFilter {
    fn prune_col(&self, required_cols: &[usize]) -> PlanRef {
        let required_col_bitset = FixedBitSet::with_capacity(self.input.schema().len());
        let mut visitor = CollectInputRef::new(required_col_bitset);
        self.predicate.visit_expr(&mut visitor);
        let input_required_cols: FixedBitSet = visitor.into();
        let input_required_cols: Vec<_> = input_required_cols.ones().collect();
        let filter = LogicalFilter::new(
            self.input.prune_col(&input_required_cols),
            self.predicate.clone(),
        );

        if required_cols == input_required_cols {
            filter.into()
        } else {
            let mut remaining_columns = FixedBitSet::with_capacity(filter.schema().fields().len());
            remaining_columns.extend(required_cols.iter().copied());
            LogicalProject::with_mapping(
                filter.into(),
                ColIndexMapping::with_remaining_columns(&remaining_columns),
            )
        }
    }
}

impl ToBatch for LogicalFilter {
    fn to_batch(&self) -> PlanRef {
        let new_input = self.input().to_batch();
        let new_logical = self.clone_with_input(new_input);
        BatchFilter::new(new_logical).into()
    }
}

impl ToStream for LogicalFilter {
    fn to_stream(&self) -> PlanRef {
        let new_input = self.input().to_stream();
        let new_logical = self.clone_with_input(new_input);
        StreamFilter::new(new_logical).into()
    }

    fn logical_rewrite_for_stream(&self) -> (PlanRef, ColIndexMapping) {
        let (input, input_col_change) = self.input.logical_rewrite_for_stream();
        let (filter, out_col_change) = self.rewrite_with_input(input, input_col_change);
        (filter.into(), out_col_change)
    }
}

#[cfg(test)]
mod tests {

    use risingwave_common::catalog::{Field, Schema};
    use risingwave_common::types::DataType;
    use risingwave_pb::expr::expr_node::Type;

    use super::*;
    use crate::expr::{assert_eq_input_ref, FunctionCall, InputRef, Literal};
    use crate::optimizer::plan_node::LogicalValues;
    use crate::session::OptimizerContext;

    #[tokio::test]
    /// Pruning
    /// ```text
    /// Filter(cond: input_ref(1)<5)
    ///   TableScan(v1, v2, v3)
    /// ```
    /// with required columns [2] will result in
    /// ```text
    /// Project(input_ref(1))
    ///   Filter(cond: input_ref(0)<5)
    ///     TableScan(v2, v3)
    /// ```
    async fn test_prune_filter() {
        let ctx = OptimizerContext::mock().await;
        let fields: Vec<Field> = vec![
            Field::with_name(DataType::Int32, "v1"),
            Field::with_name(DataType::Int32, "v2"),
            Field::with_name(DataType::Int32, "v3"),
        ];
        let values = LogicalValues::new(
            vec![],
            Schema {
                fields: fields.clone(),
            },
            ctx,
        );
        let predicate: ExprImpl = ExprImpl::FunctionCall(Box::new(
            FunctionCall::new(
                Type::LessThan,
                vec![
                    ExprImpl::InputRef(Box::new(InputRef::new(1, DataType::Int32))),
                    ExprImpl::Literal(Box::new(Literal::new(None, DataType::Int32))),
                ],
            )
            .unwrap(),
        ));
        let filter = LogicalFilter::new(values.into(), Condition::with_expr(predicate));

        // Perform the prune
        let required_cols = vec![2];
        let plan = filter.prune_col(&required_cols);

        // Check the result
        let project = plan.as_logical_project().unwrap();
        assert_eq!(project.exprs().len(), 1);
        assert_eq_input_ref!(&project.exprs()[0], 1);

        let filter = project.input();
        let filter = filter.as_logical_filter().unwrap();
        assert_eq!(filter.schema().fields().len(), 2);
        assert_eq!(filter.schema().fields()[0], fields[1]);
        assert_eq!(filter.schema().fields()[1], fields[2]);
        assert_eq!(filter.id().0, 3);

        let expr: ExprImpl = filter.predicate.clone().into();
        let call = expr.as_function_call().unwrap();
        assert_eq_input_ref!(&call.inputs()[0], 0);
        let values = filter.input();
        let values = values.as_logical_values().unwrap();
        assert_eq!(values.schema().fields().len(), 2);
        assert_eq!(values.schema().fields()[0], fields[1]);
        assert_eq!(values.schema().fields()[1], fields[2]);
    }

    #[tokio::test]
    /// Pruning
    /// ```text
    /// Filter(cond: input_ref(1)<5)
    ///   TableScan(v1, v2, v3)
    /// ```
    /// with required columns [1, 2] will result in
    /// ```text
    ///   Filter(cond: input_ref(0)<5)
    ///     TableScan(v2, v3)
    /// ```
    async fn test_prune_filter_no_project() {
        let ctx = OptimizerContext::mock().await;
        let ty = DataType::Int32;
        let fields: Vec<Field> = vec![
            Field::with_name(ty.clone(), "v1"),
            Field::with_name(ty.clone(), "v2"),
            Field::with_name(ty.clone(), "v3"),
        ];
        let values = LogicalValues::new(
            vec![],
            Schema {
                fields: fields.clone(),
            },
            ctx,
        );

        let predicate: ExprImpl = ExprImpl::FunctionCall(Box::new(
            FunctionCall::new(
                Type::LessThan,
                vec![
                    ExprImpl::InputRef(Box::new(InputRef::new(1, ty.clone()))),
                    ExprImpl::Literal(Box::new(Literal::new(None, ty))),
                ],
            )
            .unwrap(),
        ));
        let filter = LogicalFilter::new(values.into(), Condition::with_expr(predicate));

        // Perform the prune
        let required_cols = vec![1, 2];
        let plan = filter.prune_col(&required_cols);

        // Check the result
        let filter = plan.as_logical_filter().unwrap();
        assert_eq!(filter.schema().fields().len(), 2);
        assert_eq!(filter.schema().fields()[0], fields[1]);
        assert_eq!(filter.schema().fields()[1], fields[2]);
        let expr: ExprImpl = filter.predicate.clone().into();
        let call = expr.as_function_call().unwrap();
        assert_eq_input_ref!(&call.inputs()[0], 0);

        let values = filter.input();
        let values = values.as_logical_values().unwrap();
        assert_eq!(values.schema().fields().len(), 2);
        assert_eq!(values.schema().fields()[0], fields[1]);
        assert_eq!(values.schema().fields()[1], fields[2]);
    }
}
