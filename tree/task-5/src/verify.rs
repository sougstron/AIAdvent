//! Доказательство того, что «слабая / средняя / сильная» — это лестница,
//! а не три ярлыка на одинаковых ответах.
//!
//! Без этой проверки весь отчёт — гадание. Замеры времени и цены останутся
//! верными, даже если реселлер подсунет вместо заявленной модели другую, — но
//! утверждение «вот столько стоит именно эта модель» станет ложным. Поэтому
//! проверяются два независимых свойства, и оба падают громко:
//!
//! 1. **Кого нам ответили.** Площадка сама называет модель в поле `model`.
//!    Не совпало с запрошенной — все цифры относятся не к той модели, и
//!    остальная проверка уже неважна.
//! 2. **Различима ли ступень.** Короткие вопросы с *машинно проверяемым*
//!    ответом при `temperature = 0` — не «тексты отличаются», а «попал или
//!    не попал в заранее известное число». Верхняя ступень обязана набрать
//!    строго больше нижней, иначе лестница не подтверждена.
//!
//! Вердикт `Confirmed` выдаётся только когда сошлись обе половины.

use serde::{Deserialize, Serialize};

use crate::api::{Client, Request, Res};
use crate::ladder::{Tier, LADDER};

/// Вопросы подобраны так, чтобы ответ был одним числом и проверялся кодом,
/// а не другой моделью.
///
/// Набор менялся: первая версия (сколько «r» в strawberry, 9.11 против 9.9,
/// сёстры Марии, счёт семёрок до ста) оказалась слишком лёгкой — все три
/// ступени взяли 4/4, и проверка честно сообщила «ступени неразличимы».
/// Здесь задачи, где ошибка не в знании, а в аккуратности: усреднение
/// скоростей, счёт себя внутри условия, посимвольный подсчёт и вопрос
/// с «очевидным», но неверным ответом.
const PROBES: [Probe; 4] = [
    Probe {
        question: "Поезд идёт из A в B со скоростью 60 км/ч, а обратно тем же путём — \
                   со скоростью 40 км/ч. Какова средняя скорость за всю поездку в км/ч? \
                   Ответь только числом.",
        answer: 48.0,
    },
    Probe {
        question: "У Алисы столько же братьев, сколько сестёр, а у её брата Боба сестёр \
                   вдвое больше, чем братьев. Сколько всего детей в семье? \
                   Ответь только числом.",
        answer: 7.0,
    },
    Probe {
        question: "Сколько раз буква «о» встречается в предложении \
                   «Около колодца кольцо не найдено»? Ответь только числом.",
        answer: 8.0,
    },
    Probe {
        question: "Сколько раз за сутки часовая и минутная стрелки часов совпадают? \
                   Ответь только числом.",
        answer: 22.0,
    },
];

/// `temperature = 0` — чтобы результат ступени был её свойством, а не удачей
/// одного сэмпла. `max_tokens` с запасом: рассуждающим моделям нужно место,
/// а обрыв по лимиту засчитался бы как ошибка и подделал бы лестницу.
const PROBE_TEMPERATURE: f64 = 0.0;
const PROBE_MAX_TOKENS: u32 = 700;

struct Probe {
    question: &'static str,
    answer: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Модели те, что заказывали, и верхняя ступень решает больше нижней.
    Confirmed,
    /// Площадка ответила не той моделью — цифры относятся не к ней.
    Substituted,
    /// Ступени набрали поровну: проба их не различает.
    Flat,
    /// Слабая решила больше сильной — заявленный порядок не подтверждён.
    Inverted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RungCheck {
    pub tier: Tier,
    pub label: String,
    pub model: String,
    /// Как модель назвала себя площадка в ответе.
    pub served_model: String,
    pub model_matches: bool,
    /// Ответы на пробы в порядке `PROBES`.
    pub answers: Vec<String>,
    pub correct: Vec<bool>,
    pub score: usize,
    /// Суммарно потрачено на пробы этой ступени.
    pub probe_ms: u64,
    pub probe_tokens: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LadderCheck {
    pub questions: Vec<String>,
    pub expected: Vec<String>,
    pub rungs: Vec<RungCheck>,
    pub verdict: Verdict,
    pub explanation: String,
}

pub fn check_ladder() -> Res<LadderCheck> {
    let mut rungs = Vec::with_capacity(LADDER.len());
    for s in &LADDER {
        let client = Client::new(s.provider)?;
        let mut answers = Vec::new();
        let mut correct = Vec::new();
        let mut served = String::new();
        let mut probe_ms = 0u64;
        let mut probe_tokens = 0u32;

        for (i, p) in PROBES.iter().enumerate() {
            let reply = client
                .complete(&Request {
                    model: s.model.into(),
                    system: None,
                    prompt: p.question.into(),
                    temperature: PROBE_TEMPERATURE,
                    max_tokens: PROBE_MAX_TOKENS,
                    effort: s.effort,
                })
                .map_err(|e| format!("проба {} на ступени «{}»: {e}", i + 1, s.label))?;
            if served.is_empty() {
                served = reply.served_model.clone();
            }
            probe_ms += reply.latency_ms;
            probe_tokens += reply.completion_tokens;
            correct.push(matches_answer(&reply.content, p.answer));
            answers.push(reply.content.trim().to_string());
        }

        rungs.push(RungCheck {
            tier: s.tier,
            label: s.label.into(),
            model: s.model.into(),
            model_matches: same_model(s.model, &served),
            served_model: served,
            score: correct.iter().filter(|c| **c).count(),
            answers,
            correct,
            probe_ms,
            probe_tokens,
        });
    }

    let (verdict, explanation) = judge(&rungs);
    Ok(LadderCheck {
        questions: PROBES.iter().map(|p| p.question.to_string()).collect(),
        expected: PROBES.iter().map(|p| fmt_num(p.answer)).collect(),
        rungs,
        verdict,
        explanation,
    })
}

fn judge(rungs: &[RungCheck]) -> (Verdict, String) {
    if let Some(bad) = rungs.iter().find(|r| !r.model_matches) {
        return (
            Verdict::Substituted,
            format!(
                "Ступень «{}» просили у модели `{}`, а площадка ответила от имени `{}`. \
                 Значит замеры времени, токенов и цены относятся не к той модели, \
                 которая заявлена, — сравнивать по ним нельзя.",
                bad.label, bad.model, bad.served_model
            ),
        );
    }

    let weak = rungs.first().map(|r| r.score).unwrap_or(0);
    let strong = rungs.last().map(|r| r.score).unwrap_or(0);
    let total = PROBES.len();
    let table = rungs
        .iter()
        .map(|r| format!("{} — {}/{}", r.label, r.score, total))
        .collect::<Vec<_>>()
        .join(", ");

    match strong.cmp(&weak) {
        std::cmp::Ordering::Greater => (
            Verdict::Confirmed,
            format!(
                "Все три площадки ответили именно теми моделями, которые заказывали, \
                 а на задачах с заранее известным ответом при temperature=0 верхняя \
                 ступень решила больше нижней ({table}). Разница между ступенями — \
                 свойство моделей, а не случайность сэмплинга."
            ),
        ),
        // «Все решили всё» и «все не решили ничего» — одинаково ничейный
        // счёт, но чинится он по-разному, и читателю нужно знать, куда
        // упёрлась шкала.
        std::cmp::Ordering::Equal => {
            let why = if strong == total {
                "проба слишком лёгкая: её берут все ступени, включая слабую"
            } else if strong == 0 {
                "проба слишком трудная: её не берёт никто, включая сильную"
            } else {
                "ступени встали на одну отметку в середине шкалы"
            };
            (
                Verdict::Flat,
                format!(
                    "Модели те, что заказывали, но проба их не разделила: {table} — {why}. \
                     Вывод о «сильнее/слабее» из неё не следует."
                ),
            )
        }
        std::cmp::Ordering::Less => (
            Verdict::Inverted,
            format!(
                "Модели те, что заказывали, но слабая ступень решила больше сильной \
                 ({table}). Заявленный порядок лестницы этой пробой не подтверждается."
            ),
        ),
    }
}

/// Совпадает ли модель, которую вернула площадка, с запрошенной.
///
/// Сравнение нестрогое ровно в двух местах: Synthetic отрезает свой префикс
/// `hf:`, а роутеры иногда добавляют или убирают имя вендора. Всё остальное
/// расхождение — это подмена модели, и её нужно увидеть.
fn same_model(requested: &str, served: &str) -> bool {
    if served.is_empty() {
        return false;
    }
    let norm = |s: &str| {
        s.trim()
            .to_lowercase()
            .trim_start_matches("hf:")
            .to_string()
    };
    let (a, b) = (norm(requested), norm(served));
    if a == b {
        return true;
    }
    let tail = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    tail(&a) == tail(&b)
}

/// Верен ли ответ. Берём **последнее** число в тексте: модели любят закончить
/// выводом («…значит, 3»), а начать с повтора условия.
fn matches_answer(text: &str, expected: f64) -> bool {
    last_number(text).is_some_and(|n| (n - expected).abs() < 1e-6)
}

fn last_number(text: &str) -> Option<f64> {
    let mut found = None;
    let mut cur = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() || (c == '.' && !cur.is_empty()) || (c == ',' && !cur.is_empty()) {
            cur.push(if c == ',' { '.' } else { c });
        } else {
            if let Ok(v) = cur.trim_end_matches('.').parse::<f64>() {
                found = Some(v);
            }
            cur.clear();
        }
    }
    found
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

pub fn to_text(c: &LadderCheck) -> String {
    let label = match c.verdict {
        Verdict::Confirmed => "ЛЕСТНИЦА ПОДТВЕРЖДЕНА",
        Verdict::Substituted => "ПОДМЕНА МОДЕЛИ",
        Verdict::Flat => "СТУПЕНИ НЕРАЗЛИЧИМЫ",
        Verdict::Inverted => "ПОРЯДОК НЕ ПОДТВЕРЖДЁН",
    };
    let mut out = format!("{label}\n");
    for r in &c.rungs {
        out.push_str(&format!(
            "  {:<8} {:<28} {}/{}  ответы: {:?}{}\n",
            r.label,
            r.model,
            r.score,
            c.questions.len(),
            r.answers
                .iter()
                .map(|a| last_number(a).map(fmt_num).unwrap_or_else(|| "—".into()))
                .collect::<Vec<_>>(),
            if r.model_matches {
                String::new()
            } else {
                format!("  ⚠ площадка ответила от `{}`", r.served_model)
            }
        ));
    }
    out.push_str(&format!("  {}\n", c.explanation));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(tier: Tier, label: &str, score: usize, matches: bool) -> RungCheck {
        RungCheck {
            tier,
            label: label.into(),
            model: "asked".into(),
            served_model: if matches { "asked".into() } else { "other".into() },
            model_matches: matches,
            answers: vec![],
            correct: vec![],
            score,
            probe_ms: 0,
            probe_tokens: 0,
        }
    }

    fn v(weak: usize, strong: usize) -> Verdict {
        judge(&[
            rc(Tier::Weak, "слабая", weak, true),
            rc(Tier::Medium, "средняя", (weak + strong) / 2, true),
            rc(Tier::Strong, "сильная", strong, true),
        ])
        .0
    }

    #[test]
    fn a_stronger_top_rung_confirms_the_ladder() {
        assert_eq!(v(1, 4), Verdict::Confirmed);
    }

    #[test]
    fn equal_scores_prove_nothing_either_way() {
        assert_eq!(v(3, 3), Verdict::Flat);
    }

    /// Ничья на потолке и ничья на полу — разные диагнозы, и объяснение
    /// обязано их различать: иначе непонятно, какие пробы искать взамен.
    #[test]
    fn a_tie_says_which_end_of_the_scale_it_hit() {
        let full = PROBES.len();
        let ceiling = judge(&[
            rc(Tier::Weak, "слабая", full, true),
            rc(Tier::Medium, "средняя", full, true),
            rc(Tier::Strong, "сильная", full, true),
        ]);
        assert_eq!(ceiling.0, Verdict::Flat);
        assert!(ceiling.1.contains("слишком лёгкая"));

        let floor = judge(&[
            rc(Tier::Weak, "слабая", 0, true),
            rc(Tier::Medium, "средняя", 0, true),
            rc(Tier::Strong, "сильная", 0, true),
        ]);
        assert!(floor.1.contains("слишком трудная"));
    }

    #[test]
    fn a_weaker_top_rung_is_reported_not_hidden() {
        assert_eq!(v(4, 1), Verdict::Inverted);
    }

    /// Подмена важнее любых очков: если ответила не та модель, то и очки,
    /// и время, и цена — не про неё.
    #[test]
    fn substitution_outranks_a_perfect_score() {
        let (verdict, why) = judge(&[
            rc(Tier::Weak, "слабая", 0, true),
            rc(Tier::Medium, "средняя", 4, false),
            rc(Tier::Strong, "сильная", 4, true),
        ]);
        assert_eq!(verdict, Verdict::Substituted);
        assert!(why.contains("средняя"));
    }

    #[test]
    fn served_model_is_matched_up_to_prefixes_only() {
        assert!(same_model("qwen3.8-27b", "qwen3.8-27b"));
        assert!(same_model("hf:moonshotai/Kimi-K3", "moonshotai/Kimi-K3"));
        assert!(same_model("anthropic/claude-opus-5", "claude-opus-5"));
        // А вот это уже другая модель, а не другой префикс.
        assert!(!same_model("anthropic/claude-opus-5", "anthropic/claude-haiku-4.5"));
        assert!(!same_model("grok-4.6", "grok-4.3"));
        assert!(!same_model("grok-4.6", ""));
    }

    #[test]
    fn the_last_number_wins_because_models_restate_the_question() {
        assert!(matches_answer("В слове strawberry 3 буквы r", 3.0));
        assert!(matches_answer("Считаем: s-t-r-a-w... Ответ: 3", 3.0));
        assert!(matches_answer("9.11 против 9.9 — больше 9.9", 9.9));
        assert!(matches_answer("Ответ: 9,9", 9.9));
        assert!(!matches_answer("Ответ: 9.11", 9.9));
        assert!(!matches_answer("не знаю", 3.0));
    }

    /// Точка в конце предложения не должна превращать «20.» в мусор.
    #[test]
    fn trailing_punctuation_does_not_break_the_number() {
        assert!(matches_answer("Итого 20.", 20.0));
        assert!(matches_answer("Итого 20", 20.0));
    }

    #[test]
    fn every_probe_has_a_checkable_numeric_answer() {
        for p in &PROBES {
            assert!(p.question.contains("числом"), "{}", p.question);
            assert!(matches_answer(&fmt_num(p.answer), p.answer));
        }
    }

    #[test]
    fn text_report_names_the_verdict() {
        let c = LadderCheck {
            questions: vec!["q".into()],
            expected: vec!["3".into()],
            rungs: vec![rc(Tier::Weak, "слабая", 0, false)],
            verdict: Verdict::Substituted,
            explanation: "почему".into(),
        };
        assert!(to_text(&c).contains("ПОДМЕНА МОДЕЛИ"));
    }
}
