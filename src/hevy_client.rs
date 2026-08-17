//! Thin first-party client for Hevy's public REST API.
//!
//! The API key is attached per request through [`HevyClient::with_api_key`].
//! It is sent only as the `api-key` header and omitted from every `Debug`
//! value and error message.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

const DEFAULT_PAGE: u32 = 1;
const DEFAULT_PAGE_SIZE: u32 = 5;
const MAX_STANDARD_PAGE_SIZE: u32 = 10;
const MAX_TEMPLATE_PAGE_SIZE: u32 = 100;
const MAX_TEMPLATE_SEARCH_PAGES: u32 = 1_000;

#[derive(Debug, Error)]
pub enum HevyError {
    #[error("hevy_api_key_missing")]
    ApiKeyMissing,
    #[error("invalid Hevy request: {0}")]
    InvalidInput(String),
    #[error("hevy_api_key_rejected")]
    Unauthorized,
    #[error("Hevy resource not found")]
    NotFound,
    #[error("Hevy resource conflict")]
    Conflict,
    #[error("Hevy API rate limit exceeded")]
    RateLimited,
    #[error("Hevy API returned HTTP {status}")]
    Upstream { status: u16 },
    #[error("Hevy API transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("Hevy API returned invalid JSON")]
    InvalidJson(#[source] serde_json::Error),
    #[error("could not serialize Hevy request")]
    Serialization(#[source] serde_json::Error),
}

impl HevyError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ApiKeyMissing => "hevy_api_key_missing",
            Self::InvalidInput(_) => "hevy_invalid_input",
            Self::Unauthorized => "hevy_api_key_rejected",
            Self::NotFound => "hevy_not_found",
            Self::Conflict => "hevy_conflict",
            Self::RateLimited => "hevy_rate_limited",
            Self::Upstream { .. } => "hevy_upstream_error",
            Self::Transport(_) => "hevy_transport_error",
            Self::InvalidJson(_) => "hevy_invalid_response",
            Self::Serialization(_) => "hevy_serialization_error",
        }
    }
}

#[derive(Clone)]
pub struct HevyClient {
    http: reqwest::Client,
    base_url: Url,
    api_key: Option<String>,
}

impl std::fmt::Debug for HevyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HevyClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

impl HevyClient {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let mut base_url = Url::parse(base_url)?;
        anyhow::ensure!(
            matches!(base_url.scheme(), "http" | "https") && base_url.host_str().is_some(),
            "Hevy base URL must be an absolute http(s) URL"
        );
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("hevy-mcp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base_url,
            api_key: None,
        })
    }

    /// Clone the shared HTTP client and base URL with one request's Hevy key.
    #[must_use]
    pub fn with_api_key(&self, key: &str) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            api_key: (!key.trim().is_empty()).then(|| key.to_owned()),
        }
    }

    pub async fn user_info(&self) -> Result<Value, HevyError> {
        self.get_json("v1/user/info", Vec::new()).await
    }

    pub async fn list_workouts(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let query = pagination_query(page, page_size, MAX_STANDARD_PAGE_SIZE)?;
        self.get_json("v1/workouts", query).await
    }

    pub async fn get_workout(&self, workout_id: &str) -> Result<Value, HevyError> {
        validate_id(workout_id, "workout_id")?;
        self.get_json(&format!("v1/workouts/{workout_id}"), Vec::new())
            .await
    }

    pub async fn create_workout(&self, workout: &WorkoutInput) -> Result<Value, HevyError> {
        workout.validate()?;
        self.post_json("v1/workouts", &json!({ "workout": workout }))
            .await
    }

    pub async fn update_workout(
        &self,
        workout_id: &str,
        workout: &WorkoutInput,
    ) -> Result<Value, HevyError> {
        validate_id(workout_id, "workout_id")?;
        workout.validate()?;
        self.put_json(
            &format!("v1/workouts/{workout_id}"),
            &json!({ "workout": workout }),
        )
        .await
    }

    pub async fn count_workouts(&self) -> Result<Value, HevyError> {
        self.get_json("v1/workouts/count", Vec::new()).await
    }

    pub async fn list_workout_events(
        &self,
        since: Option<&str>,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let mut query = pagination_query(page, page_size, MAX_STANDARD_PAGE_SIZE)?;
        if let Some(since) = nonempty_optional(since, "since")? {
            query.push(("since".to_owned(), since.to_owned()));
        }
        self.get_json("v1/workouts/events", query).await
    }

    pub async fn list_routines(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let query = pagination_query(page, page_size, MAX_STANDARD_PAGE_SIZE)?;
        self.get_json("v1/routines", query).await
    }

    pub async fn get_routine(&self, routine_id: &str) -> Result<Value, HevyError> {
        validate_id(routine_id, "routine_id")?;
        self.get_json(&format!("v1/routines/{routine_id}"), Vec::new())
            .await
    }

    pub async fn create_routine(&self, routine: &RoutineInput) -> Result<Value, HevyError> {
        routine.validate()?;
        self.post_json("v1/routines", &json!({ "routine": routine }))
            .await
    }

    pub async fn update_routine(
        &self,
        routine_id: &str,
        routine: &RoutineInput,
    ) -> Result<Value, HevyError> {
        validate_id(routine_id, "routine_id")?;
        routine.validate()?;
        self.put_json(
            &format!("v1/routines/{routine_id}"),
            &json!({ "routine": routine }),
        )
        .await
    }

    pub async fn list_exercise_templates(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let query = pagination_query(page, page_size, MAX_TEMPLATE_PAGE_SIZE)?;
        self.get_json("v1/exercise_templates", query).await
    }

    pub async fn search_exercise_templates(
        &self,
        search: &str,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let search = search.trim();
        if search.is_empty() || search.len() > 256 {
            return Err(HevyError::InvalidInput(
                "query must contain 1 to 256 bytes".to_owned(),
            ));
        }
        let (page, page_size) = pagination(page, page_size, MAX_TEMPLATE_PAGE_SIZE)?;
        let needle = search.to_lowercase();
        let mut matches = Vec::new();
        let mut api_page = 1;
        loop {
            let response = self
                .list_exercise_templates(Some(api_page), Some(MAX_TEMPLATE_PAGE_SIZE))
                .await?;
            let templates = response
                .get("exercise_templates")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    HevyError::InvalidInput("Hevy response omitted exercise_templates".to_owned())
                })?;
            matches.extend(
                templates
                    .iter()
                    .filter(|template| template_matches(template, &needle))
                    .cloned(),
            );
            let page_count = response
                .get("page_count")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(api_page);
            if api_page >= page_count || templates.is_empty() {
                break;
            }
            api_page = api_page.saturating_add(1);
            if api_page > MAX_TEMPLATE_SEARCH_PAGES {
                return Err(HevyError::InvalidInput(
                    "exercise template result exceeded safety page limit".to_owned(),
                ));
            }
        }

        let total = matches.len();
        let page_size_usize = usize::try_from(page_size).unwrap_or(usize::MAX);
        let start = usize::try_from(page.saturating_sub(1))
            .unwrap_or(usize::MAX)
            .saturating_mul(page_size_usize);
        let page_matches: Vec<Value> = matches
            .into_iter()
            .skip(start)
            .take(page_size_usize)
            .collect();
        let page_count = total.div_ceil(page_size_usize);
        Ok(json!({
            "page": page,
            "page_count": page_count,
            "total_count": total,
            "exercise_templates": page_matches,
        }))
    }

    pub async fn get_exercise_template(
        &self,
        exercise_template_id: &str,
    ) -> Result<Value, HevyError> {
        validate_id(exercise_template_id, "exercise_template_id")?;
        self.get_json(
            &format!("v1/exercise_templates/{exercise_template_id}"),
            Vec::new(),
        )
        .await
    }

    pub async fn create_exercise_template(
        &self,
        body: &CreateExerciseTemplateBody,
    ) -> Result<Value, HevyError> {
        body.validate()?;
        self.post_json("v1/exercise_templates", body).await
    }

    pub async fn list_routine_folders(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let query = pagination_query(page, page_size, MAX_STANDARD_PAGE_SIZE)?;
        self.get_json("v1/routine_folders", query).await
    }

    pub async fn get_routine_folder(&self, folder_id: u64) -> Result<Value, HevyError> {
        self.get_json(&format!("v1/routine_folders/{folder_id}"), Vec::new())
            .await
    }

    pub async fn create_routine_folder(
        &self,
        body: &CreateRoutineFolderBody,
    ) -> Result<Value, HevyError> {
        if body.routine_folder.title.trim().is_empty() {
            return Err(HevyError::InvalidInput(
                "routine folder title must not be empty".to_owned(),
            ));
        }
        self.post_json("v1/routine_folders", body).await
    }

    pub async fn get_exercise_history(
        &self,
        exercise_template_id: &str,
        start_date: Option<&str>,
        end_date: Option<&str>,
    ) -> Result<Value, HevyError> {
        validate_id(exercise_template_id, "exercise_template_id")?;
        let mut query = Vec::new();
        if let Some(start_date) = nonempty_optional(start_date, "start_date")? {
            query.push(("start_date".to_owned(), start_date.to_owned()));
        }
        if let Some(end_date) = nonempty_optional(end_date, "end_date")? {
            query.push(("end_date".to_owned(), end_date.to_owned()));
        }
        self.get_json(
            &format!("v1/exercise_history/{exercise_template_id}"),
            query,
        )
        .await
    }

    pub async fn list_body_measurements(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Value, HevyError> {
        let query = pagination_query(page, page_size, MAX_STANDARD_PAGE_SIZE)?;
        self.get_json("v1/body_measurements", query).await
    }

    pub async fn get_body_measurement(&self, date: &str) -> Result<Value, HevyError> {
        validate_date(date)?;
        self.get_json(&format!("v1/body_measurements/{date}"), Vec::new())
            .await
    }

    pub async fn create_body_measurement(
        &self,
        body: &BodyMeasurementInput,
    ) -> Result<Value, HevyError> {
        validate_date(&body.date)?;
        self.post_json("v1/body_measurements", body).await
    }

    pub async fn update_body_measurement(
        &self,
        date: &str,
        body: &UpdateBodyMeasurementInput,
    ) -> Result<Value, HevyError> {
        validate_date(date)?;
        self.put_json(&format!("v1/body_measurements/{date}"), body)
            .await
    }

    async fn get_json(&self, path: &str, query: Vec<(String, String)>) -> Result<Value, HevyError> {
        self.request_json(Method::GET, path, query, None).await
    }

    async fn post_json<T: Serialize + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<Value, HevyError> {
        let body = serde_json::to_value(body).map_err(HevyError::Serialization)?;
        self.request_json(Method::POST, path, Vec::new(), Some(body))
            .await
    }

    async fn put_json<T: Serialize + Sync>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<Value, HevyError> {
        let body = serde_json::to_value(body).map_err(HevyError::Serialization)?;
        self.request_json(Method::PUT, path, Vec::new(), Some(body))
            .await
    }

    async fn request_json(
        &self,
        method: Method,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> Result<Value, HevyError> {
        let api_key = self.api_key.as_deref().ok_or(HevyError::ApiKeyMissing)?;
        let url = self
            .base_url
            .join(path)
            .map_err(|error| HevyError::InvalidInput(error.to_string()))?;
        let mut request = self
            .http
            .request(method, url)
            .header("api-key", api_key)
            .query(&query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(HevyError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(map_status(status));
        }
        let bytes = response.bytes().await.map_err(HevyError::Transport)?;
        if bytes.is_empty() {
            return Ok(json!({ "ok": true }));
        }
        serde_json::from_slice(&bytes).map_err(HevyError::InvalidJson)
    }
}

fn map_status(status: StatusCode) -> HevyError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => HevyError::Unauthorized,
        StatusCode::NOT_FOUND => HevyError::NotFound,
        StatusCode::CONFLICT => HevyError::Conflict,
        StatusCode::TOO_MANY_REQUESTS => HevyError::RateLimited,
        _ => HevyError::Upstream {
            status: status.as_u16(),
        },
    }
}

fn pagination(
    page: Option<u32>,
    page_size: Option<u32>,
    max_page_size: u32,
) -> Result<(u32, u32), HevyError> {
    let page = page.unwrap_or(DEFAULT_PAGE);
    let page_size = page_size.unwrap_or(DEFAULT_PAGE_SIZE);
    if page == 0 {
        return Err(HevyError::InvalidInput(
            "page must be at least 1".to_owned(),
        ));
    }
    if page_size == 0 || page_size > max_page_size {
        return Err(HevyError::InvalidInput(format!(
            "page_size must be between 1 and {max_page_size}"
        )));
    }
    Ok((page, page_size))
}

fn pagination_query(
    page: Option<u32>,
    page_size: Option<u32>,
    max_page_size: u32,
) -> Result<Vec<(String, String)>, HevyError> {
    let (page, page_size) = pagination(page, page_size, max_page_size)?;
    Ok(vec![
        ("page".to_owned(), page.to_string()),
        ("pageSize".to_owned(), page_size.to_string()),
    ])
}

fn validate_id(value: &str, field: &str) -> Result<(), HevyError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(HevyError::InvalidInput(format!(
            "{field} must be a non-empty Hevy identifier"
        )));
    }
    Ok(())
}

fn validate_date(date: &str) -> Result<(), HevyError> {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return Err(HevyError::InvalidInput(
            "date must use YYYY-MM-DD".to_owned(),
        ));
    }
    Ok(())
}

fn nonempty_optional<'a>(
    value: Option<&'a str>,
    field: &str,
) -> Result<Option<&'a str>, HevyError> {
    match value {
        Some(value) if value.trim().is_empty() => Err(HevyError::InvalidInput(format!(
            "{field} must not be empty"
        ))),
        other => Ok(other),
    }
}

fn template_matches(template: &Value, needle: &str) -> bool {
    ["title", "primary_muscle_group", "equipment_category"]
        .into_iter()
        .filter_map(|field| template.get(field).and_then(Value::as_str))
        .chain(
            template
                .get("secondary_muscle_groups")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .any(|value| value.to_lowercase().contains(needle))
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkoutSetType {
    Warmup,
    Normal,
    Failure,
    Dropset,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkoutSetInput {
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub set_type: WorkoutSetType,
    pub weight_kg: Option<f64>,
    pub reps: Option<u32>,
    pub distance_meters: Option<u32>,
    pub duration_seconds: Option<u32>,
    pub custom_metric: Option<f64>,
    /// Null or one of 6, 7, 7.5, 8, 8.5, 9, 9.5, 10.
    pub rpe: Option<f64>,
}

impl WorkoutSetInput {
    fn validate(&self) -> Result<(), HevyError> {
        const ALLOWED_RPE: &[f64] = &[6.0, 7.0, 7.5, 8.0, 8.5, 9.0, 9.5, 10.0];
        if let Some(rpe) = self.rpe
            && !ALLOWED_RPE.contains(&rpe)
        {
            return Err(HevyError::InvalidInput(
                "rpe must be null or one of 6, 7, 7.5, 8, 8.5, 9, 9.5, 10".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkoutExerciseInput {
    /// Required ID returned by list/search/get exercise template tools.
    pub exercise_template_id: String,
    pub superset_id: Option<u32>,
    pub notes: Option<String>,
    pub sets: Vec<WorkoutSetInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkoutInput {
    pub title: String,
    pub description: Option<String>,
    pub start_time: String,
    pub end_time: String,
    pub is_private: bool,
    pub exercises: Vec<WorkoutExerciseInput>,
}

impl WorkoutInput {
    fn validate(&self) -> Result<(), HevyError> {
        if self.title.trim().is_empty() {
            return Err(HevyError::InvalidInput(
                "workout title must not be empty".to_owned(),
            ));
        }
        if self.start_time.trim().is_empty() || self.end_time.trim().is_empty() {
            return Err(HevyError::InvalidInput(
                "workout start_time and end_time must not be empty".to_owned(),
            ));
        }
        for exercise in &self.exercises {
            validate_id(&exercise.exercise_template_id, "exercise_template_id")?;
            for set in &exercise.sets {
                set.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepRangeInput {
    pub start: Option<u32>,
    pub end: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutineSetInput {
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub set_type: WorkoutSetType,
    pub weight_kg: Option<f64>,
    pub reps: Option<u32>,
    pub distance_meters: Option<u32>,
    pub duration_seconds: Option<u32>,
    pub custom_metric: Option<f64>,
    pub rep_range: Option<RepRangeInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutineExerciseInput {
    /// Required ID returned by list/search/get exercise template tools.
    pub exercise_template_id: String,
    pub superset_id: Option<u32>,
    pub rest_seconds: Option<u32>,
    pub notes: Option<String>,
    pub sets: Vec<RoutineSetInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutineInput {
    pub title: String,
    pub folder_id: Option<u64>,
    pub notes: Option<String>,
    pub exercises: Vec<RoutineExerciseInput>,
}

impl RoutineInput {
    fn validate(&self) -> Result<(), HevyError> {
        if self.title.trim().is_empty() {
            return Err(HevyError::InvalidInput(
                "routine title must not be empty".to_owned(),
            ));
        }
        for exercise in &self.exercises {
            validate_id(&exercise.exercise_template_id, "exercise_template_id")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CustomExerciseType {
    WeightReps,
    RepsOnly,
    BodyweightReps,
    BodyweightAssistedReps,
    Duration,
    WeightDuration,
    DistanceDuration,
    ShortDistanceWeight,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MuscleGroup {
    Abdominals,
    Shoulders,
    Biceps,
    Triceps,
    Forearms,
    Quadriceps,
    Hamstrings,
    Calves,
    Glutes,
    Abductors,
    Adductors,
    Lats,
    UpperBack,
    Traps,
    LowerBack,
    Chest,
    Cardio,
    Neck,
    FullBody,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EquipmentCategory {
    None,
    Barbell,
    Dumbbell,
    Kettlebell,
    Machine,
    Plate,
    ResistanceBand,
    Suspension,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CustomExerciseInput {
    pub title: String,
    pub exercise_type: CustomExerciseType,
    pub equipment_category: EquipmentCategory,
    pub muscle_group: MuscleGroup,
    pub other_muscles: Vec<MuscleGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateExerciseTemplateBody {
    pub exercise: CustomExerciseInput,
}

impl CreateExerciseTemplateBody {
    fn validate(&self) -> Result<(), HevyError> {
        if self.exercise.title.trim().is_empty() {
            return Err(HevyError::InvalidInput(
                "exercise title must not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutineFolderInput {
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateRoutineFolderBody {
    pub routine_folder: RoutineFolderInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BodyMeasurementInput {
    pub date: String,
    #[serde(flatten)]
    pub measurements: UpdateBodyMeasurementInput,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateBodyMeasurementInput {
    pub weight_kg: Option<f64>,
    pub lean_mass_kg: Option<f64>,
    pub fat_percent: Option<f64>,
    pub neck_cm: Option<f64>,
    pub shoulder_cm: Option<f64>,
    pub chest_cm: Option<f64>,
    pub left_bicep_cm: Option<f64>,
    pub right_bicep_cm: Option<f64>,
    pub left_forearm_cm: Option<f64>,
    pub right_forearm_cm: Option<f64>,
    pub abdomen: Option<f64>,
    pub waist: Option<f64>,
    pub hips: Option<f64>,
    pub left_thigh: Option<f64>,
    pub right_thigh: Option<f64>,
    pub left_calf: Option<f64>,
    pub right_calf: Option<f64>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_workouts_uses_api_key_and_official_query_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/workouts"))
            .and(header("api-key", "test-hevy-key"))
            .and(query_param("page", "2"))
            .and(query_param("pageSize", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "page": 2,
                "page_count": 3,
                "workouts": [{"id": "workout-1", "title": "Leg Day"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = HevyClient::new(&server.uri())
            .unwrap()
            .with_api_key("test-hevy-key");
        let response = client.list_workouts(Some(2), Some(10)).await.unwrap();

        assert_eq!(response["workouts"][0]["id"], "workout-1");
    }

    #[tokio::test]
    async fn user_info_uses_first_party_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/user/info"))
            .and(header("api-key", "test-hevy-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "hevy-user", "name": "Julian"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = HevyClient::new(&server.uri())
            .unwrap()
            .with_api_key("test-hevy-key");
        let response = client.user_info().await.unwrap();

        assert_eq!(response["data"]["id"], "hevy-user");
    }

    #[tokio::test]
    async fn missing_key_is_structured_and_does_not_block_construction() {
        let client = HevyClient::new("https://api.hevyapp.com").unwrap();
        let error = client.list_workouts(None, None).await.unwrap_err();
        assert_eq!(error.code(), "hevy_api_key_missing");
        assert_eq!(error.to_string(), "hevy_api_key_missing");
    }

    #[test]
    fn debug_redacts_api_key() {
        let client = HevyClient::new("https://api.hevyapp.com")
            .unwrap()
            .with_api_key("never-print-this-key");
        let debug = format!("{client:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("never-print-this-key"));
    }

    #[test]
    fn workout_validation_rejects_unofficial_rpe() {
        let workout = WorkoutInput {
            title: "Test".to_owned(),
            description: None,
            start_time: "2026-08-17T10:00:00Z".to_owned(),
            end_time: "2026-08-17T11:00:00Z".to_owned(),
            is_private: true,
            exercises: vec![WorkoutExerciseInput {
                exercise_template_id: "D04AC939".to_owned(),
                superset_id: None,
                notes: None,
                sets: vec![WorkoutSetInput {
                    set_type: WorkoutSetType::Normal,
                    weight_kg: Some(100.0),
                    reps: Some(5),
                    distance_meters: None,
                    duration_seconds: None,
                    custom_metric: None,
                    rpe: Some(8.2),
                }],
            }],
        };

        assert!(workout.validate().is_err());
    }
}
