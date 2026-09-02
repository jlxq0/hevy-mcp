//! MCP tool catalogue for Hevy's public REST API.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{Instrument as _, Span};

use crate::audit::{self, HEVY_RATE_LIMITED_CODE, outcome};
use crate::auth::AccessToken;
use crate::hevy_client::{
    BodyMeasurementInput, CreateExerciseTemplateBody, CreateRoutineFolderBody, HevyClient,
    HevyError, RoutineInput, UpdateBodyMeasurementInput, WorkoutInput,
};
use crate::rate_limit::{Category, Limiter};

const HEVY_API_KEY_MISSING_CODE: i32 = -32_010;
const HEVY_API_KEY_REJECTED_CODE: i32 = -32_011;

#[derive(Clone)]
pub struct HevyMcpService {
    hevy: HevyClient,
    rate_limiter: Arc<Limiter>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for HevyMcpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HevyMcpService").finish()
    }
}

impl HevyMcpService {
    pub fn new(hevy: HevyClient, rate_limiter: Arc<Limiter>) -> Self {
        Self {
            hevy,
            rate_limiter,
            tool_router: Self::hevy_router(),
        }
    }

    fn rate_limit_check(
        &self,
        context: &RequestContext<RoleServer>,
        category: Category,
    ) -> Result<(), ErrorData> {
        let token = token_from_context(context).ok_or_else(missing_token_error)?;
        let bearer_hash = audit::token_hash(&token.0);
        self.rate_limiter
            .check(&bearer_hash, category)
            .map_err(|_| {
                structured_error(
                    audit::RATE_LIMITED_CODE,
                    "rate_limited",
                    "rate limit exceeded; try again in a minute",
                )
            })
    }

    fn hevy_from_context(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<HevyClient, ErrorData> {
        let token = token_from_context(context).ok_or_else(missing_token_error)?;
        Ok(self.hevy.with_api_key(&token.0))
    }

    async fn run_hevy_call<F>(
        &self,
        context: &RequestContext<RoleServer>,
        tool: &'static str,
        resource: Option<&str>,
        category: Category,
        future: F,
    ) -> Result<rmcp::model::CallToolResult, ErrorData>
    where
        F: Future<Output = Result<Value, HevyError>>,
    {
        let started = Instant::now();
        let token_hash = token_hash_from_context(context).unwrap_or_default();
        let span = make_tool_span(tool, &token_hash, resource);
        let result = async {
            self.rate_limit_check(context, category)?;
            let value = future.await.map_err(map_hevy_error)?;
            structured_result(&value)
        }
        .instrument(span.clone())
        .await;
        emit_tool_audit(tool, &token_hash, resource, started, None, &span, &result);
        result
    }
}

fn token_from_context(context: &RequestContext<RoleServer>) -> Option<AccessToken> {
    let parts = context.extensions.get::<http::request::Parts>()?;
    parts.extensions.get::<AccessToken>().cloned()
}

fn token_hash_from_context(context: &RequestContext<RoleServer>) -> Option<String> {
    token_from_context(context).map(|token| audit::token_hash(&token.0))
}

fn structured_result<T: Serialize>(value: &T) -> Result<rmcp::model::CallToolResult, ErrorData> {
    let value = serde_json::to_value(value).map_err(|error| {
        ErrorData::internal_error(format!("serialize tool result: {error}"), None)
    })?;
    Ok(rmcp::model::CallToolResult::structured(value))
}

fn missing_token_error() -> ErrorData {
    ErrorData::internal_error("no access token in request context", None)
}

/// Build an application error whose `data` a caller can match on.
///
/// `code` identifies the exact condition; `class` is the single predicate,
/// derived from `audit::class_for_code` so the wire, the audit event and the
/// metric always agree. Both rate limits carry `class == "rate_limited"`, so a
/// caller distinguishing "unreadable" from "empty" writes one comparison
/// rather than enumerating codes or substring-matching an English sentence.
fn structured_error(code: i32, condition: &str, message: &str) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(code),
        message.to_owned(),
        Some(json!({ "code": condition, "class": audit::class_for_code(code) })),
    )
}

fn map_hevy_error(error: HevyError) -> ErrorData {
    match error {
        HevyError::ApiKeyMissing => structured_error(
            HEVY_API_KEY_MISSING_CODE,
            "hevy_api_key_missing",
            "hevy_api_key_missing",
        ),
        HevyError::Unauthorized => structured_error(
            HEVY_API_KEY_REJECTED_CODE,
            "hevy_api_key_rejected",
            "hevy_api_key_rejected",
        ),
        HevyError::RateLimited => structured_error(
            HEVY_RATE_LIMITED_CODE,
            "hevy_rate_limited",
            "hevy_rate_limited",
        ),
        HevyError::InvalidInput(message) => ErrorData::invalid_params(message, None),
        HevyError::NotFound => ErrorData::invalid_params("Hevy resource not found", None),
        HevyError::Conflict => ErrorData::invalid_params("Hevy resource conflict", None),
        other => ErrorData::internal_error(other.code(), None),
    }
}

fn make_tool_span(tool: &'static str, token_hash: &str, resource: Option<&str>) -> Span {
    tracing::info_span!(
        "mcp.tool",
        tool,
        token_hash,
        resource = resource.unwrap_or(""),
        outcome = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
    )
}

fn emit_tool_audit(
    tool: &'static str,
    token_hash: &str,
    resource: Option<&str>,
    started: Instant,
    result_count: Option<usize>,
    span: &Span,
    result: &Result<rmcp::model::CallToolResult, ErrorData>,
) {
    let (outcome_value, error_class) = match result {
        Ok(_) => (outcome::OK, None),
        Err(error) => {
            // One function decides, so the two fields cannot disagree about
            // one event. See `audit::error_class`.
            let class = audit::error_class(error);
            let outcome_value = if class == outcome::RATE_LIMITED {
                outcome::RATE_LIMITED
            } else {
                outcome::ERROR
            };
            (outcome_value, Some(class))
        }
    };
    span.record("outcome", outcome_value);
    span.record(
        "latency_ms",
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    audit::tool_call(
        tool,
        token_hash,
        resource,
        outcome_value,
        started,
        result_count,
        error_class,
    );
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PaginationParams {
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WorkoutIdParams {
    workout_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateWorkoutParams {
    workout: WorkoutInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateWorkoutParams {
    workout_id: String,
    workout: WorkoutInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WorkoutEventsParams {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RoutineIdParams {
    routine_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateRoutineParams {
    routine: RoutineInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateRoutineParams {
    routine_id: String,
    routine: RoutineInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchTemplatesParams {
    /// Case-insensitive match against title, muscle groups, and equipment.
    query: String,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExerciseTemplateIdParams {
    exercise_template_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateExerciseTemplateParams {
    body: CreateExerciseTemplateBody,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RoutineFolderIdParams {
    folder_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateRoutineFolderParams {
    body: CreateRoutineFolderBody,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExerciseHistoryParams {
    exercise_template_id: String,
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    end_date: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BodyMeasurementDateParams {
    /// Measurement date in YYYY-MM-DD format.
    date: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateBodyMeasurementParams {
    body: BodyMeasurementInput,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateBodyMeasurementParams {
    /// Measurement date in YYYY-MM-DD format.
    date: String,
    body: UpdateBodyMeasurementInput,
}

#[tool_router(router = hevy_router)]
impl HevyMcpService {
    #[tool(
        description = "Return the Hevy user profile for the request's API key.",
        annotations(title = "Who am I", read_only_hint = true, idempotent_hint = true)
    )]
    async fn whoami(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(&context, "whoami", None, Category::Read, hevy.user_info())
            .await
    }

    #[tool(
        description = "List Hevy workouts.",
        annotations(title = "List workouts", read_only_hint = true, idempotent_hint = true)
    )]
    async fn list_workouts(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<PaginationParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "list_workouts",
            None,
            Category::Read,
            hevy.list_workouts(params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "Get one Hevy workout by ID.",
        annotations(title = "Get workout", read_only_hint = true, idempotent_hint = true)
    )]
    async fn get_workout(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<WorkoutIdParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "get_workout",
            Some(&params.workout_id),
            Category::Read,
            hevy.get_workout(&params.workout_id),
        )
        .await
    }

    #[tool(
        description = "Create a Hevy workout. Every exercise requires an exercise_template_id returned by the template tools.",
        annotations(
            title = "Create workout",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_workout(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<CreateWorkoutParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "create_workout",
            None,
            Category::Write,
            hevy.create_workout(&params.workout),
        )
        .await
    }

    #[tool(
        description = "Replace a Hevy workout by ID. Every exercise requires an exercise_template_id returned by the template tools.",
        annotations(
            title = "Update workout",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_workout(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<UpdateWorkoutParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "update_workout",
            Some(&params.workout_id),
            Category::Write,
            hevy.update_workout(&params.workout_id, &params.workout),
        )
        .await
    }

    #[tool(
        description = "Count workouts on the Hevy account.",
        annotations(
            title = "Count workouts",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn count_workouts(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "count_workouts",
            None,
            Category::Read,
            hevy.count_workouts(),
        )
        .await
    }

    #[tool(
        description = "List paginated workout update/delete events, optionally since an ISO 8601 instant.",
        annotations(
            title = "List workout events",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_workout_events(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<WorkoutEventsParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "list_workout_events",
            None,
            Category::Read,
            hevy.list_workout_events(params.since.as_deref(), params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "List Hevy routines.",
        annotations(title = "List routines", read_only_hint = true, idempotent_hint = true)
    )]
    async fn list_routines(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<PaginationParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "list_routines",
            None,
            Category::Read,
            hevy.list_routines(params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "Get one Hevy routine by ID.",
        annotations(title = "Get routine", read_only_hint = true, idempotent_hint = true)
    )]
    async fn get_routine(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<RoutineIdParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "get_routine",
            Some(&params.routine_id),
            Category::Read,
            hevy.get_routine(&params.routine_id),
        )
        .await
    }

    #[tool(
        description = "Create a Hevy routine. Every exercise requires an exercise_template_id returned by the template tools.",
        annotations(
            title = "Create routine",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_routine(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<CreateRoutineParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "create_routine",
            None,
            Category::Write,
            hevy.create_routine(&params.routine),
        )
        .await
    }

    #[tool(
        description = "Replace a Hevy routine by ID. Every exercise requires an exercise_template_id returned by the template tools.",
        annotations(
            title = "Update routine",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_routine(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<UpdateRoutineParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "update_routine",
            Some(&params.routine_id),
            Category::Write,
            hevy.update_routine(&params.routine_id, &params.routine),
        )
        .await
    }

    #[tool(
        description = "List exercise templates available to the Hevy account.",
        annotations(
            title = "List exercise templates",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_exercise_templates(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<PaginationParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "list_exercise_templates",
            None,
            Category::Read,
            hevy.list_exercise_templates(params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "Search exercise templates client-side by title, muscle group, or equipment. This calls only Hevy's list endpoint.",
        annotations(
            title = "Search exercise templates",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn search_exercise_templates(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<SearchTemplatesParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "search_exercise_templates",
            None,
            Category::Read,
            hevy.search_exercise_templates(&params.query, params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "Get one exercise template by its Hevy ID.",
        annotations(
            title = "Get exercise template",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn get_exercise_template(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<ExerciseTemplateIdParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "get_exercise_template",
            Some(&params.exercise_template_id),
            Category::Read,
            hevy.get_exercise_template(&params.exercise_template_id),
        )
        .await
    }

    #[tool(
        description = "Create a custom Hevy exercise template.",
        annotations(
            title = "Create exercise template",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_exercise_template(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<CreateExerciseTemplateParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "create_exercise_template",
            None,
            Category::Write,
            hevy.create_exercise_template(&params.body),
        )
        .await
    }

    #[tool(
        description = "List Hevy routine folders.",
        annotations(
            title = "List routine folders",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_routine_folders(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<PaginationParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "list_routine_folders",
            None,
            Category::Read,
            hevy.list_routine_folders(params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "Get one Hevy routine folder by ID.",
        annotations(
            title = "Get routine folder",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn get_routine_folder(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<RoutineFolderIdParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let resource = params.folder_id.to_string();
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "get_routine_folder",
            Some(&resource),
            Category::Read,
            hevy.get_routine_folder(params.folder_id),
        )
        .await
    }

    #[tool(
        description = "Create a Hevy routine folder.",
        annotations(
            title = "Create routine folder",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_routine_folder(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<CreateRoutineFolderParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "create_routine_folder",
            None,
            Category::Write,
            hevy.create_routine_folder(&params.body),
        )
        .await
    }

    #[tool(
        description = "Get exercise history for one exercise template, optionally bounded by ISO 8601 start and end dates.",
        annotations(
            title = "Get exercise history",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn get_exercise_history(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<ExerciseHistoryParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "get_exercise_history",
            Some(&params.exercise_template_id),
            Category::Read,
            hevy.get_exercise_history(
                &params.exercise_template_id,
                params.start_date.as_deref(),
                params.end_date.as_deref(),
            ),
        )
        .await
    }

    #[tool(
        description = "List Hevy body measurements.",
        annotations(
            title = "List body measurements",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_body_measurements(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<PaginationParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "list_body_measurements",
            None,
            Category::Read,
            hevy.list_body_measurements(params.page, params.page_size),
        )
        .await
    }

    #[tool(
        description = "Get one body measurement by YYYY-MM-DD date.",
        annotations(
            title = "Get body measurement",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn get_body_measurement(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<BodyMeasurementDateParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "get_body_measurement",
            Some(&params.date),
            Category::Read,
            hevy.get_body_measurement(&params.date),
        )
        .await
    }

    #[tool(
        description = "Create a body measurement for a YYYY-MM-DD date.",
        annotations(
            title = "Create body measurement",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_body_measurement(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<CreateBodyMeasurementParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "create_body_measurement",
            Some(&params.body.date),
            Category::Write,
            hevy.create_body_measurement(&params.body),
        )
        .await
    }

    #[tool(
        description = "Replace the body measurement at a YYYY-MM-DD date. Omitted measurement fields become null in Hevy.",
        annotations(
            title = "Update body measurement",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_body_measurement(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<UpdateBodyMeasurementParams>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let hevy = self.hevy_from_context(&context)?;
        self.run_hevy_call(
            &context,
            "update_body_measurement",
            Some(&params.date),
            Category::Write,
            hevy.update_body_measurement(&params.date, &params.body),
        )
        .await
    }
}

// `unused_async_trait_impl` (new in clippy 1.98) fires on the `ServerHandler`
// methods `#[tool_handler]` generates, not on anything written here. The macro
// decides whether its expansions await, so there is nothing to rewrite.
//
// This attribute is what pins the tree's lint floor at clippy 1.98 exactly: an
// `#[allow]` naming a lint the running clippy does not have is itself an error
// under `-D warnings`, so this line reds 1.97 and below. The tree still
// *builds* on 1.93, which is what `rust-version` says and what the Dockerfile's
// `rust:1.93-bookworm` builder enforces. See AGENTS.md.
#[allow(clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for HevyMcpService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "hevy-mcp manages Julian's Hevy account through the official Hevy REST API. \
             Use list/search/get_exercise_template before creating workouts or routines; \
             every exercise requires a returned exercise_template_id. Writes execute immediately.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect was two fields of one event disagreeing, so the assertion is
    /// the agreement rather than either value on its own.
    #[test]
    fn outcome_and_error_class_agree_on_every_code() {
        for code in [
            audit::RATE_LIMITED_CODE,
            HEVY_RATE_LIMITED_CODE,
            HEVY_API_KEY_REJECTED_CODE,
            HEVY_API_KEY_MISSING_CODE,
            -32602,
            -32603,
        ] {
            let error = structured_error(code, "probe", "probe");
            let class = audit::error_class(&error);
            let outcome_value = if class == outcome::RATE_LIMITED {
                outcome::RATE_LIMITED
            } else {
                outcome::ERROR
            };
            assert_eq!(
                class == outcome::RATE_LIMITED,
                outcome_value == outcome::RATE_LIMITED,
                "code {code} classified two ways"
            );
            let wire_class = error
                .data
                .as_ref()
                .and_then(|data| data.get("class"))
                .and_then(Value::as_str);
            assert_eq!(
                wire_class,
                Some(class),
                "code {code}: wire class differs from audit class"
            );
        }

        // Both rate limits, specifically, reach the rate-limited outcome.
        for code in [audit::RATE_LIMITED_CODE, HEVY_RATE_LIMITED_CODE] {
            assert_eq!(
                audit::error_class(&structured_error(code, "probe", "probe")),
                outcome::RATE_LIMITED,
                "code {code} is not counted as rate limited"
            );
        }
    }

    #[test]
    fn missing_key_error_has_machine_readable_code() {
        let error = map_hevy_error(HevyError::ApiKeyMissing);
        assert_eq!(error.code.0, HEVY_API_KEY_MISSING_CODE);
        assert_eq!(error.message, "hevy_api_key_missing");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&Value::String("hevy_api_key_missing".to_owned()))
        );
    }
}
