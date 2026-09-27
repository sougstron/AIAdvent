//! Task 18: scheduler and background jobs, exposed as an MCP server.
//!
//! * [`Store`] — SQLite: jobs (what runs when), weather samples (what the
//!   collector gathered), notifications (what the agent sent) and a small
//!   key/value table (Telegram owner, pause flag).
//! * [`Server`] — MCP server `ask-scheduler-mcp` on the same Streamable HTTP
//!   transport as the git server: schedule reminders / periodic collection /
//!   periodic summaries, list and cancel jobs, and `weather_summary` — the
//!   aggregate over stored samples.
//! * [`Runner`] — the scheduler loop: every tick runs due jobs and
//!   reschedules periodic ones. A `summary` job is done by the agent: it
//!   calls `weather_summary` over MCP and writes the message from its result.
//! * [`run_daemon`] — everything together, 24/7: MCP server + scheduler +
//!   Telegram long polling, where every incoming message goes to the same
//!   agent with the same MCP tools.
//!
//! [`verify`] is the causal proof (`ask --verify-scheduler`).

use rusqlite::{params, Connection as Db, OptionalExtension};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::api::Endpoint;
use crate::config::{Res, Settings};
use crate::mcp::Connection;
use crate::mcp_agent::{self, ToolStep};
use crate::mcp_server::{self, CallLog, ServerInfo};

pub const DEFAULT_PORT: u16 = 8766;
/// Shortest allowed period: a job every second is a runaway, not a schedule.
pub const MIN_EVERY: i64 = 10;
pub const DEFAULT_CITY: &str = "Липецк";
/// Moscow time, no DST — every timestamp shown to a human is MSK.
const MSK: i64 = 3 * 3600;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-09-27 15:04:05` in MSK.
pub fn fmt_time(ts: i64) -> String {
    let t = ts + MSK;
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // civil_from_days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn hhmm(ts: i64) -> String {
    fmt_time(ts)[11..16].to_string()
}

/// HTTP agent for the scheduler's own traffic (weather, Telegram). The
/// model endpoint is not affected: it goes through `api.rs` as usual.
static HTTP: OnceLock<ureq::Agent> = OnceLock::new();

/// Route weather and Telegram through `proxy` (e.g. `http://host:8118`).
/// Must run before the first request.
pub fn set_proxy(proxy: &str) -> Res<()> {
    let p = ureq::Proxy::new(proxy).map_err(|e| format!("proxy {proxy}: {e}"))?;
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).proxy(p).build();
    HTTP.set(agent).map_err(|_| "proxy must be set before the first request".to_string())
}

fn http() -> &'static ureq::Agent {
    HTTP.get_or_init(|| ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).build())
}

// ---------------------------------------------------------------- places

/// Cities that resolve without the network; anything else goes through the
/// open-meteo geocoder.
const PLACES: &[(&[&str], &str, f64, f64)] = &[
    (&["липецк", "lipetsk"], "Липецк", 52.6031, 39.5708),
    (&["москва", "moscow"], "Москва", 55.7558, 37.6173),
    (&["воронеж", "voronezh"], "Воронеж", 51.6720, 39.1843),
    (&["санкт-петербург", "петербург", "спб", "saint petersburg"], "Санкт-Петербург", 59.9386, 30.3141),
];

pub fn resolve_city(city: &str) -> Res<(String, f64, f64)> {
    let key = city.trim().to_lowercase();
    if let Some((_, name, lat, lon)) = PLACES.iter().find(|(a, ..)| a.contains(&key.as_str())) {
        return Ok((name.to_string(), *lat, *lon));
    }
    let resp = http().get("https://geocoding-api.open-meteo.com/v1/search")
        .query("name", city.trim())
        .query("count", "1")
        .query("language", "ru")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("geocoding {city}: {e}"))?;
    let v: Value = resp.into_json().map_err(|e| e.to_string())?;
    let hit = &v["results"][0];
    match (hit["name"].as_str(), hit["latitude"].as_f64(), hit["longitude"].as_f64()) {
        (Some(n), Some(lat), Some(lon)) => Ok((n.to_string(), lat, lon)),
        _ => Err(format!("город не найден: {city}")),
    }
}

// ---------------------------------------------------------------- store

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub params: Value,
    /// `None` — one-shot, deactivated after the run.
    pub every: Option<i64>,
    pub next_run: i64,
    pub active: bool,
    pub runs: i64,
    pub last_run: Option<i64>,
    pub last_result: Option<String>,
}

impl Job {
    pub fn describe(&self) -> String {
        let what = match self.kind.as_str() {
            "reminder" => format!("напоминание «{}»", self.params["text"].as_str().unwrap_or("")),
            "collect" => format!("сбор погоды: {}", self.params["city"].as_str().unwrap_or("?")),
            "summary" => format!(
                "сводка погоды: {} за {} мин",
                self.params["city"].as_str().unwrap_or("?"),
                self.params["window_min"].as_i64().unwrap_or(60)
            ),
            k => k.to_string(),
        };
        let when = match (self.active, self.every) {
            (false, _) => "выполнено/отменено".to_string(),
            (true, Some(e)) => format!("каждые {e} с, следующий запуск {}", hhmm_s(self.next_run)),
            (true, None) => format!("один раз в {}", hhmm_s(self.next_run)),
        };
        format!("#{} {what} — {when}, запусков: {}", self.id, self.runs)
    }
}

fn hhmm_s(ts: i64) -> String {
    fmt_time(ts)[11..19].to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub city: String,
    pub temp: f64,
    pub feels: f64,
    pub humidity: f64,
    pub wind: f64,
    pub code: i64,
    /// Observation time reported by the source; the same observation can
    /// be collected several times (open-meteo refreshes every 15 minutes).
    pub observed: String,
}

pub struct Store {
    db: Mutex<Db>,
}

impl Store {
    pub fn open(path: &Path) -> Res<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Store::init(Db::open(path).map_err(|e| format!("sqlite {}: {e}", path.display()))?)
    }

    pub fn in_memory() -> Res<Store> {
        Store::init(Db::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(db: Db) -> Res<Store> {
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS jobs (
                 id INTEGER PRIMARY KEY, kind TEXT NOT NULL, params TEXT NOT NULL,
                 every_sec INTEGER, next_run INTEGER NOT NULL, active INTEGER NOT NULL DEFAULT 1,
                 created_at INTEGER NOT NULL, last_run INTEGER, runs INTEGER NOT NULL DEFAULT 0,
                 last_result TEXT);
             CREATE TABLE IF NOT EXISTS samples (
                 id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, city TEXT NOT NULL, temp REAL NOT NULL,
                 feels REAL NOT NULL, humidity REAL NOT NULL, wind REAL NOT NULL, code INTEGER NOT NULL,
                 observed TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS samples_city_ts ON samples(city, ts);
             CREATE TABLE IF NOT EXISTS notifications (
                 id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, job_id INTEGER, kind TEXT NOT NULL,
                 text TEXT NOT NULL, delivered INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(|e| format!("sqlite schema: {e}"))?;
        Ok(Store { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn add_job(&self, kind: &str, params: &Value, every: Option<i64>, first_run: i64) -> Res<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO jobs (kind, params, every_sec, next_run, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, params.to_string(), every, first_run, now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(db.last_insert_rowid())
    }

    fn query_jobs(&self, sql: &str, arg: i64) -> Res<Vec<Job>> {
        let db = self.db();
        let mut st = db.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map([arg], |r| {
                Ok(Job {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    params: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
                    every: r.get(3)?,
                    next_run: r.get(4)?,
                    active: r.get::<_, i64>(5)? != 0,
                    runs: r.get(6)?,
                    last_run: r.get(7)?,
                    last_result: r.get(8)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    const JOB_COLS: &'static str = "SELECT id, kind, params, every_sec, next_run, active, runs, last_run, last_result FROM jobs";

    /// All jobs when `active_only` is false, newest last.
    pub fn jobs(&self, active_only: bool) -> Res<Vec<Job>> {
        let sql = format!("{} WHERE active >= ?1 ORDER BY id", Self::JOB_COLS);
        self.query_jobs(&sql, i64::from(active_only))
    }

    pub fn job(&self, id: i64) -> Res<Option<Job>> {
        let sql = format!("{} WHERE id = ?1", Self::JOB_COLS);
        Ok(self.query_jobs(&sql, id)?.into_iter().next())
    }

    pub fn due(&self, at: i64) -> Res<Vec<Job>> {
        let sql = format!("{} WHERE active = 1 AND next_run <= ?1 ORDER BY next_run, id", Self::JOB_COLS);
        self.query_jobs(&sql, at)
    }

    pub fn cancel(&self, id: i64) -> Res<bool> {
        let n = self
            .db()
            .execute("UPDATE jobs SET active = 0 WHERE id = ?1 AND active = 1", [id])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// Record a run: periodic jobs move to `next`, one-shot ones (`next`
    /// = `None`) are deactivated.
    pub fn finish(&self, id: i64, at: i64, result: &str, next: Option<i64>) -> Res<()> {
        self.db()
            .execute(
                "UPDATE jobs SET runs = runs + 1, last_run = ?2, last_result = ?3,
                     next_run = COALESCE(?4, next_run), active = CASE WHEN ?4 IS NULL THEN 0 ELSE active END
                 WHERE id = ?1",
                params![id, at, result, next],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn add_sample(&self, s: &Sample) -> Res<()> {
        self.db()
            .execute(
                "INSERT INTO samples (ts, city, temp, feels, humidity, wind, code, observed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![s.ts, s.city, s.temp, s.feels, s.humidity, s.wind, s.code, s.observed],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn samples(&self, city: &str, since: i64) -> Res<Vec<Sample>> {
        let db = self.db();
        let mut st = db
            .prepare(
                "SELECT ts, city, temp, feels, humidity, wind, code, observed FROM samples
                 WHERE city = ?1 AND ts >= ?2 ORDER BY ts, id",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![city, since], |r| {
                Ok(Sample {
                    ts: r.get(0)?,
                    city: r.get(1)?,
                    temp: r.get(2)?,
                    feels: r.get(3)?,
                    humidity: r.get(4)?,
                    wind: r.get(5)?,
                    code: r.get(6)?,
                    observed: r.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    pub fn add_notification(&self, job_id: Option<i64>, kind: &str, text: &str, delivered: bool) -> Res<()> {
        self.db()
            .execute(
                "INSERT INTO notifications (ts, job_id, kind, text, delivered) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now(), job_id, kind, text, delivered],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Newest first: `(ts, kind, text, delivered)`.
    pub fn notifications(&self, limit: i64) -> Res<Vec<(i64, String, String, bool)>> {
        let db = self.db();
        let mut st = db
            .prepare("SELECT ts, kind, text, delivered FROM notifications ORDER BY id DESC LIMIT ?1")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    pub fn get(&self, key: &str) -> Res<Option<String>> {
        self.db()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn set(&self, key: &str, value: &str) -> Res<()> {
        self.db()
            .execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                [key, value],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn paused(&self) -> bool {
        matches!(self.get("paused"), Ok(Some(v)) if v == "1")
    }
}

// ---------------------------------------------------------------- weather

pub fn describe_code(code: i64) -> &'static str {
    match code {
        0 => "ясно",
        1 => "преимущественно ясно",
        2 => "переменная облачность",
        3 => "пасмурно",
        45 | 48 => "туман",
        51..=57 => "морось",
        61..=67 => "дождь",
        71..=77 => "снег",
        80..=82 => "ливень",
        85 | 86 => "снегопад",
        95..=99 => "гроза",
        _ => "?",
    }
}

/// Current weather from open-meteo (free, no key).
pub fn fetch_weather(city: &str, lat: f64, lon: f64) -> Res<Sample> {
    let resp = http().get("https://api.open-meteo.com/v1/forecast")
        .query("latitude", &lat.to_string())
        .query("longitude", &lon.to_string())
        .query(
            "current",
            "temperature_2m,apparent_temperature,relative_humidity_2m,wind_speed_10m,weather_code",
        )
        .query("wind_speed_unit", "ms")
        .query("timezone", "Europe/Moscow")
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| format!("open-meteo: {e}"))?;
    let v: Value = resp.into_json().map_err(|e| e.to_string())?;
    let c = &v["current"];
    let num = |k: &str| c[k].as_f64().ok_or_else(|| format!("open-meteo: no `{k}` in {c}"));
    Ok(Sample {
        ts: now(),
        city: city.to_string(),
        temp: num("temperature_2m")?,
        feels: num("apparent_temperature")?,
        humidity: num("relative_humidity_2m")?,
        wind: num("wind_speed_10m")?,
        code: c["weather_code"].as_i64().unwrap_or(-1),
        observed: c["time"].as_str().unwrap_or("").to_string(),
    })
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The aggregated result: `(text, structuredContent)` over the samples of
/// one city. Pure, so tests and the proof compute it without a server.
pub fn aggregate(city: &str, window_min: i64, samples: &[Sample]) -> (String, Value) {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return (
            format!("{city}: за последние {window_min} мин замеров нет — сбор ещё не запускался или выключен"),
            json!({"city": city, "window_min": window_min, "samples": 0}),
        );
    };
    let n = samples.len() as f64;
    let temps: Vec<f64> = samples.iter().map(|s| s.temp).collect();
    let min = temps.iter().copied().fold(f64::INFINITY, f64::min);
    let max = temps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = round1(temps.iter().sum::<f64>() / n);
    let delta = round1(last.temp - first.temp);
    let hum = (samples.iter().map(|s| s.humidity).sum::<f64>() / n).round();
    let wind = samples.iter().map(|s| s.wind).fold(0.0, f64::max);
    let mut observed: Vec<&str> = samples.iter().map(|s| s.observed.as_str()).collect();
    observed.dedup();
    let desc = describe_code(last.code);
    let text = format!(
        "{city}, последние {window_min} мин: {} замеров ({} разных наблюдений источника), {}–{} МСК\n\
         температура: сейчас {:.1} °C (ощущается {:.1}), мин {min:.1}, макс {max:.1}, средняя {avg:.1}, изменение {delta:+.1}\n\
         влажность в среднем {hum:.0}%, ветер до {wind:.1} м/с, погода сейчас: {desc}",
        samples.len(),
        observed.len(),
        hhmm(first.ts),
        hhmm(last.ts),
        last.temp,
        last.feels,
    );
    let structured = json!({
        "city": city,
        "window_min": window_min,
        "samples": samples.len(),
        "observations": observed.len(),
        "from": fmt_time(first.ts),
        "to": fmt_time(last.ts),
        "temp": {"min": min, "max": max, "avg": avg, "first": first.temp, "last": last.temp, "delta": delta},
        "feels_last": last.feels,
        "humidity_avg": hum,
        "wind_max": wind,
        "weather_now": desc,
    });
    (text, structured)
}

// ---------------------------------------------------------------- MCP server

#[derive(Clone)]
pub struct Server {
    pub store: Arc<Store>,
    pub calls: CallLog,
    verbose: bool,
}

impl Server {
    pub fn new(store: Arc<Store>, verbose: bool) -> Server {
        Server { store, calls: Arc::default(), verbose }
    }

    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            let r = mcp_server::handle_connection(stream, "mcp-sched", self.verbose, &|m| self.handle(m));
            if let (Err(e), true) = (r, self.verbose) {
                eprintln!("[mcp-sched] connection error: {e}");
            }
        }
    }

    /// Bind 127.0.0.1:`port` (0 = any free port), serve on a background
    /// thread, return the endpoint URL.
    pub fn spawn(&self, port: u16) -> Res<String> {
        let listener =
            TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let server = self.clone();
        std::thread::spawn(move || server.serve(listener));
        Ok(format!("http://{addr}/mcp"))
    }

    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let info = ServerInfo {
            name: "ask-scheduler-mcp",
            instructions: "Scheduler with persistent storage (SQLite): reminders, periodic weather \
                           collection, periodic summaries, and aggregates over collected data."
                .into(),
        };
        mcp_server::dispatch(msg, &info, &tool_specs(), &self.calls, &|n, a| self.call(n, a))
    }

    fn call(&self, name: &str, args: &Value) -> Res<(String, Value)> {
        match name {
            "schedule_reminder" => {
                let text = args["text"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or("`text` (string) is required")?;
                let delay = opt_int(args, "delay_sec", 0, i64::MAX)?.unwrap_or(0);
                let every = opt_int(args, "every_sec", MIN_EVERY, i64::MAX)?;
                self.add(json!({"text": text}), "reminder", every, now() + delay)
            }
            "schedule_collect" => {
                let (city, lat, lon) = resolve_city(opt_city(args)?)?;
                let every = opt_int(args, "every_sec", MIN_EVERY, i64::MAX)?.unwrap_or(60);
                self.add(json!({"city": city, "lat": lat, "lon": lon}), "collect", Some(every), now())
            }
            "schedule_summary" => {
                let (city, ..) = resolve_city(opt_city(args)?)?;
                let every = opt_int(args, "every_sec", MIN_EVERY, i64::MAX)?.unwrap_or(60);
                let window = opt_int(args, "window_min", 1, 7 * 24 * 60)?.unwrap_or(60);
                self.add(json!({"city": city, "window_min": window}), "summary", Some(every), now() + every)
            }
            "list_jobs" => {
                let all = args["include_inactive"].as_bool().unwrap_or(false);
                let jobs = self.store.jobs(!all)?;
                let text = if jobs.is_empty() {
                    "заданий нет".to_string()
                } else {
                    jobs.iter().map(Job::describe).collect::<Vec<_>>().join("\n")
                };
                let paused = self.store.paused();
                let text = if paused { format!("(планировщик на паузе)\n{text}") } else { text };
                let list: Vec<Value> = jobs.iter().map(job_json).collect();
                Ok((text, json!({"paused": paused, "jobs": list})))
            }
            "cancel_job" => {
                let id = args["id"].as_i64().ok_or("`id` (integer) is required")?;
                if self.store.cancel(id)? {
                    Ok((format!("задание #{id} отменено"), json!({"id": id, "cancelled": true})))
                } else {
                    Err(format!("активного задания #{id} нет"))
                }
            }
            "collect_now" => {
                let (city, lat, lon) = resolve_city(opt_city(args)?)?;
                let s = fetch_weather(&city, lat, lon)?;
                self.store.add_sample(&s)?;
                Ok((
                    format!(
                        "{city}: {:.1} °C (ощущается {:.1}), влажность {:.0}%, ветер {:.1} м/с, {} (наблюдение {})",
                        s.temp, s.feels, s.humidity, s.wind, describe_code(s.code), s.observed
                    ),
                    sample_json(&s),
                ))
            }
            "weather_summary" => {
                let (city, ..) = resolve_city(opt_city(args)?)?;
                let window = opt_int(args, "window_min", 1, 7 * 24 * 60)?.unwrap_or(60);
                let samples = self.store.samples(&city, now() - window * 60)?;
                Ok(aggregate(&city, window, &samples))
            }
            "recent_notifications" => {
                let limit = opt_int(args, "limit", 1, 50)?.unwrap_or(10);
                let list = self.store.notifications(limit)?;
                let text = if list.is_empty() {
                    "уведомлений ещё не было".to_string()
                } else {
                    list.iter()
                        .map(|(ts, kind, text, ok)| {
                            let first = text.lines().next().unwrap_or("");
                            format!("{} [{kind}{}] {first}", hhmm_s(*ts), if *ok { "" } else { ", не доставлено" })
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let items: Vec<Value> = list
                    .iter()
                    .map(|(ts, k, t, ok)| json!({"at": fmt_time(*ts), "kind": k, "text": t, "delivered": ok}))
                    .collect();
                Ok((text, json!({"notifications": items})))
            }
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn add(&self, params: Value, kind: &str, every: Option<i64>, first: i64) -> Res<(String, Value)> {
        let id = self.store.add_job(kind, &params, every, first)?;
        let job = self.store.job(id)?.ok_or("job vanished")?;
        Ok((format!("создано: {}", job.describe()), job_json(&job)))
    }
}

fn job_json(j: &Job) -> Value {
    json!({
        "id": j.id, "kind": j.kind, "params": j.params, "every_sec": j.every,
        "next_run": fmt_time(j.next_run), "active": j.active, "runs": j.runs,
        "last_run": j.last_run.map(fmt_time), "last_result": j.last_result,
    })
}

fn sample_json(s: &Sample) -> Value {
    json!({
        "city": s.city, "at": fmt_time(s.ts), "temp": s.temp, "feels": s.feels,
        "humidity": s.humidity, "wind": s.wind, "weather": describe_code(s.code), "observed": s.observed,
    })
}

fn opt_city(args: &Value) -> Res<&str> {
    match &args["city"] {
        Value::Null => Ok(DEFAULT_CITY),
        Value::String(s) if s.trim().is_empty() => Ok(DEFAULT_CITY),
        Value::String(s) => Ok(s),
        _ => Err("`city` must be a string".into()),
    }
}

fn opt_int(args: &Value, key: &str, min: i64, max: i64) -> Res<Option<i64>> {
    match &args[key] {
        Value::Null => Ok(None),
        v => v
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be an integer ≥ {min}")),
    }
}

pub fn tool_specs() -> Vec<Value> {
    let city = json!({"type": "string", "description": "City name, default Липецк."});
    vec![
        json!({
            "name": "schedule_reminder",
            "description": "Schedule a reminder message to the user: once after `delay_sec` seconds, or periodically every `every_sec` seconds.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string", "description": "What to remind about."},
                "delay_sec": {"type": "integer", "minimum": 0, "description": "Seconds from now until the (first) reminder; default 0."},
                "every_sec": {"type": "integer", "minimum": MIN_EVERY, "description": "Repeat period in seconds; omit for a one-time reminder."},
            }, "required": ["text"], "additionalProperties": false},
        }),
        json!({
            "name": "schedule_collect",
            "description": "Start periodic collection of current weather for a city into the database.",
            "inputSchema": {"type": "object", "properties": {
                "city": city,
                "every_sec": {"type": "integer", "minimum": MIN_EVERY, "description": "Collection period in seconds, default 60."},
            }, "additionalProperties": false},
        }),
        json!({
            "name": "schedule_summary",
            "description": "Start a periodic weather summary for a city, sent to the user every `every_sec` seconds and built from collected data over the last `window_min` minutes.",
            "inputSchema": {"type": "object", "properties": {
                "city": city,
                "every_sec": {"type": "integer", "minimum": MIN_EVERY, "description": "Period in seconds, default 60."},
                "window_min": {"type": "integer", "minimum": 1, "description": "Aggregation window in minutes, default 60."},
            }, "additionalProperties": false},
        }),
        json!({
            "name": "list_jobs",
            "description": "List scheduled jobs (reminders, collection, summaries) with their period, next run and run count.",
            "inputSchema": {"type": "object", "properties": {
                "include_inactive": {"type": "boolean", "description": "Also list finished and cancelled jobs."},
            }, "additionalProperties": false},
        }),
        json!({
            "name": "cancel_job",
            "description": "Cancel a scheduled job by its id.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "integer", "description": "Job id from list_jobs."},
            }, "required": ["id"], "additionalProperties": false},
        }),
        json!({
            "name": "collect_now",
            "description": "Fetch current weather for a city right now, store it and return it.",
            "inputSchema": {"type": "object", "properties": {"city": city}, "additionalProperties": false},
        }),
        json!({
            "name": "weather_summary",
            "description": "Aggregate the collected weather samples of a city over the last `window_min` minutes: sample count, current, min, max, average and change of temperature, average humidity, max wind.",
            "inputSchema": {"type": "object", "properties": {
                "city": city,
                "window_min": {"type": "integer", "minimum": 1, "description": "Window in minutes, default 60."},
            }, "additionalProperties": false},
        }),
        json!({
            "name": "recent_notifications",
            "description": "The latest messages the scheduler sent to the user (reminders and summaries).",
            "inputSchema": {"type": "object", "properties": {
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Default 10."},
            }, "additionalProperties": false},
        }),
    ]
}

// ---------------------------------------------------------------- runner

pub type Notify = Box<dyn FnMut(Option<i64>, &str, &str) -> bool + Send>;
pub type Weather = Box<dyn FnMut(&str, f64, f64) -> Res<Sample> + Send>;
pub type Summarize = Box<dyn FnMut(&Job) -> Res<String> + Send>;

/// The scheduler loop. Side effects are injected, so the proof and the
/// tests drive it with a fake clock, fake weather and a captured outbox.
pub struct Runner {
    pub store: Arc<Store>,
    pub notify: Notify,
    pub weather: Weather,
    pub summarize: Summarize,
}

impl Runner {
    /// Run every job due at `at`; returns one log line per job.
    pub fn tick(&mut self, at: i64) -> Vec<String> {
        if self.store.paused() {
            return Vec::new();
        }
        let due = match self.store.due(at) {
            Ok(d) => d,
            Err(e) => return vec![format!("due: {e}")],
        };
        let mut log = Vec::new();
        for job in due {
            let result = self.run_job(&job);
            let line = match &result {
                Ok(s) => s.clone(),
                Err(e) => format!("ошибка: {e}"),
            };
            // A late tick does not replay missed periods: next run counts from now.
            let next = job.every.map(|e| at.max(job.next_run) + e);
            if let Err(e) = self.store.finish(job.id, at, &line, next) {
                log.push(format!("#{} finish: {e}", job.id));
            }
            log.push(format!("#{} {}: {}", job.id, job.kind, line.lines().next().unwrap_or("")));
        }
        log
    }

    fn run_job(&mut self, job: &Job) -> Res<String> {
        match job.kind.as_str() {
            "reminder" => {
                let text = format!("⏰ Напоминание: {}", job.params["text"].as_str().unwrap_or(""));
                let ok = (self.notify)(Some(job.id), "reminder", &text);
                Ok(if ok { "отправлено".into() } else { "сохранено, не доставлено".into() })
            }
            "collect" => {
                let p = &job.params;
                let city = p["city"].as_str().unwrap_or(DEFAULT_CITY);
                let s = (self.weather)(city, p["lat"].as_f64().unwrap_or(0.0), p["lon"].as_f64().unwrap_or(0.0))?;
                self.store.add_sample(&s)?;
                Ok(format!("{city} {:.1} °C ({})", s.temp, s.observed))
            }
            "summary" => {
                let text = (self.summarize)(job)?;
                let ok = (self.notify)(Some(job.id), "summary", &text);
                Ok(format!("{}{text}", if ok { "" } else { "(не доставлено) " }))
            }
            k => Err(format!("unknown job kind `{k}`")),
        }
    }
}

// ---------------------------------------------------------------- agent

pub fn system_prompt(at: i64) -> String {
    format!(
        "Ты — фоновый агент-планировщик: живёшь 24/7 на домашнем сервере и общаешься с владельцем \
         через Telegram. Сейчас {} МСК. У тебя есть MCP-сервер ask-scheduler-mcp: напоминания, \
         периодический сбор погоды, регулярные сводки и агрегаты по собранным данным (всё хранится \
         в SQLite). Всё, что касается расписания, напоминаний, погоды и собранных данных, делай через \
         инструменты и отвечай по их результатам, ничего не выдумывай. Интервалы в инструментах — в \
         секундах. Отвечай кратко, по-русски, простым текстом без Markdown.",
        fmt_time(at)
    )
}

/// The agent with the scheduler's tools: every question goes through
/// `mcp_agent::tool_loop` against the MCP server at `url`.
pub struct ChatAgent {
    pub ep: Endpoint,
    pub settings: Settings,
    pub url: String,
    /// Last turns of the Telegram conversation (user + assistant text).
    history: Vec<Value>,
}

pub struct Reply {
    pub text: String,
    pub steps: Vec<ToolStep>,
    pub model: String,
}

impl ChatAgent {
    pub fn new(ep: Endpoint, settings: Settings, url: String) -> ChatAgent {
        ChatAgent { ep, settings, url, history: Vec::new() }
    }

    /// One turn. `remember` = false for the scheduler's own summary prompts,
    /// which must not pile up in the chat history.
    pub fn ask(&mut self, question: &str, remember: bool) -> Res<Reply> {
        let mut conn = Connection::connect(&self.url)?;
        let functions: Vec<Value> = conn.list_tools()?.iter().map(mcp_agent::to_function).collect();
        let mut messages = if remember { self.history.clone() } else { Vec::new() };
        messages.push(json!({"role": "user", "content": question}));
        let (out, steps, _) = mcp_agent::tool_loop(
            &self.ep,
            &self.settings,
            &system_prompt(now()),
            messages,
            &mut conn,
            &functions,
            &mut |_| {},
        )?;
        let text = out.text().trim().to_string();
        if remember {
            self.history.push(json!({"role": "user", "content": question}));
            self.history.push(json!({"role": "assistant", "content": text}));
            let excess = self.history.len().saturating_sub(12);
            self.history.drain(..excess);
        }
        Ok(Reply { text, steps, model: out.model.unwrap_or_default() })
    }

    /// A `summary` job: the agent must fetch the aggregate over MCP. If it
    /// answered without calling `weather_summary`, the message says so and
    /// carries the tool's own text instead of an unfounded answer.
    pub fn summary(&mut self, store: &Store, job: &Job) -> Res<String> {
        let city = job.params["city"].as_str().unwrap_or(DEFAULT_CITY);
        let window = job.params["window_min"].as_i64().unwrap_or(60);
        let q = format!(
            "Сделай короткую сводку погоды в городе {city} за последние {window} минут по собранным \
             данным. Сначала вызови weather_summary (city=\"{city}\", window_min={window}), затем \
             напиши 2–4 строки: погода сейчас, диапазон и тренд температуры, влажность и ветер."
        );
        let head = format!("🌤 Сводка {} МСК", hhmm(now()));
        match self.ask(&q, false) {
            Ok(r) if r.steps.iter().any(|s| s.name == "weather_summary" && !s.is_error) && !r.text.is_empty() => {
                Ok(format!("{head}\n{}\n\n— {} · MCP: {}", r.text, r.model, step_names(&r.steps)))
            }
            other => {
                let why = match other {
                    Ok(_) => "агент не вызвал weather_summary".to_string(),
                    Err(e) => format!("агент недоступен: {e}"),
                };
                let samples = store.samples(city, now() - window * 60)?;
                Ok(format!("{head}\n{}\n\n— без модели ({why})", aggregate(city, window, &samples).0))
            }
        }
    }
}

fn step_names(steps: &[ToolStep]) -> String {
    steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
}

// ---------------------------------------------------------------- telegram

pub struct Telegram {
    base: String,
}

impl Telegram {
    pub fn new(token: &str) -> Telegram {
        Telegram { base: format!("https://api.telegram.org/bot{}", token.trim()) }
    }

    /// Long polling; the token is never part of an error message.
    pub fn updates(&self, offset: i64) -> Res<Vec<Value>> {
        let v: Value = http().get(&format!("{}/getUpdates", self.base))
            .query("offset", &offset.to_string())
            .query("timeout", "25")
            .timeout(Duration::from_secs(40))
            .call()
            .map_err(|e| format!("telegram getUpdates: {}", redact(&e.to_string())))?
            .into_json()
            .map_err(|e| e.to_string())?;
        Ok(v["result"].as_array().cloned().unwrap_or_default())
    }

    pub fn send(&self, chat: i64, text: &str) -> Res<()> {
        let text: String = text.chars().take(4000).collect();
        http().post(&format!("{}/sendMessage", self.base))
            .timeout(Duration::from_secs(20))
            .send_json(json!({"chat_id": chat, "text": text}))
            .map_err(|e| format!("telegram sendMessage: {}", redact(&e.to_string())))?;
        Ok(())
    }
}

fn redact(s: &str) -> String {
    match (s.find("/bot"), s.find("/getUpdates").or_else(|| s.find("/sendMessage"))) {
        (Some(a), Some(b)) if a < b => format!("{}/bot***{}", &s[..a], &s[b..]),
        _ => s.to_string(),
    }
}

// ---------------------------------------------------------------- daemon

pub struct DaemonOpts {
    pub db: std::path::PathBuf,
    pub port: u16,
    pub settings: Settings,
    pub telegram_token: Option<String>,
    pub owner: Option<i64>,
    /// HTTP proxy for weather and Telegram (see [`set_proxy`]).
    pub proxy: Option<String>,
}

/// First start on an empty database: the task's example — Lipetsk weather
/// collected every minute and summarized every minute.
pub fn seed(store: &Store) -> Res<bool> {
    if !store.jobs(false)?.is_empty() {
        return Ok(false);
    }
    let (city, lat, lon) = resolve_city(DEFAULT_CITY)?;
    let t = now();
    store.add_job("collect", &json!({"city": city, "lat": lat, "lon": lon}), Some(60), t)?;
    store.add_job("summary", &json!({"city": city, "window_min": 60}), Some(60), t + 5)?;
    Ok(true)
}

fn log(line: &str) {
    println!("[{}] {line}", fmt_time(now()));
}

pub fn run_daemon(opts: DaemonOpts) -> Res<()> {
    if let Some(p) = &opts.proxy {
        set_proxy(p)?;
    }
    let store = Arc::new(Store::open(&opts.db)?);
    if seed(&store)? {
        log("пустая база: добавлены сбор погоды (Липецк, 60 с) и сводка (60 с)");
    }
    if let Some(chat) = opts.owner {
        store.set("tg_owner", &chat.to_string())?;
    }
    let server = Server::new(store.clone(), false);
    let url = server.spawn(opts.port)?;
    let ep = Endpoint::for_model(&opts.settings.model)?;
    log(&format!(
        "scheduler: база {}, MCP {url}, модель {} ({}), telegram: {}, прокси погоды/telegram: {}",
        opts.db.display(),
        opts.settings.model,
        ep.base_url,
        if opts.telegram_token.is_some() { "да" } else { "нет" },
        opts.proxy.as_deref().unwrap_or("нет")
    ));
    for j in store.jobs(true)? {
        log(&format!("  {}", j.describe()));
    }

    let tg = opts.telegram_token.as_deref().map(Telegram::new).map(Arc::new);
    let agent = Arc::new(Mutex::new(ChatAgent::new(ep, opts.settings.clone(), url)));

    let notify: Notify = {
        let (store, tg) = (store.clone(), tg.clone());
        Box::new(move |job, kind, text| {
            let owner = store.get("tg_owner").ok().flatten().and_then(|s| s.parse::<i64>().ok());
            let delivered = match (&tg, owner) {
                (Some(tg), Some(chat)) => match tg.send(chat, text) {
                    Ok(()) => true,
                    Err(e) => {
                        log(&e);
                        false
                    }
                },
                _ => false,
            };
            if let Err(e) = store.add_notification(job, kind, text, delivered) {
                log(&format!("notification: {e}"));
            }
            delivered
        })
    };
    let summarize: Summarize = {
        let (store, agent) = (store.clone(), agent.clone());
        Box::new(move |job| agent.lock().map_err(|e| e.to_string())?.summary(&store, job))
    };
    let mut runner = Runner { store: store.clone(), notify, weather: Box::new(fetch_weather), summarize };
    std::thread::spawn(move || loop {
        for line in runner.tick(now()) {
            log(&line);
        }
        std::thread::sleep(Duration::from_secs(1));
    });

    match tg {
        Some(tg) => telegram_loop(&tg, &store, &agent),
        None => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
    }
}

const HELP: &str = "Я фоновый агент-планировщик на домашнем сервере.\n\
    Пишите обычным текстом: «напомни через 10 минут выпить чай», «какие задания запущены?», \
    «какая была погода в Липецке за последний час?», «собирай погоду в Москве каждые 5 минут».\n\
    Команды: /jobs — задания, /summary — сводка сейчас (без модели), /pause и /resume — \
    остановить и продолжить расписание, /help.";

fn telegram_loop(tg: &Telegram, store: &Arc<Store>, agent: &Arc<Mutex<ChatAgent>>) -> ! {
    let tools = Server::new(store.clone(), false);
    let mut offset = store.get("tg_offset").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut last_err = String::new();
    loop {
        let updates = match tg.updates(offset) {
            Ok(u) => {
                if !last_err.is_empty() {
                    log("telegram: приём сообщений восстановлен");
                    last_err.clear();
                }
                u
            }
            Err(e) => {
                // 409: another process polls the same bot token. Sending still
                // works, so notifications keep going; only incoming chat stops.
                let conflict = e.contains("409");
                if e != last_err {
                    log(&if conflict {
                        format!("{e} — этот токен уже опрашивает другой процесс; уведомления отправляются, входящие не принимаются")
                    } else {
                        e.clone()
                    });
                    last_err = e;
                }
                std::thread::sleep(Duration::from_secs(if conflict { 60 } else { 5 }));
                continue;
            }
        };
        for u in updates {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let _ = store.set("tg_offset", &offset.to_string());
            let msg = &u["message"];
            let (Some(chat), Some(text)) = (msg["chat"]["id"].as_i64(), msg["text"].as_str()) else {
                continue;
            };
            let owner = store.get("tg_owner").ok().flatten().and_then(|s| s.parse::<i64>().ok());
            let reply = match owner {
                None if text.starts_with("/start") => {
                    let _ = store.set("tg_owner", &chat.to_string());
                    log(&format!("telegram: владелец — чат {chat}"));
                    format!("Готово, этот чат — владелец.\n\n{HELP}")
                }
                None => "Отправьте /start, чтобы привязать этот чат.".to_string(),
                Some(o) if o != chat => "Это приватный бот.".to_string(),
                Some(_) => handle_owner(text, store, &tools, agent),
            };
            if let Err(e) = tg.send(chat, &reply) {
                log(&e);
            }
        }
    }
}

fn handle_owner(text: &str, store: &Store, tools: &Server, agent: &Arc<Mutex<ChatAgent>>) -> String {
    let direct = |name: &str, args: Value| match tools.call(name, &args) {
        Ok((t, _)) => t,
        Err(e) => format!("ошибка: {e}"),
    };
    match text.split_whitespace().next().unwrap_or("") {
        "/start" | "/help" => HELP.to_string(),
        "/jobs" => direct("list_jobs", json!({})),
        "/summary" => direct("weather_summary", json!({})),
        "/pause" => {
            let _ = store.set("paused", "1");
            "Расписание на паузе. /resume — продолжить.".into()
        }
        "/resume" => {
            let _ = store.set("paused", "0");
            "Расписание снова работает.".into()
        }
        _ => {
            log(&format!("telegram ← {text}"));
            let r = agent.lock().map_err(|e| e.to_string()).and_then(|mut a| a.ask(text, true));
            match r {
                Ok(r) => {
                    let calls = step_names(&r.steps);
                    log(&format!("telegram → {} (MCP: {calls})", r.text.lines().next().unwrap_or("")));
                    if calls.is_empty() {
                        r.text
                    } else {
                        format!("{}\n\n— MCP: {calls}", r.text)
                    }
                }
                Err(e) => format!("Агент недоступен: {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------- proof

/// `ask --verify-scheduler`. Three causal checks:
/// 1. `schedule` (no network): a periodic job on a fake clock runs exactly
///    at its due times, stores its data in SQLite, and a one-shot job fires
///    once and deactivates.
/// 2. `aggregate`: samples with random temperatures are stored; the agent
///    must call `weather_summary` over MCP and quote min / max / average that
///    exist only in the database, while the same question without tools
///    must not produce them.
/// 3. `reminder`: asked in plain language to remind a random codeword in a
///    few seconds, the agent must create the job through MCP, and the runner
///    must deliver the codeword when (and only when) it is due.
pub fn verify(settings: &Settings) -> Res<bool> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    let mut rng = stamp ^ u64::from(std::process::id()).rotate_left(32);
    let mut next = move |m: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % m
    };

    // 1. schedule, offline
    let store = Arc::new(Store::in_memory()?);
    let outbox: Arc<Mutex<Vec<String>>> = Arc::default();
    let t0 = 1_000_000;
    let collect = store.add_job("collect", &json!({"city": "Липецк", "lat": 0, "lon": 0}), Some(60), t0)?;
    let once = store.add_job("reminder", &json!({"text": "once"}), None, t0 + 90)?;
    let mut runner = fake_runner(&store, &outbox);
    let mut ran = Vec::new();
    for t in (t0..=t0 + 180).step_by(10) {
        if !runner.tick(t).is_empty() {
            ran.push(t - t0);
        }
    }
    let samples = store.samples("Липецк", 0)?.len();
    let c = store.job(collect)?.ok_or("collect job lost")?;
    let o = store.job(once)?.ok_or("reminder job lost")?;
    let sched_ok = ran == [0, 60, 90, 120, 180] && samples == 4 && c.active && c.runs == 4 && !o.active && o.runs == 1
        && outbox.lock().map_err(|e| e.to_string())?.len() == 1;
    println!(
        "[{}] schedule (без сети): запуски на секундах {ran:?}, замеров в SQLite: {samples}, collect: runs={} active={}, \
         разовое напоминание: runs={} active={}",
        mark(sched_ok),
        c.runs,
        c.active,
        o.runs,
        o.active
    );

    // 2. aggregate through the agent
    let store = Arc::new(Store::in_memory()?);
    let t = now();
    let temps: Vec<f64> = (0..6).map(|_| round1((next(300) as f64) / 10.0 - 5.0)).collect();
    for (i, temp) in temps.iter().enumerate() {
        store.add_sample(&Sample {
            ts: t - 50 * 60 + i as i64 * 600,
            city: "Липецк".into(),
            temp: *temp,
            feels: temp - 2.0,
            humidity: 60.0 + i as f64,
            wind: 3.0,
            code: 3,
            observed: format!("obs-{i}"),
        })?;
    }
    let min = temps.iter().copied().fold(f64::INFINITY, f64::min);
    let max = temps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = round1(temps.iter().sum::<f64>() / 6.0);
    println!("замеры-фикстура (случайные): {temps:?} → мин {min:.1}, макс {max:.1}, средняя {avg:.1}");
    let server = Server::new(store.clone(), false);
    let url = server.spawn(0)?;
    let ep = Endpoint::for_model(&settings.model)?;
    let mut agent = ChatAgent::new(ep.clone(), settings.clone(), url);
    let q = "Какая была минимальная, максимальная и средняя температура в Липецке за последний час по собранным данным? Назови три числа.";
    let r = agent.ask(q, false)?;
    // Models write negatives with U+2212 "−"; the number is the same.
    let has = |text: &str, x: f64| {
        let text = text.replace('\u{2212}', "-");
        let a = format!("{x:.1}");
        text.contains(&a) || text.contains(&a.replace('.', ","))
    };
    let hits = |text: &str| [min, max, avg].iter().filter(|x| has(text, **x)).count();
    let tool_used = r.steps.iter().any(|s| s.name == "weather_summary" && !s.is_error);
    let agg_ok = tool_used && hits(&r.text) == 3;
    println!(
        "[{}] aggregate: модель {} ({}), вызовы MCP [{}], чисел из базы в ответе: {}/3",
        mark(agg_ok),
        r.model,
        ep.base_url,
        step_names(&r.steps),
        hits(&r.text)
    );
    println!("    ответ: {}", one_line(&r.text));
    let control = crate::api::chat_with_tools(
        &ep,
        settings,
        &system_prompt(now()),
        &[json!({"role": "user", "content": q})],
        &[],
    )?;
    let leak = hits(control.text()) == 3;
    println!(
        "[{}] контроль без инструментов: чисел из базы в ответе: {}/3 (finish_reason {}, {}→{} токенов)",
        mark(!leak),
        hits(control.text()),
        control.finish_reason.as_deref().unwrap_or("?"),
        control.usage.prompt_tokens,
        control.usage.completion_tokens
    );

    // 3. reminder: natural language → MCP job → delivered when due
    let code = format!("код-{:06}", next(1_000_000));
    let q = format!("Напомни мне через 3 секунды: «{code}».");
    let r = agent.ask(&q, false)?;
    let job = store
        .jobs(true)?
        .into_iter()
        .find(|j| j.kind == "reminder" && j.params["text"].as_str().unwrap_or("").contains(&code));
    let outbox: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut runner = fake_runner(&store, &outbox);
    let (early, late) = match &job {
        Some(j) => (runner.tick(j.next_run - 1).len(), runner.tick(j.next_run).len()),
        None => (0, 0),
    };
    let delivered = outbox.lock().map_err(|e| e.to_string())?.iter().any(|m| m.contains(&code));
    let rem_ok = job.is_some() && early == 0 && late == 1 && delivered;
    println!(
        "[{}] reminder: вызовы MCP [{}], задание в SQLite: {}, до срока запусков {early}, в срок {late}, код доставлен: {delivered}",
        mark(rem_ok),
        step_names(&r.steps),
        job.as_ref().map(Job::describe).unwrap_or_else(|| "нет".into()),
    );

    let confirmed = sched_ok && agg_ok && !leak && rem_ok;
    println!(
        "\nScheduler: {}",
        if confirmed {
            "Confirmed — расписание исполняется, данные в SQLite, агент берёт агрегат и ставит задания через MCP"
        } else {
            "Flat — см. строки FAIL выше"
        }
    );
    Ok(confirmed)
}

fn fake_runner(store: &Arc<Store>, outbox: &Arc<Mutex<Vec<String>>>) -> Runner {
    let out = outbox.clone();
    Runner {
        store: store.clone(),
        notify: Box::new(move |_, _, text| {
            out.lock().map(|mut o| o.push(text.to_string())).is_ok()
        }),
        weather: Box::new(|city, _, _| {
            Ok(Sample {
                ts: 0,
                city: city.into(),
                temp: 10.0,
                feels: 8.0,
                humidity: 70.0,
                wind: 2.0,
                code: 0,
                observed: "fake".into(),
            })
        }),
        summarize: Box::new(|_| Ok("fake summary".into())),
    }
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "FAIL"
    }
}

fn one_line(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64, temp: f64, observed: &str) -> Sample {
        Sample {
            ts,
            city: "Липецк".into(),
            temp,
            feels: temp - 1.0,
            humidity: 50.0,
            wind: 1.5,
            code: 3,
            observed: observed.into(),
        }
    }

    #[test]
    fn msk_time_formatting() {
        assert_eq!(fmt_time(0), "1970-01-01 03:00:00");
        assert_eq!(fmt_time(1_790_499_398), "2026-09-27 11:56:38");
    }

    #[test]
    fn aggregate_counts_and_stats() {
        let s = [sample(100, 10.0, "a"), sample(160, 12.0, "a"), sample(220, 14.5, "b")];
        let (text, v) = aggregate("Липецк", 60, &s);
        assert_eq!(v["samples"], 3);
        assert_eq!(v["observations"], 2);
        assert_eq!(v["temp"]["min"], 10.0);
        assert_eq!(v["temp"]["max"], 14.5);
        assert_eq!(v["temp"]["avg"], 12.2);
        assert_eq!(v["temp"]["delta"], 4.5);
        assert!(text.contains("средняя 12.2") && text.contains("+4.5"), "{text}");
        assert_eq!(aggregate("Липецк", 60, &[]).1["samples"], 0);
    }

    #[test]
    fn runner_reschedules_periodic_and_retires_one_shot() {
        let store = Arc::new(Store::in_memory().unwrap());
        let outbox: Arc<Mutex<Vec<String>>> = Arc::default();
        let p = store.add_job("collect", &json!({"city": "Липецк"}), Some(60), 1000).unwrap();
        let o = store.add_job("reminder", &json!({"text": "чай"}), None, 1030).unwrap();
        let mut r = fake_runner(&store, &outbox);
        assert_eq!(r.tick(999).len(), 0);
        assert_eq!(r.tick(1000).len(), 1);
        assert_eq!(r.tick(1030).len(), 1);
        assert_eq!(r.tick(1059).len(), 0);
        // late tick: one run, no replay of missed periods
        assert_eq!(r.tick(1200).len(), 1);
        assert_eq!(store.job(p).unwrap().unwrap().next_run, 1260);
        assert!(!store.job(o).unwrap().unwrap().active);
        assert_eq!(store.samples("Липецк", 0).unwrap().len(), 2);
        assert_eq!(outbox.lock().unwrap().as_slice(), ["⏰ Напоминание: чай"]);
        store.set("paused", "1").unwrap();
        assert!(r.tick(5000).is_empty());
    }

    #[test]
    fn mcp_tools_over_http() {
        let store = Arc::new(Store::in_memory().unwrap());
        let s = Server::new(store.clone(), false);
        let url = s.spawn(0).unwrap();
        let mut conn = Connection::connect(&url).unwrap();
        assert_eq!(conn.server_name, "ask-scheduler-mcp");
        assert_eq!(conn.list_tools().unwrap().len(), 8);

        let r = conn.call_tool("schedule_reminder", json!({"text": "позвонить", "delay_sec": 30})).unwrap();
        assert!(!r.is_error && r.text.contains("позвонить"), "{}", r.text);
        let r = conn.call_tool("schedule_reminder", json!({"text": "x", "every_sec": 1})).unwrap();
        assert!(r.is_error, "period below MIN_EVERY must be refused");
        let r = conn.call_tool("schedule_summary", json!({"every_sec": 60})).unwrap();
        assert!(!r.is_error && r.text.contains("Липецк"), "{}", r.text);
        let r = conn.call_tool("list_jobs", json!({})).unwrap();
        assert_eq!(r.text.lines().count(), 2, "{}", r.text);
        let r = conn.call_tool("cancel_job", json!({"id": 1})).unwrap();
        assert!(!r.is_error);
        let r = conn.call_tool("cancel_job", json!({"id": 1})).unwrap();
        assert!(r.is_error);

        let t = now();
        store.add_sample(&sample(t - 120, 5.0, "a")).unwrap();
        store.add_sample(&sample(t - 60, 7.0, "b")).unwrap();
        store.add_sample(&sample(t - 7200, 30.0, "old")).unwrap();
        let r = conn.call_tool("weather_summary", json!({"window_min": 60})).unwrap();
        assert!(r.text.contains("2 замеров") && r.text.contains("средняя 6.0"), "{}", r.text);
        assert_eq!(s.calls.lock().unwrap().len(), 7);
    }
}
