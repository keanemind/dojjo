mod dojo_io_lock;
mod dojo_status;
mod dojo_tus_store;
mod git_http;
mod git_remote;
mod mirror_get;
mod mirror_manifest;
mod mirror_normalize;
mod mirror_sync_complete;
mod tus_handlers;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, head, options, post},
};
use petname::Generator;
use serde_json::json;
use sqlx::{
    Pool, Sqlite,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use thiserror::Error;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error("not found")]
    NotFound,
    #[error("bad request")]
    BadRequest,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::BadRequest => StatusCode::BAD_REQUEST,
        };

        let body = match self {
            AppError::Internal(_) => json!({ "error": "internal server error" }),
            AppError::NotFound => json!({ "error": "not found" }),
            AppError::BadRequest => json!({ "error": "bad request" }),
        };

        (status, Json(body)).into_response()
    }
}

pub(crate) struct AppState {
    pub(crate) pool: Pool<Sqlite>,
    pub(crate) data_dir: PathBuf,
    /// When set, `git_remote_url` uses smart HTTP at `{base}/git/{dojo_id}.git` (see `DOJJO_PUBLIC_URL`).
    pub(crate) public_url: Option<git_remote::PublicUrl>,
    /// Serializes bare.git HTTP and mirror normalize per dojo across concurrent requests.
    pub(crate) dojo_io_locks: dojo_io_lock::DojoIoLocks,
}

/// OpenAPI paths for handlers annotated with `#[utoipa::path]` and registered via `routes!`.
fn documented_openapi_router() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create_dojo, upload_dojo))
        .routes(routes!(get_dojo))
        .routes(routes!(health))
        .routes(routes!(dojo_status::get_dojo_status))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        crate_dir.as_os_str().len() > 0,
        "CARGO_MANIFEST_DIR must not be empty"
    );
    let default_data_db = crate_dir.join("data.db");
    assert!(
        default_data_db.as_os_str().len() > 0,
        "default data.db path must not be empty"
    );

    let _ = dotenvy::from_path(crate_dir.join(".env"));
    let _ = dotenvy::dotenv();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        assert!(
            default_data_db.is_absolute(),
            "default sqlite path must be absolute (got {default_data_db:?})"
        );
        format!("sqlite://{}", default_data_db.display())
    });
    assert!(
        !database_url.is_empty(),
        "DATABASE_URL must not be empty after resolution"
    );

    let connect_opts = SqliteConnectOptions::from_str(&database_url)
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(connect_opts)
        .await
        .unwrap();

    sqlx::migrate!().run(&pool).await.unwrap();

    let cwd = std::env::current_dir().unwrap();
    let data_dir = std::env::var("DOJJO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| cwd.join("dojjo-data"));
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_dir = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.clone());
    assert!(
        data_dir.is_absolute(),
        "data_dir must be absolute after canonicalize (got {data_dir:?})"
    );

    let public_url = match std::env::var("DOJJO_PUBLIC_URL") {
        Ok(v) if !v.trim().is_empty() => {
            let public = git_remote::PublicUrl::parse(&v)
                .expect("DOJJO_PUBLIC_URL must be http(s)://host[:port] with no path or trailing slash");
            tracing::info!(
                "git remotes will use {}/git/<dojo-id>.git (smart HTTP)",
                public.base()
            );
            Some(public)
        }
        _ => None,
    };

    let shared_state = Arc::new(AppState {
        pool,
        data_dir,
        public_url,
        dojo_io_locks: dojo_io_lock::DojoIoLocks::new(),
    });

    // One handler per `routes!(…)` when methods overlap: a single macro with multiple GET
    // handlers reuses one MethodRouter and panics at startup.
    let api = documented_openapi_router();
    let (openapi_router, api) = api
        .with_state(shared_state.clone())
        .layer(TraceLayer::new_for_http())
        .split_for_parts();

    let tus_router = Router::new()
        .route(
            "/dojo/{dojo_id}/uploads",
            options(tus_handlers::tus_uploads_options).post(tus_handlers::tus_uploads_post),
        )
        .route(
            "/dojo/{dojo_id}/uploads/{upload_id}",
            head(tus_handlers::tus_upload_head)
                .patch(tus_handlers::tus_upload_patch)
                .delete(tus_handlers::tus_upload_delete),
        )
        .with_state(shared_state.clone());

    let mirror_router = Router::new()
        .route(
            "/dojo/{dojo_id}/mirror/manifest",
            get(mirror_manifest::get_mirror_manifest),
        )
        .route(
            "/dojo/{dojo_id}/mirror/sync-complete",
            post(mirror_sync_complete::post_mirror_sync_complete),
        )
        .route(
            "/dojo/{dojo_id}/mirror/{*jj_path}",
            get(mirror_get::get_mirror_object),
        )
        .with_state(shared_state.clone());

    let git_router = git_http::router().with_state(shared_state);

    let app = Router::new()
        .nest(
            "/api",
            openapi_router.merge(tus_router).merge(mirror_router),
        )
        .merge(git_router)
        .with_state(api);

    let listen = std::env::var("DOJJO_LISTEN").unwrap_or_else(|_| "0.0.0.0:3000".into());
    assert!(
        !listen.is_empty(),
        "DOJJO_LISTEN must not be empty when set"
    );
    let addr: std::net::SocketAddr = listen
        .parse()
        .expect("DOJJO_LISTEN must be a valid host:port (e.g. 127.0.0.1:3000)");
    let sock = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )
    .expect("create TCP socket");
    sock.set_reuse_address(true).expect("set SO_REUSEADDR");
    sock.bind(&addr.into()).expect("bind DOJJO_LISTEN");
    sock.listen(1024).expect("listen");
    let std_listener: std::net::TcpListener = sock.into();
    std_listener
        .set_nonblocking(true)
        .expect("set_nonblocking for async listener");
    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio TcpListener");
    tracing::info!("dojjo sync-server listening on http://{}", listen);
    axum::serve(listener, app).await.unwrap();
}

#[derive(utoipa::ToSchema, serde::Serialize)]
struct DojoPublic {
    id: String,
    git_remote_url: String,
}

#[derive(utoipa::ToSchema, serde::Deserialize)]
enum CreateType {
    Upload,
    RemoteClone,
}

#[derive(utoipa::ToSchema, serde::Deserialize)]
struct CreateDojoPayload {
    create_type: CreateType,
}

#[derive(utoipa::ToSchema, serde::Serialize)]
struct HealthResponse {
    ok: bool,
}

#[utoipa::path(get, path = "/health", responses((status = OK, body = HealthResponse)))]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

#[utoipa::path(post, path = "/dojo", responses((status = OK, body = DojoPublic)))]
async fn create_dojo(
    State(state): State<Arc<AppState>>,
    Json(create_dojo_payload): Json<CreateDojoPayload>,
) -> Result<Json<DojoPublic>, AppError> {
    let id = generate_dojo_id();
    match create_dojo_payload.create_type {
        CreateType::Upload => {
            sqlx::query!("INSERT INTO dojos (external_id) VALUES (?)", id)
                .execute(&state.pool)
                .await
                .map_err(anyhow::Error::from)?;

            let dir = state.data_dir.join(&id);
            assert!(
                dir.starts_with(&state.data_dir),
                "dojo dir must stay under data_dir"
            );
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| AppError::Internal(e.into()))?;

            let bare = dir.join("bare.git");
            git_init_bare_repo(&bare).await?;
            assert!(bare.is_dir(), "git init must create bare.git directory");
            let git_remote_url = git_remote::git_remote_url_for_dojo(
                &state.data_dir,
                &id,
                state.public_url.as_ref(),
            )?;
            assert!(
                !git_remote_url.is_empty(),
                "git_remote_url must not be empty"
            );

            Ok(Json(DojoPublic { id, git_remote_url }))
        }
        _ => Err(AppError::BadRequest),
    }
}

async fn git_init_bare_repo(bare_path: &std::path::Path) -> Result<(), AppError> {
    assert!(
        bare_path.as_os_str().len() > 0,
        "bare_path must be non-empty"
    );
    if bare_path.exists() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "bare repo path already exists: {}",
            bare_path.display()
        )));
    }
    if let Some(parent) = bare_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    }
    let out = tokio::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(bare_path)
        .output()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if !out.status.success() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    assert!(bare_path.is_dir(), "git init must create bare_path");

    let canon = bare_path
        .canonicalize()
        .unwrap_or_else(|_| bare_path.to_path_buf());
    let cfg = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(&canon)
        .args(["config", "http.receivepack", "true"])
        .output()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if !cfg.status.success() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "git config http.receivepack failed: {}",
            String::from_utf8_lossy(&cfg.stderr)
        )));
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/dojo/{id}",
    responses((status = OK, body = DojoPublic)),
    params(("id" = String, Path, description = "Dojo external id"))
)]
async fn get_dojo(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<DojoPublic>, AppError> {
    tus_handlers::assert_safe_dojo_external_id(&id)?;
    let _ = tus_handlers::dojo_data_dir(&state, &id).await?;
    let bare = state.data_dir.join(&id).join("bare.git");
    assert!(
        bare.starts_with(&state.data_dir),
        "bare path must stay under data_dir"
    );
    if !bare.is_dir() {
        return Err(AppError::NotFound);
    }
    let git_remote_url = git_remote::git_remote_url_for_dojo(
        &state.data_dir,
        &id,
        state.public_url.as_ref(),
    )?;
    assert!(!git_remote_url.is_empty(), "git_remote_url must not be empty");
    Ok(Json(DojoPublic {
        id,
        git_remote_url,
    }))
}

fn generate_dojo_id() -> String {
    let mut rng = rand::thread_rng();
    petname::Petnames::default()
        .generate(&mut rng, 4, "-")
        .expect("no names")
}

#[utoipa::path(
    put,
    path = "/dojo/{id}",
    responses(
        (status = OK, body = ()),
    ),
    params(
        ("id" = String, Path, description = "ID of the dojo that will be uploaded"),
    )
)]
async fn upload_dojo(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<(), AppError> {
    let found = sqlx::query_scalar!("SELECT id FROM dojos WHERE external_id = ?", id)
        .fetch_optional(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    match found {
        Some(_) => {
            let dir = state.data_dir.join(&id);
            std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
            let destination = dir.join("legacy-put-body.bin");
            std::fs::write(destination, body).map_err(anyhow::Error::from)?;
            Ok(())
        }
        None => return Err(AppError::NotFound),
    }
}

#[cfg(test)]
mod openapi_spec_tests {
    use super::documented_openapi_router;

    fn documented_openapi() -> utoipa::openapi::OpenApi {
        documented_openapi_router().into_openapi()
    }

    #[test]
    fn openapi_lists_utopia_documented_handlers() {
        let api = documented_openapi();
        let dojo = api.paths.paths.get("/dojo").expect("/dojo");
        assert!(dojo.post.is_some(), "POST /dojo");
        let dojo_id = api.paths.paths.get("/dojo/{id}").expect("/dojo/{{id}}");
        assert!(dojo_id.get.is_some(), "GET /dojo/{{id}}");
        assert!(dojo_id.put.is_some(), "PUT /dojo/{{id}}");
        let health = api.paths.paths.get("/health").expect("/health");
        assert!(health.get.is_some(), "GET /health");
        let status = api
            .paths
            .paths
            .get("/dojo/{id}/status")
            .expect("/dojo/{{id}}/status");
        assert!(status.get.is_some(), "GET /dojo/{{id}}/status");
    }

    #[test]
    fn openapi_includes_dojo_public_and_status_schemas() {
        let api = documented_openapi();
        let components = api
            .components
            .as_ref()
            .expect("components must be collected from handlers");
        assert!(components.schemas.contains_key("DojoPublic"));
        assert!(components.schemas.contains_key("DojoStatus"));
        assert!(components.schemas.contains_key("HealthResponse"));
    }
}
