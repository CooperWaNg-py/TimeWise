//! Notification engines: warning escalation ladder (FR8, BR9) and screen-break
//! prompts (FR9, BR10). Pure state machines; the `Notifier` trait is
//! implemented by the Tauri shell (Unit 3) with OS-native notifications.

use timewise_core::model::{GoalConfig, Thresholds};

pub trait Notifier: Send {
    fn notify(&mut self, title: &str, body: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WarningTier {
    Nudge,
    Limit,
    Over,
}

/// Fires positively-framed, non-blocking notifications as daily usage crosses
/// goal thresholds. Each tier fires at most once per local day.
pub struct WarningLadder {
    thresholds: Thresholds,
    /// (date, tier) pairs already fired.
    fired: std::collections::HashSet<(String, WarningTier)>,
}

impl WarningLadder {
    pub fn new(thresholds: Thresholds) -> Self {
        WarningLadder { thresholds, fired: std::collections::HashSet::new() }
    }

    /// `date` is the local date string (resets daily), `usage_s` includes the
    /// in-progress session (BR9). No-op without a daily goal.
    pub fn evaluate(
        &mut self,
        date: &str,
        usage_s: i64,
        goal: &GoalConfig,
        notifier: &mut dyn Notifier,
    ) {
        let Some(daily_min) = goal.daily_min else { return };
        let goal_s = (daily_min as i64) * 60;
        if goal_s <= 0 {
            return;
        }
        let pct = usage_s * 100 / goal_s;
        let tiers = [
            (self.thresholds.nudge_pct, WarningTier::Nudge, "Almost there", "Great job staying close to your goal!"),
            (self.thresholds.limit_pct, WarningTier::Limit, "Goal reached", "You've hit your screen time goal for today - nice self-awareness!"),
            (self.thresholds.over_pct, WarningTier::Over, "A little over", "You've been at it a while - maybe take a stretch break?"),
        ];
        for (threshold, tier, title, body) in tiers {
            if pct >= threshold as i64 && self.fired.insert((date.to_string(), tier)) {
                notifier.notify(title, body);
            }
        }
    }
}

/// Suggests a stretch/eye-rest after continuous usage (BR10). A gap of 5+
/// minutes without a focused window resets the counter.
pub struct BreakPrompt {
    threshold_s: i64,
    reset_gap_s: i64,
    continuous_start: Option<i64>,
    last_active: Option<i64>,
    prompted: bool,
}

impl BreakPrompt {
    pub fn new(threshold_s: i64) -> Self {
        BreakPrompt { threshold_s, reset_gap_s: 300, continuous_start: None, last_active: None, prompted: false }
    }

    /// `in_session` = a window is currently focused (tracked).
    pub fn tick(&mut self, now: i64, in_session: bool, notifier: &mut dyn Notifier) {
        if in_session {
            self.last_active = Some(now);
            let start = *self.continuous_start.get_or_insert(now);
            if !self.prompted && now - start >= self.threshold_s {
                notifier.notify(
                    "Time for a quick break?",
                    "You've been going for a while - stand up, stretch, rest your eyes!",
                );
                self.prompted = true;
            }
        } else if let Some(last) = self.last_active {
            if now - last >= self.reset_gap_s {
                self.continuous_start = None;
                self.last_active = None;
                self.prompted = false;
            }
        }
    }

    pub fn continuous_elapsed(&self, now: i64) -> i64 {
        self.continuous_start.map(|s| (now - s).max(0)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Spy(Vec<(String, String)>);
    impl Notifier for Spy {
        fn notify(&mut self, title: &str, body: &str) {
            self.0.push((title.to_string(), body.to_string()));
        }
    }

    fn goal(min: u32) -> GoalConfig {
        GoalConfig { daily_min: Some(min), weekly_min: None }
    }

    #[test]
    fn br9_tiers_fire_in_order_once_per_day() {
        let mut ladder = WarningLadder::new(Thresholds::default()); // 90/100/110
        let mut spy = Spy::default();
        let g = goal(60); // 3600s

        ladder.evaluate("2026-07-25", 3000, &g, &mut spy); // 83%: nothing
        assert!(spy.0.is_empty());
        ladder.evaluate("2026-07-25", 3300, &g, &mut spy); // 91%: nudge
        assert_eq!(spy.0.len(), 1);
        assert_eq!(spy.0[0].0, "Almost there");
        ladder.evaluate("2026-07-25", 3400, &g, &mut spy); // still nudge tier
        assert_eq!(spy.0.len(), 1);
        ladder.evaluate("2026-07-25", 3650, &g, &mut spy); // 101%: limit
        assert_eq!(spy.0.len(), 2);
        ladder.evaluate("2026-07-25", 4000, &g, &mut spy); // 111%: over
        assert_eq!(spy.0.len(), 3);
        ladder.evaluate("2026-07-25", 5000, &g, &mut spy); // no repeats
        assert_eq!(spy.0.len(), 3);
        // New day: tiers re-arm.
        ladder.evaluate("2026-07-26", 4000, &g, &mut spy);
        assert_eq!(spy.0.len(), 6);
    }

    #[test]
    fn no_daily_goal_no_notifications() {
        let mut ladder = WarningLadder::new(Thresholds::default());
        let mut spy = Spy::default();
        ladder.evaluate("2026-07-25", 999_999, &GoalConfig::default(), &mut spy);
        assert!(spy.0.is_empty());
    }

    #[test]
    fn br10_prompts_after_threshold_and_resets_after_gap() {
        let mut bp = BreakPrompt::new(2400); // 40 min
        let mut spy = Spy::default();

        bp.tick(1000, true, &mut spy); // session starts
        bp.tick(1000 + 2399, true, &mut spy);
        assert!(spy.0.is_empty());
        bp.tick(1000 + 2400, true, &mut spy); // threshold reached
        assert_eq!(spy.0.len(), 1);
        bp.tick(1000 + 3000, true, &mut spy); // no repeat while same streak
        assert_eq!(spy.0.len(), 1);

        // Short gap (< 5 min): streak continues, still no re-prompt.
        bp.tick(1000 + 3100, false, &mut spy);
        bp.tick(1000 + 3300, true, &mut spy);
        bp.tick(1000 + 5000, true, &mut spy);
        assert_eq!(spy.0.len(), 1);

        // Long gap: reset; a new streak can prompt again.
        bp.tick(1000 + 5300, false, &mut spy); // inactive since 5300
        bp.tick(1000 + 5601, false, &mut spy); // gap > 300 -> reset
        assert_eq!(bp.continuous_elapsed(1000 + 5601), 0);
        bp.tick(1000 + 5700, true, &mut spy);
        bp.tick(1000 + 5700 + 2400, true, &mut spy);
        assert_eq!(spy.0.len(), 2);
    }
}
