use common::{
    clickhouse_parser::datatype::{ClickHouseDataType, Identifier},
    config::ServerConfig,
    schema::{
        single_column_aggregate_function::ClickHouseSingleColumnAggregateFunction,
        type_definition::ClickHouseTypeDefinition,
    },
};
use indexmap::IndexMap;
use ndc_models::{
    self as models, AggregateFunctionName, CollectionName, FieldName, NestedField, ObjectTypeName,
    RelationshipName,
};
use std::{collections::BTreeMap, str::FromStr};

use super::QueryBuilderError;

/// Tuple(rows <RowsCastString>, aggregates <RowsCastString>)
pub struct RowsetTypeString {
    rows: Option<RowTypeString>,
    aggregates: Option<AggregatesTypeString>,
}
/// Tuple("a1" T1, "a2" T2)
pub struct AggregatesTypeString {
    aggregates: Vec<(FieldName, ClickHouseDataType)>,
}
/// Tuple("f1" T1, "f2" <RowSetTypeString>)
pub struct RowTypeString {
    fields: Vec<(FieldName, FieldTypeString)>,
}
pub enum FieldTypeString {
    Relationship(RowsetTypeString),
    Array(Box<FieldTypeString>),
    Object(Vec<(FieldName, FieldTypeString)>),
    Scalar(ClickHouseDataType),
}

impl RowsetTypeString {
    pub fn new(
        table_alias: &CollectionName,
        query: &models::Query,
        relationships: &BTreeMap<RelationshipName, models::Relationship>,
        config: &ServerConfig,
    ) -> Result<Self, TypeStringError> {
        let rows = if let Some(fields) = &query.fields {
            Some(RowTypeString::new(
                table_alias,
                fields,
                relationships,
                config,
            )?)
        } else {
            None
        };
        let aggregates = if let Some(aggregates) = &query.aggregates {
            Some(AggregatesTypeString::new(table_alias, aggregates, config)?)
        } else {
            None
        };

        Ok(Self { rows, aggregates })
    }
    pub fn into_cast_type(self) -> ClickHouseDataType {
        match (self.rows, self.aggregates) {
            (None, None) => ClickHouseDataType::Map {
                key: Box::new(ClickHouseDataType::Nothing),
                value: Box::new(ClickHouseDataType::Nothing),
            },
            (None, Some(aggregates)) => ClickHouseDataType::Tuple(vec![(
                Some(Identifier::Unquoted("aggregates".to_string())),
                aggregates.into_cast_type(),
            )]),
            (Some(rows), None) => ClickHouseDataType::Tuple(vec![(
                Some(Identifier::Unquoted("rows".to_string())),
                ClickHouseDataType::Array(Box::new(rows.into_cast_type())),
            )]),
            (Some(rows), Some(aggregates)) => ClickHouseDataType::Tuple(vec![
                (
                    Some(Identifier::Unquoted("rows".to_string())),
                    ClickHouseDataType::Array(Box::new(rows.into_cast_type())),
                ),
                (
                    Some(Identifier::Unquoted("aggregates".to_string())),
                    aggregates.into_cast_type(),
                ),
            ]),
        }
    }
}

impl AggregatesTypeString {
    fn new(
        table_alias: &CollectionName,
        aggregates: &IndexMap<FieldName, models::Aggregate>,
        config: &ServerConfig,
    ) -> Result<Self, TypeStringError> {
        Ok(Self {
            aggregates: aggregates
                .iter()
                .map(|(alias, aggregate)| match aggregate {
                    models::Aggregate::StarCount {} | models::Aggregate::ColumnCount { .. } => {
                        Ok((alias.to_owned(), ClickHouseDataType::UInt32))
                    }
                    models::Aggregate::SingleColumn {
                        column: column_alias,
                        function,
                        field_path: _,
                    } => {
                        let return_type = get_return_type(table_alias, config)?;
                        let column_type = get_column(column_alias, return_type, config)?;
                        let type_definition = ClickHouseTypeDefinition::from_table_column(
                            column_type,
                            column_alias,
                            return_type,
                            &config.namespace_separator,
                        );

                        let aggregate_function =
                            ClickHouseSingleColumnAggregateFunction::from_str(function.inner())
                                .map_err(|_err| TypeStringError::UnknownAggregateFunction {
                                    table: table_alias.to_owned(),
                                    column: column_alias.to_owned(),
                                    data_type: column_type.to_owned(),
                                    function: function.to_owned(),
                                })?;

                        let aggregate_functions = type_definition.aggregate_functions();

                        let result_type = aggregate_functions
                            .iter()
                            .find(|(function, _)| function == &aggregate_function)
                            .map(|(_, result_type)| result_type)
                            .ok_or_else(|| TypeStringError::UnknownAggregateFunction {
                                table: table_alias.to_owned(),
                                column: column_alias.to_owned(),
                                data_type: column_type.to_owned(),
                                function: function.to_owned(),
                            })?;

                        Ok((alias.to_owned(), result_type.to_owned()))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
    fn into_cast_type(self) -> ClickHouseDataType {
        if self.aggregates.is_empty() {
            ClickHouseDataType::Map {
                key: Box::new(ClickHouseDataType::Nothing),
                value: Box::new(ClickHouseDataType::Nothing),
            }
        } else {
            ClickHouseDataType::Tuple(
                self.aggregates
                    .into_iter()
                    .map(|(alias, t)| (Some(Identifier::DoubleQuoted(alias.into())), t))
                    .collect(),
            )
        }
    }
}

impl RowTypeString {
    fn new(
        table_alias: &CollectionName,
        fields: &IndexMap<FieldName, models::Field>,
        relationships: &BTreeMap<RelationshipName, models::Relationship>,
        config: &ServerConfig,
    ) -> Result<Self, TypeStringError> {
        Ok(Self {
            fields: fields
                .iter()
                .map(|(alias, field)| {
                    Ok((
                        alias.to_owned(),
                        match field {
                            models::Field::Column {
                                column: column_alias,
                                fields,
                                arguments: _,
                            } => {
                                let return_type = get_return_type(table_alias, config)?;
                                let column_type = get_column(column_alias, return_type, config)?;
                                let type_definition = ClickHouseTypeDefinition::from_table_column(
                                    column_type,
                                    column_alias,
                                    return_type,
                                    &config.namespace_separator,
                                );

                                FieldTypeString::new(
                                    &type_definition,
                                    fields.as_ref(),
                                    relationships,
                                    config,
                                )?
                            }
                            models::Field::Relationship {
                                query,
                                relationship,
                                arguments: _,
                            } => {
                                let relationship =
                                    relationships.get(relationship).ok_or_else(|| {
                                        TypeStringError::MissingRelationship(
                                            relationship.to_owned(),
                                        )
                                    })?;

                                let table_alias = &relationship.target_collection;

                                FieldTypeString::Relationship(RowsetTypeString::new(
                                    table_alias,
                                    query,
                                    relationships,
                                    config,
                                )?)
                            }
                        },
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
    fn into_cast_type(self) -> ClickHouseDataType {
        if self.fields.is_empty() {
            ClickHouseDataType::Map {
                key: Box::new(ClickHouseDataType::Nothing),
                value: Box::new(ClickHouseDataType::Nothing),
            }
        } else {
            ClickHouseDataType::Tuple(
                self.fields
                    .into_iter()
                    .map(|(alias, field)| {
                        (
                            Some(Identifier::DoubleQuoted(alias.into())),
                            field.into_cast_type(),
                        )
                    })
                    .collect(),
            )
        }
    }
}

impl FieldTypeString {
    fn new(
        type_definition: &ClickHouseTypeDefinition,
        fields: Option<&NestedField>,
        relationships: &BTreeMap<RelationshipName, models::Relationship>,
        config: &ServerConfig,
    ) -> Result<Self, TypeStringError> {
        if let Some(fields) = fields {
            match (type_definition.non_nullable(), fields) {
                (
                    ClickHouseTypeDefinition::Array { element_type },
                    NestedField::Array(subfield_selector),
                ) => {
                    let type_definition = &**element_type;
                    let fields = Some(&*subfield_selector.fields);
                    let underlying_typestring =
                        FieldTypeString::new(type_definition, fields, relationships, config)?;
                    Ok(FieldTypeString::Array(Box::new(underlying_typestring)))
                }
                (
                    ClickHouseTypeDefinition::Object { name: _, fields },
                    NestedField::Object(subfield_selector),
                ) => {
                    let subfields = subfield_selector
                        .fields
                        .iter()
                        .map(|(alias, field)| {
                            match field {
                                models::Field::Column {
                                    column,
                                    fields: subfield_selector,
                                    arguments: _,
                                } => {
                                    let type_definition = fields.get(column).ok_or_else(|| {
                                        TypeStringError::MissingNestedField {
                                            field_name: column.to_owned(),
                                            object_type: type_definition
                                                .cast_type()
                                                .to_string()
                                                .into(),
                                        }
                                    })?;

                                    Ok((
                                        alias.to_owned(),
                                        FieldTypeString::new(
                                            type_definition,
                                            subfield_selector.as_ref(),
                                            relationships,
                                            config,
                                        )?,
                                    ))
                                }
                                models::Field::Relationship {
                                    query,
                                    relationship,
                                    arguments: _,
                                } => {
                                    let relationship =
                                        relationships.get(relationship).ok_or_else(|| {
                                            TypeStringError::MissingRelationship(
                                                relationship.to_owned(),
                                            )
                                        })?;

                                    let table_alias = &relationship.target_collection;

                                    Ok((
                                        alias.to_owned(),
                                        FieldTypeString::Relationship(RowsetTypeString::new(
                                            table_alias,
                                            query,
                                            relationships,
                                            config,
                                        )?),
                                    ))
                                }
                            }
                            // Ok((alias, FieldTypeString::new(type_definition, fields)))
                        })
                        .collect::<Result<_, _>>()?;
                    Ok(FieldTypeString::Object(subfields))
                }
                (ClickHouseTypeDefinition::Scalar(_), NestedField::Object(_)) => {
                    Err(TypeStringError::NestedFieldTypeMismatch {
                        expected: "Object".to_owned(),
                        got: type_definition.cast_type().to_string(),
                    })
                }
                (ClickHouseTypeDefinition::Scalar(_), NestedField::Array(_)) => {
                    Err(TypeStringError::NestedFieldTypeMismatch {
                        expected: "Array".to_owned(),
                        got: type_definition.cast_type().to_string(),
                    })
                }
                (ClickHouseTypeDefinition::Nullable { .. }, NestedField::Object(_)) => {
                    Err(TypeStringError::NestedFieldTypeMismatch {
                        expected: "Object".to_owned(),
                        got: type_definition.cast_type().to_string(),
                    })
                }
                (ClickHouseTypeDefinition::Nullable { .. }, NestedField::Array(_)) => {
                    Err(TypeStringError::NestedFieldTypeMismatch {
                        expected: "Array".to_owned(),
                        got: type_definition.cast_type().to_string(),
                    })
                }
                (ClickHouseTypeDefinition::Array { .. }, NestedField::Object(_)) => {
                    Err(TypeStringError::NestedFieldTypeMismatch {
                        expected: "Object".to_owned(),
                        got: type_definition.cast_type().to_string(),
                    })
                }
                (ClickHouseTypeDefinition::Object { .. }, NestedField::Array(_)) => {
                    Err(TypeStringError::NestedFieldTypeMismatch {
                        expected: "Array".to_owned(),
                        got: type_definition.cast_type().to_string(),
                    })
                }
            }
        } else {
            Ok(FieldTypeString::Scalar(type_definition.cast_type()))
        }
    }
    fn into_cast_type(self) -> ClickHouseDataType {
        match self {
            FieldTypeString::Relationship(rel) => rel.into_cast_type(),
            FieldTypeString::Array(inner) => {
                ClickHouseDataType::Array(Box::new(inner.into_cast_type()))
            }
            FieldTypeString::Object(fields) => ClickHouseDataType::Tuple(
                fields
                    .into_iter()
                    .map(|(alias, field)| {
                        (
                            Some(Identifier::DoubleQuoted(alias.into())),
                            field.into_cast_type(),
                        )
                    })
                    .collect(),
            ),
            FieldTypeString::Scalar(inner) => inner,
        }
    }
}

fn get_column<'a>(
    column_alias: &FieldName,
    return_type: &ObjectTypeName,
    config: &'a ServerConfig,
) -> Result<&'a ClickHouseDataType, TypeStringError> {
    let table_type =
        config
            .table_types
            .get(return_type)
            .ok_or_else(|| TypeStringError::UnknownTableType {
                table: return_type.to_owned(),
            })?;

    let column =
        table_type
            .columns
            .get(column_alias)
            .ok_or_else(|| TypeStringError::UnknownColumn {
                table: return_type.to_owned(),
                column: column_alias.to_owned(),
            })?;

    Ok(column)
}

fn get_return_type<'a>(
    table_alias: &CollectionName,
    config: &'a ServerConfig,
) -> Result<&'a ObjectTypeName, TypeStringError> {
    config
        .tables
        .get(table_alias)
        .map(|table| &table.return_type)
        .or_else(|| {
            config
                .queries
                .get(table_alias)
                .map(|query| &query.return_type)
        })
        .ok_or_else(|| TypeStringError::UnknownTable {
            table: table_alias.to_owned(),
        })
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum TypeStringError {
    #[error("Unknown table: {table}")]
    UnknownTable { table: CollectionName },
    #[error("Unknown table type: {table}")]
    UnknownTableType { table: ObjectTypeName },
    #[error("Unknown column: {column} in table: {table}")]
    UnknownColumn {
        table: ObjectTypeName,
        column: FieldName,
    },
    #[error("Unknown aggregate function: {function} for column {column} of type: {data_type} in table {table}")]
    UnknownAggregateFunction {
        table: CollectionName,
        column: FieldName,
        data_type: ClickHouseDataType,
        function: AggregateFunctionName,
    },
    #[error("Missing relationship: {0}")]
    MissingRelationship(RelationshipName),
    #[error("Not supported: {0}")]
    NotSupported(String),
    #[error("Nested field selector type mismatch, expected: {expected}, got {got}")]
    NestedFieldTypeMismatch { expected: String, got: String },
    #[error("Missing field {field_name} in object type {object_type}")]
    MissingNestedField {
        field_name: FieldName,
        object_type: ObjectTypeName,
    },
}

impl From<TypeStringError> for QueryBuilderError {
    fn from(value: TypeStringError) -> Self {
        QueryBuilderError::Typecasting(value)
    }
}
