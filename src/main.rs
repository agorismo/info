use std::str::FromStr;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use tower_http::services::ServeDir;

mod catalog;
mod pins;
mod scrobble;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    http: reqwest::Client,
    tmdb_key: Option<String>,
}

impl axum::extract::FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

// ---------- erros ----------

struct AppError(StatusCode, String);

impl AppError {
    fn new(code: StatusCode, msg: &str) -> Self {
        AppError(code, msg.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        eprintln!("erro de banco: {e}");
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "erro interno")
    }
}

type ApiResult<T> = Result<T, AppError>;

// ---------- sessão ----------

async fn start_session(db: &SqlitePool, jar: CookieJar, user_id: i64) -> ApiResult<CookieJar> {
    // Rotaciona a sessão deste navegador ao entrar novamente.
    if let Some(previous) = jar.get("session") {
        sqlx::query("DELETE FROM sessions WHERE token = ?")
            .bind(previous.value())
            .execute(db)
            .await?;
    }

    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    sqlx::query("INSERT INTO sessions (token, user_id) VALUES (?, ?)")
        .bind(&token)
        .bind(user_id)
        .execute(db)
        .await?;
    // Ative COOKIE_SECURE=1 quando o app estiver servido por HTTPS.
    // Mantém false por padrão para que o desenvolvimento local em http://localhost funcione.
    let secure_cookie = std::env::var("COOKIE_SECURE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let cookie = Cookie::build(("session", token))
        .path("/")
        .http_only(true)
        .secure(secure_cookie)
        .same_site(SameSite::Lax)
        .permanent()
        .build();
    Ok(jar.add(cookie))
}

async fn current_user(db: &SqlitePool, jar: &CookieJar) -> ApiResult<Option<i64>> {
    let Some(c) = jar.get("session") else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT user_id FROM sessions WHERE token = ?")
        .bind(c.value())
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<i64, _>("user_id")))
}

async fn require_user(db: &SqlitePool, jar: &CookieJar) -> ApiResult<i64> {
    current_user(db, jar)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "faça login primeiro"))
}

async fn user_id_by_name(db: &SqlitePool, username: &str) -> ApiResult<i64> {
    sqlx::query("SELECT id FROM users WHERE username = ?")
        .bind(username.to_lowercase())
        .fetch_optional(db)
        .await?
        .map(|r| r.get::<i64, _>("id"))
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "usuário não encontrado"))
}

// ---------- auth ----------

#[derive(Deserialize)]
struct Creds {
    username: String,
    password: String,
}

async fn register(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(c): Json<Creds>,
) -> ApiResult<(CookieJar, Json<Value>)> {
    let username = c.username.trim().to_lowercase();
    let valid_name = (3..=20).contains(&username.chars().count())
        && username.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if !valid_name {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "usuário: 3 a 20 caracteres (letras, números e _)",
        ));
    }
    if c.password.chars().count() < 8 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "a senha precisa ter pelo menos 8 caracteres",
        ));
    }

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(c.password.as_bytes(), &salt)
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "erro interno"))?
        .to_string();

    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, display_name) VALUES (?, ?, ?)",
    )
    .bind(&username)
    .bind(&hash)
    .bind(&username)
    .execute(&db)
    .await;

    let id = match res {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::new(StatusCode::CONFLICT, "esse usuário já existe"))
        }
        Err(e) => return Err(e.into()),
    };

    let jar = start_session(&db, jar, id).await?;
    Ok((jar, Json(json!({ "id": id, "username": username }))))
}

async fn login(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(c): Json<Creds>,
) -> ApiResult<(CookieJar, Json<Value>)> {
    let bad = || AppError::new(StatusCode::UNAUTHORIZED, "usuário ou senha incorretos");
    let username = c.username.trim().to_lowercase();

    let row = sqlx::query("SELECT id, password_hash FROM users WHERE username = ?")
        .bind(&username)
        .fetch_optional(&db)
        .await?
        .ok_or_else(bad)?;

    let id: i64 = row.get("id");
    let stored: String = row.get("password_hash");
    let parsed = PasswordHash::new(&stored)
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "erro interno"))?;
    Argon2::default()
        .verify_password(c.password.as_bytes(), &parsed)
        .map_err(|_| bad())?;

    let jar = start_session(&db, jar, id).await?;
    Ok((jar, Json(json!({ "id": id, "username": username }))))
}

async fn logout(State(db): State<SqlitePool>, jar: CookieJar) -> ApiResult<(CookieJar, Json<Value>)> {
    if let Some(c) = jar.get("session") {
        sqlx::query("DELETE FROM sessions WHERE token = ?")
            .bind(c.value())
            .execute(&db)
            .await?;
    }
    let jar = jar.remove(Cookie::build(("session", "")).path("/"));
    Ok((jar, Json(json!({ "ok": true }))))
}

// ---------- perfil ----------

#[derive(Serialize)]
struct Profile {
    id: i64,
    username: String,
    display_name: String,
    bio: String,
    followers: i64,
    following: i64,
    is_following: bool,
    is_me: bool,
}

async fn load_profile(db: &SqlitePool, username: &str, me: Option<i64>) -> ApiResult<Profile> {
    let row = sqlx::query("SELECT id, username, display_name, bio FROM users WHERE username = ?")
        .bind(username.to_lowercase())
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "usuário não encontrado"))?;
    let id: i64 = row.get("id");

    let followers: i64 = sqlx::query("SELECT COUNT(*) AS n FROM follows WHERE followee_id = ?")
        .bind(id)
        .fetch_one(db)
        .await?
        .get("n");
    let following: i64 = sqlx::query("SELECT COUNT(*) AS n FROM follows WHERE follower_id = ?")
        .bind(id)
        .fetch_one(db)
        .await?
        .get("n");

    let is_following = match me {
        Some(m) if m != id => sqlx::query("SELECT 1 FROM follows WHERE follower_id = ? AND followee_id = ?")
            .bind(m)
            .bind(id)
            .fetch_optional(db)
            .await?
            .is_some(),
        _ => false,
    };

    Ok(Profile {
        id,
        username: row.get("username"),
        display_name: row.get("display_name"),
        bio: row.get("bio"),
        followers,
        following,
        is_following,
        is_me: me == Some(id),
    })
}

async fn get_me(State(db): State<SqlitePool>, jar: CookieJar) -> ApiResult<Json<Profile>> {
    let me = require_user(&db, &jar).await?;
    let username: String = sqlx::query("SELECT username FROM users WHERE id = ?")
        .bind(me)
        .fetch_one(&db)
        .await?
        .get("username");
    Ok(Json(load_profile(&db, &username, Some(me)).await?))
}

#[derive(Deserialize)]
struct UpdateMe {
    display_name: String,
    bio: String,
}

async fn update_me(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(b): Json<UpdateMe>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let name = b.display_name.trim();
    let bio = b.bio.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "nome: 1 a 40 caracteres"));
    }
    if bio.chars().count() > 300 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "bio: no máximo 300 caracteres"));
    }
    sqlx::query("UPDATE users SET display_name = ?, bio = ? WHERE id = ?")
        .bind(name)
        .bind(bio)
        .bind(me)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_profile(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(username): Path<String>,
) -> ApiResult<Json<Profile>> {
    let me = current_user(&db, &jar).await?;
    Ok(Json(load_profile(&db, &username, me).await?))
}

// ---------- seguir ----------

async fn follow(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(username): Path<String>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let target = user_id_by_name(&db, &username).await?;
    if me == target {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "você não pode seguir a si mesmo"));
    }
    sqlx::query("INSERT OR IGNORE INTO follows (follower_id, followee_id) VALUES (?, ?)")
        .bind(me)
        .bind(target)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn unfollow(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(username): Path<String>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let target = user_id_by_name(&db, &username).await?;
    sqlx::query("DELETE FROM follows WHERE follower_id = ? AND followee_id = ?")
        .bind(me)
        .bind(target)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Serialize)]
struct UserLite {
    username: String,
    display_name: String,
}

fn to_users(rows: Vec<sqlx::sqlite::SqliteRow>) -> Vec<UserLite> {
    rows.into_iter()
        .map(|r| UserLite {
            username: r.get("username"),
            display_name: r.get("display_name"),
        })
        .collect()
}

async fn followers(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<UserLite>>> {
    let id = user_id_by_name(&db, &username).await?;
    let rows = sqlx::query(
        "SELECT u.username, u.display_name FROM follows f \
         JOIN users u ON u.id = f.follower_id \
         WHERE f.followee_id = ? ORDER BY f.rowid DESC LIMIT 100",
    )
    .bind(id)
    .fetch_all(&db)
    .await?;
    Ok(Json(to_users(rows)))
}

async fn following(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<UserLite>>> {
    let id = user_id_by_name(&db, &username).await?;
    let rows = sqlx::query(
        "SELECT u.username, u.display_name FROM follows f \
         JOIN users u ON u.id = f.followee_id \
         WHERE f.follower_id = ? ORDER BY f.rowid DESC LIMIT 100",
    )
    .bind(id)
    .fetch_all(&db)
    .await?;
    Ok(Json(to_users(rows)))
}

// ---------- busca ----------

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(
    State(db): State<SqlitePool>,
    Query(p): Query<SearchQuery>,
) -> ApiResult<Json<Vec<UserLite>>> {
    let q = p.q.trim();
    if q.is_empty() {
        return Ok(Json(vec![]));
    }
    let like = format!("%{}%", q.replace('%', "").replace('_', ""));
    let rows = sqlx::query(
        "SELECT username, display_name FROM users \
         WHERE username LIKE ? OR display_name LIKE ? ORDER BY username LIMIT 20",
    )
    .bind(&like)
    .bind(&like)
    .fetch_all(&db)
    .await?;
    Ok(Json(to_users(rows)))
}

// ---------- main ----------

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        username TEXT NOT NULL UNIQUE,
        password_hash TEXT NOT NULL,
        display_name TEXT NOT NULL,
        bio TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS sessions (
        token TEXT PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id)
    )",
    "CREATE TABLE IF NOT EXISTS follows (
        follower_id INTEGER NOT NULL REFERENCES users(id),
        followee_id INTEGER NOT NULL REFERENCES users(id),
        PRIMARY KEY (follower_id, followee_id)
    )",
    "CREATE TABLE IF NOT EXISTS items (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        kind TEXT NOT NULL,
        ext_id TEXT NOT NULL,
        title TEXT NOT NULL,
        year INTEGER,
        creator TEXT NOT NULL DEFAULT '',
        image_url TEXT NOT NULL DEFAULT '',
        UNIQUE (kind, ext_id)
    )",
    "CREATE TABLE IF NOT EXISTS ratings (
        user_id INTEGER NOT NULL REFERENCES users(id),
        item_id INTEGER NOT NULL REFERENCES items(id),
        rating INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 10),
        review TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL DEFAULT (datetime('now')),
        PRIMARY KEY (user_id, item_id)
    )",
    "CREATE INDEX IF NOT EXISTS ratings_item ON ratings(item_id)",
    "CREATE TABLE IF NOT EXISTS watchlist (
        user_id INTEGER NOT NULL REFERENCES users(id),
        item_id INTEGER NOT NULL REFERENCES items(id),
        PRIMARY KEY (user_id, item_id)
    )",
    "CREATE TABLE IF NOT EXISTS lists (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL REFERENCES users(id),
        title TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS list_items (
        list_id INTEGER NOT NULL REFERENCES lists(id),
        item_id INTEGER NOT NULL REFERENCES items(id),
        position INTEGER NOT NULL,
        PRIMARY KEY (list_id, item_id)
    )",
    "CREATE TABLE IF NOT EXISTS scrobbles (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL REFERENCES users(id),
        artist TEXT NOT NULL,
        track TEXT NOT NULL,
        album TEXT NOT NULL DEFAULT '',
        duration INTEGER,
        played_at INTEGER NOT NULL,
        UNIQUE (user_id, artist, track, played_at)
    )",
    "CREATE INDEX IF NOT EXISTS scrobbles_user_time ON scrobbles(user_id, played_at)",
    "CREATE TABLE IF NOT EXISTS now_playing (
        user_id INTEGER PRIMARY KEY REFERENCES users(id),
        artist TEXT NOT NULL,
        track TEXT NOT NULL,
        album TEXT NOT NULL DEFAULT '',
        updated_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS api_tokens (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL REFERENCES users(id),
        name TEXT NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    "CREATE TABLE IF NOT EXISTS pins (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL REFERENCES users(id),
        title TEXT NOT NULL DEFAULT '',
        description TEXT NOT NULL DEFAULT '',
        image_url TEXT NOT NULL,
        link TEXT NOT NULL DEFAULT '',
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    "CREATE INDEX IF NOT EXISTS pins_user ON pins(user_id, id)",
    "CREATE TABLE IF NOT EXISTS boards (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL REFERENCES users(id),
        title TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT ''
    )",
    "CREATE TABLE IF NOT EXISTS board_pins (
        board_id INTEGER NOT NULL REFERENCES boards(id),
        pin_id INTEGER NOT NULL REFERENCES pins(id),
        PRIMARY KEY (board_id, pin_id)
    )",
    "CREATE INDEX IF NOT EXISTS board_pins_pin ON board_pins(pin_id)",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://app.db?mode=rwc".into());
    // SQLite não habilita foreign keys por padrão; habilite em todas as conexões do pool.
    let options = SqliteConnectOptions::from_str(&url)?.foreign_keys(true);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    for stmt in SCHEMA {
        sqlx::query(stmt).execute(&db).await?;
    }

    // MusicBrainz pede um User-Agent identificável; ajuste com APP_USER_AGENT se quiser.
    let user_agent = std::env::var("APP_USER_AGENT").unwrap_or_else(|_| "social-app/0.1".into());
    let http = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let tmdb_key = std::env::var("TMDB_API_KEY").ok().filter(|k| !k.trim().is_empty());
    if tmdb_key.is_none() {
        println!("aviso: TMDB_API_KEY não definida, a busca de filmes fica desativada");
    }
    let state = AppState { db, http, tmdb_key };

    let api: Router<AppState> = Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(get_me).put(update_me))
        .route("/search", get(search))
        .route("/users/:username", get(get_profile))
        .route("/users/:username/follow", post(follow).delete(unfollow))
        .route("/users/:username/followers", get(followers))
        .route("/users/:username/following", get(following))
        .merge(catalog::routes())
        .merge(scrobble::routes())
        .merge(pins::routes());

    let app = Router::new()
        .nest("/api", api)
        .nest_service("/uploads", ServeDir::new(pins::UPLOAD_DIR))
        .fallback_service(ServeDir::new("static"))
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    println!("rodando em http://localhost:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}
