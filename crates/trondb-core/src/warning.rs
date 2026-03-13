// ---------------------------------------------------------------------------
// PlanWarning — degraded plan notifications
// ---------------------------------------------------------------------------
//
// The engine always tries to give an answer. When the answer is expensive
// or suboptimal, it tells you what happened and how to fix it.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningSeverity {
    /// Informational: the plan is fine but could be cheaper.
    Info,
    /// Warning: the plan is significantly more expensive than optimal.
    Warning,
    /// Critical: the plan is extremely expensive or lossy.
    Critical,
}

impl fmt::Display for WarningSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WarningSeverity::Info => write!(f, "INFO"),
            WarningSeverity::Warning => write!(f, "WARN"),
            WarningSeverity::Critical => write!(f, "CRIT"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanWarning {
    pub severity: WarningSeverity,
    pub message: String,
    pub suggestion: Option<String>,
    pub acu_impact: Option<f64>,
}

impl PlanWarning {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: WarningSeverity::Info,
            message: message.into(),
            suggestion: None,
            acu_impact: None,
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: WarningSeverity::Warning,
            message: message.into(),
            suggestion: None,
            acu_impact: None,
        }
    }

    pub fn critical(message: impl Into<String>) -> Self {
        Self {
            severity: WarningSeverity::Critical,
            message: message.into(),
            suggestion: None,
            acu_impact: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn with_acu_impact(mut self, acu: f64) -> Self {
        self.acu_impact = Some(acu);
        self
    }
}

impl fmt::Display for PlanWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.severity, self.message)?;
        if let Some(acu) = self.acu_impact {
            write!(f, " (+{:.1} ACU)", acu)?;
        }
        if let Some(ref sugg) = self.suggestion {
            write!(f, " -- suggestion: {}", sugg)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_builder() {
        let w = PlanWarning::warning("full scan on 10k entities")
            .with_suggestion("add an index on 'city'")
            .with_acu_impact(5000.0);
        assert_eq!(w.severity, WarningSeverity::Warning);
        assert!(w.message.contains("full scan"));
        assert!(w.suggestion.as_ref().unwrap().contains("index"));
        assert!((w.acu_impact.unwrap() - 5000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn warning_display() {
        let w = PlanWarning::critical("archive-only results")
            .with_acu_impact(100.0)
            .with_suggestion("PROMOTE hot entities");
        let s = format!("{}", w);
        assert!(s.contains("[CRIT]"));
        assert!(s.contains("archive-only"));
        assert!(s.contains("+100.0 ACU"));
        assert!(s.contains("PROMOTE"));
    }

    #[test]
    fn info_no_suggestion() {
        let w = PlanWarning::info("using over-fetch 4x for pre-filter");
        let s = format!("{}", w);
        assert!(s.contains("[INFO]"));
        assert!(!s.contains("suggestion"));
    }
}
