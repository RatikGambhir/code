//! Token budgets: soft warnings and hard, pre-turn admission.
//!
//! Admission happens before a turn starts, against usage recorded so far plus
//! an estimate of the prompt. The model's output is not known in advance, so
//! a turn admitted just under a hard limit can end slightly over it; the next
//! turn is then refused.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone};
use cockpit_persistence::{BudgetPeriod, BudgetRecord, BudgetScope, UsageFilter};

/// What a turn is about to spend on.
#[derive(Copy, Clone, Debug, Default)]
pub struct TurnContext<'a> {
    pub workspace_id: Option<i64>,
    pub thread_id: Option<&'a str>,
    pub model: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    Warn(Vec<String>),
    Deny(String),
}

/// A rough token count for text: about four characters per token.
pub fn estimate_tokens(text: &str) -> i64 {
    (text.chars().count() as i64 + 3) / 4
}

pub fn applies(budget: &BudgetRecord, ctx: &TurnContext<'_>) -> bool {
    match budget.scope {
        BudgetScope::Global => true,
        BudgetScope::Workspace => ctx.workspace_id.is_some_and(|id| id.to_string() == budget.scope_key),
        BudgetScope::Thread => ctx.thread_id == Some(budget.scope_key.as_str()),
        BudgetScope::Model => ctx.model == Some(budget.scope_key.as_str()),
    }
}

/// Start of the current period as a Unix timestamp, in local time. Weeks
/// start on Monday. `None` for budgets that never reset.
pub fn period_start(period: BudgetPeriod, now: DateTime<Local>) -> Option<i64> {
    let today = now.date_naive();
    let start_date = match period {
        BudgetPeriod::Total => return None,
        BudgetPeriod::Day => today,
        BudgetPeriod::Week => today - Duration::days(today.weekday().num_days_from_monday() as i64),
        BudgetPeriod::Month => today.with_day(1)?,
    };
    let midnight = start_date.and_time(NaiveTime::MIN);
    // A DST gap at midnight is resolved to the earliest valid instant.
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|start| start.timestamp())
}

/// The usage rows a budget counts.
pub fn usage_filter(budget: &BudgetRecord, now: DateTime<Local>) -> UsageFilter {
    let mut filter = UsageFilter {
        since: period_start(budget.period, now),
        ..Default::default()
    };
    match budget.scope {
        BudgetScope::Global => {}
        BudgetScope::Workspace => filter.workspace_id = budget.scope_key.parse().ok(),
        BudgetScope::Thread => filter.thread_id = Some(budget.scope_key.clone()),
        BudgetScope::Model => filter.model = Some(budget.scope_key.clone()),
    }
    filter
}

/// "the daily budget for this workspace", for messages.
pub fn describe(budget: &BudgetRecord) -> String {
    let period = match budget.period {
        BudgetPeriod::Day => "daily",
        BudgetPeriod::Week => "weekly",
        BudgetPeriod::Month => "monthly",
        BudgetPeriod::Total => "lifetime",
    };
    let scope = match budget.scope {
        BudgetScope::Global => "overall".to_string(),
        BudgetScope::Workspace => "this workspace".to_string(),
        BudgetScope::Thread => "this thread".to_string(),
        BudgetScope::Model => format!("model {}", budget.scope_key),
    };
    format!("the {period} budget for {scope}")
}

/// Decides whether a turn estimated at `estimated_tokens` may start.
/// `usage` returns the total tokens recorded for a filter.
pub fn admit(
    budgets: &[BudgetRecord],
    ctx: &TurnContext<'_>,
    estimated_tokens: i64,
    now: DateTime<Local>,
    mut usage: impl FnMut(&UsageFilter) -> Result<i64>,
) -> Result<Admission> {
    let mut warnings = Vec::new();
    for budget in budgets.iter().filter(|b| applies(b, ctx)) {
        let used = usage(&usage_filter(budget, now))?;
        let projected = used + estimated_tokens;
        if let Some(hard) = budget.hard_limit_tokens {
            if projected > hard {
                return Ok(Admission::Deny(format!(
                    "This turn would exceed {}: {used} of {hard} tokens used.",
                    describe(budget)
                )));
            }
        }
        if let Some(soft) = budget.soft_limit_tokens {
            if projected >= soft {
                warnings.push(format!(
                    "Approaching {}: {used} of {soft} tokens used.",
                    describe(budget)
                ));
            }
        }
    }
    Ok(if warnings.is_empty() {
        Admission::Allowed
    } else {
        Admission::Warn(warnings)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(scope: BudgetScope, key: &str, period: BudgetPeriod, soft: Option<i64>, hard: Option<i64>) -> BudgetRecord {
        BudgetRecord {
            id: None,
            scope,
            scope_key: key.into(),
            period,
            soft_limit_tokens: soft,
            hard_limit_tokens: hard,
        }
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, 30, 0).earliest().unwrap()
    }

    #[test]
    fn periods_start_at_local_midnight() {
        // 2026-10-15 is a Thursday.
        let now = at(2026, 10, 15, 14);
        let day = period_start(BudgetPeriod::Day, now).unwrap();
        assert_eq!(Local.timestamp_opt(day, 0).unwrap().date_naive().day(), 15);
        let week = period_start(BudgetPeriod::Week, now).unwrap();
        let week_start = Local.timestamp_opt(week, 0).unwrap();
        assert_eq!(week_start.weekday(), chrono::Weekday::Mon);
        assert_eq!(week_start.day(), 12);
        let month = period_start(BudgetPeriod::Month, now).unwrap();
        assert_eq!(Local.timestamp_opt(month, 0).unwrap().day(), 1);
        assert_eq!(period_start(BudgetPeriod::Total, now), None);
    }

    #[test]
    fn budgets_apply_by_scope() {
        let ctx = TurnContext { workspace_id: Some(7), thread_id: Some("t1"), model: Some("m1") };
        assert!(applies(&budget(BudgetScope::Global, "", BudgetPeriod::Day, None, None), &ctx));
        assert!(applies(&budget(BudgetScope::Workspace, "7", BudgetPeriod::Day, None, None), &ctx));
        assert!(!applies(&budget(BudgetScope::Workspace, "8", BudgetPeriod::Day, None, None), &ctx));
        assert!(applies(&budget(BudgetScope::Thread, "t1", BudgetPeriod::Day, None, None), &ctx));
        assert!(!applies(&budget(BudgetScope::Model, "m2", BudgetPeriod::Day, None, None), &ctx));
    }

    #[test]
    fn soft_limits_warn_and_hard_limits_deny() {
        let budgets = vec![
            budget(BudgetScope::Global, "", BudgetPeriod::Day, Some(1000), Some(2000)),
            budget(BudgetScope::Model, "other", BudgetPeriod::Day, None, Some(1)),
        ];
        let ctx = TurnContext { workspace_id: Some(1), thread_id: None, model: Some("m1") };
        let now = at(2026, 10, 15, 9);

        let fine = admit(&budgets, &ctx, 100, now, |_| Ok(100)).unwrap();
        assert_eq!(fine, Admission::Allowed);

        let warn = admit(&budgets, &ctx, 100, now, |_| Ok(950)).unwrap();
        assert!(matches!(warn, Admission::Warn(ref w) if w[0].contains("daily budget for overall")));

        let deny = admit(&budgets, &ctx, 100, now, |_| Ok(1950)).unwrap();
        assert!(matches!(deny, Admission::Deny(ref m) if m.contains("1950 of 2000")));
    }

    #[test]
    fn filters_follow_the_budget() {
        let now = at(2026, 10, 15, 9);
        let filter = usage_filter(&budget(BudgetScope::Workspace, "42", BudgetPeriod::Total, None, None), now);
        assert_eq!(filter.workspace_id, Some(42));
        assert_eq!(filter.since, None);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }
}
