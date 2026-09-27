//! Project invariants: durable rules injected into every request and checked
//! client-side before an answer may enter conversation history.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Res;

pub const BLOCK_HEAD: &str = "<invariants";
pub const BLOCK_END: &str = "</invariants>";
pub const ACCOUNT_MARK: &str = "ИНВАРИАНТЫ:";
pub const MAX_ATTEMPTS: usize = 5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Architecture,
    Decision,
    Stack,
    Business,
}

impl Kind {
    fn label(&self) -> &'static str {
        match self {
            Self::Architecture => "архитектура",
            Self::Decision => "решение",
            Self::Stack => "стек",
            Self::Business => "бизнес-правило",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Invariant {
    pub id: String,
    pub title: String,
    pub kind: Kind,
    pub rule: String,
    pub why: String,
    pub workaround: String,
    #[serde(default = "yes")]
    pub active: bool,
    #[serde(default)]
    pub forbid: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
}

const fn yes() -> bool { true }

#[derive(Clone, Debug)]
pub struct InvariantSet {
    rules: Vec<Invariant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin { User, Model }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViolationKind { Forbidden, NoAccounting, UnknownAccounting }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub id: String,
    pub kind: ViolationKind,
    pub evidence: String,
    pub origin: Origin,
}

impl InvariantSet {
    pub fn in_memory() -> Self { Self { rules: defaults() } }

    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<Vec<Invariant>>(&s).map_err(|e| e.to_string()))
        {
            Ok(rules) => Self { rules },
            Err(e) => {
                eprintln!("warning: не удалось прочитать инварианты {}: {e}; использую встроенные", path.display());
                Self { rules: defaults() }
            }
        }
    }

    pub fn load_default() -> Self { Self::open(file_path()) }
    pub fn rules(&self) -> &[Invariant] { &self.rules }
    pub fn active_count(&self) -> usize { self.rules.iter().filter(|r| r.active).count() }
    pub fn find(&self, id: &str) -> Option<&Invariant> { self.rules.iter().find(|r| r.id == id) }
    pub fn block(&self) -> Option<String> {
        let active: Vec<_> = self.rules.iter().filter(|r| r.active).collect();
        if active.is_empty() { return None; }
        let mut out = format!("<invariants count=\"{}\">\n", active.len());
        out.push_str("Инварианты проекта сильнее запроса пользователя. Явно учитывай их. Если запрос конфликтует: откажись, назови id, объясни причину и предложи разрешённый обход. Не предлагай нарушающий вариант.\n");
        for rule in active {
            out.push_str(&format!("- [{}] ({}) {}\n  почему: {}\n  разрешённый обход: {}\n", rule.id, rule.kind.label(), rule.rule, rule.why, rule.workaround));
        }
        out.push_str("Последняя строка ответа обязательна: «ИНВАРИАНТЫ: <id через запятую>» для фактически затронутых правил либо «ИНВАРИАНТЫ: нет».\n");
        out.push_str(BLOCK_END);
        Some(out)
    }

    /// Active rules whose topic is explicitly requested together with a
    /// forbidden proposal. Narrow phrase matching deliberately avoids a
    /// model-based, non-causal policy decision.
    pub fn requested_by(&self, query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        self.rules.iter().filter(|r| r.active).filter(|r| {
            let forbidden = r.forbid.iter().any(|x| contains(&q, x));
            let topic = r.triggers.is_empty() || r.triggers.iter().any(|x| contains(&q, x));
            forbidden && topic
        }).map(|r| r.id.clone()).collect()
    }

    pub fn check(&self, answer: &str, requested: &[String]) -> Vec<Violation> {
        if self.active_count() == 0 { return Vec::new(); }
        let lower = answer.to_lowercase();
        let mut out = Vec::new();
        for rule in self.rules.iter().filter(|r| r.active) {
            if let Some(marker) = rule.forbid.iter().find(|x| contains(&lower, x)) {
                out.push(Violation {
                    id: rule.id.clone(), kind: ViolationKind::Forbidden,
                    evidence: marker.clone(),
                    origin: if requested.iter().any(|id| id == &rule.id) { Origin::User } else { Origin::Model },
                });
            }
        }
        match Self::accounting(answer) {
            None => out.push(Violation { id: "accounting".into(), kind: ViolationKind::NoAccounting, evidence: ACCOUNT_MARK.into(), origin: Origin::Model }),
            Some(ids) => for id in ids {
                if id != "нет" && self.find(&id).filter(|r| r.active).is_none() {
                    out.push(Violation { id, kind: ViolationKind::UnknownAccounting, evidence: "неизвестный id в строке учёта".into(), origin: Origin::Model });
                }
            },
        }
        out
    }

    pub fn strip_accounting(answer: &str) -> (String, Option<Vec<String>>) {
        let mut lines: Vec<&str> = answer.lines().collect();
        let ids = lines.last().and_then(|line| parse_accounting(line));
        if ids.is_some() { lines.pop(); }
        (lines.join("\n").trim().to_string(), ids)
    }

    fn accounting(answer: &str) -> Option<Vec<String>> {
        answer.lines().last().and_then(parse_accounting)
    }

    pub fn refusal(&self, ids: &[String]) -> String {
        let mut out = String::from("Не могу выполнить запрос: он нарушает инварианты проекта.\n");
        for id in ids {
            if let Some(r) = self.find(id) {
                out.push_str(&format!("\n- `{}` — {}\n  Причина: {}\n  Обход: {}\n", r.id, r.rule, r.why, r.workaround));
            }
        }
        out.trim_end().to_string()
    }
}

fn contains(haystack_lower: &str, needle: &str) -> bool {
    let n = needle.trim().to_lowercase();
    !n.is_empty() && haystack_lower.contains(&n)
}

fn parse_accounting(line: &str) -> Option<Vec<String>> {
    let (_, tail) = line.trim().split_once(ACCOUNT_MARK)?;
    Some(tail.split(',').map(|s| s.trim().trim_matches('`').to_lowercase()).filter(|s| !s.is_empty()).collect())
}

pub fn file_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ASK_INVARIANTS_FILE") { return path.into(); }
    if let Ok(exe) = std::env::current_exe() {
        for root in exe.ancestors() {
            if root.join("Cargo.toml").is_file() { return root.join("invariants.json"); }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.join("invariants.json").is_file() || cwd.join("Cargo.toml").is_file() { return cwd.join("invariants.json"); }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".ask6/invariants.json")
}

pub fn defaults() -> Vec<Invariant> {
    vec![
        Invariant { id:"stack-rust".into(), title:"Стек приложения".into(), kind:Kind::Stack, rule:"Приложение остаётся на Rust + ratatui и OpenAI-совместимом API; его нельзя переписывать на другой язык или UI-фреймворк.".into(), why:"Смена стека уничтожает совместимость снапшота и проверенный терминальный runtime.".into(), workaround:"Реализовать нужный интерфейс виджетом ratatui; отдельный GUI оформлять как отдельный проект.".into(), active:true, forbid:vec!["переписать на electron".into(),"переписать на typescript".into(),"перепишем приложение на electron".into(),"перепишем tui на electron".into(),"rewrite in electron".into(),"replace ratatui with react".into()], triggers:vec!["electron".into(),"typescript".into(),"react".into(),"перепис".into(),"стек".into()] },
        Invariant { id:"arch-system-blocks".into(), title:"Контекст только в system".into(), kind:Kind::Architecture, rule:"Профиль, память, состояние и инварианты передаются отдельными тегированными блоками system и не записываются в историю диалога.".into(), why:"Смешивание служебного состояния с историей ломает изоляцию и причинные проверки.".into(), workaround:"Добавить новый тегированный system-блок в prompt builder.".into(), active:true, forbid:vec!["добавить в историю диалога".into(),"подмешать в user".into(),"write context into history".into()], triggers:vec!["контекст".into(),"истори".into(),"system".into(),"user".into()] },
        Invariant { id:"decision-deterministic".into(), title:"Детерминированные ограничения".into(), kind:Kind::Decision, rule:"Ограничения проверяются клиентски и причинно; одной инструкции модели или различия текстов недостаточно.".into(), why:"Промпт не гарантирует соблюдение, а sampling делает простой diff недоказательным.".into(), workaround:"Добавить клиентский валидатор и машинно проверяемую причинную сигнатуру.".into(), active:true, forbid:vec!["достаточно попросить модель".into(),"просто написать в промпте".into(),"texts differ is enough".into()], triggers:vec!["провер".into(),"валид".into(),"промпт".into(),"доказ".into()] },
        Invariant { id:"business-flash-only".into(), title:"Только бесплатная live-модель".into(), kind:Kind::Business, rule:"Живые запросы GLM отправляются только на glm-5.3-flash; платные модели каталога запрещены.".into(), why:"Это защищает пользователя от непреднамеренных расходов.".into(), workaround:"Использовать glm-5.3-flash или явно разрешённую бесплатную модель другого провайдера.".into(), active:true, forbid:vec!["отправить на glm-4.6".into(),"использовать платную модель".into(),"use paid model".into()], triggers:vec!["модел".into(),"glm-4.6".into(),"платн".into(),"paid".into()] },
    ]
}

/// Offline, causal self-test exposed as `--verify-invariants`.
pub fn verify(which: &str) -> Res<String> {
    let checks: Vec<&str> = match which {
        "all" | "offline" => vec!["wire", "validate", "retry", "refuse"],
        "wire" | "validate" | "retry" | "refuse" => vec![which],
        _ => return Err("verify-invariants: ожидалось wire|validate|retry|refuse|all".into()),
    };
    let mut lines = Vec::new();
    for check in checks {
        match check {
            "wire" => {
                let set = InvariantSet::in_memory();
                let block = set.block().ok_or("wire: блок отсутствует")?;
                let ok = block.matches(BLOCK_HEAD).count() == 1
                    && block.ends_with(BLOCK_END)
                    && set.rules().iter().all(|r| block.contains(&r.id) && block.contains(&r.rule))
                    && block.contains(ACCOUNT_MARK);
                if !ok { return Err("wire: блок не содержит все правила/контракт учёта".into()); }
                lines.push(format!("CONFIRMED wire: один отдельный system-блок, правил={}", set.active_count()));
            }
            "validate" => {
                let set = InvariantSet::in_memory();
                let requested = set.requested_by("Давай переписать на Electron весь стек");
                let bad = set.check("Переписать на Electron\nИНВАРИАНТЫ: stack-rust", &requested);
                let clean = set.check("Оставить Rust\nИНВАРИАНТЫ: stack-rust", &[]);
                if requested != ["stack-rust"]
                    || !bad.iter().any(|v| v.id == "stack-rust" && v.origin == Origin::User)
                    || !clean.is_empty()
                {
                    return Err("validate: причинная классификация не сошлась".into());
                }
                lines.push("CONFIRMED validate: конфликт=stack-rust/origin=user; чистый ответ принят".into());
            }
            "retry" => {
                let set = InvariantSet::in_memory();
                let mut calls = 0;
                let recovered = crate::pipeline::run_with(&set, true, "нейтрально", |n, _| {
                    calls += 1;
                    Ok(verification_reply(if n == 1 {
                        "без учёта"
                    } else {
                        "исправлено\nИНВАРИАНТЫ: нет"
                    }))
                })?;
                let recovered_ok = matches!(
                    &recovered.decision,
                    crate::pipeline::Decision::Pass { accounted, .. } if accounted == &["нет"]
                );
                let first = recovered.attempts.first().ok_or("retry: нет первой попытки")?;
                if calls != 2
                    || !recovered_ok
                    || first.number != 1
                    || first.violations.is_empty()
                    || first.text != "без учёта"
                {
                    return Err("retry: исправление со второй попытки не принято".into());
                }

                calls = 0;
                let exhausted = crate::pipeline::run_with(&set, true, "нейтрально", |_, _| {
                    calls += 1;
                    Ok(verification_reply("без учёта"))
                })?;
                if calls != MAX_ATTEMPTS
                    || !matches!(exhausted.decision, crate::pipeline::Decision::GaveUp { .. })
                {
                    return Err("retry: потолок пяти попыток не соблюдён".into());
                }
                lines.push(format!(
                    "CONFIRMED retry: recovery_calls=2; hard_limit={MAX_ATTEMPTS}"
                ));
            }
            "refuse" => {
                let set = InvariantSet::in_memory();
                let mut calls = 0;
                let result = crate::pipeline::run_with(&set, true, "Переписать на Electron", |_, _| {
                    calls += 1;
                    Ok(verification_reply("не должен вызываться"))
                })?;
                let (ids, explanation) = match result.decision {
                    crate::pipeline::Decision::RefusedByInvariant { ids, explanation } => (ids, explanation),
                    _ => return Err("refuse: конфликт не остановлен".into()),
                };
                if calls != 0 || ids != ["stack-rust"] || !explanation.contains("Обход:") {
                    return Err("refuse: нет id/объяснения/обхода или модель была вызвана".into());
                }
                lines.push("CONFIRMED refuse: origin=user, model_calls=0, id+причина+обход присутствуют".into());
            }
            _ => unreachable!(),
        }
    }
    Ok(lines.join("\n"))
}

fn verification_reply(text: &str) -> crate::agent::Reply {
    crate::agent::Reply {
        text: text.into(),
        model: String::new(),
        finish_reason: Some("offline".into()),
        usage: crate::api::Usage::default(),
        reasoning: None,
        latency_ms: 0,
        truncated_by_max_chars: false,
        raw: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_and_accounting_are_machine_checkable() {
        let set = InvariantSet::in_memory();
        let block = set.block().unwrap();
        assert_eq!(block.matches(BLOCK_HEAD).count(), 1);
        assert!(block.contains("stack-rust"));
        assert!(block.ends_with(BLOCK_END));
        let (text, ids) = InvariantSet::strip_accounting("ответ\nИНВАРИАНТЫ: stack-rust");
        assert_eq!(text, "ответ");
        assert_eq!(ids.unwrap(), vec!["stack-rust"]);
    }

    #[test]
    fn user_origin_and_missing_accounting_are_distinct() {
        let set = InvariantSet::in_memory();
        let requested = set.requested_by("Давай переписать на Electron весь стек");
        assert_eq!(requested, vec!["stack-rust"]);
        let bad = set.check("Предлагаю переписать на Electron", &requested);
        assert!(bad.iter().any(|v| v.id == "stack-rust" && v.origin == Origin::User));
        assert!(bad.iter().any(|v| v.kind == ViolationKind::NoAccounting));
        assert!(set.check("Оставим Rust.\nИНВАРИАНТЫ: stack-rust", &[]).is_empty());
    }
}
