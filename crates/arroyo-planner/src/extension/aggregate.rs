use std::{fmt::Formatter, sync::Arc, time::Duration};

use arrow::datatypes::IntervalMonthDayNanoType;

use arroyo_datastream::{
    logical::{LogicalEdge, LogicalEdgeType, LogicalNode, OperatorName},
    WindowType,
};
use arroyo_rpc::{
    df::{ArroyoSchema, ArroyoSchemaRef},
    grpc::api::{
        SessionWindowAggregateOperator, SlidingWindowAggregateOperator,
        TumblingWindowAggregateOperator,
    },
    TIMESTAMP_FIELD,
};
use datafusion::common::{
    internal_err, plan_err, Column, DFSchema, DFSchemaRef, Result, ScalarValue,
};
use datafusion::error::DataFusionError;
use datafusion::logical_expr;
use datafusion::logical_expr::{
    expr::ScalarFunction, Aggregate, BinaryExpr, Expr, Extension, LogicalPlan,
    UserDefinedLogicalNodeCore,
};
use datafusion_proto::physical_plan::to_proto::serialize_physical_expr;
use datafusion_proto::physical_plan::DefaultPhysicalExtensionCodec;
use datafusion_proto::{physical_plan::AsExecutionPlan, protobuf::PhysicalPlanNode};
use prost::Message;

use super::{ArroyoExtension, NodeWithIncomingEdges, TimestampAppendExtension};
use crate::physical::window;
use crate::{
    builder::{NamedNode, Planner, SplitPlanOutput},
    fields_with_qualifiers, multifield_partial_ord,
    physical::ArroyoPhysicalExtensionCodec,
    schema_from_df_fields, schema_from_df_fields_with_metadata, DFField, WindowBehavior,
};

pub(crate) const AGGREGATE_EXTENSION_NAME: &str = "AggregateExtension";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AggregateExtension {
    pub(crate) window_behavior: WindowBehavior,
    pub(crate) aggregate: LogicalPlan,
    pub(crate) schema: DFSchemaRef,
    pub(crate) key_fields: Vec<usize>,
    pub(crate) final_calculation: LogicalPlan,
}

multifield_partial_ord!(AggregateExtension, aggregate, key_fields, final_calculation);

impl AggregateExtension {
    pub fn new(
        window_behavior: WindowBehavior,
        aggregate: LogicalPlan,
        key_fields: Vec<usize>,
    ) -> Self {
        let final_calculation =
            Self::final_projection(&aggregate, window_behavior.clone()).unwrap();

        Self {
            window_behavior,
            aggregate,
            schema: final_calculation.schema().clone(),
            key_fields,
            final_calculation,
        }
    }

    pub fn tumbling_window_config(
        &self,
        planner: &Planner,
        index: usize,
        input_schema: DFSchemaRef,
        width: Duration,
    ) -> Result<LogicalNode> {
        let binning_function_proto = planner.binning_function_proto(width, input_schema.clone())?;
        let SplitPlanOutput {
            partial_aggregation_plan,
            partial_schema,
            finish_plan,
        } = planner.split_physical_plan(self.key_fields.clone(), &self.aggregate, true)?;

        let final_physical_plan = planner.sync_plan(&self.final_calculation)?;
        let final_physical_plan_node = PhysicalPlanNode::try_from_physical_plan(
            final_physical_plan,
            &ArroyoPhysicalExtensionCodec::default(),
        )?;

        let config = TumblingWindowAggregateOperator {
            name: "TumblingWindow".to_string(),
            width_micros: width.as_micros() as u64,
            binning_function: binning_function_proto.encode_to_vec(),
            input_schema: Some(
                ArroyoSchema::from_schema_keys(
                    Arc::new(input_schema.as_ref().into()),
                    self.key_fields.clone(),
                )?
                .into(),
            ),
            partial_schema: Some(partial_schema.into()),
            partial_aggregation_plan: partial_aggregation_plan.encode_to_vec(),
            final_aggregation_plan: finish_plan.encode_to_vec(),
            final_projection: Some(final_physical_plan_node.encode_to_vec()),
        };

        Ok(LogicalNode::single(
            index as u32,
            format!("tumbling_{index}"),
            OperatorName::TumblingWindowAggregate,
            config.encode_to_vec(),
            format!("TumblingWindow<{}>", config.name),
            1,
        ))
    }

    pub fn sliding_window_config(
        &self,
        planner: &Planner,
        index: usize,
        input_schema: DFSchemaRef,
        width: Duration,
        slide: Duration,
    ) -> Result<LogicalNode> {
        let binning_function_proto = planner.binning_function_proto(slide, input_schema.clone())?;

        let SplitPlanOutput {
            partial_aggregation_plan,
            partial_schema,
            finish_plan,
        } = planner.split_physical_plan(self.key_fields.clone(), &self.aggregate, true)?;

        let final_physical_plan = planner.sync_plan(&self.final_calculation)?;
        let final_physical_plan_node = PhysicalPlanNode::try_from_physical_plan(
            final_physical_plan,
            &ArroyoPhysicalExtensionCodec::default(),
        )?;

        let config = SlidingWindowAggregateOperator {
            name: format!("SlidingWindow<{width:?}>"),
            width_micros: width.as_micros() as u64,
            slide_micros: slide.as_micros() as u64,
            binning_function: binning_function_proto.encode_to_vec(),
            input_schema: Some(
                ArroyoSchema::from_schema_keys(
                    Arc::new(input_schema.as_ref().into()),
                    self.key_fields.clone(),
                )?
                .into(),
            ),
            partial_schema: Some(partial_schema.into()),
            partial_aggregation_plan: partial_aggregation_plan.encode_to_vec(),
            final_aggregation_plan: finish_plan.encode_to_vec(),
            final_projection: final_physical_plan_node.encode_to_vec(),
            // TODO add final aggregation.
        };

        Ok(LogicalNode::single(
            index as u32,
            format!("sliding_window_{index}"),
            OperatorName::SlidingWindowAggregate,
            config.encode_to_vec(),
            "sliding window".to_string(),
            1,
        ))
    }

    pub fn session_window_config(
        &self,
        planner: &Planner,
        index: usize,
        input_schema: DFSchemaRef,
    ) -> Result<LogicalNode> {
        let WindowBehavior::FromOperator {
            window: WindowType::Session { gap },
            window_index,
            window_field,
            is_nested: false,
        } = &self.window_behavior
        else {
            return plan_err!("expected sliding window");
        };
        let output_schema = fields_with_qualifiers(self.aggregate.schema());
        let LogicalPlan::Aggregate(agg) = self.aggregate.clone() else {
            return plan_err!("expected aggregate");
        };
        let key_count = self.key_fields.len();
        let unkeyed_aggregate_schema = Arc::new(schema_from_df_fields_with_metadata(
            &output_schema[key_count..],
            self.aggregate.schema().metadata().clone(),
        )?);

        let unkeyed_aggregate = Aggregate::try_new_with_schema(
            agg.input.clone(),
            vec![],
            agg.aggr_expr.clone(),
            unkeyed_aggregate_schema.clone(),
        )?;
        let aggregate_plan = planner.sync_plan(&LogicalPlan::Aggregate(unkeyed_aggregate))?;

        let physical_plan_node = PhysicalPlanNode::try_from_physical_plan(
            aggregate_plan,
            &ArroyoPhysicalExtensionCodec::default(),
        )?;
        let input_schema = ArroyoSchema::from_schema_keys(
            Arc::new(input_schema.as_ref().into()),
            self.key_fields.clone(),
        )?;

        let config = SessionWindowAggregateOperator {
            name: format!("session_window_{index}"),
            gap_micros: gap.as_micros() as u64,
            window_field_name: window_field.name().to_string(),
            window_index: *window_index as u64,
            input_schema: Some(input_schema.into()),
            unkeyed_aggregate_schema: None,
            partial_aggregation_plan: vec![],
            final_aggregation_plan: physical_plan_node.encode_to_vec(),
        };

        Ok(LogicalNode::single(
            index as u32,
            format!("SessionWindow<{gap:?}>"),
            OperatorName::SessionWindowAggregate,
            config.encode_to_vec(),
            config.name.clone(),
            1,
        ))
    }

    pub fn instant_window_config(
        &self,
        planner: &Planner,
        index: usize,
        input_schema: DFSchemaRef,
        use_final_projection: bool,
    ) -> Result<LogicalNode> {
        let binning_function = planner.create_physical_expr(
            &Expr::Column(Column::new_unqualified("_timestamp".to_string())),
            &input_schema,
        )?;
        let binning_function_proto =
            serialize_physical_expr(&binning_function, &DefaultPhysicalExtensionCodec {})?;

        let final_projection = use_final_projection
            .then(|| {
                let final_physical_plan = planner.sync_plan(&self.final_calculation)?;
                let final_physical_plan_node = PhysicalPlanNode::try_from_physical_plan(
                    final_physical_plan,
                    &ArroyoPhysicalExtensionCodec::default(),
                )?;
                Ok::<Vec<u8>, DataFusionError>(final_physical_plan_node.encode_to_vec())
            })
            .transpose()?;

        let SplitPlanOutput {
            partial_aggregation_plan,
            partial_schema,
            finish_plan,
        } = planner.split_physical_plan(self.key_fields.clone(), &self.aggregate, true)?;

        let config = TumblingWindowAggregateOperator {
            name: "InstantWindow".to_string(),
            width_micros: 0,
            binning_function: binning_function_proto.encode_to_vec(),
            input_schema: Some(
                ArroyoSchema::from_schema_keys(
                    Arc::new(input_schema.as_ref().into()),
                    self.key_fields.clone(),
                )?
                .into(),
            ),
            partial_schema: Some(partial_schema.into()),
            partial_aggregation_plan: partial_aggregation_plan.encode_to_vec(),
            final_aggregation_plan: finish_plan.encode_to_vec(),
            final_projection,
        };

        Ok(LogicalNode::single(
            index as u32,
            format!("instant_window_{index}"),
            OperatorName::TumblingWindowAggregate,
            config.encode_to_vec(),
            "instant window".to_string(),
            1,
        ))
    }

    // projection assuming that _timestamp has been populated with the start of the bin.
    pub fn final_projection(
        aggregate_plan: &LogicalPlan,
        window_behavior: WindowBehavior,
    ) -> Result<LogicalPlan> {
        let timestamp_field: DFField = aggregate_plan.inputs()[0]
            .schema()
            .qualified_field_with_unqualified_name(TIMESTAMP_FIELD)?
            .into();
        let timestamp_append = LogicalPlan::Extension(Extension {
            node: Arc::new(TimestampAppendExtension::new(
                aggregate_plan.clone(),
                timestamp_field.qualifier().cloned(),
            )),
        });
        let mut aggregate_fields = fields_with_qualifiers(aggregate_plan.schema());
        let mut aggregate_expressions: Vec<_> = aggregate_fields
            .iter()
            .map(|field| Expr::Column(field.qualified_column()))
            .collect();
        let (window_field, window_index, width, is_nested) = match window_behavior {
            WindowBehavior::InData => return Ok(timestamp_append),
            WindowBehavior::FromOperator {
                window,
                window_field,
                window_index,
                is_nested,
            } => match window {
                WindowType::Tumbling { width, .. } | WindowType::Sliding { width, .. } => {
                    (window_field, window_index, width, is_nested)
                }
                WindowType::Session { .. } => {
                    return Ok(LogicalPlan::Extension(Extension {
                        node: Arc::new(WindowAppendExtension::new(
                            timestamp_append,
                            window_field,
                            window_index,
                        )),
                    }))
                }
                WindowType::Instant => return Ok(timestamp_append),
            },
        };
        if is_nested {
            return Self::nested_final_projection(
                timestamp_append,
                window_field,
                window_index,
                width,
            );
        }
        let timestamp_column =
            Column::new(timestamp_field.qualifier().cloned(), timestamp_field.name());
        aggregate_fields.insert(window_index, window_field.clone());

        let window_expression = Expr::ScalarFunction(ScalarFunction {
            func: window(),
            args: vec![
                // copy bin_start as first argument
                Expr::Column(timestamp_column.clone()),
                // add width interval to _timestamp for bin end
                Expr::BinaryExpr(BinaryExpr {
                    left: Box::new(Expr::Column(timestamp_column.clone())),
                    op: logical_expr::Operator::Plus,
                    right: Box::new(Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                        IntervalMonthDayNanoType::make_value(0, 0, width.as_nanos() as i64),
                    )))),
                }),
            ],
        });
        aggregate_expressions.insert(
            window_index,
            window_expression
                .alias_qualified(window_field.qualifier().cloned(), window_field.name()),
        );
        aggregate_fields.push(timestamp_field);
        let bin_end_calculation = Expr::BinaryExpr(BinaryExpr {
            left: Box::new(Expr::Column(timestamp_column.clone())),
            op: logical_expr::Operator::Plus,
            right: Box::new(Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                IntervalMonthDayNanoType::make_value(0, 0, (width.as_nanos() - 1) as i64),
            )))),
        });
        aggregate_expressions.push(bin_end_calculation);
        Ok(LogicalPlan::Projection(
            logical_expr::Projection::try_new_with_schema(
                aggregate_expressions,
                Arc::new(timestamp_append),
                Arc::new(schema_from_df_fields(&aggregate_fields)?),
            )?,
        ))
    }

    fn nested_final_projection(
        aggregate_plan: LogicalPlan,
        window_field: DFField,
        window_index: usize,
        width: Duration,
    ) -> Result<LogicalPlan> {
        let timestamp_field: DFField = aggregate_plan
            .schema()
            .qualified_field_with_unqualified_name(TIMESTAMP_FIELD)
            .unwrap()
            .into();
        let timestamp_column =
            Column::new(timestamp_field.qualifier().cloned(), timestamp_field.name());

        let mut aggregate_fields = fields_with_qualifiers(aggregate_plan.schema());
        let mut aggregate_expressions: Vec<_> = aggregate_fields
            .iter()
            .map(|field| Expr::Column(field.qualified_column()))
            .collect();
        aggregate_fields.insert(window_index, window_field.clone());
        let window_expression = Expr::ScalarFunction(ScalarFunction {
            func: window(),
            args: vec![
                // calculate the start of the bin
                Expr::BinaryExpr(BinaryExpr {
                    left: Box::new(Expr::Column(timestamp_column.clone())),
                    op: logical_expr::Operator::Minus,
                    right: Box::new(Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                        IntervalMonthDayNanoType::make_value(0, 0, width.as_nanos() as i64 - 1),
                    )))),
                }),
                // add 1 nanosecond to the timestamp
                Expr::BinaryExpr(BinaryExpr {
                    left: Box::new(Expr::Column(timestamp_column.clone())),
                    op: logical_expr::Operator::Plus,
                    right: Box::new(Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                        IntervalMonthDayNanoType::make_value(0, 0, 1),
                    )))),
                }),
            ],
        });
        aggregate_expressions.insert(
            window_index,
            window_expression
                .alias_qualified(window_field.qualifier().cloned(), window_field.name()),
        );
        Ok(LogicalPlan::Projection(
            logical_expr::Projection::try_new_with_schema(
                aggregate_expressions,
                Arc::new(aggregate_plan),
                Arc::new(schema_from_df_fields(&aggregate_fields).unwrap()),
            )
            .unwrap(),
        ))
    }
}

impl UserDefinedLogicalNodeCore for AggregateExtension {
    fn name(&self) -> &str {
        AGGREGATE_EXTENSION_NAME
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.aggregate]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<Expr> {
        vec![]
    }

    fn fmt_for_explain(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "AggregateExtension: {} | window_behavior: {:?}",
            self.schema(),
            match &self.window_behavior {
                WindowBehavior::InData => "InData".to_string(),
                WindowBehavior::FromOperator { window, .. } => format!("FromOperator({window:?})"),
            }
        )
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        if inputs.len() != 1 {
            return internal_err!("input size inconsistent");
        }

        Ok(Self::new(
            self.window_behavior.clone(),
            inputs[0].clone(),
            self.key_fields.clone(),
        ))
    }
}

impl ArroyoExtension for AggregateExtension {
    fn node_name(&self) -> Option<NamedNode> {
        None
    }

    fn plan_node(
        &self,
        planner: &Planner,
        index: usize,
        input_schemas: Vec<ArroyoSchemaRef>,
    ) -> Result<NodeWithIncomingEdges> {
        if input_schemas.len() != 1 {
            return plan_err!("AggregateExtension should have exactly one input");
        }
        let input_schema = input_schemas[0].clone();
        let input_df_schema =
            Arc::new(DFSchema::try_from(input_schema.schema.as_ref().clone()).unwrap());
        let logical_node = match &self.window_behavior {
            WindowBehavior::FromOperator {
                window,
                window_field: _,
                window_index: _,
                is_nested,
            } => {
                if *is_nested {
                    self.instant_window_config(planner, index, input_df_schema, true)?
                } else {
                    match window {
                        WindowType::Tumbling { width } => {
                            self.tumbling_window_config(planner, index, input_df_schema, *width)?
                        }
                        WindowType::Sliding { width, slide } => self.sliding_window_config(
                            planner,
                            index,
                            input_df_schema,
                            *width,
                            *slide,
                        )?,
                        WindowType::Instant => {
                            return plan_err!(
                                "instant window not supported in aggregate extension"
                            );
                        }
                        WindowType::Session { gap: _ } => {
                            self.session_window_config(planner, index, input_df_schema)?
                        }
                    }
                }
            }
            WindowBehavior::InData => self
                .instant_window_config(planner, index, input_df_schema, false)
                .map_err(|e| e.context("instant window"))?,
        };
        let edge = LogicalEdge::project_all(LogicalEdgeType::Shuffle, (*input_schema).clone());
        Ok(NodeWithIncomingEdges {
            node: logical_node,
            edges: vec![edge],
        })
    }

    fn output_schema(&self) -> ArroyoSchema {
        let output_schema = (*self.schema).clone().into();
        ArroyoSchema::from_schema_keys(Arc::new(output_schema), vec![]).unwrap()
    }
}

/*
This is a plan used for appending a _timestamp field to an existing record batch.
 */

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WindowAppendExtension {
    pub(crate) input: LogicalPlan,
    pub(crate) window_field: DFField,
    pub(crate) window_index: usize,
    pub(crate) schema: DFSchemaRef,
}

multifield_partial_ord!(WindowAppendExtension, input, window_index);

impl WindowAppendExtension {
    fn new(input: LogicalPlan, window_field: DFField, window_index: usize) -> Self {
        let mut fields = fields_with_qualifiers(input.schema());
        fields.insert(window_index, window_field.clone());
        let metadata = input.schema().metadata().clone();
        Self {
            input,
            window_field,
            window_index,
            schema: Arc::new(schema_from_df_fields_with_metadata(&fields, metadata).unwrap()),
        }
    }
}

impl UserDefinedLogicalNodeCore for WindowAppendExtension {
    fn name(&self) -> &str {
        "WindowAppendExtension"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.input]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<Expr> {
        vec![]
    }

    fn fmt_for_explain(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "WindowAppendExtension: field {:?} at {}",
            self.window_field, self.window_index
        )
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        Ok(Self::new(
            inputs[0].clone(),
            self.window_field.clone(),
            self.window_index,
        ))
    }
}
