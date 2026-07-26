//! Wire protocol types and DTOs shared by TimeWise master and worker roles.
//! All timestamps on the wire are UTC epoch seconds.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Application categories (PRD §4.3). Serialized as display strings ("Social Media").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Games,
    Educational,
    Entertainment,
    SocialMedia,
    Productivity,
    Browsers,
    Other,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Games,
        Category::Educational,
        Category::Entertainment,
        Category::SocialMedia,
        Category::Productivity,
        Category::Browsers,
        Category::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Games => "Games",
            Category::Educational => "Educational",
            Category::Entertainment => "Entertainment",
            Category::SocialMedia => "Social Media",
            Category::Productivity => "Productivity",
            Category::Browsers => "Browsers",
            Category::Other => "Other",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Category {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Games" => Category::Games,
            "Educational" => Category::Educational,
            "Entertainment" => Category::Entertainment,
            "Social Media" | "SocialMedia" => Category::SocialMedia,
            "Productivity" => Category::Productivity,
            "Browsers" => Category::Browsers,
            "Other" => Category::Other,
            _ => return Err(()),
        })
    }
}

impl Serialize for Category {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Category {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Category::from_str(&s).map_err(|_| serde::de::Error::custom(format!("unknown category: {s}")))
    }
}

/// One focused-window session. `id` is a client-generated UUID: uploads are
/// idempotent across retries and across multiple masters (BR1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub app_name: String,
    pub window_title: String,
    pub category: Category,
    pub start_ts: i64,
    pub end_ts: i64,
    pub duration_s: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchUpload {
    pub sessions: Vec<SessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchAccepted {
    pub accepted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub worker_id: String,
    pub hostname: String,
    pub os: String,
    pub os_user: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationStatus {
    Pending,
    Approved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub status: RegistrationStatus,
    pub child_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub server_time: i64,
}

/// Regex categorization rule distributed to workers (Layer 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryRule {
    pub pattern: String,
    pub category: Category,
}

/// Parent override, exact app name match (Layer 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryOverride {
    pub app_name: String,
    pub category: Category,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalConfig {
    pub daily_min: Option<u32>,
    pub weekly_min: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thresholds {
    pub nudge_pct: u32,
    pub limit_pct: u32,
    pub over_pct: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { nudge_pct: 90, limit_pct: 100, over_pct: 110 }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub today_s: i64,
    pub week_s: i64,
}

/// Pulled by the worker each sync cycle (application-design §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub rules: Vec<CategoryRule>,
    pub overrides: Vec<CategoryOverride>,
    pub goal: GoalConfig,
    pub thresholds: Thresholds,
    pub usage: UsageTotals,
    pub break_prompt_after_min: u32,
}

/// Time-of-day bucket (BR4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TodBucket {
    Morning,
    Afternoon,
    Evening,
    Night,
}

impl TodBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            TodBucket::Morning => "morning",
            TodBucket::Afternoon => "afternoon",
            TodBucket::Evening => "evening",
            TodBucket::Night => "night",
        }
    }
}

// ---- Dashboard DTOs ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSummary {
    pub worker_id: String,
    pub child_name: Option<String>,
    pub approved: bool,
    pub online: bool,
    pub last_seen: Option<i64>,
    pub today_s: i64,
    pub week_s: i64,
    pub points_balance: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppBreakdown {
    pub app_name: String,
    pub category: Category,
    pub duration_s: i64,
    pub pct: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TodDistribution {
    pub morning_s: i64,
    pub afternoon_s: i64,
    pub evening_s: i64,
    pub night_s: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointsEntry {
    pub date: String,
    pub points: i64,
    pub reason: String,
}

/// Master-side worker registry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub hostname: String,
    pub os: String,
    pub os_user: String,
    pub child_name: Option<String>,
    pub approved: bool,
    pub last_seen: Option<i64>,
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_roundtrip() {
        for c in Category::ALL {
            let s = c.as_str();
            assert_eq!(Category::from_str(s).unwrap(), c);
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Category>(&json).unwrap(), c);
        }
        assert_eq!(Category::from_str("SocialMedia").unwrap(), Category::SocialMedia);
        assert!(Category::from_str("Nope").is_err());
    }

    #[test]
    fn session_record_wire_shape() {
        let r = SessionRecord {
            id: "u1".into(),
            app_name: "Roblox".into(),
            window_title: "Roblox".into(),
            category: Category::Games,
            start_ts: 1000,
            end_ts: 1060,
            duration_s: 60,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["category"], "Games");
        assert_eq!(json["duration_s"], 60);
        let back: SessionRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }
}
