use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post, put},
    Json, Router,
};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{sqlite::SqliteRow, Row, SqlitePool};

use crate::{current_user, require_user, user_id_by_name, ApiResult, AppError, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/catalog/search", get(catalog_search))
        .route("/items/:id", get(item_detail))
        .route("/items/:id/rating", put(put_rating).delete(delete_rating))
        .route("/items/:id/watchlist", post(add_watchlist).delete(remove_watchlist))
        .route("/users/:username/ratings", get(user_ratings))
        .route("/users/:username/watchlist", get(user_watchlist))
        .route("/users/:username/lists", get(user_lists))
        .route("/lists", post(create_list))
        .route("/lists/:id", get(get_list).delete(delete_list))
        .route("/lists/:id/items", post(add_list_item))
        .route("/lists/:id/items/:item_id", delete(remove_list_item))
}

// ---------- itens ----------

#[derive(Serialize)]
struct Item {
    id: i64,
    kind: String,
    title: String,
    year: Option<i64>,
    creator: String,
    image_url: String,
    /// média em estrelas (0,5 a 5)
    avg: Option<f64>,
    /// quantidade de notas
    n: i64,
}

const ITEM_COLS: &str = "i.id, i.kind, i.title, i.year, i.creator, i.image_url, \
    (SELECT AVG(x.rating) FROM ratings x WHERE x.item_id = i.id) / 2.0 AS avg, \
    (SELECT COUNT(*) FROM ratings x WHERE x.item_id = i.id) AS n";

fn row_to_item(r: &SqliteRow) -> Item {
    Item {
        id: r.get("id"),
        kind: r.get("kind"),
        title: r.get("title"),
        year: r.get("year"),
        creator: r.get("creator"),
        image_url: r.get("image_url"),
        avg: r.get("avg"),
        n: r.get("n"),
    }
}

async fn fetch_item(db: &SqlitePool, id: i64) -> ApiResult<Option<Item>> {
    let row = sqlx::query(&format!("SELECT {ITEM_COLS} FROM items i WHERE i.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row.as_ref().map(row_to_item))
}

async fn require_item(db: &SqlitePool, id: i64) -> ApiResult<Item> {
    fetch_item(db, id)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "item não encontrado"))
}

// ---------- busca externa ----------

struct NewItem {
    kind: &'static str,
    ext_id: String,
    title: String,
    year: Option<i64>,
    creator: String,
    image_url: String,
}

fn ext_err(e: reqwest::Error) -> AppError {
    eprintln!("erro em serviço externo: {}", e.without_url());
    AppError::new(StatusCode::BAD_GATEWAY, "falha ao consultar o serviço externo")
}

fn year_of(date: Option<&str>) -> Option<i64> {
    date.and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())
}

async fn search_tmdb(app: &AppState, q: &str) -> ApiResult<Vec<NewItem>> {
    let key = app.tmdb_key.as_deref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "a busca de filmes precisa da variável TMDB_API_KEY",
        )
    })?;

    let mut req = app
        .http
        .get("https://api.themoviedb.org/3/search/movie")
        .query(&[("query", q), ("language", "pt-BR"), ("include_adult", "false")]);
    // aceita tanto a "API Key" (hex) quanto o "Read Access Token" (começa com eyJ)
    req = if key.starts_with("eyJ") {
        req.bearer_auth(key)
    } else {
        req.query(&[("api_key", key)])
    };

    let resp: Value = req
        .send()
        .await
        .map_err(ext_err)?
        .error_for_status()
        .map_err(ext_err)?
        .json()
        .await
        .map_err(ext_err)?;

    let items = resp["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(NewItem {
                        kind: "movie",
                        ext_id: r["id"].as_i64()?.to_string(),
                        title: r["title"].as_str()?.to_string(),
                        year: year_of(r["release_date"].as_str()),
                        creator: String::new(),
                        image_url: r["poster_path"]
                            .as_str()
                            .map(|p| format!("https://image.tmdb.org/t/p/w342{p}"))
                            .unwrap_or_default(),
                    })
                })
                .take(20)
                .collect()
        })
        .unwrap_or_default();
    Ok(items)
}

async fn search_musicbrainz(app: &AppState, q: &str) -> ApiResult<Vec<NewItem>> {
    let resp: Value = app
        .http
        .get("https://musicbrainz.org/ws/2/release-group/")
        .query(&[("query", q), ("fmt", "json"), ("limit", "20")])
        .send()
        .await
        .map_err(ext_err)?
        .error_for_status()
        .map_err(ext_err)?
        .json()
        .await
        .map_err(ext_err)?;

    let items = resp["release-groups"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let id = r["id"].as_str()?;
                    Some(NewItem {
                        kind: "album",
                        ext_id: id.to_string(),
                        title: r["title"].as_str()?.to_string(),
                        year: year_of(r["first-release-date"].as_str()),
                        creator: r["artist-credit"][0]["name"].as_str().unwrap_or("").to_string(),
                        image_url: format!("https://coverartarchive.org/release-group/{id}/front-250"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(items)
}

#[derive(Deserialize)]
struct CatalogQuery {
    kind: String,
    q: String,
}

async fn catalog_search(
    State(app): State<AppState>,
    Query(p): Query<CatalogQuery>,
) -> ApiResult<Json<Vec<Item>>> {
    let q = p.q.trim();
    if q.is_empty() {
        return Ok(Json(vec![]));
    }
    if q.chars().count() > 120 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "busca: no máximo 120 caracteres"));
    }
    let found = match p.kind.as_str() {
        "movie" => search_tmdb(&app, q).await?,
        "album" => search_musicbrainz(&app, q).await?,
        _ => return Err(AppError::new(StatusCode::BAD_REQUEST, "kind deve ser movie ou album")),
    };

    let mut out = Vec::with_capacity(found.len());
    for n in found {
        sqlx::query(
            "INSERT INTO items (kind, ext_id, title, year, creator, image_url) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (kind, ext_id) DO UPDATE SET \
               title = excluded.title, year = excluded.year, \
               creator = excluded.creator, image_url = excluded.image_url",
        )
        .bind(n.kind)
        .bind(&n.ext_id)
        .bind(&n.title)
        .bind(n.year)
        .bind(&n.creator)
        .bind(&n.image_url)
        .execute(&app.db)
        .await?;

        let id: i64 = sqlx::query("SELECT id FROM items WHERE kind = ? AND ext_id = ?")
            .bind(n.kind)
            .bind(&n.ext_id)
            .fetch_one(&app.db)
            .await?
            .get("id");
        if let Some(item) = fetch_item(&app.db, id).await? {
            out.push(item);
        }
    }
    Ok(Json(out))
}

// ---------- página do item ----------

async fn item_detail(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = current_user(&db, &jar).await?;
    let item = require_item(&db, id).await?;

    let mine = match me {
        Some(m) => sqlx::query("SELECT rating, review FROM ratings WHERE user_id = ? AND item_id = ?")
            .bind(m)
            .bind(id)
            .fetch_optional(&db)
            .await?
            .map(|r| {
                json!({
                    "rating": r.get::<i64, _>("rating") as f64 / 2.0,
                    "review": r.get::<String, _>("review"),
                })
            }),
        None => None,
    };

    let in_watchlist = match me {
        Some(m) => sqlx::query("SELECT 1 FROM watchlist WHERE user_id = ? AND item_id = ?")
            .bind(m)
            .bind(id)
            .fetch_optional(&db)
            .await?
            .is_some(),
        None => false,
    };

    let rows = sqlx::query(
        "SELECT u.username, u.display_name, r.rating, r.review, r.updated_at \
         FROM ratings r JOIN users u ON u.id = r.user_id \
         WHERE r.item_id = ? AND r.review != '' \
         ORDER BY r.updated_at DESC LIMIT 30",
    )
    .bind(id)
    .fetch_all(&db)
    .await?;
    let reviews: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "username": r.get::<String, _>("username"),
                "display_name": r.get::<String, _>("display_name"),
                "rating": r.get::<i64, _>("rating") as f64 / 2.0,
                "review": r.get::<String, _>("review"),
                "at": r.get::<String, _>("updated_at"),
            })
        })
        .collect();

    Ok(Json(json!({
        "item": item,
        "mine": mine,
        "in_watchlist": in_watchlist,
        "reviews": reviews,
    })))
}

// ---------- notas e reviews ----------

#[derive(Deserialize)]
struct RatingIn {
    rating: f64,
    #[serde(default)]
    review: String,
}

async fn put_rating(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
    Json(b): Json<RatingIn>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    require_item(&db, id).await?;

    let twice = b.rating * 2.0;
    if !(1.0..=10.0).contains(&twice) || twice.fract() != 0.0 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "a nota vai de 0,5 a 5 estrelas, de meia em meia",
        ));
    }
    let review = b.review.trim();
    if review.chars().count() > 5000 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "review: no máximo 5000 caracteres"));
    }

    sqlx::query(
        "INSERT INTO ratings (user_id, item_id, rating, review) VALUES (?, ?, ?, ?) \
         ON CONFLICT (user_id, item_id) DO UPDATE SET \
           rating = excluded.rating, review = excluded.review, updated_at = datetime('now')",
    )
    .bind(me)
    .bind(id)
    .bind(twice as i64)
    .bind(review)
    .execute(&db)
    .await?;
    // se estava na watchlist, já saiu: foi assistido/ouvido
    sqlx::query("DELETE FROM watchlist WHERE user_id = ? AND item_id = ?")
        .bind(me)
        .bind(id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_rating(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    sqlx::query("DELETE FROM ratings WHERE user_id = ? AND item_id = ?")
        .bind(me)
        .bind(id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- watchlist ----------

async fn add_watchlist(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    require_item(&db, id).await?;
    sqlx::query("INSERT OR IGNORE INTO watchlist (user_id, item_id) VALUES (?, ?)")
        .bind(me)
        .bind(id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove_watchlist(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    sqlx::query("DELETE FROM watchlist WHERE user_id = ? AND item_id = ?")
        .bind(me)
        .bind(id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- diário e watchlist de um usuário ----------

#[derive(Deserialize)]
struct KindQuery {
    kind: Option<String>,
}

#[derive(Serialize)]
struct DiaryEntry {
    item: Item,
    rating: f64,
    review: String,
    at: String,
}

async fn user_ratings(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
    Query(k): Query<KindQuery>,
) -> ApiResult<Json<Vec<DiaryEntry>>> {
    let uid = user_id_by_name(&db, &username).await?;
    let kind = k.kind.unwrap_or_default();
    let sql = format!(
        "SELECT {ITEM_COLS}, r.rating AS my_rating, r.review AS my_review, r.updated_at AS logged_at \
         FROM ratings r JOIN items i ON i.id = r.item_id \
         WHERE r.user_id = ? AND (? = '' OR i.kind = ?) \
         ORDER BY r.updated_at DESC, i.id DESC LIMIT 100"
    );
    let rows = sqlx::query(&sql)
        .bind(uid)
        .bind(&kind)
        .bind(&kind)
        .fetch_all(&db)
        .await?;
    let out = rows
        .iter()
        .map(|r| DiaryEntry {
            item: row_to_item(r),
            rating: r.get::<i64, _>("my_rating") as f64 / 2.0,
            review: r.get("my_review"),
            at: r.get("logged_at"),
        })
        .collect();
    Ok(Json(out))
}

async fn user_watchlist(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<Item>>> {
    let uid = user_id_by_name(&db, &username).await?;
    let sql = format!(
        "SELECT {ITEM_COLS} FROM watchlist w JOIN items i ON i.id = w.item_id \
         WHERE w.user_id = ? ORDER BY w.rowid DESC LIMIT 100"
    );
    let rows = sqlx::query(&sql).bind(uid).fetch_all(&db).await?;
    Ok(Json(rows.iter().map(row_to_item).collect()))
}

// ---------- listas ----------

#[derive(Deserialize)]
struct NewList {
    title: String,
    #[serde(default)]
    description: String,
}

async fn create_list(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Json(b): Json<NewList>,
) -> ApiResult<Json<Value>> {
    let me = require_user(&db, &jar).await?;
    let title = b.title.trim();
    let description = b.description.trim();
    if title.is_empty() || title.chars().count() > 80 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "título: 1 a 80 caracteres"));
    }
    if description.chars().count() > 500 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "descrição: no máximo 500 caracteres"));
    }
    let id = sqlx::query("INSERT INTO lists (user_id, title, description) VALUES (?, ?, ?)")
        .bind(me)
        .bind(title)
        .bind(description)
        .execute(&db)
        .await?
        .last_insert_rowid();
    Ok(Json(json!({ "id": id })))
}

async fn require_list_owner(db: &SqlitePool, jar: &CookieJar, list_id: i64) -> ApiResult<i64> {
    let me = require_user(db, jar).await?;
    let owner = sqlx::query("SELECT user_id FROM lists WHERE id = ?")
        .bind(list_id)
        .fetch_optional(db)
        .await?
        .map(|r| r.get::<i64, _>("user_id"));
    match owner {
        None => Err(AppError::new(StatusCode::NOT_FOUND, "lista não encontrada")),
        Some(o) if o != me => Err(AppError::new(StatusCode::FORBIDDEN, "essa lista não é sua")),
        Some(_) => Ok(me),
    }
}

async fn get_list(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let me = current_user(&db, &jar).await?;
    let row = sqlx::query(
        "SELECT l.title, l.description, l.user_id, u.username, u.display_name \
         FROM lists l JOIN users u ON u.id = l.user_id WHERE l.id = ?",
    )
    .bind(id)
    .fetch_optional(&db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "lista não encontrada"))?;

    let sql = format!(
        "SELECT {ITEM_COLS} FROM list_items li JOIN items i ON i.id = li.item_id \
         WHERE li.list_id = ? ORDER BY li.position"
    );
    let rows = sqlx::query(&sql).bind(id).fetch_all(&db).await?;
    let items: Vec<Item> = rows.iter().map(row_to_item).collect();

    let owner_id: i64 = row.get("user_id");
    Ok(Json(json!({
        "id": id,
        "title": row.get::<String, _>("title"),
        "description": row.get::<String, _>("description"),
        "owner": {
            "username": row.get::<String, _>("username"),
            "display_name": row.get::<String, _>("display_name"),
        },
        "is_owner": me == Some(owner_id),
        "items": items,
    })))
}

async fn delete_list(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    require_list_owner(&db, &jar, id).await?;
    sqlx::query("DELETE FROM list_items WHERE list_id = ?")
        .bind(id)
        .execute(&db)
        .await?;
    sqlx::query("DELETE FROM lists WHERE id = ?").bind(id).execute(&db).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct AddListItem {
    item_id: i64,
}

async fn add_list_item(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path(id): Path<i64>,
    Json(b): Json<AddListItem>,
) -> ApiResult<Json<Value>> {
    require_list_owner(&db, &jar, id).await?;
    require_item(&db, b.item_id).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO list_items (list_id, item_id, position) \
         VALUES (?, ?, (SELECT COALESCE(MAX(position), 0) + 1 FROM list_items WHERE list_id = ?))",
    )
    .bind(id)
    .bind(b.item_id)
    .bind(id)
    .execute(&db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove_list_item(
    State(db): State<SqlitePool>,
    jar: CookieJar,
    Path((id, item_id)): Path<(i64, i64)>,
) -> ApiResult<Json<Value>> {
    require_list_owner(&db, &jar, id).await?;
    sqlx::query("DELETE FROM list_items WHERE list_id = ? AND item_id = ?")
        .bind(id)
        .bind(item_id)
        .execute(&db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn user_lists(
    State(db): State<SqlitePool>,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<Value>>> {
    let uid = user_id_by_name(&db, &username).await?;
    let rows = sqlx::query(
        "SELECT l.id, l.title, l.description, \
           (SELECT COUNT(*) FROM list_items li WHERE li.list_id = l.id) AS n \
         FROM lists l WHERE l.user_id = ? ORDER BY l.id DESC LIMIT 100",
    )
    .bind(uid)
    .fetch_all(&db)
    .await?;
    let out = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "title": r.get::<String, _>("title"),
                "description": r.get::<String, _>("description"),
                "n": r.get::<i64, _>("n"),
            })
        })
        .collect();
    Ok(Json(out))
}
