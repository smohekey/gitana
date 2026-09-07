//! Path-aware MCP registration for the clap command tree.
//!
//! `clap-mcp`'s leaf-name registry cannot represent two nested commands with the same leaf. This
//! bridge still reuses its well-tested clap-to-JSON-schema conversion, but owns identity, routing,
//! argv construction, serialization, and rmcp serving. Top-level IDs stay unchanged; nested IDs are
//! their normalized full paths (`worktree_add`, `submodule_status`, ...).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::CommandFactory;
use clap_mcp::{ClapArg, ClapArgGroup, ClapMcpConfig, ClapMcpSchemaMetadata, ClapSchema};
use rmcp::model::{
	CallToolRequestParams, CallToolResult, Content, Implementation, ListResourcesResult,
	ListToolsResult, PaginatedRequestParams, RawResource, ReadResourceRequestParams,
	ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{Map, Value};

use crate::cli::Cli;

const SERVING_ARGUMENTS: &[&str] = &["mcp", "mcp-http"];
const MAX_COUNT_ARGUMENT: u64 = u8::MAX as u64;

#[derive(Clone)]
struct ToolSpec {
	path: Vec<String>,
	args: Vec<ClapArg>,
	arg_groups: Vec<ClapArgGroup>,
	prefix_count: usize,
}

#[derive(Clone)]
struct GtaMcpServer {
	tools: Arc<Vec<Tool>>,
	specs: Arc<HashMap<String, ToolSpec>>,
	schema: Arc<ClapSchema>,
	schema_json: Arc<String>,
	executable: Arc<PathBuf>,
	execution_lock: Arc<tokio::sync::Mutex<()>>,
}

impl GtaMcpServer {
	fn new() -> Result<Self> {
		let command = Cli::command();
		let schema = clap_mcp::schema_from_command(&command);
		let specs = collect_specs(&schema, &command)?;
		let mut resource_schema = schema;
		resource_schema
			.root
			.args
			.retain(|argument| !SERVING_ARGUMENTS.contains(&argument.id.as_str()));
		let schema_json = serde_json::to_string_pretty(&resource_schema)?;
		let mut tool_schema = resource_schema.clone();
		// Root options are invocation prefixes even when clap cannot mark them global because a leaf
		// reuses the same short spelling (notably root `-c` vs `ls-files -c`). MCP has no argv-order
		// ambiguity, so make them inheritable in the generated tool schemas and render them before the
		// command path below.
		for argument in &mut tool_schema.root.args {
			argument.global = true;
		}
		rename_nested_commands(&mut tool_schema.root, &mut Vec::new());
		let metadata = ClapMcpSchemaMetadata {
			skip_root_command_when_subcommands: true,
			..Default::default()
		};
		let generated = clap_mcp::tools_from_schema_with_metadata(
			&tool_schema,
			&ClapMcpConfig {
				reinvocation_safe: false,
				parallel_safe: false,
				..Default::default()
			},
			&metadata,
		);
		let mut tools: Vec<_> = generated
			.into_iter()
			.filter(|tool| specs.contains_key(tool.name.as_ref()))
			.collect();
		constrain_count_schemas(&mut tools, &specs);
		constrain_required_exclusive_group_schemas(&mut tools, &specs);
		Ok(Self {
			tools: Arc::new(tools),
			specs: Arc::new(specs),
			schema: Arc::new(resource_schema),
			schema_json: Arc::new(schema_json),
			executable: Arc::new(std::env::current_exe()?),
			execution_lock: Arc::new(tokio::sync::Mutex::new(())),
		})
	}

	async fn execute(&self, params: CallToolRequestParams) -> Result<CallToolResult, McpError> {
		let name = params.name.to_string();
		let spec = self
			.specs
			.get(&name)
			.ok_or_else(|| McpError::invalid_params(format!("unknown tool: {name}"), None))?;
		let arguments = params.arguments.unwrap_or_default();
		validate_arguments(spec, &arguments, &name)?;

		let argv = build_argv(spec, &arguments);
		let executable = self.executable.clone();
		let output = serialized_blocking(self.execution_lock.clone(), move || {
			std::process::Command::new(executable.as_path())
				.args(argv)
				.output()
		})
		.await
		.map_err(|error| McpError::internal_error(error.to_string(), None))?
		.map_err(|error| McpError::internal_error(error.to_string(), None))?;
		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);
		if !output.status.success() {
			let code = output
				.status
				.code()
				.map_or_else(|| "signal".to_owned(), |code| code.to_string());
			let message = failed_process_message(&code, &stdout, &stderr);
			return Ok(CallToolResult::error(vec![Content::text(message)]));
		}
		let text = successful_process_message(&stdout, &stderr);
		Ok(CallToolResult::success(vec![Content::text(text)]))
	}
}

pub(crate) fn export_skills(directory: Option<PathBuf>) -> Result<()> {
	let server = GtaMcpServer::new()?;
	let metadata = ClapMcpSchemaMetadata {
		skip_root_command_when_subcommands: true,
		..Default::default()
	};
	let directory = directory.unwrap_or_else(|| PathBuf::from(".agents").join("skills"));
	clap_mcp::content::export_skills(
		server.schema.as_ref(),
		&metadata,
		server.tools.as_ref(),
		&[],
		&[],
		&directory,
		&server.schema.root.name,
	)
	.map_err(|error| anyhow!("exporting skills to {}: {error}", directory.display()))?;
	Ok(())
}

fn failed_process_message(code: &str, stdout: &str, stderr: &str) -> String {
	let stdout = trim_line_terminators(stdout);
	let stderr = trim_line_terminators(stderr);
	let mut message = format!("Tool process exited with non-zero status ({code})");
	if !stdout.is_empty() {
		message.push_str("\nstdout:\n");
		message.push_str(stdout);
	}
	if !stderr.is_empty() {
		message.push_str("\nstderr:\n");
		message.push_str(stderr);
	}
	message
}

fn successful_process_message(stdout: &str, stderr: &str) -> String {
	match (trim_line_terminators(stdout), trim_line_terminators(stderr)) {
		(stdout, "") => stdout.to_owned(),
		("", stderr) => format!("stderr:\n{stderr}"),
		(stdout, stderr) => format!("{stdout}\nstderr:\n{stderr}"),
	}
}

fn trim_line_terminators(output: &str) -> &str {
	output.trim_end_matches(['\r', '\n'])
}

/// Run one blocking command while its owned serialization guard remains with the worker. Dropping
/// the awaiting MCP request detaches `spawn_blocking`, so the worker—not the request future—must own
/// the guard until the subprocess has exited.
async fn serialized_blocking<T, F>(
	lock: Arc<tokio::sync::Mutex<()>>,
	worker: F,
) -> Result<T, tokio::task::JoinError>
where
	T: Send + 'static,
	F: FnOnce() -> T + Send + 'static,
{
	let guard = lock.lock_owned().await;
	tokio::task::spawn_blocking(move || {
		let _guard = guard;
		worker()
	})
	.await
}

impl ServerHandler for GtaMcpServer {
	fn get_info(&self) -> ServerInfo {
		ServerInfo::new(
			ServerCapabilities::builder()
				.enable_tools()
				.enable_resources()
				.build(),
		)
		.with_server_info(
			Implementation::new("gta-mcp", env!("CARGO_PKG_VERSION"))
				.with_description("Gitana command tools with collision-free path identities"),
		)
	}

	fn list_resources(
		&self,
		_request: Option<PaginatedRequestParams>,
		_context: RequestContext<RoleServer>,
	) -> impl Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
		std::future::ready(Ok(ListResourcesResult::with_all_items(vec![
			clap_schema_resource(),
		])))
	}

	fn read_resource(
		&self,
		params: ReadResourceRequestParams,
		_context: RequestContext<RoleServer>,
	) -> impl Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
		let result = if params.uri == clap_mcp::MCP_RESOURCE_URI_SCHEMA {
			Ok(ReadResourceResult::new(vec![
				ResourceContents::TextResourceContents {
					uri: params.uri,
					mime_type: Some("application/json".to_owned()),
					text: (*self.schema_json).clone(),
					meta: None,
				},
			]))
		} else {
			Err(McpError::invalid_params(
				format!("unknown resource uri: {}", params.uri),
				None,
			))
		};
		std::future::ready(result)
	}

	fn list_tools(
		&self,
		_request: Option<PaginatedRequestParams>,
		_context: RequestContext<RoleServer>,
	) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
		std::future::ready(Ok(ListToolsResult::with_all_items((*self.tools).clone())))
	}

	fn call_tool(
		&self,
		params: CallToolRequestParams,
		_context: RequestContext<RoleServer>,
	) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
		self.execute(params)
	}
}

fn clap_schema_resource() -> Resource {
	Resource::new(
		RawResource::new(clap_mcp::MCP_RESOURCE_URI_SCHEMA, "clap-schema")
			.with_title("Clap CLI schema")
			.with_description("JSON schema extracted from clap Command definitions")
			.with_mime_type("application/json"),
		None,
	)
}

pub(crate) async fn serve_stdio() -> Result<()> {
	let service = GtaMcpServer::new()?
		.serve(rmcp::transport::stdio())
		.await
		.map_err(|error| anyhow!("initializing MCP stdio service: {error}"))?;
	service
		.waiting()
		.await
		.map_err(|error| anyhow!("MCP stdio service failed: {error}"))?;
	Ok(())
}

pub(crate) async fn serve_http(address: SocketAddr) -> Result<()> {
	use rmcp::transport::streamable_http_server::{
		StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
	};

	validate_http_address(address)?;
	let listener = tokio::net::TcpListener::bind(address).await?;
	let listening = listener.local_addr()?;
	let server = Arc::new(GtaMcpServer::new()?);
	let service = StreamableHttpService::new(
		{
			let server = server.clone();
			move || Ok((*server).clone())
		},
		Arc::new(LocalSessionManager::default()),
		StreamableHttpServerConfig::default()
			.with_allowed_hosts(allowed_hosts_for_address(listening))
			.with_allowed_origins(allowed_origins_for_address(listening)),
	);
	axum::serve(listener, axum::Router::new().nest_service("/mcp", service)).await?;
	Ok(())
}

pub(crate) fn validate_http_address(address: SocketAddr) -> Result<()> {
	if !address.ip().is_loopback() {
		return Err(anyhow!(
			"MCP HTTP listen address `{address}` is not loopback; unauthenticated HTTP serving is restricted to loopback"
		));
	}
	Ok(())
}

fn allowed_hosts_for_address(address: SocketAddr) -> Vec<String> {
	debug_assert!(address.ip().is_loopback());
	let port = address.port();
	let mut hosts = vec![
		"localhost".to_owned(),
		"127.0.0.1".to_owned(),
		"::1".to_owned(),
		format!("localhost:{port}"),
		format!("127.0.0.1:{port}"),
		format!("[::1]:{port}"),
	];
	let configured = address.to_string();
	if !hosts.contains(&configured) {
		hosts.push(configured);
	}
	hosts
}

fn allowed_origins_for_address(address: SocketAddr) -> Vec<String> {
	debug_assert!(address.ip().is_loopback());
	let port = address.port();
	let mut origins = vec![
		format!("http://localhost:{port}"),
		format!("http://127.0.0.1:{port}"),
		format!("http://[::1]:{port}"),
	];
	let configured = format!("http://{address}");
	if !origins.contains(&configured) {
		origins.push(configured);
	}
	origins
}

fn collect_specs(
	schema: &ClapSchema,
	command: &clap::Command,
) -> Result<HashMap<String, ToolSpec>> {
	fn walk(
		command: &clap::Command,
		schema: &clap_mcp::ClapCommand,
		path: &mut Vec<String>,
		inherited: &[ClapArg],
		out: &mut HashMap<String, ToolSpec>,
	) -> Result<()> {
		path.push(schema.name.clone());
		let mut args = inherited.to_vec();
		let prefix_count = args.len();
		args.extend(schema.args.clone());
		let executable = schema.subcommands.is_empty() || !command.is_subcommand_required_set();
		if path.len() > 1 && executable {
			let name = tool_name(&path[1..]);
			if out
				.insert(
					name.clone(),
					ToolSpec {
						path: path[1..].to_vec(),
						args: args.clone(),
						arg_groups: schema.arg_groups.clone(),
						prefix_count,
					},
				)
				.is_some()
			{
				bail!("MCP tool identity collision after normalization: {name}");
			}
		}
		let globals: Vec<ClapArg> = if path.len() == 1 {
			schema
				.args
				.iter()
				.filter(|argument| !SERVING_ARGUMENTS.contains(&argument.id.as_str()))
				.cloned()
				.collect()
		} else {
			inherited
				.iter()
				.cloned()
				.chain(schema.args.iter().filter(|arg| arg.global).cloned())
				.collect()
		};
		for child in &schema.subcommands {
			let actual = command
				.get_subcommands()
				.find(|candidate| candidate.get_name() == child.name)
				.ok_or_else(|| anyhow!("clap schema drift at {}", child.name))?;
			walk(actual, child, path, &globals, out)?;
		}
		path.pop();
		Ok(())
	}

	let mut out = HashMap::new();
	walk(command, &schema.root, &mut Vec::new(), &[], &mut out)?;
	Ok(out)
}

fn rename_nested_commands(command: &mut clap_mcp::ClapCommand, path: &mut Vec<String>) {
	let original = command.name.clone();
	path.push(original);
	if path.len() > 2 {
		command.name = tool_name(&path[1..]);
	}
	for child in &mut command.subcommands {
		rename_nested_commands(child, path);
	}
	path.pop();
}

fn tool_name(path: &[String]) -> String {
	if path.len() == 1 {
		return path[0].clone();
	}
	path
		.iter()
		.map(|segment| segment.replace('-', "_"))
		.collect::<Vec<_>>()
		.join("_")
}

fn validate_arguments(
	spec: &ToolSpec,
	arguments: &Map<String, Value>,
	tool: &str,
) -> Result<(), McpError> {
	let known: HashSet<&str> = spec.args.iter().map(|arg| arg.id.as_str()).collect();
	if let Some(unknown) = arguments.keys().find(|key| !known.contains(key.as_str())) {
		return Err(McpError::invalid_params(
			format!("unknown argument: {unknown} (tool: {tool})"),
			None,
		));
	}
	for arg in &spec.args {
		let Some(value) = arguments.get(&arg.id) else {
			if arg.required {
				return Err(invalid_argument(tool, arg, "is required"));
			}
			continue;
		};
		validate_argument(tool, arg, value)?;
	}
	validate_required_exclusive_groups(spec, arguments, tool)?;
	Ok(())
}

fn validate_required_exclusive_groups(
	spec: &ToolSpec,
	arguments: &Map<String, Value>,
	tool: &str,
) -> Result<(), McpError> {
	for group in spec
		.arg_groups
		.iter()
		.filter(|group| group.required && !group.multiple)
	{
		let active = group
			.args
			.iter()
			.filter_map(|id| spec.args.iter().find(|argument| argument.id == *id))
			.filter(|argument| argument_is_active(argument, arguments.get(&argument.id)))
			.count();
		if active != 1 {
			return Err(McpError::invalid_params(
				format!(
					"tool '{tool}' requires exactly one of arguments {}",
					group.args.join(", ")
				),
				None,
			));
		}
	}
	Ok(())
}

fn validate_argument(tool: &str, arg: &ClapArg, value: &Value) -> Result<(), McpError> {
	let invalid = |expected: &str| {
		invalid_argument(
			tool,
			arg,
			&format!("must be {expected}, got {}", json_type(value)),
		)
	};
	match arg.action.as_deref().unwrap_or("Set") {
		"SetTrue" | "SetFalse" => {
			if !value.is_boolean() {
				return Err(invalid("a boolean"));
			}
		}
		"Count" => {
			let Some(count) = value.as_u64() else {
				return Err(invalid("a non-negative integer"));
			};
			if count > MAX_COUNT_ARGUMENT {
				return Err(invalid_argument(
					tool,
					arg,
					&format!("must be at most {MAX_COUNT_ARGUMENT}"),
				));
			}
		}
		"Append" => validate_string_array(tool, arg, value, false)?,
		_ if argument_is_array(arg) => validate_string_array(tool, arg, value, true)?,
		_ => {
			if !value.is_string() {
				return Err(invalid("a string"));
			}
		}
	}
	Ok(())
}

fn constrain_count_schemas(tools: &mut [Tool], specs: &HashMap<String, ToolSpec>) {
	for tool in tools {
		let Some(spec) = specs.get(tool.name.as_ref()) else {
			continue;
		};
		let count_ids: HashSet<&str> = spec
			.args
			.iter()
			.filter(|argument| argument.action.as_deref() == Some("Count"))
			.map(|argument| argument.id.as_str())
			.collect();
		if count_ids.is_empty() {
			continue;
		}
		let schema = Arc::make_mut(&mut tool.input_schema);
		let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
			continue;
		};
		for id in count_ids {
			let Some(property) = properties.get_mut(id).and_then(Value::as_object_mut) else {
				continue;
			};
			property.insert("minimum".to_owned(), Value::from(0));
			property.insert("maximum".to_owned(), Value::from(MAX_COUNT_ARGUMENT));
		}
	}
}

fn constrain_required_exclusive_group_schemas(
	tools: &mut [Tool],
	specs: &HashMap<String, ToolSpec>,
) {
	for tool in tools {
		let Some(spec) = specs.get(tool.name.as_ref()) else {
			continue;
		};
		let constraints: Vec<Value> = spec
			.arg_groups
			.iter()
			.filter(|group| group.required && !group.multiple)
			.filter_map(|group| {
				let choices: Vec<Value> = group
					.args
					.iter()
					.filter_map(|id| spec.args.iter().find(|argument| argument.id == *id))
					.map(required_active_argument_schema)
					.collect();
				(!choices.is_empty()).then(|| {
					let mut constraint = Map::new();
					constraint.insert("oneOf".to_owned(), Value::Array(choices));
					Value::Object(constraint)
				})
			})
			.collect();
		if constraints.is_empty() {
			continue;
		}
		let schema = Arc::make_mut(&mut tool.input_schema);
		let all_of = schema
			.entry("allOf".to_owned())
			.or_insert_with(|| Value::Array(Vec::new()));
		let Some(all_of) = all_of.as_array_mut() else {
			continue;
		};
		all_of.extend(constraints);
	}
}

fn required_active_argument_schema(argument: &ClapArg) -> Value {
	let mut schema = Map::new();
	schema.insert(
		"required".to_owned(),
		Value::Array(vec![Value::String(argument.id.clone())]),
	);
	let mut property = Map::new();
	match argument.action.as_deref().unwrap_or("Set") {
		"SetTrue" => {
			property.insert("const".to_owned(), Value::Bool(true));
		}
		"SetFalse" => {
			property.insert("const".to_owned(), Value::Bool(false));
		}
		"Count" => {
			property.insert("minimum".to_owned(), Value::from(1));
		}
		"Append" => {
			property.insert("minItems".to_owned(), Value::from(1));
		}
		_ if argument_is_array(argument) => {
			property.insert("minItems".to_owned(), Value::from(1));
		}
		_ => {}
	}
	if !property.is_empty() {
		let mut properties = Map::new();
		properties.insert(argument.id.clone(), Value::Object(property));
		schema.insert("properties".to_owned(), Value::Object(properties));
	}
	Value::Object(schema)
}

fn validate_string_array(
	tool: &str,
	arg: &ClapArg,
	value: &Value,
	check_declared_range: bool,
) -> Result<(), McpError> {
	let Some(values) = value.as_array() else {
		return Err(invalid_argument(
			tool,
			arg,
			&format!("must be an array of strings, got {}", json_type(value)),
		));
	};
	if values.iter().any(|value| !value.is_string()) {
		return Err(invalid_argument(tool, arg, "must contain only strings"));
	}
	if arg.required && values.is_empty() {
		return Err(invalid_argument(tool, arg, "must not be empty"));
	}
	if check_declared_range
		&& let Some((minimum, maximum)) = arg.num_args.as_deref().and_then(argument_range)
		&& (values.len() < minimum || maximum.is_some_and(|maximum| values.len() > maximum))
	{
		let expected = maximum.map_or_else(
			|| format!("at least {minimum}"),
			|maximum| format!("between {minimum} and {maximum}"),
		);
		return Err(invalid_argument(
			tool,
			arg,
			&format!("must contain {expected} values"),
		));
	}
	Ok(())
}

fn invalid_argument(tool: &str, arg: &ClapArg, reason: &str) -> McpError {
	McpError::invalid_params(
		format!("invalid argument '{}' for tool '{tool}': {reason}", arg.id),
		None,
	)
}

fn json_type(value: &Value) -> &'static str {
	match value {
		Value::Null => "null",
		Value::Bool(_) => "a boolean",
		Value::Number(number) if number.is_i64() || number.is_u64() => "an integer",
		Value::Number(_) => "a number",
		Value::String(_) => "a string",
		Value::Array(_) => "an array",
		Value::Object(_) => "an object",
	}
}

fn argument_is_array(arg: &ClapArg) -> bool {
	arg
		.num_args
		.as_deref()
		.is_some_and(|range| range.contains("..") && !range.contains("=1"))
}

fn argument_is_active(arg: &ClapArg, value: Option<&Value>) -> bool {
	match arg.action.as_deref().unwrap_or("Set") {
		"SetTrue" => value.and_then(Value::as_bool) == Some(true),
		"SetFalse" => value.and_then(Value::as_bool) == Some(false),
		"Count" => value.and_then(Value::as_u64).is_some_and(|count| count > 0),
		"Append" => value
			.and_then(Value::as_array)
			.is_some_and(|values| !values.is_empty()),
		_ if argument_is_array(arg) => value
			.and_then(Value::as_array)
			.is_some_and(|values| !values.is_empty()),
		_ => value.is_some(),
	}
}

fn argument_range(range: &str) -> Option<(usize, Option<usize>)> {
	if let Some((minimum, maximum)) = range.split_once("..=") {
		return Some((minimum.parse().ok()?, Some(maximum.parse().ok()?)));
	}
	if let Some((minimum, maximum)) = range.split_once("..") {
		let maximum = (!maximum.is_empty())
			.then(|| maximum.parse().ok())
			.flatten();
		return Some((minimum.parse().ok()?, maximum));
	}
	None
}

fn build_argv(spec: &ToolSpec, arguments: &Map<String, Value>) -> Vec<String> {
	let mut argv = Vec::new();
	for arg in &spec.args[..spec.prefix_count] {
		render_option(arg, arguments.get(&arg.id), &mut argv);
	}
	argv.extend(spec.path.iter().cloned());

	let local: Vec<&ClapArg> = spec.args[spec.prefix_count..].iter().collect();
	for arg in local
		.iter()
		.copied()
		.filter(|arg| arg.long.is_some() || arg.short.is_some())
	{
		render_option(arg, arguments.get(&arg.id), &mut argv);
	}
	let mut positionals: Vec<&ClapArg> = local
		.into_iter()
		.filter(|arg| arg.long.is_none() && arg.short.is_none())
		.collect();
	positionals.sort_by_key(|arg| arg.index.unwrap_or(0));
	let positional_values: Vec<String> = positionals
		.into_iter()
		.filter_map(|arg| arguments.get(&arg.id))
		.flat_map(value_to_strings)
		.collect();
	if !positional_values.is_empty() {
		argv.push("--".to_owned());
		argv.extend(positional_values);
	}
	argv
}

fn render_option(arg: &ClapArg, value: Option<&Value>, output: &mut Vec<String>) {
	let spelling = arg
		.long
		.as_ref()
		.map(|long| format!("--{long}"))
		.or_else(|| arg.short.map(|short| format!("-{short}")));
	let Some(spelling) = spelling else { return };
	match arg.action.as_deref().unwrap_or("Set") {
		"SetTrue" if value.and_then(Value::as_bool).unwrap_or(false) => output.push(spelling),
		"SetFalse" if value.and_then(Value::as_bool) == Some(false) => output.push(spelling),
		"Count" => {
			for _ in 0..value.and_then(Value::as_u64).unwrap_or(0) {
				output.push(spelling.clone());
			}
		}
		"Append" => {
			for item in value.and_then(Value::as_array).into_iter().flatten() {
				push_attached(
					&spelling,
					item.as_str().expect("validated string value"),
					output,
				);
			}
		}
		"SetTrue" | "SetFalse" => {}
		_ => {
			if argument_is_array(arg) {
				if let Some(items) = value.and_then(Value::as_array) {
					for item in items {
						push_attached(
							&spelling,
							item.as_str().expect("validated string value"),
							output,
						);
					}
				}
			} else if let Some(item) = value.and_then(Value::as_str) {
				push_attached(&spelling, item, output);
			}
		}
	}
}

fn push_attached(spelling: &str, value: &str, output: &mut Vec<String>) {
	output.push(format!("{spelling}={value}"));
}

fn value_to_strings(value: &Value) -> Vec<String> {
	match value {
		Value::Array(values) => values
			.iter()
			.map(|value| value.as_str().expect("validated string value").to_owned())
			.collect(),
		Value::String(value) => vec![value.clone()],
		_ => unreachable!("MCP arguments are validated before rendering"),
	}
}

#[cfg(test)]
mod tests {
	use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
	use std::sync::mpsc;
	use std::time::Duration;

	use super::*;

	#[test]
	fn http_hosts_allow_only_loopback_authorities() {
		let ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 7000);
		let hosts = allowed_hosts_for_address(ipv4);
		assert!(hosts.contains(&"127.0.0.2:7000".to_owned()));
		assert!(hosts.contains(&"localhost:7000".to_owned()));

		let ipv6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7001);
		let hosts = allowed_hosts_for_address(ipv6);
		assert!(hosts.contains(&"[::1]:7001".to_owned()));
		assert!(validate_http_address(ipv4).is_ok());
		assert!(validate_http_address(ipv6).is_ok());
		assert!(
			validate_http_address(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7002)).is_err()
		);

		let origins = allowed_origins_for_address(ipv4);
		assert!(origins.contains(&"http://localhost:7000".to_owned()));
		assert!(origins.contains(&"http://127.0.0.2:7000".to_owned()));
		assert!(!origins.contains(&"https://attacker.example".to_owned()));
	}

	#[test]
	fn nested_tools_use_the_full_normalized_path() {
		let server = GtaMcpServer::new().expect("server schema");
		assert!(server.specs.contains_key("hash-object"));
		assert!(server.specs.contains_key("worktree_add"));
		assert!(server.specs.contains_key("submodule_status"));
		assert!(server.specs.contains_key("submodule_deinit"));
		assert!(server.specs.contains_key("submodule_set_branch"));
		assert!(server.specs.contains_key("remote_set_url"));
		assert!(server.specs.contains_key("add"));
		assert!(!server.specs.contains_key("status_2"));
		let spec = server.specs.get("submodule_update").unwrap();
		assert!(spec.args.iter().any(|argument| argument.id == "config"));
		let arguments = serde_json::from_value(serde_json::json!({
			"config": ["protocol.file.allow=always"],
			"init": true
		}))
		.unwrap();
		assert_eq!(
			build_argv(spec, &arguments),
			[
				"-c=protocol.file.allow=always",
				"submodule",
				"update",
				"--init"
			]
		);
		let deinit = server.specs.get("submodule_deinit").unwrap();
		let arguments = serde_json::from_value(serde_json::json!({
			"force": true,
			"paths": ["modules/one"]
		}))
		.unwrap();
		assert_eq!(
			build_argv(deinit, &arguments),
			["submodule", "deinit", "--force", "--path=modules/one"]
		);
		let set_branch = server.specs.get("submodule_set_branch").unwrap();
		let arguments = serde_json::from_value(serde_json::json!({
			"branch": "bad..name",
			"path": "modules/one"
		}))
		.unwrap();
		assert_eq!(
			build_argv(set_branch, &arguments),
			[
				"submodule",
				"set-branch",
				"--branch=bad..name",
				"--path=modules/one"
			]
		);
	}

	#[test]
	fn options_precede_a_terminated_positional_suffix() {
		let server = GtaMcpServer::new().expect("server schema");
		let hash = server.specs.get("hash-object").unwrap();
		let arguments = serde_json::from_value(serde_json::json!({
			"write": true,
			"file": "-input"
		}))
		.unwrap();
		assert_eq!(
			build_argv(hash, &arguments),
			["hash-object", "-w", "--", "-input"]
		);

		let ls_files = server.specs.get("ls-files").unwrap();
		let arguments = serde_json::from_value(serde_json::json!({
			"cached": true,
			"pathspecs": ["-first", "second"]
		}))
		.unwrap();
		assert_eq!(
			build_argv(ls_files, &arguments),
			["ls-files", "--cached", "--", "-first", "second"]
		);
	}

	#[test]
	fn argument_validation_rejects_values_the_schema_does_not_advertise() {
		let server = GtaMcpServer::new().expect("server schema");
		let init = server.specs.get("init").unwrap();
		for arguments in [
			serde_json::json!({ "path": {} }),
			serde_json::json!({ "directory": {} }),
			serde_json::json!({ "config": "core.bare=true" }),
			serde_json::json!({ "config": ["core.bare=true", false] }),
		] {
			let arguments = serde_json::from_value(arguments).unwrap();
			assert!(validate_arguments(init, &arguments, "init").is_err());
		}

		let cat_file = server.specs.get("cat-file").unwrap();
		assert!(validate_arguments(cat_file, &Map::new(), "cat-file").is_err());
	}

	#[test]
	fn required_exclusive_groups_are_enforced_by_schema_and_runtime() {
		let server = GtaMcpServer::new().expect("server schema");
		let spec = server.specs.get("submodule_set_branch").unwrap();
		assert!(spec.arg_groups.iter().any(|group| {
			group.required && !group.multiple && group.args == ["branch".to_owned(), "default".to_owned()]
		}));

		for arguments in [
			serde_json::json!({ "branch": "main", "path": "modules/one" }),
			serde_json::json!({ "default": true, "path": "modules/one" }),
			serde_json::json!({
				"branch": "main",
				"default": false,
				"path": "modules/one"
			}),
		] {
			let arguments = serde_json::from_value(arguments).unwrap();
			validate_arguments(spec, &arguments, "submodule_set_branch").unwrap();
		}
		for arguments in [
			serde_json::json!({ "path": "modules/one" }),
			serde_json::json!({ "default": false, "path": "modules/one" }),
			serde_json::json!({
				"branch": "main",
				"default": true,
				"path": "modules/one"
			}),
		] {
			let arguments = serde_json::from_value(arguments).unwrap();
			assert!(validate_arguments(spec, &arguments, "submodule_set_branch").is_err());
		}

		let tool = server
			.tools
			.iter()
			.find(|tool| tool.name.as_ref() == "submodule_set_branch")
			.unwrap();
		let all_of = tool.input_schema["allOf"].as_array().unwrap();
		let choices = all_of
			.iter()
			.find_map(|constraint| constraint["oneOf"].as_array())
			.unwrap();
		assert_eq!(choices.len(), 2);
		assert!(
			choices
				.iter()
				.any(|choice| { choice["required"] == serde_json::json!(["branch"]) })
		);
		assert!(choices.iter().any(|choice| {
			choice["required"] == serde_json::json!(["default"])
				&& choice["properties"]["default"]["const"] == true
		}));
	}

	#[test]
	fn count_arguments_are_bounded_before_argv_expansion() {
		let server = GtaMcpServer::new().expect("server schema");
		let remove = server.specs.get("worktree_remove").unwrap();
		let maximum = serde_json::from_value(serde_json::json!({
			"path": "worktree",
			"force": MAX_COUNT_ARGUMENT,
		}))
		.unwrap();
		validate_arguments(remove, &maximum, "worktree_remove").unwrap();
		assert_eq!(
			build_argv(remove, &maximum)
				.iter()
				.filter(|argument| argument.as_str() == "--force")
				.count(),
			MAX_COUNT_ARGUMENT as usize
		);

		let excessive = serde_json::from_value(serde_json::json!({
			"path": "worktree",
			"force": MAX_COUNT_ARGUMENT + 1,
		}))
		.unwrap();
		assert!(validate_arguments(remove, &excessive, "worktree_remove").is_err());

		let tool = server
			.tools
			.iter()
			.find(|tool| tool.name.as_ref() == "worktree_remove")
			.unwrap();
		assert_eq!(
			tool.input_schema["properties"]["force"]["minimum"],
			Value::from(0)
		);
		assert_eq!(
			tool.input_schema["properties"]["force"]["maximum"],
			Value::from(MAX_COUNT_ARGUMENT)
		);
	}

	#[test]
	fn option_values_are_attached_to_their_spelling() {
		let server = GtaMcpServer::new().expect("server schema");
		let commit = server.specs.get("commit").unwrap();
		let arguments = serde_json::from_value(serde_json::json!({
			"message": "-hello"
		}))
		.unwrap();
		validate_arguments(commit, &arguments, "commit").unwrap();
		assert_eq!(
			build_argv(commit, &arguments),
			["commit", "--message=-hello"]
		);

		let hash = server.specs.get("hash-object").unwrap();
		let arguments = serde_json::from_value(serde_json::json!({
			"kind": "-commit",
			"stdin": true
		}))
		.unwrap();
		validate_arguments(hash, &arguments, "hash-object").unwrap();
		assert_eq!(
			build_argv(hash, &arguments),
			["hash-object", "-t=-commit", "--stdin"]
		);
	}

	#[test]
	fn failed_tools_retain_completed_stdout_before_stderr() {
		assert_eq!(
			failed_process_message("1", " completed module  \n", " later module failed  \r\n"),
			"Tool process exited with non-zero status (1)\nstdout:\n completed module  \nstderr:\n later module failed  "
		);
	}

	#[test]
	fn successful_tools_preserve_leading_status_sigils() {
		assert_eq!(successful_process_message(" M tracked\n", ""), " M tracked");
		assert_eq!(
			successful_process_message(" abc123 modules/one\r\n", ""),
			" abc123 modules/one"
		);
		assert_eq!(
			successful_process_message("result  \n", " warning  \n"),
			"result  \nstderr:\n warning  "
		);
	}

	#[tokio::test]
	async fn cancelled_calls_retain_serialization_until_the_worker_finishes() {
		let lock = Arc::new(tokio::sync::Mutex::new(()));
		let (started_tx, started_rx) = tokio::sync::oneshot::channel();
		let (release_tx, release_rx) = mpsc::channel();
		let task_lock = lock.clone();
		let task = tokio::spawn(async move {
			serialized_blocking(task_lock, move || {
				let _ = started_tx.send(());
				release_rx.recv().expect("release worker");
			})
			.await
		});

		started_rx.await.expect("worker started");
		task.abort();
		tokio::task::yield_now().await;
		assert!(
			lock.try_lock().is_err(),
			"caller cancellation must not release the worker's execution lock"
		);

		release_tx.send(()).expect("finish worker");
		let guard = tokio::time::timeout(Duration::from_secs(5), lock.lock())
			.await
			.expect("worker releases the lock after completion");
		drop(guard);
	}
}
