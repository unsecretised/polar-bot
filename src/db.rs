use std::path::Path;
use std::sync::Mutex;

use chrono::{Duration, NaiveDate};
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};

pub const DISCOUNT_COST: i64 = 200;

const DAILY_MIN: i64 = 15;
const DAILY_MAX: i64 = 25;
const STREAK_BONUS_STEP: i64 = 5;
const MAX_STREAK_BONUS: i64 = 25;

pub struct Db {
    conn: Mutex<Connection>,
}

pub enum DailyOutcome {
    AlreadyClaimed { streak: i64, balance: i64 },
    Claimed {
        roll: i64,
        bonus: i64,
        streak: i64,
        balance: i64,
    },
}

#[derive(Debug)]
pub enum DiscountError {
    InsufficientCoins { balance: i64 },
    OnCooldown { eligible_at: i64 },
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for DiscountError {
    fn from(value: rusqlite::Error) -> Self {
        DiscountError::Db(value)
    }
}

#[derive(Debug)]
pub struct TentativeDiscount {
    pub id: i64,
    pub balance_after: i64,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().expect("db lock poisoned");
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn get_setting_u64(&self, key: &str) -> Option<u64> {
        self.get_setting(key)?.parse().ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("db lock poisoned");
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn user_state(&self, discord_id: i64) -> rusqlite::Result<(i64, i64)> {
        let conn = self.conn.lock().expect("db lock poisoned");
        let row = conn
            .query_row(
                "SELECT coins, streak FROM users WHERE discord_id = ?1",
                params![discord_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.unwrap_or((0, 0)))
    }

    pub fn daily(&self, discord_id: i64, today: NaiveDate) -> rusqlite::Result<DailyOutcome> {
        let mut conn = self.conn.lock().expect("db lock poisoned");
        let tx = conn.transaction()?;
        let today_str = today.to_string();
        let yesterday_str = (today - Duration::days(1)).to_string();

        let user: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT last_daily, coins, streak FROM users WHERE discord_id = ?1",
                params![discord_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let Some((last_daily, coins, streak)) = user else {
            let roll = rand::rng().random_range(DAILY_MIN..=DAILY_MAX);
            tx.execute(
                "INSERT INTO users (discord_id, coins, last_daily, streak) VALUES (?1, ?2, ?3, 1)",
                params![discord_id, roll, today_str],
            )?;
            tx.commit()?;
            return Ok(DailyOutcome::Claimed {
                roll,
                bonus: 0,
                streak: 1,
                balance: roll,
            });
        };

        if last_daily == today_str {
            tx.commit()?;
            return Ok(DailyOutcome::AlreadyClaimed {
                streak,
                balance: coins,
            });
        }

        let streak = if last_daily == yesterday_str {
            streak + 1
        } else {
            1
        };
        let roll = rand::rng().random_range(DAILY_MIN..=DAILY_MAX);
        let bonus = ((streak - 1) * STREAK_BONUS_STEP).min(MAX_STREAK_BONUS);
        tx.execute(
            "UPDATE users SET coins = coins + ?1, streak = ?2, last_daily = ?3 WHERE discord_id = ?4",
            params![roll + bonus, streak, today_str, discord_id],
        )?;
        let balance: i64 = tx.query_row(
            "SELECT coins FROM users WHERE discord_id = ?1",
                params![discord_id],
                |r| r.get::<_, i64>(0),
        )?;
        tx.commit()?;
        Ok(DailyOutcome::Claimed {
            roll,
            bonus,
            streak,
            balance,
        })
    }

    pub fn try_start_discount(
        &self,
        discord_id: i64,
        cost: i64,
        now_ts: i64,
        cooldown_secs: i64,
    ) -> Result<TentativeDiscount, DiscountError> {
        let mut conn = self.conn.lock().expect("db lock poisoned");
        let tx = conn.transaction()?;

        if let Some(last_ts) = tx
            .query_row(
                "SELECT created_at FROM discount_redemptions
                 WHERE discord_id = ?1 ORDER BY created_at DESC LIMIT 1",
                params![discord_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            let eligible_at = last_ts + cooldown_secs;
            if now_ts < eligible_at {
                return Err(DiscountError::OnCooldown { eligible_at });
            }
        }

        let balance: i64 = tx
            .query_row(
                "SELECT coins FROM users WHERE discord_id = ?1",
                params![discord_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if balance < cost {
            return Err(DiscountError::InsufficientCoins { balance });
        }

        let updated = tx.execute(
            "UPDATE users SET coins = coins - ?1 WHERE discord_id = ?2 AND coins >= ?1",
            params![cost, discord_id],
        )?;
        if updated == 0 {
            return Err(DiscountError::InsufficientCoins { balance });
        }

        tx.execute(
            "INSERT INTO discount_redemptions (discord_id, code, polar_discount_id, created_at)
             VALUES (?1, 'PENDING', 'PENDING', ?2)",
            params![discord_id, now_ts],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(TentativeDiscount {
            id,
            balance_after: balance - cost,
        })
    }

    pub fn finalize_discount(
        &self,
        id: i64,
        code: &str,
        polar_discount_id: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("db lock poisoned");
        conn.execute(
            "UPDATE discount_redemptions SET code = ?1, polar_discount_id = ?2 WHERE id = ?3",
            params![code, polar_discount_id, id],
        )?;
        Ok(())
    }

    pub fn cancel_discount(&self, id: i64, cost: i64) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().expect("db lock poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE users SET coins = coins + ?1
             WHERE discord_id = (SELECT discord_id FROM discount_redemptions WHERE id = ?2)",
            params![cost, id],
        )?;
        tx.execute(
            "DELETE FROM discount_redemptions WHERE id = ?1",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn leaderboard(&self, limit: i64) -> rusqlite::Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().expect("db lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT discord_id, coins FROM users WHERE coins > 0
             ORDER BY coins DESC, discord_id ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS users (
                discord_id INTEGER PRIMARY KEY,
                coins      INTEGER NOT NULL DEFAULT 0,
                last_daily TEXT,
                streak     INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_users_coins ON users (coins DESC);

            CREATE TABLE IF NOT EXISTS discount_redemptions (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                discord_id        INTEGER NOT NULL REFERENCES users (discord_id),
                code              TEXT NOT NULL,
                polar_discount_id TEXT NOT NULL,
                created_at        INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_redemptions_user
                ON discount_redemptions (discord_id, created_at DESC);

            PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(":memory:").unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn epoch(d: NaiveDate) -> i64 {
        d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
    }

    #[test]
    fn daily_first_claim_then_streak() {
        let db = db();
        let day1 = date(2026, 1, 1);
        let day2 = date(2026, 1, 2);

        let out = db.daily(1, day1).unwrap();
        let DailyOutcome::Claimed {
            roll,
            bonus,
            streak,
            balance,
        } = out
        else {
            panic!("expected claim")
        };
        assert!((DAILY_MIN..=DAILY_MAX).contains(&roll));
        assert_eq!((bonus, streak, balance), (0, 1, roll));

        assert!(matches!(
            db.daily(1, day1).unwrap(),
            DailyOutcome::AlreadyClaimed { .. }
        ));

        let out = db.daily(1, day2).unwrap();
        let DailyOutcome::Claimed {
            bonus, streak, ..
        } = out
        else {
            panic!("expected claim")
        };
        assert_eq!((bonus, streak), (5, 2));
    }

    #[test]
    fn daily_streak_resets_after_gap() {
        let db = db();
        db.daily(1, date(2026, 1, 1)).unwrap();
        let out = db.daily(1, date(2026, 1, 3)).unwrap();
        let DailyOutcome::Claimed {
            bonus, streak, ..
        } = out
        else {
            panic!("expected claim")
        };
        assert_eq!((bonus, streak), (0, 1));
    }

    #[test]
    fn daily_bonus_caps() {
        let db = db();
        for d in 1..=8 {
            let out = db.daily(1, date(2026, 1, d)).unwrap();
            let DailyOutcome::Claimed { bonus, streak, .. } = out else {
                panic!("expected claim")
            };
            let expected_bonus = (((d - 1) as i64) * STREAK_BONUS_STEP).min(MAX_STREAK_BONUS);
            assert_eq!((bonus, streak), (expected_bonus, d as i64));
        }
    }

    #[test]
    fn daily_users_are_isolated() {
        let db = db();
        let out1 = db.daily(1, date(2026, 1, 1)).unwrap();
        let DailyOutcome::Claimed { roll: roll1, .. } = out1 else {
            panic!("expected claim")
        };
        let out2 = db.daily(2, date(2026, 1, 1)).unwrap();
        let DailyOutcome::Claimed { roll: roll2, .. } = out2 else {
            panic!("expected claim")
        };
        assert!((DAILY_MIN..=DAILY_MAX).contains(&roll1));
        assert!((DAILY_MIN..=DAILY_MAX).contains(&roll2));
        assert_eq!(db.user_state(1).unwrap().0, roll1);
        assert_eq!(db.user_state(2).unwrap().0, roll2);
    }

    #[test]
    fn discount_happy_path() {
        let db = db();
        db.conn
            .lock()
            .unwrap()
            .execute("INSERT INTO users (discord_id, coins) VALUES (1, 250)", [])
            .unwrap();
        let t0 = epoch(date(2026, 2, 1));

        let tent = db.try_start_discount(1, DISCOUNT_COST, t0, 30 * 86_400).unwrap();
        assert_eq!(tent.balance_after, 50);
        db.finalize_discount(tent.id, "CODE1", "POLAR1").unwrap();

        let (coins, _) = db.user_state(1).unwrap();
        assert_eq!(coins, 50);

        let err = db
            .try_start_discount(1, DISCOUNT_COST, t0 + 86_400, 30 * 86_400)
            .unwrap_err();
        let DiscountError::OnCooldown { eligible_at } = err else {
            panic!("expected cooldown")
        };
        assert_eq!(eligible_at, t0 + 30 * 86_400);
    }

    #[test]
    fn discount_insufficient_and_cancel_refund() {
        let db = db();
        let t0 = epoch(date(2026, 3, 1));

        let err = db
            .try_start_discount(1, DISCOUNT_COST, t0, 30 * 86_400)
            .unwrap_err();
        let DiscountError::InsufficientCoins { balance } = err else {
            panic!("expected insufficient")
        };
        assert_eq!(balance, 0);

        db.conn
            .lock()
            .unwrap()
            .execute("INSERT INTO users (discord_id, coins) VALUES (2, 300)", [])
            .unwrap();
        let tent = db
            .try_start_discount(2, DISCOUNT_COST, t0, 30 * 86_400)
            .unwrap();
        db.cancel_discount(tent.id, DISCOUNT_COST).unwrap();
        let (coins, _) = db.user_state(2).unwrap();
        assert_eq!(coins, 300);
        assert!(db
            .try_start_discount(2, DISCOUNT_COST, t0, 30 * 86_400)
            .is_ok());
    }

    #[test]
    fn leaderboard_orders_by_coins() {
        let db = db();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO users (discord_id, coins) VALUES (1, 100), (2, 300), (3, 200);",
            )
            .unwrap();
        let board = db.leaderboard(10).unwrap();
        assert_eq!(board, vec![(2, 300), (3, 200), (1, 100)]);
    }
}
