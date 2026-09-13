//! Стратегия «sticky facts»: key-value память диалога.
//!
//! # Что это делает
//!
//! Окно (`ContextStrategy::Window`) честно забывает всё, что уехало за
//! границу: цель разговора, ограничения, договорённости. Стратегия
//! [`ContextStrategy::Facts`](crate::config::ContextStrategy) добавляет к
//! тому же окну отдельный блок `facts` — короткий список `ключ: значение`,
//! который живёт вне истории и уезжает в **system** (там же, где AGENTS.md и
//! summary: это sticky-слот, он не ломает чередование user/assistant и не
//! размножается по истории).
//!
//! # Как обновляется
//!
//! После каждого сообщения пользователя — отдельным запросом к модели
//! (`Agent::update_facts`, по образцу `Agent::fold_history`). Экстрактор
//! видит **не всю историю**, а текущие факты плюс последнюю пару реплик:
//! иначе стоимость памяти растёт квадратично и стратегия теряет смысл.
//!
//! # Почему операции, а не «допиши строчку»
//!
//! Append-only память копит противоречия: «бюджет 200 тысяч» и «бюджет 150
//! тысяч» лежат рядом, и модель выбирает случайно. Поэтому экстрактор
//! возвращает список операций `add` / `update` / `delete` (пустой список =
//! ничего не менять) — это разрешение конфликтов, а не дописывание.
//! `add` и `update` здесь одно и то же действие (upsert): модель регулярно
//! называет `add` для ключа, который уже есть, и отдельная семантика только
//! плодила бы дубликаты.
//!
//! # Потолок
//!
//! Без предела блок фактов сам становится тем, от чего мы уходили, поэтому
//! фактов не больше [`MAX_FACTS`], значение — не длиннее
//! [`MAX_VALUE_CHARS`], весь блок — не длиннее [`MAX_BLOCK_CHARS`].
//! Переполнение вытесняет те факты, которых дольше всего не касались.

use serde::{Deserialize, Serialize};

use crate::api::ChatMessage;

/// Сколько фактов помещается в памяти.
pub const MAX_FACTS: usize = 40;
/// Предел длины одного значения.
pub const MAX_VALUE_CHARS: usize = 200;
/// Предел длины всего блока, который уезжает в system.
pub const MAX_BLOCK_CHARS: usize = 2000;

/// Заголовок блока в system-сообщении.
pub const FACTS_HEADER: &str = "## Факты (key-value память диалога)";

/// Системный промпт экстрактора. Просим строгий JSON и операции, а не прозу:
/// разобрать прозу нельзя, а потеря хода из-за неразобранного ответа — это
/// потерянная память.
pub const FACTS_SYSTEM: &str = "Ты ведёшь key-value память диалога: цели, ограничения, предпочтения, \
решения и договорённости пользователя. Тебе дают текущие факты и последние реплики. \
Верни ТОЛЬКО JSON вида {\"ops\":[{\"op\":\"add|update|delete\",\"key\":\"...\",\"value\":\"...\"}]}. \
op=add — новый факт, op=update — исправить значение существующего ключа, op=delete — факт больше не верен \
(value можно не указывать). Если менять нечего, верни {\"ops\":[]}. \
Ключ — короткое существительное в нижнем регистре (например \"цель\", \"бюджет\", \"срок\", \"стек\"). \
Значение — одна строка до 200 символов. Не выдумывай того, чего не было, не записывай болтовню \
и не дублируй ключи. Без пояснений, без markdown-заборов.";

/// Один факт памяти.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub key: String,
    pub value: String,
    /// Номер обновления, на котором факт последний раз трогали — по нему
    /// вытесняем при переполнении.
    #[serde(default)]
    pub turn: usize,
}

/// Что именно сделало одно обновление памяти.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FactsDelta {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub evicted: usize,
}

impl FactsDelta {
    pub fn touched(&self) -> usize {
        self.added + self.updated + self.deleted
    }

    pub fn line(&self) -> String {
        format!(
            "+{} ~{} -{}{}",
            self.added,
            self.updated,
            self.deleted,
            if self.evicted > 0 {
                format!(" (вытеснено {})", self.evicted)
            } else {
                String::new()
            }
        )
    }
}

/// Операция над памятью, как её вернул экстрактор.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// add и update — одно действие: положить значение по ключу.
    Upsert {
        key: String,
        value: String,
    },
    Delete {
        key: String,
    },
}

/// Key-value память одного диалога (или одной ветки — см. `branch.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactStore {
    #[serde(default)]
    facts: Vec<Fact>,
    /// Сколько раз память обновлялась (в том числе вручную).
    #[serde(default)]
    updates: usize,
    /// Сколько раз экстрактор ходил к модели.
    #[serde(default)]
    extractions: usize,
    /// Последняя ошибка разбора ответа экстрактора, если была.
    #[serde(default)]
    last_error: Option<String>,
}

impl FactStore {
    pub fn new() -> FactStore {
        FactStore::default()
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub fn updates(&self) -> usize {
        self.updates
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn note_extraction(&mut self) {
        self.extractions += 1;
    }

    pub fn note_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    pub fn reset(&mut self) {
        *self = FactStore::new();
    }

    /// Ключи памяти — живой источник автокомплита `/facts del`.
    pub fn keys(&self) -> Vec<&str> {
        self.facts.iter().map(|f| f.key.as_str()).collect()
    }

    fn position_of(&self, key: &str) -> Option<usize> {
        let key = normalize_key(key);
        self.facts.iter().position(|f| normalize_key(&f.key) == key)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.position_of(key).map(|i| self.facts[i].value.as_str())
    }

    /// Ручная правка (`/facts set`). Возвращает `true`, если факт новый.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        self.updates += 1;
        let turn = self.updates;
        self.upsert(key, value, turn)
    }

    fn upsert(&mut self, key: &str, value: &str, turn: usize) -> bool {
        let key = key.trim();
        let value = clip_value(value);
        match self.position_of(key) {
            Some(i) => {
                self.facts[i].value = value;
                self.facts[i].turn = turn;
                false
            }
            None => {
                self.facts.push(Fact {
                    key: key.to_string(),
                    value,
                    turn,
                });
                true
            }
        }
    }

    /// Ручное удаление (`/facts del`).
    pub fn remove(&mut self, key: &str) -> bool {
        self.updates += 1;
        match self.position_of(key) {
            Some(i) => {
                self.facts.remove(i);
                true
            }
            None => false,
        }
    }

    /// Применить пачку операций экстрактора. Пустой список — законный NOOP.
    pub fn apply_ops(&mut self, ops: &[Op]) -> FactsDelta {
        self.updates += 1;
        self.last_error = None;
        let turn = self.updates;
        let mut delta = FactsDelta::default();
        for op in ops {
            match op {
                Op::Upsert { key, value } => {
                    if key.trim().is_empty() || value.trim().is_empty() {
                        continue;
                    }
                    if self.upsert(key, value, turn) {
                        delta.added += 1;
                    } else {
                        delta.updated += 1;
                    }
                }
                Op::Delete { key } => {
                    if let Some(i) = self.position_of(key) {
                        self.facts.remove(i);
                        delta.deleted += 1;
                    }
                }
            }
        }
        delta.evicted = self.evict_overflow();
        delta
    }

    /// Вытеснение по «давно не трогали»: сначала лишние по количеству, потом
    /// пока рендер блока не влезет в [`MAX_BLOCK_CHARS`].
    fn evict_overflow(&mut self) -> usize {
        let mut evicted = 0;
        while self.facts.len() > MAX_FACTS
            || (!self.facts.is_empty() && self.body().chars().count() > MAX_BLOCK_CHARS)
        {
            let Some(oldest) = self
                .facts
                .iter()
                .enumerate()
                .min_by_key(|(i, f)| (f.turn, *i))
                .map(|(i, _)| i)
            else {
                break;
            };
            self.facts.remove(oldest);
            evicted += 1;
        }
        evicted
    }

    /// Тело блока: детерминированный порядок (по ключу), чтобы один и тот же
    /// набор фактов давал один и тот же промпт.
    fn body(&self) -> String {
        let mut sorted: Vec<&Fact> = self.facts.iter().collect();
        sorted.sort_by(|a, b| normalize_key(&a.key).cmp(&normalize_key(&b.key)));
        sorted
            .iter()
            .map(|f| format!("- {}: {}", f.key.trim(), f.value.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Блок для system-сообщения. Пустая память — блока нет.
    pub fn block(&self) -> Option<String> {
        if self.facts.is_empty() {
            return None;
        }
        Some(format!(
            "{FACTS_HEADER}\n\
             Это выжимка из уже не передаваемой части разговора. Считай её частью диалога.\n\n{}",
            self.body()
        ))
    }

    /// Что показывает `/facts` и футер.
    pub fn listing(&self) -> String {
        let mut lines = vec![format!(
            "фактов {}/{}, обновлений {}, вызовов экстрактора {}",
            self.facts.len(),
            MAX_FACTS,
            self.updates,
            self.extractions
        )];
        if let Some(err) = self.last_error() {
            lines.push(format!("последняя ошибка разбора: {err}"));
        }
        if self.facts.is_empty() {
            lines.push("память пуста".into());
        } else {
            lines.push(String::new());
            lines.push(self.body());
        }
        lines.join("\n")
    }
}

/// Текущий блок фактов плюс последние реплики — вход экстрактора.
///
/// `recent` намеренно короткий (последняя пара): промпт обновления памяти не
/// должен расти вместе с историей, иначе память дороже того, что экономит.
pub fn extract_prompt(store: &FactStore, recent: &[ChatMessage]) -> String {
    let current = if store.is_empty() {
        "ТЕКУЩИЕ ФАКТЫ: пусто.".to_string()
    } else {
        format!("ТЕКУЩИЕ ФАКТЫ:\n{}", store.body())
    };
    let transcript = recent
        .iter()
        .map(|m| format!("{}: {}", m.role.as_str(), m.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{current}\n\nНОВЫЕ РЕПЛИКИ:\n{transcript}\n\nВерни только JSON с операциями.")
}

/// Последняя пара реплик (предыдущий ответ + текущий вопрос) — вход
/// экстрактора. Берём именно пару: одно сообщение пользователя часто
/// осмысленно только вместе с вопросом ассистента («да, вариант Б»).
pub fn recent_slice(history: &[ChatMessage]) -> &[ChatMessage] {
    let n = history.len().min(2);
    &history[history.len() - n..]
}

#[derive(Deserialize)]
struct RawOp {
    #[serde(default)]
    op: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: String,
}

#[derive(Deserialize)]
struct RawOps {
    #[serde(default)]
    ops: Vec<RawOp>,
}

/// Толерантный разбор ответа экстрактора.
///
/// Модели уровня «бесплатная на OpenRouter» регулярно заворачивают JSON в
/// markdown-забор или предваряют его фразой. Ошибка разбора не должна ни ронять
/// приложение, ни терять ход: вызывающий оставляет старые факты и пишет
/// текст ошибки в статус.
pub fn parse_ops(raw: &str) -> Result<Vec<Op>, String> {
    let text = strip_fences(raw);
    if text.trim().is_empty() {
        return Err("пустой ответ экстрактора".into());
    }
    let slice = json_slice(&text).ok_or_else(|| "в ответе нет JSON".to_string())?;
    let raw_ops: Vec<RawOp> = if slice.trim_start().starts_with('[') {
        serde_json::from_str(&slice).map_err(|e| e.to_string())?
    } else {
        serde_json::from_str::<RawOps>(&slice)
            .map_err(|e| e.to_string())?
            .ops
    };
    Ok(raw_ops
        .into_iter()
        .filter_map(|r| {
            let key = r.key.trim().to_string();
            if key.is_empty() {
                return None;
            }
            match r.op.trim().to_ascii_lowercase().as_str() {
                "delete" | "remove" | "del" => Some(Op::Delete { key }),
                "noop" | "none" | "skip" => None,
                // add / update / set / всё прочее с непустым значением —
                // upsert. Быть строгим тут значит терять факты на опечатке
                // в поле `op`.
                _ if !r.value.trim().is_empty() => Some(Op::Upsert {
                    key,
                    value: r.value.trim().to_string(),
                }),
                _ => None,
            }
        })
        .collect())
}

/// Снять markdown-забор вокруг JSON, если модель его поставила.
fn strip_fences(raw: &str) -> String {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
    rest.rsplit_once("```")
        .map(|(body, _)| body)
        .unwrap_or(rest)
        .trim()
        .to_string()
}

/// От первой `{`/`[` до парной закрывающей — так преамбула «Вот JSON:» не
/// мешает.
fn json_slice(text: &str) -> Option<String> {
    let open = text.find(['{', '['])?;
    let close = text.rfind(['}', ']'])?;
    if close <= open {
        return None;
    }
    Some(text[open..=close].to_string())
}

fn normalize_key(key: &str) -> String {
    key.trim().to_lowercase()
}

fn clip_value(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= MAX_VALUE_CHARS {
        return value.to_string();
    }
    value.chars().take(MAX_VALUE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let ops = parse_ops(r#"{"ops":[{"op":"add","key":"цель","value":"нагрузочный стенд"}]}"#)
            .unwrap();
        assert_eq!(
            ops,
            vec![Op::Upsert {
                key: "цель".into(),
                value: "нагрузочный стенд".into()
            }]
        );
    }

    #[test]
    fn parses_json_inside_a_fence() {
        let ops =
            parse_ops("```json\n{\"ops\":[{\"op\":\"delete\",\"key\":\"бюджет\"}]}\n```").unwrap();
        assert_eq!(
            ops,
            vec![Op::Delete {
                key: "бюджет".into()
            }]
        );
    }

    #[test]
    fn parses_json_after_a_preamble() {
        let ops = parse_ops("Вот обновление памяти:\n{\"ops\":[{\"op\":\"update\",\"key\":\"срок\",\"value\":\"две недели\"}]}\nГотово.")
            .unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn parses_a_bare_array_too() {
        let ops = parse_ops(r#"[{"op":"add","key":"стек","value":"Rust"}]"#).unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn garbage_and_empty_are_errors_not_panics() {
        assert!(parse_ops("не знаю что сказать").is_err());
        assert!(parse_ops("").is_err());
        assert!(parse_ops("   \n ").is_err());
        assert!(parse_ops("{ это не json }").is_err());
    }

    #[test]
    fn empty_ops_list_is_a_legal_noop() {
        assert_eq!(parse_ops(r#"{"ops":[]}"#).unwrap(), vec![]);
        let mut store = FactStore::new();
        store.set("цель", "стенд");
        let delta = store.apply_ops(&[]);
        assert_eq!(delta.touched(), 0);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn ops_without_key_or_value_are_dropped() {
        let ops = parse_ops(r#"{"ops":[{"op":"add","key":"","value":"x"},{"op":"add","key":"k"},{"op":"noop","key":"k2"}]}"#)
            .unwrap();
        assert!(ops.is_empty());
    }

    #[test]
    fn add_update_delete_resolve_conflicts_instead_of_appending() {
        let mut store = FactStore::new();
        let d = store.apply_ops(&[
            Op::Upsert {
                key: "бюджет".into(),
                value: "200 тысяч".into(),
            },
            Op::Upsert {
                key: "срок".into(),
                value: "две недели".into(),
            },
        ]);
        assert_eq!((d.added, d.updated, d.deleted), (2, 0, 0));
        // Новое значение того же ключа заменяет старое, а не ложится рядом.
        let d = store.apply_ops(&[Op::Upsert {
            key: "бюджет".into(),
            value: "150 тысяч".into(),
        }]);
        assert_eq!((d.added, d.updated), (0, 1));
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("бюджет"), Some("150 тысяч"));
        let d = store.apply_ops(&[Op::Delete {
            key: "срок".into()
        }]);
        assert_eq!(d.deleted, 1);
        assert_eq!(store.len(), 1);
        // Удаление несуществующего ключа — не ошибка и ничего не ломает.
        let d = store.apply_ops(&[Op::Delete {
            key: "нет-такого".into(),
        }]);
        assert_eq!(d.deleted, 0);
    }

    #[test]
    fn keys_are_case_insensitive_so_the_same_fact_does_not_split() {
        let mut store = FactStore::new();
        store.apply_ops(&[Op::Upsert {
            key: "Цель".into(),
            value: "A".into(),
        }]);
        store.apply_ops(&[Op::Upsert {
            key: "цель".into(),
            value: "Б".into(),
        }]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("ЦЕЛЬ"), Some("Б"));
        store.apply_ops(&[Op::Delete {
            key: "  ЦЕЛЬ  ".into(),
        }]);
        assert!(store.is_empty());
    }

    #[test]
    fn long_values_are_clipped() {
        let mut store = FactStore::new();
        let long = "я".repeat(500);
        store.set("длинный", &long);
        assert_eq!(
            store.get("длинный").unwrap().chars().count(),
            MAX_VALUE_CHARS
        );
    }

    #[test]
    fn overflow_evicts_the_least_recently_touched_fact() {
        let mut store = FactStore::new();
        for i in 0..MAX_FACTS {
            store.apply_ops(&[Op::Upsert {
                key: format!("k{i}"),
                value: format!("v{i}"),
            }]);
        }
        assert_eq!(store.len(), MAX_FACTS);
        // k0 самый старый, но мы его трогаем — вытесниться должен k1.
        store.apply_ops(&[Op::Upsert {
            key: "k0".into(),
            value: "свежий".into(),
        }]);
        let d = store.apply_ops(&[Op::Upsert {
            key: "новый".into(),
            value: "v".into(),
        }]);
        assert_eq!(d.evicted, 1);
        assert_eq!(store.len(), MAX_FACTS);
        assert!(store.get("k1").is_none());
        assert_eq!(store.get("k0"), Some("свежий"));
    }

    #[test]
    fn the_block_never_outgrows_its_cap() {
        let mut store = FactStore::new();
        for i in 0..MAX_FACTS {
            store.apply_ops(&[Op::Upsert {
                key: format!("ключ{i}"),
                value: "ц".repeat(MAX_VALUE_CHARS),
            }]);
        }
        let block = store.block().unwrap();
        assert!(
            block.chars().count() < MAX_BLOCK_CHARS + 200,
            "{}",
            block.chars().count()
        );
        assert!(store.len() < MAX_FACTS);
    }

    #[test]
    fn block_is_none_while_the_memory_is_empty_and_sorted_when_it_is_not() {
        let mut store = FactStore::new();
        assert!(store.block().is_none());
        store.set("яблоко", "1");
        store.set("арбуз", "2");
        let block = store.block().unwrap();
        assert!(block.contains(FACTS_HEADER));
        let a = block.find("арбуз").unwrap();
        let ya = block.find("яблоко").unwrap();
        assert!(a < ya, "порядок должен быть детерминированным");
    }

    #[test]
    fn extract_prompt_carries_the_current_memory_and_the_last_pair() {
        let store = FactStore::new();
        let p = extract_prompt(&store, &[ChatMessage::user("привет")]);
        assert!(p.contains("пусто"));
        assert!(p.contains("привет"));
        let mut store = FactStore::new();
        store.set("цель", "стенд");
        let history = vec![
            ChatMessage::user("старое"),
            ChatMessage::assistant("ответ"),
            ChatMessage::user("новое"),
        ];
        let recent = recent_slice(&history);
        assert_eq!(recent.len(), 2);
        let p = extract_prompt(&store, recent);
        assert!(p.contains("цель: стенд"));
        assert!(p.contains("новое"));
        assert!(
            !p.contains("старое"),
            "вход экстрактора не растёт с историей"
        );
        // Пустая история не паникует.
        assert!(recent_slice(&[]).is_empty());
    }

    #[test]
    fn reset_clears_everything() {
        let mut store = FactStore::new();
        store.set("k", "v");
        store.note_extraction();
        store.note_error("сломалось");
        store.reset();
        assert!(store.is_empty());
        assert_eq!(store.updates(), 0);
        assert!(store.listing().contains("вызовов экстрактора 0"));
        assert!(store.last_error().is_none());
        assert!(store.listing().contains("память пуста"));
    }
}
