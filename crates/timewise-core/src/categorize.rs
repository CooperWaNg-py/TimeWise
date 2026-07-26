//! Layered app categorization (application-design §7.2).
//! Precedence: parent override (exact app name) -> regex rules (app name or window
//! title, first match wins) -> bundled static table -> Other.

use crate::model::{Category, CategoryOverride, CategoryRule};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;

const BUNDLED_JSON: &str = include_str!("categories.json");

#[derive(Deserialize)]
struct Bundled {
    #[serde(rename = "static")]
    static_table: HashMap<String, Category>,
    rules: Vec<CategoryRule>,
}

/// Normalize an app name for lookup: lowercase, strip a trailing ".exe".
fn normalize(app_name: &str) -> String {
    let lower = app_name.trim().to_lowercase();
    lower.strip_suffix(".exe").map(str::to_owned).unwrap_or(lower)
}

pub struct Categorizer {
    static_table: HashMap<String, Category>,
    regex_rules: Vec<(Regex, Category)>,
    overrides: HashMap<String, Category>,
}

impl Categorizer {
    /// Bundled static table + bundled regex rules, no overrides.
    pub fn from_bundled() -> Self {
        let bundled: Bundled =
            serde_json::from_str(BUNDLED_JSON).expect("categories.json must be valid");
        let static_table = bundled
            .static_table
            .into_iter()
            .map(|(k, v)| (normalize(&k), v))
            .collect();
        let regex_rules = compile_rules(&bundled.rules);
        Categorizer { static_table, regex_rules, overrides: HashMap::new() }
    }

    /// Replace the regex rule set (e.g. rules pushed from the master).
    pub fn with_rules(mut self, rules: &[CategoryRule]) -> Self {
        self.regex_rules = compile_rules(rules);
        self
    }

    /// Replace parent overrides (Layer 3).
    pub fn with_overrides(mut self, overrides: &[CategoryOverride]) -> Self {
        self.overrides = overrides
            .iter()
            .map(|o| (normalize(&o.app_name), o.category))
            .collect();
        self
    }

    pub fn categorize(&self, app_name: &str, window_title: &str) -> Category {
        let key = normalize(app_name);
        // Layer 3: parent override, exact app name.
        if let Some(c) = self.overrides.get(&key) {
            return *c;
        }
        // Layer 2: regex rules, first match wins, app name then window title.
        for (re, cat) in &self.regex_rules {
            if re.is_match(app_name) || re.is_match(window_title) {
                return *cat;
            }
        }
        // Layer 1: static table.
        if let Some(c) = self.static_table.get(&key) {
            return *c;
        }
        Category::Other
    }
}

/// Compile rules, skipping invalid patterns (BR5). Bundled rules are validated
/// by tests, so a runtime skip here can only affect master-pushed rules.
fn compile_rules(rules: &[CategoryRule]) -> Vec<(Regex, Category)> {
    rules
        .iter()
        .filter_map(|r| Regex::new(&r.pattern).ok().map(|re| (re, r.category)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_json_valid_and_nonempty() {
        let bundled: Bundled = serde_json::from_str(BUNDLED_JSON).unwrap();
        assert!(bundled.static_table.len() >= 100, "seed table should have ~100 apps");
        assert!(!bundled.rules.is_empty());
        for r in &bundled.rules {
            assert!(Regex::new(&r.pattern).is_ok(), "invalid bundled regex: {}", r.pattern);
        }
    }

    #[test]
    fn precedence_override_beats_regex_beats_static() {
        let c = Categorizer::from_bundled();
        // Static hit
        assert_eq!(c.categorize("Roblox", "Roblox"), Category::Games);
        // Regex hit on a non-static app
        assert_eq!(c.categorize("Some Browser Fork", "YouTube - lo-fi beats"), Category::Entertainment);
        // Override beats both
        let c = c.with_overrides(&[CategoryOverride {
            app_name: "Roblox".into(),
            category: Category::Educational,
        }]);
        assert_eq!(c.categorize("Roblox", "Roblox"), Category::Educational);
    }

    #[test]
    fn normalize_handles_exe_and_case() {
        let c = Categorizer::from_bundled();
        assert_eq!(c.categorize("CHROME.EXE", "New Tab"), Category::Browsers);
        assert_eq!(c.categorize("minecraft", ""), Category::Games);
    }

    #[test]
    fn unknown_app_is_other() {
        let c = Categorizer::from_bundled();
        assert_eq!(c.categorize("Totally Unknown App", "nothing"), Category::Other);
    }

    #[test]
    fn first_matching_rule_wins() {
        let c = Categorizer::from_bundled().with_rules(&[
            CategoryRule { pattern: "foo".into(), category: Category::Games },
            CategoryRule { pattern: "foo".into(), category: Category::Other },
        ]);
        assert_eq!(c.categorize("foo app", ""), Category::Games);
    }

    #[test]
    fn invalid_pushed_rule_is_skipped_not_fatal() {
        let c = Categorizer::from_bundled().with_rules(&[
            CategoryRule { pattern: "([invalid".into(), category: Category::Games },
            CategoryRule { pattern: "bar".into(), category: Category::Productivity },
        ]);
        assert_eq!(c.categorize("bar", ""), Category::Productivity);
    }
}
