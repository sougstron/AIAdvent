//! Prompt → model → deterministic invariant validation → pass/refuse/retry.

use crate::agent::Reply;
use crate::config::Res;
use crate::invariants::{InvariantSet, Origin, Violation, ViolationKind, MAX_ATTEMPTS};
use crate::todo::Stage;

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
pub fn run_with<F>(set: &InvariantSet, enabled: bool, query: &str, call: F) -> Res<PipelineOutcome>
where
    F: FnMut(usize, Option<&str>) -> Res<Reply>,
{
    run_stage_with(set, enabled, None, query, call)
}

/// Тот же цикл, но узел Validate проверяет ещё и **контракт этапа**
/// (`todo::check_stage_output`). Смысл — в задаче 15: «перепрыгнуть» этап
/// можно не только переходом, но и содержанием ответа (план, в котором
/// сразу лежит готовый ответ; `validate` без машинного вердикта). Такой
/// ответ — вина модели, поэтому он уходит в тот же retry, что и нарушение
/// инварианта, с той же формой `Violation` и тем же общим лимитом
/// [`MAX_ATTEMPTS`]: исчерпали — `GaveUp`, а не «ну ладно, поехали дальше».
pub fn run_stage_with<F>(
    set: &InvariantSet,
    enabled: bool,
    stage: Option<Stage>,
    query: &str,
    mut call: F,
) -> Res<PipelineOutcome>
where
    F: FnMut(usize, Option<&str>) -> Res<Reply>,
{
    // Контракт этапа работает всегда, даже когда инварианты выключены:
    // это не политика проекта, а условие, без которого лестница врёт.
    if !enabled || set.active_count() == 0 {
        let Some(stage) = stage else {
            let reply = call(1, None)?;
            return Ok(PipelineOutcome {
                decision: Decision::Pass { text: reply.text.clone(), accounted: Vec::new(), reply },
                attempts: Vec::new(),
            });
        };
        let mut attempts = Vec::new();
        let mut retry_note: Option<String> = None;
        for number in 1..=MAX_ATTEMPTS {
            let reply = call(number, retry_note.as_deref())?;
            let violations = contract_violations(stage, &reply.text);
            attempts.push(Attempt { number, violations: violations.clone(), text: reply.text.clone() });
            if violations.is_empty() {
                return Ok(PipelineOutcome {
                    decision: Decision::Pass { text: reply.text.clone(), accounted: Vec::new(), reply },
                    attempts,
                });
            }
            retry_note = Some(retry_note_for(&violations));
            if number == MAX_ATTEMPTS {
                return Ok(PipelineOutcome { decision: Decision::GaveUp { last: reply, violations }, attempts });
            }
        }
        unreachable!()
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
        let mut violations = set.check(&reply.text, &requested);
        if let Some(stage) = stage {
            violations.extend(contract_violations(stage, &reply.text));
        }
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
        retry_note = Some(retry_note_for(&violations));
        if number == MAX_ATTEMPTS {
            return Ok(PipelineOutcome { decision: Decision::GaveUp { last: reply, violations }, attempts });
        }
    }
    unreachable!()
}

/// Нарушения контракта этапа в форме [`Violation`] — чтобы retry-заметка,
/// диагностика и лимит попыток были одни и те же для инвариантов и для
/// лестницы, а не две похожие реализации.
fn contract_violations(stage: Stage, text: &str) -> Vec<Violation> {
    crate::todo::check_stage_output(stage, text)
        .into_iter()
        .map(|v| Violation {
            id: format!("этап:{}", v.id),
            kind: ViolationKind::Forbidden,
            evidence: v.evidence,
            origin: Origin::Model,
        })
        .collect()
}

/// Одна формулировка retry-заметки на оба источника нарушений. Строка про
/// `ИНВАРИАНТЫ:` добавляется только когда среди нарушений есть учётные —
/// иначе на этапе с выключенными инвариантами мы просили бы невозможного.
fn retry_note_for(violations: &[Violation]) -> String {
    let list = violations
        .iter()
        .map(|v| format!("{} ({})", v.id, v.evidence))
        .collect::<Vec<_>>()
        .join(", ");
    let mut note = format!(
        "Предыдущий ответ отклонён клиентским валидатором. Нарушения: {list}. \
         Сформируй новый ответ без этих нарушений"
    );
    if violations.iter().any(|v| !v.id.starts_with("этап:")) {
        note.push_str(" и обязательно закончи корректной строкой ИНВАРИАНТЫ: ...");
    }
    note.push('.');
    note
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
