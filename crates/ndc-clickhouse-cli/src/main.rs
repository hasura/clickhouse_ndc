use std::{
    collections::BTreeMap,
    env,
    error::Error,
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Parser, Subcommand, ValueEnum};
use common::{
    clickhouse_parser::{
        datatype::ClickHouseDataType,
        parameterized_query::{Parameter, ParameterizedQuery, ParameterizedQueryElement},
    },
    config::ConnectionConfig,
    config_file::{
        ParameterizedQueryConfigFile, PrimaryKey, ReturnType, ServerConfigFile, TableConfigFile,
        CONFIG_FILE_NAME, CONFIG_SCHEMA_FILE_NAME,
    },
};
use database_introspection::{introspect_database, TableInfo};
use schemars::schema_for;
use tokio::fs;
mod database_introspection;

#[derive(Parser)]
struct CliArgs {
    /// The PAT token which can be used to make authenticated calls to Hasura Cloud
    #[arg(long = "ddn-pat", value_name = "PAT", env = "HASURA_PLUGIN_DDN_PAT")]
    ddn_pat: Option<String>,
    /// If the plugins are sending any sort of telemetry back to Hasura, it should be disabled if this is true.
    #[arg(long = "disable-telemetry", env = "HASURA_PLUGIN_DISABLE_TELEMETRY")]
    disable_telemetry: bool,
    /// A UUID for every unique user. Can be used in telemetry
    #[arg(
        long = "instance-id",
        value_name = "ID",
        env = "HASURA_PLUGIN_INSTANCE_ID"
    )]
    instance_id: Option<String>,
    /// A UUID unique to every invocation of Hasura CLI
    #[arg(
        long = "execution-id",
        value_name = "ID",
        env = "HASURA_PLUGIN_EXECUTION_ID"
    )]
    execution_id: Option<String>,
    #[arg(
        long = "log-level",
        value_name = "LEVEL",
        env = "HASURA_PLUGIN_LOG_LEVEL",
        default_value = "info",
        ignore_case = true
    )]
    log_level: LogLevel,
    /// Fully qualified path to the context directory of the connector
    #[arg(
        long = "connector-context-path",
        value_name = "PATH",
        env = "HASURA_PLUGIN_CONNECTOR_CONTEXT_PATH"
    )]
    context_path: Option<PathBuf>,
    #[arg(long = "clickhouse-url", value_name = "URL", env = "CLICKHOUSE_URL")]
    clickhouse_url: String,
    #[arg(long = "clickhouse-username", value_name = "USERNAME", env = "CLICKHOUSE_USERNAME", default_value_t = String::from("default"))]
    clickhouse_username: String,
    #[arg(
        long = "clickhouse-password",
        value_name = "PASSWORD",
        env = "CLICKHOUSE_PASSWORD"
    )]
    clickhouse_password: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Subcommand)]
enum Command {
    Init {},
    Update {},
    Validate {},
    Watch {},
}

#[derive(Clone, ValueEnum)]
enum LogLevel {
    Panic,
    Fatal,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

struct Context {
    context_path: PathBuf,
    connection: ConnectionConfig,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = CliArgs::parse();

    let context_path = match args.context_path {
        None => env::current_dir()?,
        Some(path) => path,
    };

    let connection = ConnectionConfig {
        url: args.clickhouse_url,
        username: args.clickhouse_username,
        password: args.clickhouse_password,
    };

    let context = Context {
        context_path,
        connection,
    };

    match args.command {
        Command::Init {} => {
            update_tables_config(&context.context_path, &context.connection).await?;
        }
        Command::Update {} => {
            update_tables_config(&context.context_path, &context.connection).await?;
        }
        Command::Validate {} => {
            todo!("implement validate command")
        }
        Command::Watch {} => {
            todo!("implement watch command")
        }
    }

    Ok(())
}

pub async fn update_tables_config(
    configuration_dir: impl AsRef<Path> + Send,
    connection_config: &ConnectionConfig,
) -> Result<(), Box<dyn Error>> {
    let table_infos = introspect_database(connection_config).await?;

    let file_path = configuration_dir.as_ref().join(CONFIG_FILE_NAME);
    let schema_file_path = configuration_dir.as_ref().join(CONFIG_SCHEMA_FILE_NAME);

    let old_config: Option<ServerConfigFile> = match fs::read_to_string(&file_path).await {
        Ok(file) => Some(serde_json::from_str(&file)
            .map_err(|err| format!("Error parsing {CONFIG_FILE_NAME}: {err}\n\nDelete {CONFIG_FILE_NAME} to create a fresh file"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(Err(format!("Error reading {CONFIG_FILE_NAME}"))),
    }.transpose()?;

    let tables = table_infos
        .iter()
        .map(|table| {
            let old_table_config = get_old_table_config(table, &old_config);
            let table_alias = get_table_alias(table, &old_table_config);

            let arguments = ParameterizedQuery::from_str(&table.view_definition)
                // when unable to parse, default to empty arguments list
                .unwrap_or_default()
                .elements
                .iter()
                .filter_map(|element| match element {
                    ParameterizedQueryElement::String(_) => None,
                    ParameterizedQueryElement::Parameter(Parameter { name, r#type }) => {
                        Some((name.to_string(), r#type.to_string()))
                    }
                })
                .collect();

            let table_config = TableConfigFile {
                name: table.table_name.to_owned(),
                schema: table.table_schema.to_owned(),
                comment: table.table_comment.to_owned(),
                primary_key: table.primary_key.as_ref().map(|primary_key| PrimaryKey {
                    name: primary_key.to_owned(),
                    columns: table
                        .columns
                        .iter()
                        .filter_map(|column| {
                            if column.is_in_primary_key {
                                Some(column.column_name.to_owned())
                            } else {
                                None
                            }
                        })
                        .collect(),
                }),
                arguments,
                return_type: get_table_return_type(
                    table,
                    &old_table_config,
                    &old_config,
                    &table_infos,
                ),
            };

            (table_alias, table_config)
        })
        .collect();

    let config = ServerConfigFile {
        schema: CONFIG_SCHEMA_FILE_NAME.to_owned(),
        tables: tables,
        queries: old_config
            .as_ref()
            .map(|old_config| old_config.queries.to_owned())
            .unwrap_or_default(),
    };
    let config_schema = schema_for!(ServerConfigFile);

    if old_config.is_none() || old_config.is_some_and(|old_config| old_config != config) {
        fs::write(&file_path, serde_json::to_string_pretty(&config)?).await?;
        fs::write(
            &schema_file_path,
            serde_json::to_string_pretty(&config_schema)?,
        )
        .await?;
    }

    // validate after writing out the updated metadata. This should help users understand what the problem is
    // check if some column types can't be parsed
    for (table_alias, table_config) in &config.tables {
        match &table_config.return_type {
            ReturnType::TableReference {
                table_name: target_table,
            } => {
                match config.tables.get(target_table) {
                    Some(TableConfigFile {
                        return_type: ReturnType::Definition { .. },
                        ..
                    }) => {
                        // referencing a table that has a return type defintion we can use. all is well
                    }
                    Some(_) => {
                        return Err(format!(
                                "Invalid reference: table \"{table_alias}\" references table \"{target_table}\" which does not have a return type definition."
                            )
                            .into());
                    }
                    None => {
                        return Err(format!(
                                              "Orphan reference: table \"{table_alias}\" references table \"{target_table}\" which cannot be found."
                                          )
                                          .into());
                    }
                }
            }
            ReturnType::QueryReference {
                query_name: target_query,
            } => {
                match config.queries.get(target_query) {
                    Some(ParameterizedQueryConfigFile {
                        return_type: ReturnType::Definition { .. },
                        ..
                    }) => {
                        // referencing a query that has a  return type definition we can use. all is well
                    }
                    Some(_) => {
                        return Err(format!(
                            "Invalid reference: table \"{table_alias}\" references query \"{target_query}\" which does not have a return type definition."
                        )
                        .into());
                    }
                    None => {
                        return Err(format!(
                            "Orphan reference: table \"{table_alias}\" references query \"{target_query}\" which cannot be found."
                        )
                        .into());
                    }
                }
            }
            ReturnType::Definition { columns } => {
                for (column_alias, column_data_type) in columns {
                    let _data_type =
                        ClickHouseDataType::from_str(&column_data_type).map_err(|err| {
                            format!(
                                "Unable to parse data type \"{}\" for column {} in table {}: {}",
                                column_data_type, column_alias, table_alias, err
                            )
                        })?;
                }
            }
        }
    }

    for (query_alias, query_config) in &config.queries {
        // check for duplicate alias
        if config.tables.contains_key(query_alias) {
            return Err(format!(
                "Name collision: query \"{query_alias}\" has the same name as a collection"
            )
            .into());
        }

        // if return type is a reference, check it exists and is valid:
        match &query_config.return_type {
            ReturnType::TableReference {
                table_name: target_table,
            } => {
                match config.tables.get(target_table) {
                    Some(TableConfigFile {
                        return_type: ReturnType::Definition { .. },
                        ..
                    }) => {
                        // referencing a table that has a return type defintion we can use. all is well
                    }
                    Some(_) => {
                        return Err(format!(
                                "Invalid reference: query \"{query_alias}\" references table \"{target_table}\" which does not have a return type definition."
                            )
                            .into());
                    }
                    None => {
                        return Err(format!(
                                              "Orphan reference: query \"{query_alias}\" references table \"{target_table}\" which cannot be found."
                                          )
                                          .into());
                    }
                }
            }
            ReturnType::QueryReference {
                query_name: target_query,
            } => {
                match config.queries.get(target_query) {
                    Some(ParameterizedQueryConfigFile {
                        return_type: ReturnType::Definition { .. },
                        ..
                    }) => {
                        // referencing a query that has a  return type definition we can use. all is well
                    }
                    Some(_) => {
                        return Err(format!(
                            "Invalid reference: query \"{query_alias}\" references \"{target_query}\" which does not have a return type definition."
                        )
                        .into());
                    }
                    None => {
                        return Err(format!(
                            "Orphan reference: query \"{query_alias}\" references query \"{target_query}\" which cannot be found."
                        )
                        .into());
                    }
                }
            }
            ReturnType::Definition { columns } => {
                for (column_name, column_data_type) in columns {
                    let _data_type =
                        ClickHouseDataType::from_str(&column_data_type).map_err(|err| {
                            format!(
                                "Unable to parse data type \"{}\" for field {} in query {}: {}",
                                column_data_type, column_name, query_alias, err
                            )
                        })?;
                }
            }
        }

        // validate that we can find the referenced sql file
        let file_path = configuration_dir.as_ref().join(&query_config.file);
        let file_content = fs::read_to_string(&file_path).await.map_err(|err| {
            format!(
                "Error reading {} for query {query_alias}: {err}",
                query_config.file
            )
        })?;
        // validate that we can parse the reference sql file
        let _query = ParameterizedQuery::from_str(&file_content).map_err(|err| {
            format!(
                "Unable to parse file {} for parameterized query {}: {}",
                query_config.file, query_alias, err
            )
        })?;
    }

    Ok(())
}

/// Get old table config, if any
/// Note this uses the table name and schema to search, not the alias
/// This allows custom aliases to be preserved
fn get_old_table_config<'a>(
    table: &TableInfo,
    old_config: &'a Option<ServerConfigFile>,
) -> Option<(&'a String, &'a TableConfigFile)> {
    old_config.as_ref().and_then(|old_config| {
        old_config.tables.iter().find(|(_, old_table)| {
            old_table.name == table.table_name && old_table.schema == table.table_schema
        })
    })
}

/// Table aliases default to <schema_name>_<table_name>,
/// except for tables in the default schema where the table name is used.
/// Prefer existing, old aliases over creating a new one
fn get_table_alias(table: &TableInfo, old_table: &Option<(&String, &TableConfigFile)>) -> String {
    // to preserve any customization, aliases are kept throught updates
    if let Some((old_table_alias, _)) = old_table {
        old_table_alias.to_string()
    } else if table.table_schema == "default" {
        table.table_name.to_owned()
    } else {
        format!("{}_{}", table.table_schema, table.table_name)
    }
}

/// Given table info, and optionally old table info, get the return type for this table
///
/// If the old configuration's return type is a reference
/// to a table: check that table still exists, and that it returns the same type as this table
/// to a query: check that query still exists, and that it returns the same type as this table
fn get_table_return_type(
    table: &TableInfo,
    old_table: &Option<(&String, &TableConfigFile)>,
    old_config: &Option<ServerConfigFile>,
    introspection: &Vec<TableInfo>,
) -> ReturnType {
    let new_columns = get_return_type_columns(table);

    let old_return_type =
        old_table.and_then(
            |(_table_alias, table_config)| match &table_config.return_type {
                ReturnType::Definition { .. } => None,
                ReturnType::TableReference { table_name } => {
                    // get the old table config for the referenced table
                    let referenced_table_config = old_config
                        .as_ref()
                        .and_then(|old_config| old_config.tables.get(table_name));
                    // get the new table info for the referenced table, if the referenced table's return type is a definition
                    let referenced_table_info =
                        referenced_table_config.and_then(|old_table| match old_table.return_type {
                            ReturnType::TableReference { .. }
                            | ReturnType::QueryReference { .. } => None,
                            ReturnType::Definition { .. } => {
                                introspection.iter().find(|table_info| {
                                    table_info.table_schema == old_table.schema
                                        && table_info.table_name == table_config.name
                                })
                            }
                        });

                    // get the new return type for the referenced table
                    let referenced_table_columns =
                        referenced_table_info.map(get_return_type_columns);

                    // preserve the reference if the return type for the referenced table matches this table
                    if referenced_table_columns.is_some_and(|r| r == new_columns) {
                        Some(ReturnType::TableReference {
                            table_name: table_name.to_owned(),
                        })
                    } else {
                        None
                    }
                }
                // if the old config references a query, keep the it if it points to a query that returns the same type as we just introspected
                ReturnType::QueryReference { query_name } => old_config
                    .as_ref()
                    .and_then(|old_config| old_config.queries.get(query_name))
                    .and_then(|query| match &query.return_type {
                        ReturnType::TableReference { .. } | ReturnType::QueryReference { .. } => {
                            None
                        }
                        ReturnType::Definition { columns } => {
                            if columns == &new_columns {
                                Some(ReturnType::QueryReference {
                                    query_name: query_name.to_owned(),
                                })
                            } else {
                                None
                            }
                        }
                    }),
            },
        );

    old_return_type.unwrap_or_else(|| ReturnType::Definition {
        columns: new_columns,
    })
}

fn get_return_type_columns(table: &TableInfo) -> BTreeMap<String, String> {
    table
        .columns
        .iter()
        .map(|column| (column.column_name.to_owned(), column.data_type.to_owned()))
        .collect()
}
