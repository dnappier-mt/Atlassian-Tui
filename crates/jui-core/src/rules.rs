//! User-editable automation rules. Each `Rule` binds a `Trigger` (fired by the
//! daemon when a meaningful event happens) to an optional set of `Condition`
//! filters and an ordered list of `Action`s. Persisted under `[[rules]]` in
//! `~/.config/jui/config.toml` (see `crates/jui-core/src/config.rs`).
//!
//! v1 fires for mutation-driven events only — no polling-diff comment
//! triggers, no expression-language conditions. The hardcoded automations in
//! the daemon continue to run alongside rules; rules layer on top (additive).

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub trigger: Trigger,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
}

fn yes() -> bool {
    true
}

/// Discriminant only: each variant's payload is what `Trigger::matches` uses
/// to filter incoming events. For example `TicketStatusChanged { to: Some("X") }`
/// only matches when the daemon-fired event's `to` is "X".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    PrCreated,
    StartWork,
    StopWork,
    TicketStatusChanged {
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
    },
    TicketAssigned {
        #[serde(default)]
        to_me: Option<bool>,
    },
}

impl Trigger {
    /// True when this rule's trigger spec matches a fired event. Both args
    /// share the same enum; comparison is variant-equal plus
    /// `Some(filter)` constraints on payload fields.
    pub fn matches(&self, fired: &Trigger) -> bool {
        match (self, fired) {
            (Trigger::PrCreated, Trigger::PrCreated) => true,
            (Trigger::StartWork, Trigger::StartWork) => true,
            (Trigger::StopWork, Trigger::StopWork) => true,
            (
                Trigger::TicketStatusChanged { from, to },
                Trigger::TicketStatusChanged { from: ef, to: et },
            ) => {
                let from_ok = match from {
                    Some(f) => ef.as_deref().map(|s| s.eq_ignore_ascii_case(f)).unwrap_or(false),
                    None => true,
                };
                let to_ok = match to {
                    Some(t) => et.as_deref().map(|s| s.eq_ignore_ascii_case(t)).unwrap_or(false),
                    None => true,
                };
                from_ok && to_ok
            }
            (Trigger::TicketAssigned { to_me }, Trigger::TicketAssigned { to_me: em }) => {
                match to_me {
                    Some(want) => em.unwrap_or(false) == *want,
                    None => true,
                }
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    ProjectKeyEquals { value: String },
    StatusEquals { value: String },
    IssueTypeIn { values: Vec<String> },
    HasLinkedRepo,
    ActorIsMe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    JiraTransition { to: String },
    JiraComment { body: String },
    GithubPrComment { body: String },
    /// Write the picked DevQA account-id from the PR-create form into the
    /// Jira ticket's DevQA custom field. Uses `jira.devqa_customfield` from
    /// the global config to find the field id; skips with a log when that
    /// config value is empty. No template fields — value comes from
    /// `RuleContext.devqa_account_id`.
    SetTicketDevQa,
}

/// Fields available for `{placeholder}` substitution and for condition
/// matching. Trigger sites populate whichever fields are known; the renderer
/// leaves unknown placeholders literal so they're easy to spot in output.
#[derive(Debug, Default, Clone)]
pub struct RuleContext {
    pub ticket_key: Option<String>,
    pub ticket_summary: Option<String>,
    pub ticket_status: Option<String>,
    pub ticket_project_key: Option<String>,
    pub ticket_issue_type: Option<String>,
    pub has_linked_repo: bool,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_repo: Option<String>,
    pub actor: Option<String>,
    pub actor_is_me: bool,
    pub reviewer_handle: Option<String>,
    pub devqa_handle: Option<String>,
    /// Jira account ids picked in the PR-create form. Distinct from the
    /// GitHub handles (`reviewer_handle`/`devqa_handle`) — these are what
    /// the Jira REST API needs to set user-shaped custom fields.
    pub reviewer_account_id: Option<String>,
    pub devqa_account_id: Option<String>,
}

impl RuleContext {
    fn placeholder_value(&self, key: &str) -> Option<String> {
        match key {
            "ticket_key" => self.ticket_key.clone(),
            "ticket_summary" => self.ticket_summary.clone(),
            "ticket_status" => self.ticket_status.clone(),
            "project_key" => self.ticket_project_key.clone(),
            "issue_type" => self.ticket_issue_type.clone(),
            "from_status" => self.from_status.clone(),
            "to_status" => self.to_status.clone(),
            "pr_url" => self.pr_url.clone(),
            "pr_number" => self.pr_number.map(|n| n.to_string()),
            "pr_repo" => self.pr_repo.clone(),
            "actor" => self.actor.clone(),
            "reviewer_handle" => self.reviewer_handle.clone(),
            "devqa_handle" => self.devqa_handle.clone(),
            "reviewer_account_id" => self.reviewer_account_id.clone(),
            "devqa_account_id" => self.devqa_account_id.clone(),
            _ => None,
        }
    }
}

impl Condition {
    pub fn matches(&self, ctx: &RuleContext) -> bool {
        match self {
            Condition::ProjectKeyEquals { value } => ctx
                .ticket_project_key
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(value))
                .unwrap_or(false),
            Condition::StatusEquals { value } => ctx
                .ticket_status
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(value))
                .unwrap_or(false),
            Condition::IssueTypeIn { values } => match ctx.ticket_issue_type.as_deref() {
                Some(t) => values.iter().any(|v| v.eq_ignore_ascii_case(t)),
                None => false,
            },
            Condition::HasLinkedRepo => ctx.has_linked_repo,
            Condition::ActorIsMe => ctx.actor_is_me,
        }
    }
}

/// Substitute `{placeholder}` tokens with values from `ctx`. Unknown
/// placeholders are left literally in place so misspellings are visible in
/// the resulting comment / commit. Single-pass scanner — no regex dep.
pub fn render(tmpl: &str, ctx: &RuleContext) -> String {
    let bytes = tmpl.as_bytes();
    let mut out = String::with_capacity(tmpl.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'}') {
                let raw = &tmpl[i + 1..i + 1 + end];
                if is_placeholder_name(raw) {
                    match ctx.placeholder_value(raw) {
                        Some(v) => out.push_str(&v),
                        None => {
                            out.push('{');
                            out.push_str(raw);
                            out.push('}');
                        }
                    }
                    i += 1 + end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn is_placeholder_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
}

/// Lightweight stable id for a rule. We're not running at scale where ULID
/// ordering matters; a timestamped suffix off `/proc/sys/kernel/random/uuid`
/// is plenty and avoids pulling in the `ulid` crate.
pub fn new_rule_id() -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let uuid = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let short: String = uuid.chars().filter(|c| c.is_ascii_hexdigit()).take(8).collect();
    format!("r{now:013}-{short}")
}

#[derive(Debug)]
pub struct ActionOutcome {
    pub rule_name: String,
    pub action_kind: &'static str,
    pub result: Result<()>,
}

/// Find every enabled rule whose trigger + conditions match.
pub fn select<'a>(rules: &'a [Rule], fired: &Trigger, ctx: &RuleContext) -> Vec<&'a Rule> {
    rules
        .iter()
        .filter(|r| r.enabled)
        .filter(|r| r.trigger.matches(fired))
        .filter(|r| r.conditions.iter().all(|c| c.matches(ctx)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_key(k: &str) -> RuleContext {
        RuleContext {
            ticket_key: Some(k.into()),
            ..Default::default()
        }
    }

    #[test]
    fn render_known_placeholder() {
        let ctx = ctx_with_key("ENG-42");
        assert_eq!(render("ticket {ticket_key} ready", &ctx), "ticket ENG-42 ready");
    }

    #[test]
    fn render_unknown_left_literal() {
        let ctx = ctx_with_key("ENG-42");
        assert_eq!(render("hi {nope}", &ctx), "hi {nope}");
    }

    #[test]
    fn render_repeated_placeholder() {
        let ctx = ctx_with_key("A-1");
        assert_eq!(render("{ticket_key} = {ticket_key}", &ctx), "A-1 = A-1");
    }

    #[test]
    fn render_unmatched_brace_kept() {
        let ctx = ctx_with_key("X");
        assert_eq!(render("plain { brace", &ctx), "plain { brace");
    }

    #[test]
    fn render_empty_context_leaves_placeholder() {
        let ctx = RuleContext::default();
        assert_eq!(render("{ticket_key}", &ctx), "{ticket_key}");
    }

    #[test]
    fn trigger_matches_variant() {
        assert!(Trigger::PrCreated.matches(&Trigger::PrCreated));
        assert!(!Trigger::PrCreated.matches(&Trigger::StartWork));
    }

    #[test]
    fn trigger_status_change_filter_to() {
        let spec = Trigger::TicketStatusChanged {
            from: None,
            to: Some("Code Review".into()),
        };
        let hit = Trigger::TicketStatusChanged {
            from: Some("In Dev".into()),
            to: Some("code review".into()), // case-insensitive
        };
        let miss = Trigger::TicketStatusChanged {
            from: Some("In Dev".into()),
            to: Some("Done".into()),
        };
        assert!(spec.matches(&hit));
        assert!(!spec.matches(&miss));
    }

    #[test]
    fn trigger_assigned_to_me_filter() {
        let spec = Trigger::TicketAssigned { to_me: Some(true) };
        assert!(spec.matches(&Trigger::TicketAssigned { to_me: Some(true) }));
        assert!(!spec.matches(&Trigger::TicketAssigned { to_me: Some(false) }));
        // None on the fired side means "unknown" — treat as not me.
        assert!(!spec.matches(&Trigger::TicketAssigned { to_me: None }));
    }

    #[test]
    fn condition_status_equals_case_insensitive() {
        let cond = Condition::StatusEquals { value: "In Dev".into() };
        let mut ctx = RuleContext::default();
        ctx.ticket_status = Some("in dev".into());
        assert!(cond.matches(&ctx));
        ctx.ticket_status = Some("Done".into());
        assert!(!cond.matches(&ctx));
    }

    #[test]
    fn select_filters_disabled() {
        let r = Rule {
            id: "1".into(),
            name: "x".into(),
            enabled: false,
            trigger: Trigger::PrCreated,
            conditions: vec![],
            actions: vec![],
        };
        let rules = vec![r];
        assert!(select(&rules, &Trigger::PrCreated, &RuleContext::default()).is_empty());
    }
}
