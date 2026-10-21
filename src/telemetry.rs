//! Presentation and parsing of per-turn execution metadata. Unknown is not Fast=false.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Execution {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub service_tier: Option<String>,
}
impl Execution {
    pub fn from_context(v: &Value) -> Self {
        Self {
            model: v["model"].as_str().unwrap_or_default().into(),
            effort: v["effort"].as_str().or_else(|| v["reasoning_effort"].as_str()).unwrap_or_default().into(),
            service_tier: v["service_tier"].as_str().or_else(|| v["serviceTier"].as_str()).map(str::to_owned),
        }
    }
    pub fn fast(&self) -> bool {
        matches!(self.service_tier.as_deref(), Some("priority" | "fast" | "ultrafast"))
    }
    pub fn short(&self) -> String {
        if self.model.is_empty() { return String::new(); }
        let model = self.model.strip_prefix("gpt-").unwrap_or(&self.model);
        let model = model.split('-').map(|s| {
            if matches!(s, "sol" | "astra" | "luna" | "terra") {
                let mut chars = s.chars();
                format!("{}{}", chars.next().unwrap().to_uppercase(), chars.as_str())
            } else { s.into() }
        }).collect::<Vec<_>>().join(" ");
        let effort = match self.effort.as_str() {
            "none" => "Off", "minimal" => "Min", "low" => "L", "medium" => "M",
            "high" => "H", "xhigh" => "XH", "max" => "Max", "ultra" => "Ultra", other => other,
        };
        format!("{model}{}{}", if effort.is_empty() { String::new() } else { format!(" · {effort}") }, if self.fast() { " ⚡" } else { "" })
    }
    pub fn tooltip(&self) -> String {
        let zh = crate::locale::chinese();
        let unknown = if zh { "未知" } else { "Unknown" };
        let tier = match self.service_tier.as_deref() {
            Some("priority" | "fast" | "ultrafast") => if zh { "已开启" } else { "On" },
            Some("default" | "standard" | "flex") => if zh { "未开启" } else { "Off" },
            _ => unknown,
        };
        format!("{}: {}\n{}: {}\nFast: {tier}{}", if zh { "模型" } else { "Model" }, if self.model.is_empty() { unknown } else { &self.model }, if zh { "推理强度" } else { "Reasoning effort" }, if self.effort.is_empty() { unknown } else { &self.effort }, self.service_tier.as_ref().map(|v| format!(" ({v})")).unwrap_or_default())
    }
}

pub fn countdown(reset: f64, at: f64) -> String {
    // Round up to minutes so this text changes only at minute boundaries.
    let minutes = ((reset - at).max(0.) / 60.).ceil() as u64;
    if minutes >= 1440 { format!("{}d{}h", minutes / 1440, minutes / 60 % 24) }
    else if minutes >= 60 { format!("{}h{:02}m", minutes / 60, minutes % 60) }
    else { format!("{minutes}m") }
}
pub fn local_reset(reset: f64) -> String {
    chrono::DateTime::from_timestamp(reset as i64, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %:z").to_string())
        .unwrap_or_else(|| crate::locale::text("Unknown").into_owned())
}
pub fn reset_tooltip(label: &str, reset: Option<f64>, at: f64) -> String {
    let zh = crate::locale::chinese();
    match reset {
        Some(reset) => format!("{label}: {}\n{}: {}", local_reset(reset), if zh { "剩余时间" } else { "Time remaining" }, countdown(reset, at)),
        None => format!("{label}: {}", if zh { "暂未提供" } else { "Not provided" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn per_turn_fast_is_not_inferred_and_efforts_are_unambiguous() {
        let fast = Execution::from_context(&serde_json::json!({"model":"gpt-6.1-sol","effort":"medium","service_tier":"priority"}));
        assert_eq!(fast.short(), "6.1 Sol · M ⚡");
        let next = Execution::from_context(&serde_json::json!({"model":"gpt-6-astra","effort":"max"}));
        assert_eq!(next.short(), "6 Astra · Max");
        assert!(next.service_tier.is_none());
        assert!(!next.fast());
        assert_eq!(countdown(8300., 0.), "2h19m");
        assert_eq!(countdown(0., 20.), "0m");
    }
}
