// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub roles: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// IANA timezone (e.g. `Europe/Berlin`). Captured from the browser
    /// on first authed page load — see `set_timezone`. None for users
    /// who only hit the gateway through `gw` CLI / external API calls,
    /// or who haven't navigated the UI since the column was added.
    /// Tools that care about wall-clock time fall back to UTC when
    /// this is null.
    pub timezone: Option<String>,
}

fn map_row(row: &SqliteRow) -> Result<User, DbError> {
    let id: String = row.try_get("id")?;
    let email: String = row.try_get("email")?;
    let name: Option<String> = row.try_get("name")?;
    let roles_json: String = row.try_get("roles_json")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    let timezone: Option<String> = row.try_get("timezone")?;

    let roles: Vec<String> = serde_json::from_str(&roles_json).map_err(|e| DbError::Decode {
        column: "roles_json",
        source: e.into(),
    })?;
    let created_at: Timestamp = created_at
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "created_at",
            source: e.into(),
        })?;
    let updated_at: Timestamp = updated_at
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "updated_at",
            source: e.into(),
        })?;

    Ok(User {
        id,
        email,
        name,
        roles,
        created_at,
        updated_at,
        timezone,
    })
}

/// Inserts a new user or, if `id` already exists, updates the mutable fields
/// (email, name, roles) and bumps `updated_at`. `created_at` is preserved.
pub async fn upsert(pool: &Pool, user: &User) -> Result<(), DbError> {
    let roles_json = serde_json::to_string(&user.roles).map_err(|e| DbError::Decode {
        column: "roles_json",
        source: e.into(),
    })?;
    sqlx::query(
        r#"INSERT INTO users (id, email, name, roles_json, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?)
           ON CONFLICT(id) DO UPDATE SET
               email      = excluded.email,
               name       = excluded.name,
               roles_json = excluded.roles_json,
               updated_at = excluded.updated_at"#,
    )
    .bind(&user.id)
    .bind(&user.email)
    .bind(&user.name)
    .bind(&roles_json)
    .bind(user.created_at.to_string())
    .bind(user.updated_at.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find_by_id(pool: &Pool, id: &str) -> Result<Option<User>, DbError> {
    let row = sqlx::query("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_row).transpose()
}

/// Every registered user, newest first. Powers the admin `/admin/users`
/// roster. Users are created lazily on first OIDC login (see
/// `oidc_handlers::callback`), so this is "everyone who has ever signed
/// in", not a pre-provisioned directory.
pub async fn list_all(pool: &Pool) -> Result<Vec<User>, DbError> {
    let rows = sqlx::query("SELECT * FROM users ORDER BY created_at DESC, id")
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_row).collect()
}

/// Updates just the `timezone` column for an existing user. Bumps
/// `updated_at`. Called from the `POST /api/v0/me/timezone` handler
/// after the browser's `Intl.DateTimeFormat().resolvedOptions().
/// timeZone` posts up. Silently no-ops if the user doesn't exist
/// (shouldn't happen — caller is always authed).
pub async fn set_timezone(pool: &Pool, user_id: &str, timezone: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query("UPDATE users SET timezone = ?, updated_at = ? WHERE id = ?")
        .bind(timezone)
        .bind(now)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A browser-shared geolocation read back off the `users` row. Distinct
/// from the IP-derived `geoip::GeoLocation`: this is the precise position
/// the user explicitly granted via `navigator.geolocation`, stamped with
/// when we recorded it so callers can judge staleness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredLocation {
    pub lat: f64,
    pub lon: f64,
    /// Reported accuracy radius in metres, when the browser supplied one.
    pub accuracy: Option<f64>,
    pub updated_at: Timestamp,
}

impl StoredLocation {
    /// Whether the fix is at most `max_age_secs` old. A precise GPS fix
    /// goes stale as the user moves, so the `get_user_location` tool only
    /// trusts a recent one and otherwise falls back to coarse GeoIP.
    pub fn is_fresh(&self, max_age_secs: i64) -> bool {
        Timestamp::now().as_second() - self.updated_at.as_second() <= max_age_secs
    }
}

/// Store the caller's browser-reported position on their user row,
/// bumping `loc_updated_at` + `updated_at`. Written by
/// `POST /api/v0/me/location` (the `/tools` button + the chat
/// feedback-loop prompt). Last shared position wins.
pub async fn set_location(
    pool: &Pool,
    user_id: &str,
    lat: f64,
    lon: f64,
    accuracy: Option<f64>,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        "UPDATE users SET loc_lat = ?, loc_lon = ?, loc_accuracy = ?, \
         loc_updated_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(lat)
    .bind(lon)
    .bind(accuracy)
    .bind(&now)
    .bind(&now)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the caller's stored browser position, if any. Returns `None`
/// when the user never shared one (all-null columns) or doesn't exist.
pub async fn find_location(pool: &Pool, user_id: &str) -> Result<Option<StoredLocation>, DbError> {
    let row = sqlx::query(
        "SELECT loc_lat, loc_lon, loc_accuracy, loc_updated_at FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let lat: Option<f64> = row.try_get("loc_lat")?;
    let lon: Option<f64> = row.try_get("loc_lon")?;
    let accuracy: Option<f64> = row.try_get("loc_accuracy")?;
    let updated_at: Option<String> = row.try_get("loc_updated_at")?;
    match (lat, lon, updated_at) {
        (Some(lat), Some(lon), Some(updated_at)) => {
            let updated_at: Timestamp =
                updated_at
                    .parse()
                    .map_err(|e: jiff::Error| DbError::Decode {
                        column: "loc_updated_at",
                        source: e.into(),
                    })?;
            Ok(Some(StoredLocation {
                lat,
                lon,
                accuracy,
                updated_at,
            }))
        }
        _ => Ok(None),
    }
}

/// Forget the caller's stored position (the "stop sharing" affordance on
/// `/tools`). No-op if none was stored.
pub async fn clear_location(pool: &Pool, user_id: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        "UPDATE users SET loc_lat = NULL, loc_lon = NULL, loc_accuracy = NULL, \
         loc_updated_at = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str) -> User {
        let now = Timestamp::now();
        User {
            id: id.into(),
            email: format!("{id}@example.com"),
            name: Some("Test User".into()),
            roles: vec!["engineering".into()],
            created_at: now,
            updated_at: now,
            timezone: None,
        }
    }

    async fn pool() -> Pool {
        super::super::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn upsert_inserts_then_updates() {
        let pool = pool().await;
        let mut u = fixture("subject-abc");
        upsert(&pool, &u).await.unwrap();

        u.email = "new@example.com".into();
        u.roles = vec!["finance".into(), "admin".into()];
        upsert(&pool, &u).await.unwrap();

        let got = find_by_id(&pool, "subject-abc").await.unwrap().unwrap();
        assert_eq!(got.email, "new@example.com");
        assert_eq!(got.roles, vec!["finance", "admin"]);
    }

    #[tokio::test]
    async fn find_by_id_returns_none_for_unknown() {
        let pool = pool().await;
        assert!(find_by_id(&pool, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_all_returns_every_user() {
        let pool = pool().await;
        assert!(list_all(&pool).await.unwrap().is_empty());
        upsert(&pool, &fixture("u1")).await.unwrap();
        upsert(&pool, &fixture("u2")).await.unwrap();
        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
        let ids: Vec<&str> = all.iter().map(|u| u.id.as_str()).collect();
        assert!(ids.contains(&"u1") && ids.contains(&"u2"));
    }

    #[tokio::test]
    async fn location_round_trips_and_clears() {
        let pool = pool().await;
        let u = fixture("subject-loc");
        upsert(&pool, &u).await.unwrap();
        assert!(find_location(&pool, &u.id).await.unwrap().is_none());

        set_location(&pool, &u.id, 52.52, 13.405, Some(25.0))
            .await
            .unwrap();
        let got = find_location(&pool, &u.id).await.unwrap().unwrap();
        assert_eq!(got.lat, 52.52);
        assert_eq!(got.lon, 13.405);
        assert_eq!(got.accuracy, Some(25.0));
        // Just-written fix is fresh under any sane threshold.
        assert!(got.is_fresh(60));
        // A zero-second window makes even a fresh fix count as stale.
        assert!(!got.is_fresh(-1));

        clear_location(&pool, &u.id).await.unwrap();
        assert!(find_location(&pool, &u.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_location_accepts_null_accuracy() {
        let pool = pool().await;
        let u = fixture("subject-loc2");
        upsert(&pool, &u).await.unwrap();
        set_location(&pool, &u.id, 1.0, 2.0, None).await.unwrap();
        let got = find_location(&pool, &u.id).await.unwrap().unwrap();
        assert_eq!(got.accuracy, None);
    }

    #[tokio::test]
    async fn set_timezone_persists_and_bumps_updated_at() {
        let pool = pool().await;
        let u = fixture("subject-tz");
        upsert(&pool, &u).await.unwrap();
        let before = find_by_id(&pool, &u.id).await.unwrap().unwrap();
        assert!(before.timezone.is_none());

        set_timezone(&pool, &u.id, "Europe/Berlin").await.unwrap();

        let after = find_by_id(&pool, &u.id).await.unwrap().unwrap();
        assert_eq!(after.timezone.as_deref(), Some("Europe/Berlin"));
        assert!(after.updated_at >= before.updated_at);
    }
}
