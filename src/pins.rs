use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{sqlite::SqliteRow, Row, SqlitePool};

use crate::{current_user, require_user, user_id_by_name, ApiResult, AppError, AppState};

pub const UPLOAD_DIR: &str = "uploads";
const MAX_UPLOAD: usize = 8 * 1024 * 1024;
const PAGE: i64 = 30;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/uploads", post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/pins", get(list_pins).post(create_pin))
        .route("/pins/:id", get(pin_detail).delete(delete_pin))
        .route("/users/:username/pins", get(user_pins))
        .route("/users/:username/boards", get(user_boards))
        .route("/boards", post(create_board))
        .route("/boards/:id", get(get_board).delete(delete_board))
        .route("/boards/:id/pins", post(add_board_pin))
        .route("/boards/:id/pins/:pin_id", delete(remove_board_pin))
}

fn bad(msg: &str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, msg)
}

fn io_err(e: std::io::Error) -> AppError {
    eprintln!("erro de arquivo: {e}");
    AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "erro interno")
}

// ---------- upload ----------

/// Descobre o tipo pelo conteúdo (não confia no nome nem no Content-Type).
/// SVG fica de fora de propósito: pode carregar script.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("gif")
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

async fn upload(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_user(&db, &jar).await?;
    let ext = sniff(&body).ok_or_else(|| bad("envie uma imagem PNG, JPEG, GIF ou WebP"))?;
    let name = format!("{}.{}", &format!("{:x}", Sha256::digest(&body))[..32], ext);
    tokio::fs::create_dir_all(UPLOAD_DIR).await.map_err(io_err)?;
    tokio::fs::write(format!("{UPLOAD_DIR}/{name}"), &body).await.map_err(io_err)?;
    Ok(Json(json!({ "url": format!("/uploads/{name}") })))
}

/// "/uploads/<32 hex>.<ext>" e nada mais: impede caminhos com ".." e afins.
fn upload_name(url: &str) -> Option<&str> {
    let name = url.strip_prefix("/uploads/")?;
    let (stem, ext) = name.split_once('.')?;
    let ok = stem.len() == 32
        && stem.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        && matches!(ext, "png" | "jpg" | "gif" | "webp");
    ok.then_some(name)
}

fn is_http_url(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://")) && !s.contains(char::is_whitespace)
}

// ---------- pins ----------

fn pin_card(r: &SqliteRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "title": r.get::<String, _>("title"),
        "image_url": r.get::<String, _>("image_url"),
        "username": r.get::<String, _>("username"),
    })
}

const CARD_COLS: &str = "p.id, p.title, p.image_url, u.username";

#[derive(Deserialize)]
struct NewPin {
    image_url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    link: String,
}

async fn create_pin(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(b): Json<NewPin>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let image_url = b.image_url.trim();
    let title = b.title.trim();
    let description = b.description.trim();
    let link = b.link.trim();

    let image_ok = image_url.len() <= 2000 && (is_http_url(image_url) || upload_name(image_url).is_some());
    if !image_ok {
        return Err(bad("imagem inválida: envie um arquivo ou use um link http(s)"));
    }
    if title.chars().count() > 100 {
        return Err(bad("título: no máximo 100 caracteres"));
    }
    if description.chars().count() > 500 {
        return Err(bad("descrição: no máximo 500 caracteres"));
    }
    if !link.is_empty() && (link.len() > 2000 || !is_http_url(link)) {
        return Err(bad("o link do pin precisa começar com http:// ou https://"));
    }

    let id = sqlx::query(
        "INSERT INTO pins (user_id, title, description, image_url, link) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(me)
    .bind(title)
    .bind(description)
    .bind(image_url)
    .bind(link)
    .execute(&db)
    .await?
    .last_insert_rowid();
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct PinsQuery {
    feed: Option<String>,
    q: Option<String>,
    before: Option<i64>,
}

fn like(q: &str) -> String {
    format!("%{}%", q.replace('%', "").replace('_', ""))
}

async fn list_pins(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Query(p): Query<PinsQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    // `follower` = 0 significa "sem filtro" (explorar)
    let follower = if p.feed.as_deref() == Some("following") {
        require_user(&db, &jar).await?
    } else {
        0
    };
    let q = p.q.unwrap_or_default();
    let q = q.trim();
    if q.chars().count() > 100 {
        return Err(bad("busca: no máximo 100 caracteres"));
    }
    let pattern = like(q);
    let rows = sqlx::query(&format!(
        "SELECT {CARD_COLS} FROM pins p JOIN users u ON u.id = p.user_id \
         WHERE p.id < ? \
           AND (? = 0 OR p.user_id = ? OR p.user_id IN \
                (SELECT followee_id FROM follows WHERE follower_id = ?)) \
           AND (? = '' OR p.title LIKE ? OR p.description LIKE ?) \
         ORDER BY p.id DESC LIMIT ?"
    ))
    .bind(p.before.unwrap_or(i64::MAX))
    .bind(follower)
    .bind(follower)
    .bind(follower)
    .bind(q)
    .bind(&pattern)
    .bind(&pattern)
    .bind(PAGE)
    .fetch_all(&db)
    .await?;
    Ok(Json(rows.iter().map(pin_card).collect()))
}

async fn user_pins(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
    Query(p): Query<PinsQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    let uid = user_id_by_name(&db, &username).await?;
    let rows = sqlx::query(&format!(
        "SELECT {CARD_COLS} FROM pins p JOIN users u ON u.id = p.user_id \
         WHERE p.user_id = ? AND p.id < ? ORDER BY p.id DESC LIMIT ?"
    ))
    .bind(uid)
    .bind(p.before.unwrap_or(i64::MAX))
    .bind(PAGE)
    .fetch_all(&db)
    .await?;
    Ok(Json(rows.iter().map(pin_card).collect()))
}

async fn pin_detail(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = current_user(&db, &jar).await?;
    let r = sqlx::query(
        "SELECT p.id, p.user_id, p.title, p.description, p.image_url, p.link, p.created_at, \
                u.username, u.display_name \
         FROM pins p JOIN users u ON u.id = p.user_id WHERE p.id = ?",
    )
    .bind(id)
    .fetch_optional(&db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "pin não encontrado"))?;

    let saves: i64 = sqlx::query("SELECT COUNT(*) AS n FROM board_pins WHERE pin_id = ?")
        .bind(id)
        .fetch_one(&db)
        .await?
        .get("n");

    let my_boards: Vec<Value> = match me {
        Some(m) => sqlx::query(
            "SELECT b.id, b.title, \
               EXISTS(SELECT 1 FROM board_pins bp WHERE bp.board_id = b.id AND bp.pin_id = ?) AS has \
             FROM boards b WHERE b.user_id = ? ORDER BY b.id DESC",
        )
        .bind(id)
        .bind(m)
        .fetch_all(&db)
        .await?
        .iter()
        .map(|b| {
            json!({
                "id": b.get::<i64, _>("id"),
                "title": b.get::<String, _>("title"),
                "has": b.get::<bool, _>("has"),
            })
        })
        .collect(),
        None => vec![],
    };

    Ok(Json(json!({
        "id": id,
        "title": r.get::<String, _>("title"),
        "description": r.get::<String, _>("description"),
        "image_url": r.get::<String, _>("image_url"),
        "link": r.get::<String, _>("link"),
        "created_at": r.get::<String, _>("created_at"),
        "owner": {
            "username": r.get::<String, _>("username"),
            "display_name": r.get::<String, _>("display_name"),
        },
        "is_owner": me == Some(r.get::<i64, _>("user_id")),
        "saves": saves,
        "my_boards": my_boards,
    })))
}

async fn delete_pin(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let r = sqlx::query("SELECT user_id, image_url FROM pins WHERE id = ?")
        .bind(id)
        .fetch_optional(&db)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "pin não encontrado"))?;
    if r.get::<i64, _>("user_id") != me {
        return Err(AppError::new(StatusCode::FORBIDDEN, "esse pin não é seu"));
    }
    let image_url: String = r.get("image_url");

    sqlx::query("DELETE FROM board_pins WHERE pin_id = ?").bind(id).execute(&db).await?;
    sqlx::query("DELETE FROM pins WHERE id = ?").bind(id).execute(&db).await?;

    // apaga o arquivo enviado, a menos que outro pin use a mesma imagem
    if let Some(name) = upload_name(&image_url) {
        let still_used = sqlx::query("SELECT 1 FROM pins WHERE image_url = ? LIMIT 1")
            .bind(&image_url)
            .fetch_optional(&db)
            .await?
            .is_some();
        if !still_used {
            let _ = tokio::fs::remove_file(format!("{UPLOAD_DIR}/{name}")).await;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- boards ----------

#[derive(Deserialize)]
struct NewBoard {
    title: String,
    #[serde(default)]
    description: String,
}

async fn create_board(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(b): Json<NewBoard>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let title = b.title.trim();
    let description = b.description.trim();
    if title.is_empty() || title.chars().count() > 80 {
        return Err(bad("título: 1 a 80 caracteres"));
    }
    if description.chars().count() > 500 {
        return Err(bad("descrição: no máximo 500 caracteres"));
    }
    let id = sqlx::query("INSERT INTO boards (user_id, title, description) VALUES (?, ?, ?)")
        .bind(me)
        .bind(title)
        .bind(description)
        .execute(&db)
        .await?
        .last_insert_rowid();
    Ok(Json(json!({ "id": id })))
}

async fn require_board_owner(db: &SqlitePool, jar: &CookieJar, board_id: i64) -> ApiResult<i64> {
    let me = require_user(db, jar).await?;
    let owner = sqlx::query("SELECT user_id FROM boards WHERE id = ?")
        .bind(board_id)
        .fetch_optional(db)
        .await?
        .map(|r| r.get::<i64, _>("user_id"));
    match owner {
        None => Err(AppError::new(StatusCode::NOT_FOUND, "board não encontrado")),
        Some(o) if o != me => Err(AppError::new(StatusCode::FORBIDDEN, "esse board não é seu")),
        Some(_) => Ok(me),
    }
}

async fn get_board(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = current_user(&db, &jar).await?;
    let b = sqlx::query(
        "SELECT b.title, b.description, b.user_id, u.username, u.display_name \
         FROM boards b JOIN users u ON u.id = b.user_id WHERE b.id = ?",
    )
    .bind(id)
    .fetch_optional(&db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "board não encontrado"))?;

    let rows = sqlx::query(&format!(
        "SELECT {CARD_COLS} FROM board_pins bp \
         JOIN pins p ON p.id = bp.pin_id JOIN users u ON u.id = p.user_id \
         WHERE bp.board_id = ? ORDER BY bp.rowid DESC"
    ))
    .bind(id)
    .fetch_all(&db)
    .await?;

    Ok(Json(json!({
        "id": id,
        "title": b.get::<String, _>("title"),
        "description": b.get::<String, _>("description"),
        "owner": {
            "username": b.get::<String, _>("username"),
            "display_name": b.get::<String, _>("display_name"),
        },
        "is_owner": me == Some(b.get::<i64, _>("user_id")),
        "pins": rows.iter().map(pin_card).collect::<Vec<_>>(),
    })))
}

async fn delete_board(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    require_board_owner(&db, &jar, id).await?;
    // os pins continuam existindo; só saem do board
    sqlx::query("DELETE FROM board_pins WHERE board_id = ?").bind(id).execute(&db).await?;
    sqlx::query("DELETE FROM boards WHERE id = ?").bind(id).execute(&db).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct AddPin {
    pin_id: i64,
}

async fn add_board_pin(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
    Json(b): Json<AddPin>,
) -> ApiResult<Json<Value>> {
    require_board_owner(&db, &jar, id).await?;
    let exists = sqlx::query("SELECT 1 FROM pins WHERE id = ?")
        .bind(b.pin_id)
        .fetch_optional(&db)
        .await?
        .is_some();
    if !exists {
        return Err(AppError::new(StatusCode::NOT_FOUND, "pin não encontrado"));
    }
    sqlx::query("INSERT OR IGNORE INTO board_pins (board_id, pin_id) VALUES (?, ?)")
        .bind(id)
        .bind(b.pin_id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove_board_pin(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path((id, pin_id)): Path<(i64, i64)>,
) -> ApiResult<Json<Value>> {
    require_board_owner(&db, &jar, id).await?;
    sqlx::query("DELETE FROM board_pins WHERE board_id = ? AND pin_id = ?")
        .bind(id)
        .bind(pin_id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn user_boards(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<Value>>> {
    let uid = user_id_by_name(&db, &username).await?;
    let boards = sqlx::query(
        "SELECT b.id, b.title, \
           (SELECT COUNT(*) FROM board_pins bp WHERE bp.board_id = b.id) AS n \
         FROM boards b WHERE b.user_id = ? ORDER BY b.id DESC LIMIT 100",
    )
    .bind(uid)
    .fetch_all(&db)
    .await?;

    let mut out = Vec::with_capacity(boards.len());
    for b in &boards {
        let id: i64 = b.get("id");
        // até 4 capas por board
        let covers: Vec<String> = sqlx::query(
            "SELECT p.image_url FROM board_pins bp JOIN pins p ON p.id = bp.pin_id \
             WHERE bp.board_id = ? ORDER BY bp.rowid DESC LIMIT 4",
        )
        .bind(id)
        .fetch_all(&db)
        .await?
        .iter()
        .map(|r| r.get::<String, _>("image_url"))
        .collect();
        out.push(json!({
            "id": id,
            "title": b.get::<String, _>("title"),
            "n": b.get::<i64, _>("n"),
            "covers": covers,
        }));
    }
    Ok(Json(out))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognizes_supported_image_signatures() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]), Some("png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0x00]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a"), Some("gif"));
        assert_eq!(sniff(b"RIFF1234WEBP0"), Some("webp"));
    }

    #[test]
    fn sniff_rejects_unknown_or_incomplete_signatures() {
        assert_eq!(sniff(b"not an image"), None);
        assert_eq!(sniff(b"RIFF1234NOPE"), None);
    }

    #[test]
    fn upload_name_accepts_only_generated_upload_paths() {
        assert_eq!(upload_name("/uploads/0123456789abcdef0123456789abcdef.png"),
                   Some("0123456789abcdef0123456789abcdef.png"));
        assert_eq!(upload_name("/uploads/../../app.db"), None);
        assert_eq!(upload_name("/uploads/0123456789abcdef0123456789abcdef.svg"), None);
        assert_eq!(upload_name("https://example.com/image.png"), None);
    }
}
