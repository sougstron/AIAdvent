//! Многоуровневый автокомплит слэш-команд.
//!
//! Грамматика команд описана деревом `Node`; модуль не зависит от `App` —
//! живые значения (ветки, чекпойнты, модели, ключи фактов) приходят снимком
//! `Values`. Всё чистое, тестируется без сети и терминала.

/// Живой источник кандидатов уровня.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Имена веток (`/branch switch|rename|delete`).
    Branches,
    /// Имена чекпойнтов (`/branch new <имя> [чекпойнт]`).
    Checkpoints,
    /// Id моделей каталога для подключённых провайдеров (`/model`).
    Models,
    /// Ключи факт-памяти (`/facts del`).
    FactKeys,
}

/// Узел дерева команд.
pub struct Node {
    /// Литерал, подставляемый в ввод. Пустой — слот свободного значения:
    /// совпадает с любым непустым токеном, в попапе не показывается.
    pub token: &'static str,
    /// Серая подсказка того, что ожидается ПОСЛЕ этого токена
    /// ("command", "name", "N", …). Пустая = дальше ничего не нужно.
    pub hint: &'static str,
    /// Неканонические синонимы, принимаемые при разборе, но не показываемые.
    pub alts: &'static [&'static str],
    /// Фиксированные продолжения и слоты (показываются литералы).
    pub next: &'static [Node],
    /// Живые значения этого уровня.
    pub sources: &'static [Source],
}

const fn lit(token: &'static str, hint: &'static str, next: &'static [Node]) -> Node {
    Node {
        token,
        hint,
        alts: &[],
        next,
        sources: &[],
    }
}

/// Слот свободного значения: любой токен совпадает, кандидатов нет.
const fn free(hint: &'static str) -> Node {
    Node {
        token: "",
        hint,
        alts: &[],
        next: &[],
        sources: &[],
    }
}

const NO_NEXT: &[Node] = &[];

const fn leaf(token: &'static str) -> Node {
    lit(token, "", NO_NEXT)
}

const STRATEGY_VALUES: &[Node] = &[
    leaf("show"),
    leaf("off"),
    leaf("summary"),
    leaf("window"),
    leaf("facts"),
    leaf("branch"),
    lit("keep", "N", &[free("")]),
    lit("every", "N", &[free("")]),
];

const BRANCH_RENAME_SLOT: &[Node] = &[Node {
    token: "",
    hint: "new name",
    alts: &[],
    next: &[free("")],
    sources: &[],
}];

/// Канонические команды корня (плоский `COMMANDS` отсюда и пошёл),
/// в алфавитном порядке — попап показывает их в этом порядке.
pub const ROOT: &[Node] = &[
    lit(
        "branch",
        "command",
        &[
            leaf("show"),
            lit(
                "new",
                "name",
                &[Node {
                    token: "",
                    hint: "checkpoint",
                    alts: &[],
                    next: &[],
                    sources: &[Source::Checkpoints],
                }],
            ),
            Node {
                token: "switch",
                hint: "branch",
                alts: &["go", "checkout"],
                next: &[],
                sources: &[Source::Branches],
            },
            Node {
                token: "rename",
                hint: "branch",
                alts: &[],
                next: BRANCH_RENAME_SLOT,
                sources: &[Source::Branches],
            },
            Node {
                token: "delete",
                hint: "branch",
                alts: &["del", "rm"],
                next: &[],
                sources: &[Source::Branches],
            },
        ],
    ),
    lit("checkpoint", "name", &[free("")]),
    Node {
        token: "context",
        hint: "command",
        alts: &["compress"],
        next: CONTEXT_NEXT,
        sources: &[],
    },
    lit("effort", "level", &[leaf("low"), leaf("high"), leaf("max")]),
    lit(
        "facts",
        "command",
        &[
            leaf("show"),
            leaf("clear"),
            lit("set", "key", &[free("value")]),
            Node {
                token: "del",
                hint: "key",
                alts: &["rm"],
                next: &[],
                sources: &[Source::FactKeys],
            },
        ],
    ),
    leaf("help"),
    lit(
        "json",
        "command",
        &[
            leaf("on"),
            leaf("off"),
            leaf("show"),
            lit("fields", "a,b,c", &[free("")]),
            lit("schema", "<json>", &[free("")]),
            lit("edit", "instruction", &[free("")]),
        ],
    ),
    leaf("login"),
    lit("max-tokens", "off|1-131072", &[leaf("off"), free("N")]),
    Node {
        token: "model",
        hint: "id",
        alts: &[],
        next: &[],
        sources: &[Source::Models],
    },
    leaf("new"),
    lit("personas", "[cast:] question", &[free("")]),
    leaf("quit"),
    lit("rename", "title", &[free("")]),
    leaf("sessions"),
    leaf("settings"),
    lit(
        "stop",
        "command",
        &[lit("add", "seq", &[free("")]), leaf("clear")],
    ),
    lit("strategy", "value", STRATEGY_VALUES),
    lit(
        "system",
        "text|edit|clear",
        &[leaf("edit"), leaf("clear"), free("text")],
    ),
    lit("temp", "off|0.0-2.0", &[leaf("off"), free("0.0-2.0")]),
    lit(
        "top-k",
        "off|full|N",
        &[leaf("off"), leaf("full"), free("N")],
    ),
    lit("top-p", "off|0.01-1.0", &[leaf("off"), free("0.01-1.0")]),
    leaf("verify"),
];

const CONTEXT_NEXT: &[Node] = &[leaf("show"), leaf("on"), leaf("off"), leaf("reload")];

/// Снимок живых списков; `tui.rs` собирает его из состояния приложения.
#[derive(Default, Clone, Debug)]
pub struct Values {
    pub branches: Vec<String>,
    pub checkpoints: Vec<String>,
    pub models: Vec<String>,
    pub fact_keys: Vec<String>,
}

impl Values {
    fn for_source(&self, source: Source) -> &[String] {
        match source {
            Source::Branches => &self.branches,
            Source::Checkpoints => &self.checkpoints,
            Source::Models => &self.models,
            Source::FactKeys => &self.fact_keys,
        }
    }
}

/// Разобранная строка ввода: пройденный путь и набираемый частичный токен.
pub struct Parsed {
    /// Полные токены после `/`, без частичного хвоста.
    pub path: Vec<String>,
    /// Незавершённый токен ("" — строка кончается пробелом).
    pub partial: String,
}

/// Требует ведущий `/`, отказывает многострочному вводу.
pub fn parse_line(input: &str) -> Option<Parsed> {
    let input = input.strip_prefix('/')?;
    if input.contains('\n') {
        return None;
    }
    let ends_with_space = input.ends_with(char::is_whitespace);
    let mut tokens: Vec<&str> = input.split_whitespace().collect();
    let partial = if ends_with_space {
        String::new()
    } else {
        tokens.pop().unwrap_or("").to_string()
    };
    Some(Parsed {
        path: tokens.into_iter().map(str::to_string).collect(),
        partial,
    })
}

fn matches(node: &Node, token: &str) -> bool {
    node.token.eq_ignore_ascii_case(token)
        || node.alts.iter().any(|a| a.eq_ignore_ascii_case(token))
}

/// Совпадение уровня: сначала литералы, затем слот свободного значения.
fn match_node<'a>(nodes: &'a [Node], token: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| matches(n, token)).or_else(|| {
        nodes
            .iter()
            .find(|n| n.token.is_empty() && !token.is_empty())
    })
}

/// Результат разбора: узел, на котором стоит ввод.
pub struct Resolved<'a> {
    /// `None` — корень (или совпасть не с чем: совпадение оборвалось).
    pub node: Option<&'a Node>,
    /// Частичный токен ПОСЛЕ поглощения точного совпадения литерала.
    pub partial: String,
    /// Пройденные литералы (`/branch new` → ["branch", "new"]).
    pub walked: Vec<&'static str>,
}

impl Resolved<'_> {
    /// Серая подсказка; видна только когда частичный токен пуст.
    pub fn hint(&self) -> &'static str {
        if !self.partial.is_empty() {
            return "";
        }
        self.node.map(|n| n.hint).unwrap_or("")
    }

    /// Кандидаты попапа: литералы `next` + живые значения, отфильтрованные
    /// по префиксу частичного токена (без учёта регистра).
    pub fn candidates(&self, values: &Values) -> Vec<String> {
        let partial = self.partial.as_str();
        let Some(node) = self.node else {
            return ROOT
                .iter()
                .filter(|n| {
                    !n.token.is_empty()
                        && n.token.starts_with(partial.to_ascii_lowercase().as_str())
                })
                .map(|n| n.token.to_string())
                .collect();
        };
        let lower = partial.to_ascii_lowercase();
        let mut out: Vec<String> = Vec::new();
        for n in node.next.iter().filter(|n| !n.token.is_empty()) {
            if n.token.starts_with(lower.as_str()) {
                out.push(n.token.to_string());
            }
        }
        for s in node.sources {
            for v in values.for_source(*s) {
                if v.to_ascii_lowercase().starts_with(lower.as_str()) {
                    out.push(v.clone());
                }
            }
        }
        out
    }

    /// Заголовок попапа: `commands` на корне, путь глубже.
    pub fn title(&self) -> String {
        if self.walked.is_empty() {
            "commands".to_string()
        } else {
            format!("/{}", self.walked.join(" "))
        }
    }

    /// Есть ли у узла продолжение — решает, ставится ли пробел после
    /// подстановки и отправляет ли Enter строку.
    pub fn continues(&self) -> bool {
        self.node
            .is_some_and(|n| !n.next.is_empty() || !n.sources.is_empty())
    }
}

/// Разбор + проход по дереву. Частичный токен, *точно* совпавший с литералом
/// уровня, считается пройденным (поэтому `/branch` уже показывает `command`).
pub fn resolve(input: &str) -> Option<Resolved<'static>> {
    let parsed = parse_line(input)?;
    let mut nodes: &'static [Node] = ROOT;
    let mut node: Option<&'static Node> = None;
    let mut walked: Vec<&'static str> = Vec::new();
    for tok in &parsed.path {
        match match_node(nodes, tok) {
            Some(n) => {
                if !n.token.is_empty() {
                    walked.push(n.token);
                }
                node = Some(n);
                nodes = n.next;
            }
            None => return None,
        }
    }
    let mut partial = parsed.partial;
    if !partial.is_empty() {
        if let Some(n) = nodes.iter().find(|n| matches(n, &partial)) {
            if !n.token.is_empty() {
                walked.push(n.token);
            }
            node = Some(n);
            partial = String::new();
        }
    }
    Some(Resolved {
        node,
        partial,
        walked,
    })
}
/// Кандидаты попапа для строки ввода.
pub fn candidates(input: &str, values: &Values) -> Vec<String> {
    resolve(input)
        .map(|r| r.candidates(values))
        .unwrap_or_default()
}

/// Серая подсказка структуры для строки ввода.
pub fn hint(input: &str) -> &'static str {
    resolve(input).map(|r| r.hint()).unwrap_or("")
}

/// Есть ли у подставляемого кандидата продолжение (пробел после подстановки).
pub fn continues(input_with_candidate: &str) -> bool {
    resolve(input_with_candidate)
        .map(|r| r.continues())
        .unwrap_or(false)
}

/// Байтовое начало заменяемого токена при подстановке кандидата: конец
/// строки, если частичный токен пуст или уже точно совпал с литералом
/// (`/branch` → дописать); иначе — начало набираемого токена (`/branch sw`
/// → заменить `sw`).
pub fn edit_start(input: &str) -> usize {
    match resolve(input) {
        Some(r) if !r.partial.is_empty() => input
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(input.starts_with('/').into()),
        _ => input.len(),
    }
}

/// Заголовок попапа: `commands` на корне, путь глубже (`/branch new`).
pub fn popup_title(input: &str) -> Option<String> {
    Some(resolve(input)?.title())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            branches: vec!["main".into(), "side quest".into()],
            checkpoints: vec!["cp1".into()],
            models: vec!["glm-5.3-flash".into()],
            fact_keys: vec!["user.name".into()],
        }
    }

    #[test]
    fn parse_handles_trailing_space_and_bare_slash() {
        let p = parse_line("/branch ").unwrap();
        assert_eq!(p.path, vec!["branch"]);
        assert_eq!(p.partial, "");
        let p = parse_line("/branch sw").unwrap();
        assert_eq!(p.path, vec!["branch"]);
        assert_eq!(p.partial, "sw");
        let p = parse_line("/").unwrap();
        assert!(p.path.is_empty());
        assert_eq!(p.partial, "");
        assert!(parse_line("branch").is_none());
        assert!(parse_line("/a\nb").is_none());
    }

    #[test]
    fn branch_level_two_offers_subcommands() {
        let v = values();
        assert_eq!(
            candidates("/branch ", &v),
            vec!["show", "new", "switch", "rename", "delete"]
        );
        assert_eq!(candidates("/branch sw", &v), vec!["switch"]);
        assert_eq!(hint("/branch "), "command");
        // Точное совпадение литерала без пробела — путь уже пройден.
        assert_eq!(hint("/branch"), "command");
    }

    #[test]
    fn free_level_closes_popup_but_keeps_hint() {
        assert_eq!(hint("/branch new "), "name");
        assert_eq!(hint("/branch new foo"), "");
        assert!(candidates("/branch new ", &values()).is_empty());
    }

    #[test]
    fn hint_appears_and_disappears_with_typing() {
        assert_eq!(hint("/branch new"), "name");
        assert_eq!(hint("/branch new f"), "");
        // Уровень чекпойнта за свободным именем.
        assert_eq!(hint("/branch new foo "), "checkpoint");
    }

    #[test]
    fn substitution_appends_single_space_for_nodes_with_children() {
        // Терминальный лист — без пробела.
        assert!(!continues("/new"));
        // `/branch new` ведёт к слоту имени — нужен пробел, ровно один.
        assert!(continues("/branch new"));
        assert!(continues("/branch"));
    }

    #[test]
    fn dynamic_branch_values_filter_by_prefix() {
        let v = values();
        assert_eq!(
            candidates("/branch switch ", &v),
            vec!["main", "side quest"]
        );
        assert_eq!(candidates("/branch switch m", &v), vec!["main"]);
        assert_eq!(
            candidates("/branch rename ", &v),
            vec!["main", "side quest"]
        );
    }

    #[test]
    fn model_uses_connected_catalog() {
        assert_eq!(candidates("/model ", &values()), vec!["glm-5.3-flash"]);
    }

    #[test]
    fn facts_del_uses_fact_keys() {
        assert_eq!(candidates("/facts del ", &values()), vec!["user.name"]);
    }

    #[test]
    fn root_and_prefix_filtering() {
        let v = values();
        assert_eq!(candidates("/mod", &v), vec!["model"]);
        assert!(candidates("/zzz", &v).is_empty());
        assert_eq!(hint("/zzz"), "");
        assert_eq!(hint("/b"), "");
    }

    #[test]
    fn aliases_are_accepted_but_not_offered() {
        let v = values();
        // compress — алиас /context, принимается разбором…
        assert_eq!(hint("/compress "), "command");
        assert_eq!(
            candidates("/compress ", &v),
            vec!["show", "on", "off", "reload"]
        );
        // …но в попапе корня не показывается.
        assert!(!candidates("/", &v).iter().any(|c| c == "compress"));
    }

    #[test]
    fn edit_start_replaces_partial_but_appends_after_exact_match() {
        assert_eq!(edit_start("/branch sw"), "/branch ".len());
        assert_eq!(edit_start("/ne"), 1);
        assert_eq!(edit_start("/branch"), "/branch".len());
        assert_eq!(edit_start("/branch "), "/branch ".len());
    }

    #[test]
    fn unmatched_path_without_free_slot_is_dead() {
        let v = values();
        assert!(candidates("/branch nope", &v).is_empty());
        assert!(candidates("/new x", &v).is_empty());
    }
}
