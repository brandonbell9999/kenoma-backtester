//! Structured validation and causality audit support.

use kenoma_types::TimestampNs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    Warn,
    Strict,
}

impl Default for ValidationMode {
    fn default() -> Self {
        Self::Warn
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditCode {
    FutureDataAccess,
    SignalAtOrAfterEntry,
    MissingInstrument,
    MissingMarketData,
    Validation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts: Option<TimestampNs>,
    pub severity: AuditSeverity,
    pub code: AuditCode,
    pub message: String,
    #[serde(default)]
    pub field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureCutoff {
    pub name: String,
    pub cutoff_ts: TimestampNs,
    pub observed_at_ts: TimestampNs,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("{0}")]
    Strict(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditTrail {
    pub mode: ValidationMode,
    #[serde(default)]
    pub events: Vec<AuditEvent>,
    #[serde(default)]
    pub feature_cutoffs: Vec<FeatureCutoff>,
}

impl AuditTrail {
    pub fn new(mode: ValidationMode) -> Self {
        Self {
            mode,
            events: Vec::new(),
            feature_cutoffs: Vec::new(),
        }
    }

    pub fn warn(
        &mut self,
        ts: Option<TimestampNs>,
        code: AuditCode,
        message: impl Into<String>,
    ) -> Result<(), AuditError> {
        let message = message.into();
        self.events.push(AuditEvent {
            ts,
            severity: AuditSeverity::Warning,
            code,
            message: message.clone(),
            field: None,
        });
        if self.mode == ValidationMode::Strict {
            Err(AuditError::Strict(message))
        } else {
            Ok(())
        }
    }

    pub fn error(
        &mut self,
        ts: Option<TimestampNs>,
        code: AuditCode,
        message: impl Into<String>,
    ) -> Result<(), AuditError> {
        let message = message.into();
        self.events.push(AuditEvent {
            ts,
            severity: AuditSeverity::Error,
            code,
            message: message.clone(),
            field: None,
        });
        Err(AuditError::Strict(message))
    }

    pub fn info(&mut self, ts: Option<TimestampNs>, code: AuditCode, message: impl Into<String>) {
        self.events.push(AuditEvent {
            ts,
            severity: AuditSeverity::Info,
            code,
            message: message.into(),
            field: None,
        });
    }

    pub fn record_feature_cutoff(
        &mut self,
        name: impl Into<String>,
        cutoff_ts: TimestampNs,
        observed_at_ts: TimestampNs,
    ) -> Result<(), AuditError> {
        let name = name.into();
        self.feature_cutoffs.push(FeatureCutoff {
            name: name.clone(),
            cutoff_ts,
            observed_at_ts,
        });
        if cutoff_ts > observed_at_ts {
            self.warn(
                Some(observed_at_ts),
                AuditCode::FutureDataAccess,
                format!(
                    "feature '{name}' cutoff {cutoff_ts} is after simulation time {observed_at_ts}"
                ),
            )?;
        }
        Ok(())
    }

    pub fn check_signal_before_entry(
        &mut self,
        name: impl Into<String>,
        signal_ts: TimestampNs,
        entry_ts: TimestampNs,
    ) -> Result<(), AuditError> {
        let name = name.into();
        if signal_ts >= entry_ts {
            self.warn(
                Some(entry_ts),
                AuditCode::SignalAtOrAfterEntry,
                format!("signal '{name}' timestamp {signal_ts} is not before entry {entry_ts}"),
            )?;
        }
        Ok(())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &AuditEvent> {
        self.events
            .iter()
            .filter(|event| event.severity == AuditSeverity::Warning)
    }

    pub fn errors(&self) -> impl Iterator<Item = &AuditEvent> {
        self.events
            .iter()
            .filter(|event| event.severity == AuditSeverity::Error)
    }
}

impl Default for AuditTrail {
    fn default() -> Self {
        Self::new(ValidationMode::Warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_feature_is_warning_by_default() {
        let mut audit = AuditTrail::default();
        let result = audit.record_feature_cutoff("alpha", 11, 10);
        assert!(result.is_ok());
        assert_eq!(audit.warnings().count(), 1);
    }

    #[test]
    fn future_feature_fails_strict() {
        let mut audit = AuditTrail::new(ValidationMode::Strict);
        let result = audit.record_feature_cutoff("alpha", 11, 10);
        assert!(matches!(result, Err(AuditError::Strict(_))));
        assert_eq!(audit.warnings().count(), 1);
    }

    #[test]
    fn signal_at_entry_is_audit_event() {
        let mut audit = AuditTrail::default();
        audit.check_signal_before_entry("entry", 20, 20).unwrap();
        assert_eq!(
            audit.events.first().map(|event| &event.code),
            Some(&AuditCode::SignalAtOrAfterEntry)
        );
    }
}
