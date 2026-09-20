//! Prompt → model → deterministic invariant validation → pass/refuse/retry.

use crate::agent::Reply;
use crate::config::Res;
use crate::invariants::{InvariantSet, Origin, Violation, MAX_ATTEMPTS};

#[derive(Clone, Debug)]
pub struct Attempt {
    pub number: usize,
    pub violations: Vec<Violation>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub enum Decision {
    Pass { text: String, accounted: Vec<String>, reply: Reply },
    RefusedByInvariant { ids: Vec<String>, explanation: String },
    GaveUp { last: Reply, violations: Vec<Violation> },
}

#[derive(Clone, Debug)]
pub struct PipelineOutcome {
    pub decision: Decision,
    pub attempts: Vec<Attempt>,
}

/// Runs the policy loop with an injectable model call. Rejected attempts are
/// returned as diagnostics only; the caller must persist only `Pass`.
pub fn run_with<F>(set: &InvariantSet, enabled: bool, query: &str, mut call: F) -> Res<PipelineOutcome>
where
    F: FnMut(usize, Option<&str>) -> Res<Reply>,
{
    if !enabled || set.active_count() == 0 {
        let reply = call(1, None)?;
        return Ok(PipelineOutcome {
            decision: Decision::Pass { text: reply.text.clone(), accounted: Vec::new(), reply },
            attempts: Vec::new(),
        });
    }

    let requested = set.requested_by(query);
    if !requested.is_empty() {
        return Ok(PipelineOutcome {
            decision: Decision::RefusedByInvariant {
                explanation: set.refusal(&requested),
                ids: requested,
            },
            attempts: Vec::new(),
        });
    }

    let mut attempts = Vec::new();
    let mut retry_note: Option<String> = None;
    for number in 1..=MAX_ATTEMPTS {
        let reply = call(number, retry_note.as_deref())?;
        let violations = set.check(&reply.text, &requested);
        attempts.push(Attempt { number, violations: violations.clone(), text: reply.text.clone() });
        if violations.is_empty() {
            let (text, accounted) = InvariantSet::strip_accounting(&reply.text);
            return Ok(PipelineOutcome { decision: Decision::Pass { text, accounted: accounted.unwrap_or_default(), reply }, attempts });
        }
        // Defensive: requested conflicts are normally caught before the call.
        let user_ids: Vec<String> = violations.iter().filter(|v| v.origin == Origin::User).map(|v| v.id.clone()).collect();
        if !user_ids.is_empty() {
            return Ok(PipelineOutcome { decision: Decision::RefusedByInvariant { explanation: set.refusal(&user_ids), ids: user_ids }, attempts });
        }
        retry_note = Some(format!(
            "Предыдущий ответ отклонён клиентским валидатором. Нарушения: {}. Сформируй новый ответ без этих нарушений и обязательно закончи корректной строкой ИНВАРИАНТЫ: ...",
            violations.iter().map(|v| format!("{} ({})", v.id, v.evidence)).collect::<Vec<_>>().join(", ")
        ));
        if number == MAX_ATTEMPTS {
            return Ok(PipelineOutcome { decision: Decision::GaveUp { last: reply, violations }, attempts });
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Outcome, Usage};

    fn reply(text: &str) -> Reply {
        Reply::from_outcome(Outcome { content: Some(text.into()), model: None, finish_reason: None, usage: Usage::default(), reasoning: None, latency_ms: 0, raw: serde_json::Value::Null }, None)
    }

    #[test]
    fn retries_model_fault_and_accepts_only_clean_attempt() {
        let set = InvariantSet::in_memory();
        let out = run_with(&set, true, "Нейтральный вопрос", |n, note| {
            if n == 1 { assert!(note.is_none()); Ok(reply("ответ без строки")) }
            else { assert!(note.unwrap().contains("accounting")); Ok(reply("чисто\nИНВАРИАНТЫ: нет")) }
        }).unwrap();
        assert_eq!(out.attempts.len(), 2);
        match out.decision { Decision::Pass { text, .. } => assert_eq!(text, "чисто"), _ => panic!("not pass") }
    }

    #[test]
    fn stops_after_five_and_user_conflict_calls_no_model() {
        let set = InvariantSet::in_memory();
        let mut calls = 0;
        let out = run_with(&set, true, "Нейтрально", |_, _| { calls += 1; Ok(reply("нет учёта")) }).unwrap();
        assert_eq!(calls, MAX_ATTEMPTS);
        assert!(matches!(out.decision, Decision::GaveUp { .. }));

        let out = run_with(&set, true, "Переписать на Electron", |_, _| -> Res<Reply> { panic!("must not call") }).unwrap();
        assert!(matches!(out.decision, Decision::RefusedByInvariant { .. }));
    }
}
