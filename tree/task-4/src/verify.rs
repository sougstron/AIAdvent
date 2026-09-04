//! Доказательство того, что `temperature` вообще доезжает до сэмплера.
//!
//! Без этой проверки всё приложение — гадание: если endpoint молча выбрасывает
//! параметр, три «разные» температуры дадут три случайно различающихся ответа,
//! и разницу легко принять за работу рычага. Так и вышло с подписочным Z.AI.
//!
//! Проверка causal, а не «тексты отличаются»:
//!
//! * берём вопрос с широким, но дискретным распределением ответа
//!   («случайное число от 1 до 100») и коротким ответом — один-два токена;
//! * `temperature = 0` — жадный выбор, значит **все** прогоны обязаны совпасть;
//! * `temperature = 2.0` — сэмплинг из размазанного распределения, значит
//!   разброс обязан вырасти.
//!
//! Вердикт `Honored` выдаётся только когда обе половины сигнатуры сошлись.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::api::{Client, Provider, Request, Res};

const PROBE: &str = "Назови одно случайное целое число от 1 до 100. Ответь только числом.";
const COLD: f64 = 0.0;
const HOT: f64 = 2.0;
const SAMPLES: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// temperature=0 детерминирован и temperature=2.0 разбросан — рычаг работает.
    Honored,
    /// temperature=0 не детерминирован — параметр до сэмплера не доходит.
    Ignored,
    /// Обе температуры дали одно и то же значение: распределение слишком узкое,
    /// проба ничего не доказывает ни в одну сторону.
    Inconclusive,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempCheck {
    pub provider: String,
    pub model: String,
    pub samples: usize,
    pub cold_temperature: f64,
    pub hot_temperature: f64,
    pub cold_answers: Vec<String>,
    pub hot_answers: Vec<String>,
    /// Сколько различных значений выдал каждый режим.
    pub cold_distinct: usize,
    pub hot_distinct: usize,
    pub verdict: Verdict,
    pub explanation: String,
}

pub fn check_temperature(client: &Client) -> Res<TempCheck> {
    let model = client.provider.answer_model().to_string();
    let cold = sample(client, &model, COLD)?;
    let hot = sample(client, &model, HOT)?;
    Ok(judge(client.provider, model, cold, hot))
}

fn sample(client: &Client, model: &str, temperature: f64) -> Res<Vec<String>> {
    let req = Request {
        model: model.to_string(),
        system: None,
        prompt: PROBE.into(),
        temperature: Some(temperature),
        // Хватает на короткий ответ; у рассуждающих моделей thinking выключен ниже.
        max_tokens: 24,
        thinking: false,
    };
    let mut out = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        let reply = client
            .complete(&req)
            .map_err(|e| format!("проба temperature={temperature}, прогон {}: {e}", i + 1))?;
        out.push(reply.content.trim().to_string());
    }
    Ok(out)
}

fn judge(provider: Provider, model: String, cold: Vec<String>, hot: Vec<String>) -> TempCheck {
    let cold_distinct = cold.iter().collect::<BTreeSet<_>>().len();
    let hot_distinct = hot.iter().collect::<BTreeSet<_>>().len();

    let (verdict, explanation) = if cold_distinct > 1 {
        (
            Verdict::Ignored,
            format!(
                "При temperature=0 декодирование обязано быть жадным, но {SAMPLES} прогонов дали \
                 {cold_distinct} разных ответа. Значит параметр до сэмплера не доходит — \
                 различия между температурами на этом провайдере случайны и ничего не доказывают."
            ),
        )
    } else if hot_distinct > cold_distinct {
        (
            Verdict::Honored,
            format!(
                "temperature=0 дала один и тот же ответ во всех {SAMPLES} прогонах, \
                 а temperature={HOT} — {hot_distinct} разных. Обе половины причинной \
                 сигнатуры сошлись: параметр применяется."
            ),
        )
    } else {
        (
            Verdict::Inconclusive,
            format!(
                "И temperature=0, и temperature={HOT} дали по одному значению. Распределение \
                 у модели слишком узкое для этой пробы — она не доказывает ни работу параметра, \
                 ни его игнорирование."
            ),
        )
    };

    TempCheck {
        provider: provider.id().to_string(),
        model,
        samples: SAMPLES,
        cold_temperature: COLD,
        hot_temperature: HOT,
        cold_answers: cold,
        hot_answers: hot,
        cold_distinct,
        hot_distinct,
        verdict,
        explanation,
    }
}

pub fn to_text(c: &TempCheck) -> String {
    let label = match c.verdict {
        Verdict::Honored => "ПРИМЕНЯЕТСЯ",
        Verdict::Ignored => "ИГНОРИРУЕТСЯ",
        Verdict::Inconclusive => "НЕ ОПРЕДЕЛЕНО",
    };
    format!(
        "temperature на {} / {}: {label}\n  temperature={} → {:?} ({} различных)\n  \
         temperature={} → {:?} ({} различных)\n  {}\n",
        c.provider,
        c.model,
        c.cold_temperature,
        c.cold_answers,
        c.cold_distinct,
        c.hot_temperature,
        c.hot_answers,
        c.hot_distinct,
        c.explanation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(cold: &[&str], hot: &[&str]) -> Verdict {
        judge(
            Provider::Zai,
            "m".into(),
            cold.iter().map(|s| s.to_string()).collect(),
            hot.iter().map(|s| s.to_string()).collect(),
        )
        .verdict
    }

    #[test]
    fn deterministic_cold_and_spread_hot_is_honored() {
        assert_eq!(v(&["53", "53", "53"], &["12", "77", "53"]), Verdict::Honored);
    }

    #[test]
    fn spread_at_zero_means_the_parameter_never_lands() {
        // Ровно то, что вернул подписочный Z.AI.
        assert_eq!(v(&["42", "47", "73"], &["47", "73", "42"]), Verdict::Ignored);
    }

    #[test]
    fn hot_no_wider_than_cold_proves_nothing() {
        assert_eq!(v(&["53", "53"], &["53", "53"]), Verdict::Inconclusive);
    }

    #[test]
    fn a_wider_hot_spread_is_required_not_just_a_different_value() {
        // Одно значение в обоих режимах, но разное: сэмплинг не показан.
        assert_eq!(v(&["53", "53"], &["77", "77"]), Verdict::Inconclusive);
    }

    #[test]
    fn text_report_names_the_verdict() {
        let c = judge(
            Provider::Zai,
            "glm".into(),
            vec!["42".into(), "47".into()],
            vec!["42".into(), "47".into()],
        );
        assert!(to_text(&c).contains("ИГНОРИРУЕТСЯ"));
    }
}
