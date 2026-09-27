//! Управление контекстом: сжатие истории в бегущее summary.
//!
//! # Что это делает
//!
//! Обычный чат отправляет провайдеру всю историю на каждом ходе, поэтому
//! `prompt_tokens` растёт квадратично по длине разговора. Стратегия
//! [`ContextStrategy::Summary`](crate::config::ContextStrategy) держит
//! **последние N сообщений как есть**, а всё, что старше, заменяет одним
//! текстовым summary.
//!
//! # Как именно
//!
//! * История — плоский `Vec<ChatMessage>` (user/assistant), она же то, что
//!   лежит в сессии и рисуется в TUI. Сжатие её **не трогает**: на экране и
//!   на диске остаётся полный разговор. Меняется только то, что уходит на
//!   провод.
//! * [`Compressor`] хранит `summary` и `covered` — сколько первых сообщений
//!   истории это summary уже описывает. На провод уходит
//!   `history[covered..]`, а summary подставляется в **system**-сообщение
//!   (там же, где AGENTS.md — см. `context.rs`: у z.ai это sticky-слот, и
//!   он не ломает чередование user/assistant).
//! * Сворачиваем **чанками** по [`Policy::every`] сообщений: пока
//!   «свёртываемых» (всё, кроме последних `keep_recent`) не накопилось на
//!   целый чанк, никакого запроса к модели не делается. Это ровно
//!   формулировка «summary каждые 10 сообщений».
//! * Summary **инкрементальное**: очередной вызов получает предыдущий текст
//!   summary плюс новый чанк и возвращает обновлённый текст. Так стоимость
//!   свёртки не растёт вместе с историей.
//!
//! # Чего здесь нет
//!
//! Модуль не ходит в сеть. Он решает *когда* и *что* сворачивать и как
//! собрать промпт; сам вызов модели делает `Agent::fold_history`, потому что
//! эндпоинт и настройки принадлежат агенту.

use crate::api::ChatMessage;
use crate::config::{ContextStrategy, Settings};
use serde::{Deserialize, Serialize};

/// Системный промпт свёртки. Просим факты, а не пересказ настроения:
/// summary подставляется вместо реальных реплик, поэтому потерянное здесь
/// теряется для модели навсегда.
pub const SUMMARY_SYSTEM: &str = "Ты сжимаешь историю диалога для экономии контекста. \
Верни ОБНОВЛЁННОЕ summary: факты, имена, числа, решения, договорённости и открытые вопросы — \
всё, что понадобится, чтобы продолжить разговор без доступа к исходным репликам. \
Пиши плотным списком коротких пунктов, без вступлений и без оценок. \
Не выдумывай того, чего не было. Максимум 200 слов.";

/// Заголовок блока, который уходит в system-сообщение.
pub const SUMMARY_HEADER: &str = "## Сводка предыдущей части диалога (сжатая история)";

/// Правило «сколько держим как есть» и «как часто сворачиваем».
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Последние `keep_recent` сообщений всегда уходят дословно.
    pub keep_recent: usize,
    /// Сворачиваем чанками такого размера.
    pub every: usize,
}

impl Policy {
    pub fn from_settings(s: &Settings) -> Policy {
        Policy {
            keep_recent: s.keep_recent.max(1),
            every: s.summarize_every.max(1),
        }
    }
}

/// Бегущее summary одной сессии.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compressor {
    /// Текст summary. Пусто — сжатия ещё не было.
    #[serde(default)]
    summary: String,
    /// Сколько первых сообщений истории покрыто этим summary.
    #[serde(default)]
    covered: usize,
    /// Сколько раз модель вызывалась для свёртки.
    #[serde(default)]
    folds: usize,
    /// Сколько символов исходных реплик ушло под summary (нарастающим итогом).
    #[serde(default)]
    folded_chars: usize,
}

impl Compressor {
    pub fn new() -> Compressor {
        Compressor::default()
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn covered(&self) -> usize {
        self.covered
    }

    pub fn folds(&self) -> usize {
        self.folds
    }

    pub fn folded_chars(&self) -> usize {
        self.folded_chars
    }

    pub fn is_empty(&self) -> bool {
        self.summary.trim().is_empty() || self.covered == 0
    }

    pub fn reset(&mut self) {
        *self = Compressor::new();
    }

    /// Сколько сообщений должно оказаться покрыто после ближайшей свёртки,
    /// или `None`, если сворачивать пока нечего.
    ///
    /// Свёртываемыми считаются все, кроме последних `keep_recent`. Сворачиваем
    /// только целыми чанками по `every`, чтобы не дёргать модель на каждом
    /// ходе.
    pub fn due(&self, history_len: usize, policy: Policy) -> Option<usize> {
        let foldable = history_len.saturating_sub(policy.keep_recent);
        let fresh = foldable.saturating_sub(self.covered);
        if fresh < policy.every {
            return None;
        }
        Some(self.covered + (fresh / policy.every) * policy.every)
    }

    /// Записать результат свёртки: `target` — новое значение `covered`.
    pub fn apply(&mut self, summary: String, target: usize, folded_chars: usize) {
        self.summary = summary.trim().to_string();
        self.covered = target;
        self.folds += 1;
        self.folded_chars += folded_chars;
    }

    /// Блок для system-сообщения, если сжатие активно и summary есть.
    pub fn block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(format!(
            "{SUMMARY_HEADER}\n\
             Ниже — сжатый пересказ первых {} сообщений этого разговора. \
             Считай его частью диалога: исходные реплики уже не передаются.\n\n{}",
            self.covered, self.summary
        ))
    }

    /// Что реально уходит на провод для данной истории.
    ///
    /// Только для стратегии `Summary`: хвост после `covered` (само summary
    /// уезжает отдельно, в system). Для любой другой стратегии summary ни при
    /// чём, и история возвращается как есть — что с ней делать дальше, решает
    /// `strategy::apply`.
    pub fn wire<'a>(
        &self,
        history: &'a [ChatMessage],
        strategy: ContextStrategy,
    ) -> &'a [ChatMessage] {
        if strategy != ContextStrategy::Summary || self.is_empty() {
            return history;
        }
        let cut = self.covered.min(history.len());
        &history[cut..]
    }

    /// Одна строка про состояние summary. Для стратегий, которым summary не
    /// нужен, строку собирает `strategy::status` — здесь только честное
    /// «эта стратегия сюда не ходит».
    pub fn status(&self, strategy: ContextStrategy, history_len: usize) -> String {
        match strategy {
            ContextStrategy::Off => "compress=off".into(),
            ContextStrategy::Summary if self.is_empty() => {
                format!("compress=summary (пока без свёртки, сообщений {history_len})")
            }
            ContextStrategy::Summary => format!(
                "compress=summary свёрнуто {}/{} сообщений, summary {} симв. вместо {}, свёрток {}",
                self.covered,
                history_len,
                self.summary.chars().count(),
                self.folded_chars,
                self.folds
            ),
            other => format!("compress=off (стратегия {other} не сворачивает историю)"),
        }
    }
}

/// Чанк истории в виде текста для промпта свёртки.
pub fn transcript(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|m| format!("{}: {}", m.role.as_str(), m.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Пользовательское сообщение запроса свёртки: предыдущее summary + новый чанк.
pub fn fold_prompt(previous: &str, chunk: &[ChatMessage]) -> String {
    let previous = previous.trim();
    let head = if previous.is_empty() {
        "Предыдущего summary нет — это первая свёртка.".to_string()
    } else {
        format!("ТЕКУЩЕЕ SUMMARY:\n{previous}")
    };
    format!(
        "{head}\n\nНОВЫЕ СООБЩЕНИЯ (добавь их в summary):\n{}\n\nВерни только обновлённое summary.",
        transcript(chunk)
    )
}

/// Суммарная длина сообщений в символах.
pub fn chars_of(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|m| m.content.chars().count()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    ChatMessage::user(format!("вопрос {i}"))
                } else {
                    ChatMessage::assistant(format!("ответ {i}"))
                }
            })
            .collect()
    }

    const P: Policy = Policy {
        keep_recent: 6,
        every: 10,
    };

    #[test]
    fn nothing_to_fold_until_a_whole_chunk_is_old_enough() {
        let c = Compressor::new();
        for len in 0..=15 {
            assert_eq!(c.due(len, P), None, "len={len}");
        }
        assert_eq!(c.due(16, P), Some(10));
    }

    #[test]
    fn folds_in_whole_chunks_and_never_eats_the_recent_tail() {
        let c = Compressor::new();
        // 27 сообщений: 21 свёртываемых -> два полных чанка = 20.
        assert_eq!(c.due(27, P), Some(20));
        let mut c = c;
        c.apply("s".into(), 20, 100);
        assert_eq!(c.due(27, P), None);
        assert_eq!(c.due(36, P), Some(30));
    }

    #[test]
    fn wire_drops_only_the_covered_prefix() {
        let history = hist(30);
        let mut c = Compressor::new();
        assert_eq!(c.wire(&history, ContextStrategy::Summary).len(), 30);
        c.apply("сводка".into(), 20, 1);
        let wire = c.wire(&history, ContextStrategy::Summary);
        assert_eq!(wire.len(), 10);
        assert_eq!(wire[0].content, history[20].content);
        // Off — стратегия выключена, история идёт целиком даже с summary.
        assert_eq!(c.wire(&history, ContextStrategy::Off).len(), 30);
    }

    #[test]
    fn wire_survives_a_history_shorter_than_covered() {
        let mut c = Compressor::new();
        c.apply("сводка".into(), 20, 1);
        let short = hist(3);
        assert!(c.wire(&short, ContextStrategy::Summary).is_empty());
    }

    #[test]
    fn block_is_none_until_a_summary_exists() {
        let mut c = Compressor::new();
        assert!(c.block().is_none());
        c.apply("   ".into(), 10, 0);
        assert!(
            c.block().is_none(),
            "пустое summary не должно подставляться"
        );
        c.apply("факт: пароль QUINCE".into(), 10, 500);
        let block = c.block().unwrap();
        assert!(block.contains(SUMMARY_HEADER));
        assert!(block.contains("QUINCE"));
        assert!(block.contains("10 сообщений"));
    }

    #[test]
    fn fold_prompt_carries_previous_summary_and_the_chunk() {
        let chunk = hist(2);
        let first = fold_prompt("", &chunk);
        assert!(first.contains("первая свёртка"));
        assert!(first.contains("вопрос 0"));
        assert!(first.contains("user:"));
        let next = fold_prompt("старое summary", &chunk);
        assert!(next.contains("старое summary"));
        assert!(next.contains("ответ 1"));
    }

    #[test]
    fn reset_clears_everything() {
        let mut c = Compressor::new();
        c.apply("s".into(), 10, 5);
        c.reset();
        assert!(c.is_empty());
        assert_eq!(c.covered(), 0);
        assert_eq!(c.folds(), 0);
        assert_eq!(c.folded_chars(), 0);
    }

    #[test]
    fn policy_never_degenerates_to_zero() {
        let s = Settings {
            keep_recent: 0,
            summarize_every: 0,
            ..Settings::default()
        };
        let p = Policy::from_settings(&s);
        assert_eq!(p.keep_recent, 1);
        assert_eq!(p.every, 1);
        // every=1 всё ещё сворачивает вперёд, а не зацикливается на месте.
        let c = Compressor::new();
        assert_eq!(c.due(5, p), Some(4));
    }

    #[test]
    fn status_reports_off_and_the_savings() {
        let mut c = Compressor::new();
        assert_eq!(c.status(ContextStrategy::Off, 40), "compress=off");
        assert!(c
            .status(ContextStrategy::Summary, 4)
            .contains("без свёртки"));
        c.apply("короткая сводка".into(), 20, 4000);
        let s = c.status(ContextStrategy::Summary, 26);
        assert!(s.contains("20/26"));
        assert!(s.contains("4000"));
    }
}
