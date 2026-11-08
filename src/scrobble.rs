use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, Query, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use axum_extra::extract::cookie::CookieJar;
use rand::{distributions::Alphanumeric, Rng};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::{require_user, user_id_by_name, ApiResult, AppError, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/scrobble", post(scrobble))
        .route("/scrobble/batch", post(scrobble_batch))
        .route("/nowplaying", post(set_nowplaying))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/:id", delete(delete_token))
        .route("/users/:username/scrobbles", get(user_scrobbles))
        .route("/users/:username/nowplaying", get(user_nowplaying))
        .route("/users/:username/stats", get(user_stats))
}

const MAX_TOKENS: i64 = 10;
const MAX_BATCH: usize = 100;
/// "ouvindo agora" some depois desse tempo sem atualização
const NOW_PLAYING_TTL: i64 = 600;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn bad(msg: &str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, msg)
}

fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Quem está chamando: token de API (`Authorization: Bearer ...`) ou sessão do navegador.
async fn auth_user(db: &SqlitePool, jar: &CookieJar, headers: &HeaderMap) -> ApiResult<i64> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let token = value.strip_prefix("Bearer ").unwrap_or("").trim();
        let row = sqlx::query("SELECT user_id FROM api_tokens WHERE token_hash = ?")
            .bind(hash_token(token))
            .fetch_optional(db)
            .await?;
        return row
            .map(|r| r.get::<i64, _>("user_id"))
            .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, "token inválido"));
    }
    require_user(db, jar).await
}

fn clean(v: &str, max: usize, label: &str, required: bool) -> ApiResult<String> {
    let v = v.trim();
    if required && v.is_empty() {
        return Err(bad(&format!("{label} é obrigatório")));
    }
    if v.chars().count() > max {
        return Err(bad(&format!("{label}: no máximo {max} caracteres")));
    }
    Ok(v.to_string())
}

// ---------- tokens ----------

async fn create_token(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(b): Json<NewToken>,
) -> ApiResult<Json<Value>> {
    // só pela sessão do navegador: um token não cria outros tokens
    let me = require_user(&db, &jar).await?;
    let name = clean(&b.name, 40, "nome", true)?;
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM api_tokens WHERE user_id = ?")
        .bind(me)
        .fetch_one(&db)
        .await?
        .get("n");
    if n >= MAX_TOKENS {
        return Err(bad("limite de 10 tokens; revogue algum antes"));
    }
    let random: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    let token = format!("sc_{random}");
    sqlx::query("INSERT INTO api_tokens (user_id, name, token_hash) VALUES (?, ?, ?)")
        .bind(me)
        .bind(&name)
        .bind(hash_token(&token))
        .execute(&db)
        .await?;
    // o token só aparece aqui; no banco fica apenas o hash
    Ok(Json(json!({ "token": token })))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

async fn list_tokens(State(db): State<SqlitePool>, jar: CookieJar) -> ApiResult<Json<Vec<Value>>> {
    let me = require_user(&db, &jar).await?;
    let rows = sqlx::query("SELECT id, name, created_at FROM api_tokens WHERE user_id = ? ORDER BY id DESC")
        .bind(me)
        .fetch_all(&db)
        .await?;
    Ok(Json(
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.get::<i64, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "created_at": r.get::<String, _>("created_at"),
                })
            })
            .collect(),
    ))
}

async fn delete_token(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    sqlx::query("DELETE FROM api_tokens WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(me)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- scrobble ----------

#[derive(Deserialize)]
struct ScrobbleIn {
    artist: String,
    track: String,
    #[serde(default)]
    album: String,
    /// duração da faixa em segundos (opcional)
    duration: Option<i64>,
    /// quando começou a tocar, em segundos Unix (padrão: agora)
    timestamp: Option<i64>,
}

struct Valid {
    artist: String,
    track: String,
    album: String,
    duration: Option<i64>,
    ts: i64,
}

fn validate(s: &ScrobbleIn) -> ApiResult<Valid> {
    let artist = clean(&s.artist, 200, "artist", true)?;
    let track = clean(&s.track, 200, "track", true)?;
    let album = clean(&s.album, 200, "album", false)?;
    let t = now();
    let ts = s.timestamp.unwrap_or(t);
    if ts > t + 60 || ts < 946_684_800 {
        return Err(bad("timestamp inválido (segundos Unix, não pode ser do futuro)"));
    }
    if let Some(d) = s.duration {
        if !(0..=86_400).contains(&d) {
            return Err(bad("duration inválida (segundos, de 0 a 86400)"));
        }
    }
    Ok(Valid { artist, track, album, duration: s.duration, ts })
}

async fn insert_valid(conn: &mut SqliteConnection, uid: i64, v: &Valid) -> ApiResult<bool> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO scrobbles (user_id, artist, track, album, duration, played_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(uid)
    .bind(&v.artist)
    .bind(&v.track)
    .bind(&v.album)
    .bind(v.duration)
    .bind(v.ts)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() > 0)
}

async fn scrobble(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(s): Json<ScrobbleIn>,
) -> ApiResult<Json<Value>> {
    let uid = auth_user(&db, &jar, &headers).await?;
    let v = validate(&s)?;
    let mut conn = db.acquire().await?;
    let inserted = insert_valid(&mut conn, uid, &v).await?;
    Ok(Json(json!({ "inserted": inserted })))
}

async fn scrobble_batch(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(items): Json<Vec<ScrobbleIn>>,
) -> ApiResult<Json<Value>> {
    let uid = auth_user(&db, &jar, &headers).await?;
    if items.is_empty() || items.len() > MAX_BATCH {
        return Err(bad("envie de 1 a 100 scrobbles por vez"));
    }
    // valida tudo antes de gravar: ou entra o lote inteiro, ou nada
    let valid: Vec<Valid> = items
        .iter()
        .enumerate()
        .map(|(i, s)| validate(s).map_err(|e| AppError(e.0, format!("item {}: {}", i + 1, e.1))))
        .collect::<ApiResult<_>>()?;

    let mut tx = db.begin().await?;
    let mut inserted = 0;
    for v in &valid {
        if insert_valid(&mut tx, uid, v).await? {
            inserted += 1;
        }
    }
    tx.commit().await?;
    Ok(Json(json!({ "inserted": inserted, "ignored": valid.len() - inserted })))
}

#[derive(Deserialize)]
struct NowPlayingIn {
    artist: String,
    track: String,
    #[serde(default)]
    album: String,
}

async fn set_nowplaying(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(b): Json<NowPlayingIn>,
) -> ApiResult<Json<Value>> {
    let uid = auth_user(&db, &jar, &headers).await?;
    let artist = clean(&b.artist, 200, "artist", true)?;
    let track = clean(&b.track, 200, "track", true)?;
    let album = clean(&b.album, 200, "album", false)?;
    sqlx::query(
        "INSERT INTO now_playing (user_id, artist, track, album, updated_at) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (user_id) DO UPDATE SET \
           artist = excluded.artist, track = excluded.track, \
           album = excluded.album, updated_at = excluded.updated_at",
    )
    .bind(uid)
    .bind(&artist)
    .bind(&track)
    .bind(&album)
    .bind(now())
    .execute(&db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- leitura ----------

#[derive(Deserialize)]
struct HistQuery {
    limit: Option<i64>,
    before: Option<i64>,
}

async fn user_scrobbles(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
    Query(q): Query<HistQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    let uid = user_id_by_name(&db, &username).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let before = q.before.unwrap_or(i64::MAX);
    let rows = sqlx::query(
        "SELECT artist, track, album, played_at FROM scrobbles \
         WHERE user_id = ? AND played_at < ? ORDER BY played_at DESC, id DESC LIMIT ?",
    )
    .bind(uid)
    .bind(before)
    .bind(limit)
    .fetch_all(&db)
    .await?;
    Ok(Json(
        rows.iter()
            .map(|r| {
                json!({
                    "artist": r.get::<String, _>("artist"),
                    "track": r.get::<String, _>("track"),
                    "album": r.get::<String, _>("album"),
                    "played_at": r.get::<i64, _>("played_at"),
                })
            })
            .collect(),
    ))
}

async fn user_nowplaying(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
) -> ApiResult<Json<Value>> {
    let uid = user_id_by_name(&db, &username).await?;
    let row = sqlx::query(
        "SELECT artist, track, album FROM now_playing WHERE user_id = ? AND updated_at >= ?",
    )
    .bind(uid)
    .bind(now() - NOW_PLAYING_TTL)
    .fetch_optional(&db)
    .await?;
    Ok(Json(match row {
        Some(r) => json!({
            "artist": r.get::<String, _>("artist"),
            "track": r.get::<String, _>("track"),
            "album": r.get::<String, _>("album"),
        }),
        None => Value::Null,
    }))
}

#[derive(Deserialize)]
struct StatsQuery {
    period: Option<String>,
}

async fn user_stats(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<Value>> {
    let uid = user_id_by_name(&db, &username).await?;
    let period = q.period.unwrap_or_else(|| "30d".into());
    let days: Option<i64> = match period.as_str() {
        "7d" => Some(7),
        "30d" => Some(30),
        "90d" => Some(90),
        "365d" => Some(365),
        "all" => None,
        _ => return Err(bad("period deve ser 7d, 30d, 90d, 365d ou all")),
    };
    let since = days.map(|d| now() - d * 86_400).unwrap_or(0);

    let totals = sqlx::query(
        "SELECT COUNT(*) AS total, COUNT(DISTINCT LOWER(artist)) AS artists \
         FROM scrobbles WHERE user_id = ? AND played_at >= ?",
    )
    .bind(uid)
    .bind(since)
    .fetch_one(&db)
    .await?;

    let artists = sqlx::query(
        "SELECT artist, COUNT(*) AS plays, MAX(played_at) AS last FROM scrobbles \
         WHERE user_id = ? AND played_at >= ? \
         GROUP BY LOWER(artist) ORDER BY plays DESC, last DESC LIMIT 10",
    )
    .bind(uid)
    .bind(since)
    .fetch_all(&db)
    .await?;

    let tracks = sqlx::query(
        "SELECT artist, track, COUNT(*) AS plays, MAX(played_at) AS last FROM scrobbles \
         WHERE user_id = ? AND played_at >= ? \
         GROUP BY LOWER(artist), LOWER(track) ORDER BY plays DESC, last DESC LIMIT 10",
    )
    .bind(uid)
    .bind(since)
    .fetch_all(&db)
    .await?;

    let albums = sqlx::query(
        "SELECT artist, album, COUNT(*) AS plays, MAX(played_at) AS last FROM scrobbles \
         WHERE user_id = ? AND played_at >= ? AND album != '' \
         GROUP BY LOWER(artist), LOWER(album) ORDER BY plays DESC, last DESC LIMIT 10",
    )
    .bind(uid)
    .bind(since)
    .fetch_all(&db)
    .await?;

    // por dia (UTC), do mais antigo para o mais novo
    let mut per_day = sqlx::query(
        "SELECT strftime('%Y-%m-%d', played_at, 'unixepoch') AS day, COUNT(*) AS plays \
         FROM scrobbles WHERE user_id = ? AND played_at >= ? \
         GROUP BY day ORDER BY day DESC LIMIT 365",
    )
    .bind(uid)
    .bind(since)
    .fetch_all(&db)
    .await?;
    per_day.reverse();

    Ok(Json(json!({
        "period": period,
        "total": totals.get::<i64, _>("total"),
        "artists": totals.get::<i64, _>("artists"),
        "top_artists": artists.iter().map(|r| json!({
            "artist": r.get::<String, _>("artist"),
            "plays": r.get::<i64, _>("plays"),
        })).collect::<Vec<_>>(),
        "top_tracks": tracks.iter().map(|r| json!({
            "artist": r.get::<String, _>("artist"),
            "track": r.get::<String, _>("track"),
            "plays": r.get::<i64, _>("plays"),
        })).collect::<Vec<_>>(),
        "top_albums": albums.iter().map(|r| json!({
            "artist": r.get::<String, _>("artist"),
            "album": r.get::<String, _>("album"),
            "plays": r.get::<i64, _>("plays"),
        })).collect::<Vec<_>>(),
        "days": per_day.iter().map(|r| json!({
            "day": r.get::<String, _>("day"),
            "plays": r.get::<i64, _>("plays"),
        })).collect::<Vec<_>>(),
    })))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scrobble() -> ScrobbleIn {
        ScrobbleIn {
            artist: " Radiohead ".into(),
            track: " Idioteque ".into(),
            album: " Kid A ".into(),
            duration: Some(250),
            timestamp: Some(now() - 1),
        }
    }

    #[test]
    fn clean_trims_and_accepts_valid_text() {
        assert_eq!(clean("  Radiohead  ", 200, "artist", true).unwrap(), "Radiohead");
    }

    #[test]
    fn clean_rejects_empty_required_text() {
        assert!(clean("   ", 200, "artist", true).is_err());
    }

    #[test]
    fn clean_rejects_text_over_limit() {
        assert!(clean(&"a".repeat(201), 200, "artist", true).is_err());
    }

    #[test]
    fn validate_accepts_a_valid_scrobble() {
        let value = validate(&sample_scrobble()).unwrap();
        assert_eq!(value.artist, "Radiohead");
        assert_eq!(value.track, "Idioteque");
        assert_eq!(value.album, "Kid A");
        assert_eq!(value.duration, Some(250));
    }

    #[test]
    fn validate_rejects_future_timestamps() {
        let mut input = sample_scrobble();
        input.timestamp = Some(now() + 61);
        assert!(validate(&input).is_err());
    }

    #[test]
    fn validate_rejects_invalid_durations() {
        let mut input = sample_scrobble();
        input.duration = Some(-1);
        assert!(validate(&input).is_err());
        input.duration = Some(86_401);
        assert!(validate(&input).is_err());
    }
}
