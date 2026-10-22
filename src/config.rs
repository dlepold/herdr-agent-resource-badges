// Thresholds and interval, read from the plugin's config.toml.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.
//
// Only flat `key = number` lines are understood, which keeps the binary free of a
// TOML dependency. Unknown keys and malformed lines are ignored, so a typo never
// stops the badges; the report pane shows the values actually in effect.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

pub const PLUGIN_ID: &str = "agent-resource-badges";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Hidden,
    Ok,
    Warn,
    Hot,
}

impl Level {
    /// The sidebar token this level is reported under; each one gets its own colour
    /// in herdr's config, because numeric `gt`/`lt` rules cannot match "42%".
    pub fn token(self) -> Option<&'static str> {
        match self {
            Level::Hidden => None,
            Level::Ok => Some("res_ok"),
            Level::Warn => Some("res_warn"),
            Level::Hot => Some("res_hot"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub interval_seconds: f64,
    pub cpu_show: f64,
    pub cpu_warn: f64,
    pub cpu_hot: f64,
    pub ram_show_mb: f64,
    pub ram_warn_mb: f64,
    pub ram_hot_mb: f64,
    /// false: badge shows only the metric that sets the colour ("22%" or "2.8G"),
    /// which fits herdr's narrow sidebar; true: both ("22% 2.8G").
    pub badge_both: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_seconds: 5.0,
            cpu_show: 10.0,
            cpu_warn: 50.0,
            cpu_hot: 100.0,
            ram_show_mb: 1536.0,
            ram_warn_mb: 2560.0,
            ram_hot_mb: 4096.0,
            badge_both: false,
        }
    }
}

impl Config {
    /// The worse of the CPU level and the RAM level decides the colour.
    pub fn level(&self, cpu_pct: f64, rss_bytes: u64) -> Level {
        let (cpu, ram) = self.levels(cpu_pct, rss_bytes);
        cpu.max(ram)
    }

    /// CPU level and RAM level separately, so the badge can name the one that counts.
    pub fn levels(&self, cpu_pct: f64, rss_bytes: u64) -> (Level, Level) {
        let ram_mb = rss_bytes as f64 / 1_048_576.0;
        let pick = |v: f64, show: f64, warn: f64, hot: f64| {
            if v >= hot {
                Level::Hot
            } else if v >= warn {
                Level::Warn
            } else if v >= show {
                Level::Ok
            } else {
                Level::Hidden
            }
        };
        (
            pick(cpu_pct, self.cpu_show, self.cpu_warn, self.cpu_hot),
            pick(ram_mb, self.ram_show_mb, self.ram_warn_mb, self.ram_hot_mb),
        )
    }

    pub fn ttl_ms(&self) -> u64 {
        // Three missed refreshes and the badge disappears on its own.
        ((self.interval_seconds * 3000.0) as u64).clamp(3_000, 600_000)
    }
}

pub fn config_path() -> PathBuf {
    if let Some(dir) = env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(dir).join("config.toml");
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("herdr/plugins/config").join(PLUGIN_ID).join("config.toml")
}

pub fn state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join("herdr/plugins").join(PLUGIN_ID)
}

pub fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn modified() -> Option<SystemTime> {
    fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

pub fn load() -> Config {
    let mut cfg = Config::default();
    let Ok(text) = fs::read_to_string(config_path()) else {
        return cfg;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() == "badge" {
            cfg.badge_both = value.trim().trim_matches('"') == "both";
            continue;
        }
        let Ok(v) = value.trim().parse::<f64>() else { continue };
        if !v.is_finite() || v < 0.0 {
            continue;
        }
        match key.trim() {
            "interval_seconds" => cfg.interval_seconds = v.clamp(1.0, 60.0),
            "cpu_show" => cfg.cpu_show = v,
            "cpu_warn" => cfg.cpu_warn = v,
            "cpu_hot" => cfg.cpu_hot = v,
            "ram_show_mb" => cfg.ram_show_mb = v,
            "ram_warn_mb" => cfg.ram_warn_mb = v,
            "ram_hot_mb" => cfg.ram_hot_mb = v,
            _ => {}
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worse_metric_wins() {
        let c = Config::default();
        assert_eq!(c.level(3.0, 500 << 20), Level::Hidden);
        assert_eq!(c.level(12.0, 500 << 20), Level::Ok);
        assert_eq!(c.level(3.0, 3000 << 20), Level::Warn);
        assert_eq!(c.level(108.0, 1000 << 20), Level::Hot);
    }
}
