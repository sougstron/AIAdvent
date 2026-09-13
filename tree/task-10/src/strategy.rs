//! Диспетчер стратегий управления контекстом: единственное место, где
//! выбранная [`ContextStrategy`] превращается в «что именно уходит на провод».
//!
//! `Agent::wire_history` и `Agent::system_for_request` зовут [`apply`] и
//! больше ничего не знают про конкретные стратегии. Поэтому новая стратегия
//! — это новый вариант перечисления плюс одна ветка здесь, а не россыпь
//! `if` по агенту, TUI и CLI.
//!
//! Договорённость про два слота:
//!
//! * [`apply`] — сообщения, которые уедут как история (user/assistant).
//! * [`blocks`] — текстовые куски, которые уедут в **system** рядом с
//!   AGENTS.md (summary, факты). Это sticky-слот: он не ломает чередование
//!   ролей и не размножается по истории. Слоты разделены, потому что блоки
//!   не зависят от того, какую именно историю передадут на этом ходе.
//!
//! Ветки (`ContextStrategy::Branch`) сюда приходят уже разрешёнными: путь
//! активной ветки собирает `branch.rs`, а диспетчер видит его как обычную
//! плоскую историю. Соседние ветки в неё просто не попадают — в этом и
//! состоит вся изоляция.

use crate::api::{ChatMessage, Role};
use crate::compress::Compressor;
use crate::config::ContextStrategy;
use crate::facts::FactStore;

/// Sliding window: последние `n` сообщений, со сдвигом границы вправо до
/// ближайшего сообщения пользователя.
///
/// # Почему сдвиг именно вправо
///
/// Если окно открывается ответом ассистента, модель видит «ответ без
/// вопроса»: часть чат-шаблонов на этом ломается, а выглядит это как
/// галлюцинация контекста. Двигать границу **влево** нельзя — это значит
/// отправить больше, чем обещали в настройке. Поэтому двигаем вправо, то
/// есть отправляем не больше, а иногда меньше `n`.
///
/// # Инвариант
///
/// Последнее сообщение (текущий вопрос) выживает всегда: если после сдвига
/// окно оказалось пустым, берём ровно последнее сообщение как есть.
///
/// Осиротевших `tool_call`/`tool_result` пар здесь не бывает: история этого
/// приложения состоит только из user/assistant, инструментов у агента нет.
/// Классическая беда скользящего окна к нам поэтому не относится.
pub fn window(history: &[ChatMessage], n: usize) -> &[ChatMessage] {
    if history.is_empty() {
        return history;
    }
    let start = history.len().saturating_sub(n.max(1));
    let mut i = start;
    while i < history.len() && history[i].role != Role::User {
        i += 1;
    }
    if i >= history.len() {
        // Весь хвост — реплики ассистента. Текущее сообщение важнее
        // аккуратной границы.
        return &history[history.len() - 1..];
    }
    &history[i..]
}

/// Какие сообщения реально уедут провайдеру.
pub fn apply<'a>(
    strategy: ContextStrategy,
    history: &'a [ChatMessage],
    compressor: &Compressor,
    keep_recent: usize,
) -> &'a [ChatMessage] {
    match strategy {
        // Вся история. Накопленное summary и факты при этом никуда не
        // деваются — просто не участвуют в запросе.
        ContextStrategy::Off => history,
        ContextStrategy::Summary => compressor.wire(history, strategy),
        // Окно ничего не помнит: что уехало за границу — потеряно. Это не
        // недоделка, а смысл стратегии, и проверка (`--verify-context
        // window`) требует, чтобы старый факт честно НЕ вспомнился. Факты
        // ездят тем же окном, только с блоком памяти в system.
        ContextStrategy::Window | ContextStrategy::Facts => window(history, keep_recent),
        // Путь активной ветки уже пришёл сюда как `history`.
        ContextStrategy::Branch => history,
    }
}

/// Sticky-блоки стратегии, которые уезжают в **system**.
///
/// Намеренно не зависят от истории: это состояние самой стратегии (summary,
/// факты), а не выборка из диалога. Поэтому `Agent::system_for_request`
/// может собрать их, не зная, какую именно историю ему сейчас передадут.
pub fn blocks(strategy: ContextStrategy, compressor: &Compressor, facts: &FactStore) -> Vec<String> {
    match strategy {
        ContextStrategy::Summary => compressor.block().into_iter().collect(),
        ContextStrategy::Facts => facts.block().into_iter().collect(),
        ContextStrategy::Off | ContextStrategy::Window | ContextStrategy::Branch => Vec::new(),
    }
}

/// Строка состояния выбранной стратегии — то, что видно в футере и в
/// `/strategy show`. `branch_line` приходит от владельца дерева (`Session`),
/// потому что дерево живёт не в агенте.
pub fn status(
    strategy: ContextStrategy,
    history: &[ChatMessage],
    compressor: &Compressor,
    facts: &FactStore,
    keep_recent: usize,
    branch_line: Option<&str>,
) -> String {
    let len = history.len();
    match strategy {
        ContextStrategy::Off => format!("strategy=off (на проводе все {len} сообщений)"),
        ContextStrategy::Summary => compressor.status(strategy, len),
        ContextStrategy::Window => format!(
            "strategy=window keep={keep_recent}: на проводе {}/{len} сообщений",
            window(history, keep_recent).len()
        ),
        ContextStrategy::Facts => format!(
            "strategy=facts keep={keep_recent}: на проводе {}/{len} сообщений, фактов {}, обновлений {}",
            window(history, keep_recent).len(),
            facts.len(),
            facts.updates()
        ),
        ContextStrategy::Branch => match branch_line {
            Some(line) => format!("strategy=branch {line}"),
            None => format!("strategy=branch: на проводе {len} сообщений активной ветки"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Чередование user/assistant, как в настоящем диалоге.
    fn hist(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    ChatMessage::user(format!("u{i}"))
                } else {
                    ChatMessage::assistant(format!("a{i}"))
                }
            })
            .collect()
    }

    #[test]
    fn window_keeps_the_last_n_when_the_boundary_is_already_a_user_turn() {
        let h = hist(10);
        let w = window(&h, 4);
        assert_eq!(w.len(), 4);
        assert_eq!(w[0].content, "u6");
        assert_eq!(w.last().unwrap().content, "a9");
    }

    #[test]
    fn window_snaps_the_boundary_right_never_left() {
        let h = hist(10);
        // n=5 открыло бы окно на a5 — ответ без вопроса. Сдвигаемся на u6,
        // то есть отправляем 4 сообщения, а не 6.
        let w = window(&h, 5);
        assert_eq!(w.len(), 4);
        assert_eq!(w[0].role, Role::User);
        assert!(w.len() <= 5, "сдвиг влево отправил бы больше обещанного");
    }

    #[test]
    fn window_always_keeps_the_current_question() {
        let h = hist(11);
        for n in 1..=20 {
            let w = window(&h, n);
            assert!(!w.is_empty(), "n={n}");
            assert_eq!(w.last().unwrap().content, h.last().unwrap().content, "n={n}");
        }
        // Даже когда весь хвост — ассистент и снапить некуда.
        let tail_assistants = vec![
            ChatMessage::user("u"),
            ChatMessage::assistant("a1"),
            ChatMessage::assistant("a2"),
        ];
        let w = window(&tail_assistants, 2);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].content, "a2");
    }

    #[test]
    fn window_degenerate_inputs_do_not_panic() {
        assert!(window(&[], 5).is_empty());
        let h = hist(3);
        // n больше истории — no-op.
        assert_eq!(window(&h, 99).len(), 3);
        // n=0 клампится до 1 (Settings::clamp делает то же самое раньше).
        assert_eq!(window(&h, 0).len(), 1);
    }

    #[test]
    fn off_sends_everything_and_adds_no_blocks() {
        let h = hist(12);
        let mut c = Compressor::new();
        c.apply("СВОДКА".into(), 6, 100);
        let mut f = FactStore::new();
        f.set("цель", "стенд");
        assert_eq!(apply(ContextStrategy::Off, &h, &c, 4).len(), 12);
        assert!(blocks(ContextStrategy::Off, &c, &f).is_empty());
    }

    #[test]
    fn summary_cuts_the_covered_prefix_and_ships_the_summary_block() {
        let h = hist(12);
        let mut c = Compressor::new();
        c.apply("СВОДКА".into(), 6, 100);
        let wire = apply(ContextStrategy::Summary, &h, &c, 4);
        assert_eq!(wire.len(), 6);
        let b = blocks(ContextStrategy::Summary, &c, &FactStore::new());
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("СВОДКА"));
    }

    #[test]
    fn window_strategy_ships_no_block_at_all() {
        let h = hist(12);
        let mut c = Compressor::new();
        c.apply("СВОДКА".into(), 6, 100);
        let mut f = FactStore::new();
        f.set("цель", "стенд");
        let wire = apply(ContextStrategy::Window, &h, &c, 4);
        assert_eq!(wire.len(), 4);
        assert_eq!(wire[0].content, "u8");
        // Ни summary, ни фактов: окно — это именно «остальное отбрасываем».
        assert!(blocks(ContextStrategy::Window, &c, &f).is_empty());
    }

    #[test]
    fn facts_strategy_is_the_same_window_plus_the_facts_block() {
        let h = hist(12);
        let mut f = FactStore::new();
        f.set("цель", "стенд");
        let c = Compressor::new();
        let plain = apply(ContextStrategy::Window, &h, &c, 4);
        let with_facts = apply(ContextStrategy::Facts, &h, &c, 4);
        assert_eq!(plain.len(), with_facts.len());
        let b = blocks(ContextStrategy::Facts, &c, &f);
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("цель: стенд"));
        // Пустая память — и блока нет, разница с window исчезает.
        assert!(blocks(ContextStrategy::Facts, &c, &FactStore::new()).is_empty());
    }

    #[test]
    fn branch_ships_the_resolved_path_whole() {
        let h = hist(7);
        let c = Compressor::new();
        assert_eq!(apply(ContextStrategy::Branch, &h, &c, 2).len(), 7, "путь ветки не режется окном");
        assert!(blocks(ContextStrategy::Branch, &c, &FactStore::new()).is_empty());
    }

    #[test]
    fn status_names_the_strategy_and_the_wire_size() {
        let h = hist(12);
        let c = Compressor::new();
        let mut f = FactStore::new();
        f.set("цель", "стенд");
        assert!(status(ContextStrategy::Off, &h, &c, &f, 4, None).contains("все 12"));
        assert!(status(ContextStrategy::Window, &h, &c, &f, 4, None).contains("4/12"));
        let facts_line = status(ContextStrategy::Facts, &h, &c, &f, 4, None);
        assert!(facts_line.contains("фактов 1"));
        let branch_line = status(ContextStrategy::Branch, &h, &c, &f, 4, Some("ветка B, 3 сообщения"));
        assert!(branch_line.contains("ветка B"));
    }
}
