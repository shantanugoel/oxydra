use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule;
use types::{ScheduleCadence, SchedulerError};

/// Compute the next run time for a given cadence after `after`.
/// Returns `None` for a one-shot that has already passed.
pub fn next_run_for_cadence(
    cadence: &ScheduleCadence,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, SchedulerError> {
    match cadence {
        ScheduleCadence::Once { at } => {
            let at_dt = parse_utc_timestamp(at)?;
            if at_dt > after {
                Ok(Some(at_dt))
            } else {
                Ok(None)
            }
        }
        ScheduleCadence::Cron {
            expression,
            timezone,
        } => {
            let tz: chrono_tz::Tz =
                timezone
                    .parse()
                    .map_err(|_| SchedulerError::InvalidCadence {
                        reason: format!("invalid timezone: {timezone}"),
                    })?;
            // The `cron` crate expects 7-field expressions (sec min hour dom month dow year).
            // We prepend "0 " (seconds=0) to the user's 5-field expression.
            let full_expression = format!("0 {expression}");
            let schedule = Schedule::from_str(&full_expression).map_err(|e| {
                SchedulerError::InvalidCronExpression {
                    expression: expression.clone(),
                    reason: e.to_string(),
                }
            })?;
            let after_in_tz = after.with_timezone(&tz);
            let next = schedule.after(&after_in_tz).next();
            Ok(next.map(|dt| dt.with_timezone(&Utc)))
        }
        ScheduleCadence::Interval { every_secs } => {
            Ok(Some(after + chrono::Duration::seconds(*every_secs as i64)))
        }
    }
}

/// Validate a cadence at creation/edit time.
pub fn validate_cadence(
    cadence: &ScheduleCadence,
    min_interval_secs: u64,
) -> Result<(), SchedulerError> {
    match cadence {
        ScheduleCadence::Once { at } => {
            let at_dt = parse_utc_timestamp(at)?;
            if at_dt <= Utc::now() {
                return Err(SchedulerError::InvalidCadence {
                    reason: "one-off time must be in the future".into(),
                });
            }
        }
        ScheduleCadence::Cron {
            expression,
            timezone,
        } => {
            let tz: chrono_tz::Tz =
                timezone
                    .parse()
                    .map_err(|_| SchedulerError::InvalidCadence {
                        reason: format!("invalid timezone: {timezone}"),
                    })?;
            let full_expression = format!("0 {expression}");
            let schedule = Schedule::from_str(&full_expression).map_err(|e| {
                SchedulerError::InvalidCronExpression {
                    expression: expression.clone(),
                    reason: e.to_string(),
                }
            })?;
            // Check minimum interval between consecutive firings
            let now = Utc::now().with_timezone(&tz);
            let mut iter = schedule.after(&now);
            if let (Some(a), Some(b)) = (iter.next(), iter.next()) {
                let gap = (b - a).num_seconds();
                if gap < min_interval_secs as i64 {
                    return Err(SchedulerError::InvalidCadence {
                        reason: format!("cron fires every {gap}s, minimum is {min_interval_secs}s"),
                    });
                }
            }
        }
        ScheduleCadence::Interval { every_secs } => {
            if *every_secs < min_interval_secs {
                return Err(SchedulerError::InvalidCadence {
                    reason: format!("interval {every_secs}s is below minimum {min_interval_secs}s"),
                });
            }
        }
    }
    Ok(())
}

/// Parse a cadence from type + value + optional timezone.
pub fn parse_cadence(
    cadence_type: &str,
    cadence_value: &str,
    timezone: &str,
) -> Result<ScheduleCadence, SchedulerError> {
    match cadence_type {
        "once" => Ok(ScheduleCadence::Once {
            at: cadence_value.to_owned(),
        }),
        "cron" => Ok(ScheduleCadence::Cron {
            expression: cadence_value.to_owned(),
            timezone: timezone.to_owned(),
        }),
        "interval" => {
            let every_secs: u64 =
                cadence_value
                    .parse()
                    .map_err(|_| SchedulerError::InvalidCadence {
                        reason: format!(
                            "interval value must be a positive integer: {cadence_value}"
                        ),
                    })?;
            Ok(ScheduleCadence::Interval { every_secs })
        }
        _ => Err(SchedulerError::InvalidCadence {
            reason: format!("unknown cadence type: {cadence_type}"),
        }),
    }
}

/// Format a UTC timestamp in the schedule's timezone for display.
pub fn format_in_timezone(utc_timestamp: &str, cadence: &ScheduleCadence) -> Option<String> {
    let utc_dt = parse_utc_timestamp(utc_timestamp).ok()?;
    match cadence {
        ScheduleCadence::Cron { timezone, .. } => {
            let tz: chrono_tz::Tz = timezone.parse().ok()?;
            let local_dt = utc_dt.with_timezone(&tz);
            Some(local_dt.format("%Y-%m-%d %H:%M:%S %Z").to_string())
        }
        _ => None,
    }
}

fn parse_utc_timestamp(s: &str) -> Result<DateTime<Utc>, SchedulerError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| SchedulerError::InvalidCadence {
            reason: format!("invalid timestamp '{s}': {e}"),
        })
}
