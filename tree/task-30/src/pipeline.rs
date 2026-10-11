//! Prompt → model → stage-contract validation → pass/retry.
//!
//! Без этапа (`stage = None`) — один вызов модели и сразу `Pass`.

use crate::agent::Reply;
use crate::config::Res;
use crate::todo::Stage;

/// Общий лимит попыток на один этап лестницы.
pub const MAX_ATTEMPTS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub id: String,
    pub evidence: String,
}

#[derive(Clone, Debug)]
pub struct Attempt {
    pub violations: Vec<Violation>,
}

#[derive(Clone, Debug)]
pub enum Decision {
    Pass { text: String, reply: Reply },
    GaveUp { last: Reply, violations: Vec<Violation> },
}

#[derive(Clone, Debug)]
pub struct PipelineOutcome {
    pub decision: Decision,
    pub attempts: Vec<Attempt>,
}

/// Узел Validate проверяет **контракт этапа** (`todo::check_stage_output`).
/// Смысл — в задаче 15: «перепрыгнуть» этап можно не только переходом, но и
/// содержанием ответа (план, в котором сразу лежит готовый ответ; `validate`
/// без машинного вердикта). Такой ответ — вина модели, поэтому он уходит в
/// retry с заметкой о нарушениях; исчерпали [`MAX_ATTEMPTS`] — `GaveUp`, а не
/// «ну ладно, поехали дальше». Отклонённые попытки — только диагностика,
/// сохранять вызывающий должен лишь `Pass`.
pub fn run_stage_with<F>(stage: Option<Stage>, mut call: F) -> Res<PipelineOutcome>
where
    F: FnMut(usize, Option<&str>) -> Res<Reply>,
{
    let Some(stage) = stage else {
        let reply = call(1, None)?;
        return Ok(PipelineOutcome {
            decision: Decision::Pass { text: reply.text.clone(), reply },
            attempts: Vec::new(),
        });
    };
    let mut attempts = Vec::new();
    let mut retry_note: Option<String> = None;
    for number in 1..=MAX_ATTEMPTS {
        let reply = call(number, retry_note.as_deref())?;
        let violations = contract_violations(stage, &reply.text);
        attempts.push(Attempt { violations: violations.clone() });
        if violations.is_empty() {
            return Ok(PipelineOutcome { decision: Decision::Pass { text: reply.text.clone(), reply }, attempts });
        }
        retry_note = Some(retry_note_for(&violations));
        if number == MAX_ATTEMPTS {
            return Ok(PipelineOutcome { decision: Decision::GaveUp { last: reply, violations }, attempts });
        }
    }
    unreachable!()
}

fn contract_violations(stage: Stage, text: &str) -> Vec<Violation> {
    crate::todo::check_stage_output(stage, text)
        .into_iter()
        .map(|v| Violation { id: format!("этап:{}", v.id), evidence: v.evidence })
        .collect()
}

fn retry_note_for(violations: &[Violation]) -> String {
    let list = violations
        .iter()
        .map(|v| format!("{} ({})", v.id, v.evidence))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Предыдущий ответ отклонён клиентским валидатором. Нарушения: {list}. \
         Сформируй новый ответ без этих нарушений."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Outcome, Usage};

    fn reply(text: &str) -> Reply {
        Reply::from_outcome(Outcome { content: Some(text.into()), model: None, finish_reason: None, usage: Usage::default(), reasoning: None, latency_ms: 0, raw: serde_json::Value::Null }, None)
    }

    #[test]
    fn no_stage_is_a_single_call() {
        let mut calls = 0;
        let out = run_stage_with(None, |_, note| { calls += 1; assert!(note.is_none()); Ok(reply("привет")) }).unwrap();
        assert_eq!(calls, 1);
        assert!(out.attempts.is_empty());
        match out.decision { Decision::Pass { text, .. } => assert_eq!(text, "привет"), _ => panic!("not pass") }
    }
}
