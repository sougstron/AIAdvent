//! Стратегия «ветки диалога»: дерево реплик, чекпойнты и независимые ветки.
//!
//! # Модель данных
//!
//! Реплики лежат в одной арене `nodes`, каждая помнит своего родителя.
//! Ветка — это **указатель на лист**, а диалог ветки — путь от корня до
//! этого листа. Форк не копирует историю: новая ветка просто указывает на
//! тот же узел (структурный шаринг), поэтому две ветки от одного чекпойнта
//! стоят ровно столько, сколько в них дописали.
//!
//! ```text
//!  u1 — a1 — u2 — a2 ── u3a — a3a      (ветка main)
//!                   └── u3b — a3b      (ветка alt, форк от чекпойнта cp1)
//! ```
//!
//! # Чекпойнт
//!
//! Именованный указатель на узел. `/checkpoint` ставит его на текущий лист,
//! `/branch new` форкает от последнего поставленного (или от текущего конца,
//! если чекпойнтов нет).
//!
//! **Снап точки форка.** Форкать осмысленно после ответа ассистента: если
//! чекпойнт указывает на реплику пользователя, ветка начнётся с двух
//! пользовательских сообщений подряд. Поэтому чекпойнт сдвигается вверх, к
//! ближайшему ответу ассистента, и об этом сообщается в статусе.
//!
//! # Своя память на ветку
//!
//! [`Branch`] держит **свои** `Compressor` и `FactStore`. Общее на всё дерево
//! состояние — главная скрытая бага этой стратегии: summary или факты ветки A
//! протекли бы в ветку B и «доказательство изоляции» стало бы ложным.
//! Переключение ветки сохраняет память текущей и поднимает память целевой
//! (`switch_memory`).
//!
//! При форке память родителя копируется — общий префикс у веток и правда
//! общий. Исключение: если summary родителя покрывает больше сообщений, чем
//! есть в пути до точки форка, оно описывает реплики, которых в новой ветке
//! нет, и копировать его нельзя — тогда новая ветка стартует с пустым
//! summary.

use serde::{Deserialize, Serialize};

use crate::compress::Compressor;
use crate::facts::FactStore;
use crate::session::StoredMessage;

/// Имя ветки по умолчанию — та, что существует всегда.
pub const MAIN_BRANCH: &str = "main";

/// Узел дерева: одна реплика и её родитель.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    #[serde(default)]
    pub parent: Option<usize>,
    pub msg: StoredMessage,
}

/// Ветка — указатель на лист плюс своя память.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    /// Последний узел ветки. `None` — ветка пуста (обычно свежий `main`).
    #[serde(default)]
    pub leaf: Option<usize>,
    /// Узел, от которого ветка отделилась. `None` у корневой.
    #[serde(default)]
    pub forked_at: Option<usize>,
    #[serde(default)]
    pub compressor: Compressor,
    #[serde(default)]
    pub facts: FactStore,
}

/// Именованная точка, от которой можно форкнуться.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub name: String,
    pub node: usize,
}

/// Итог постановки чекпойнта — имя и был ли сдвиг к ответу ассистента.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointResult {
    pub name: String,
    pub snapped: bool,
    pub depth: usize,
}

/// Дерево диалога целиком.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchStore {
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    branches: Vec<Branch>,
    #[serde(default)]
    active: usize,
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
}

/// `Default` — это сразу валидное дерево с одной пустой веткой `main`, а не
/// «ноль веток». Так `#[serde(default)]` на старом файле сессии даёт
/// работоспособное состояние, и ни один метод не индексирует пустой вектор.
impl Default for BranchStore {
    fn default() -> BranchStore {
        BranchStore {
            nodes: Vec::new(),
            branches: vec![Branch {
                name: MAIN_BRANCH.into(),
                ..Branch::default()
            }],
            active: 0,
            checkpoints: Vec::new(),
        }
    }
}

impl BranchStore {
    /// Пустое дерево с единственной веткой `main`.
    pub fn new() -> BranchStore {
        BranchStore::default()
    }

    /// Сессия, записанная до задачи 10, приходит с пустым деревом и плоским
    /// `messages`. Собираем из неё одну линейную ветку `main` — один проход,
    /// без запросов и без потерь.
    pub fn from_messages(messages: &[StoredMessage]) -> BranchStore {
        let mut store = BranchStore::new();
        for m in messages {
            store.push(m.clone());
        }
        store
    }

    /// Дерево без единой реплики. Если при этом в сессии есть плоский
    /// `messages` — файл записан до задачи 10 и его надо мигрировать.
    pub fn needs_migration(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn branches(&self) -> &[Branch] {
        &self.branches
    }

    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    pub fn active_index(&self) -> usize {
        self.active.min(self.branches.len().saturating_sub(1))
    }

    fn ensure_branch(&mut self) {
        if self.branches.is_empty() {
            self.branches.push(Branch {
                name: MAIN_BRANCH.into(),
                ..Branch::default()
            });
            self.active = 0;
        }
    }

    /// Активная ветка. Дерево всегда держит хотя бы одну (см. `Default`).
    pub fn active(&self) -> &Branch {
        &self.branches[self.active_index()]
    }

    fn active_mut(&mut self) -> &mut Branch {
        self.ensure_branch();
        let i = self.active_index();
        &mut self.branches[i]
    }

    pub fn active_name(&self) -> &str {
        &self.active().name
    }

    /// Сколько веток в дереве.
    pub fn len(&self) -> usize {
        self.branches.len()
    }

    /// Индексы узлов активной ветки, от корня к листу.
    pub fn path_indices(&self) -> Vec<usize> {
        self.path_indices_from(self.active().leaf)
    }

    fn path_indices_from(&self, leaf: Option<usize>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = leaf;
        while let Some(i) = cur {
            let Some(node) = self.nodes.get(i) else { break };
            out.push(i);
            cur = node.parent;
        }
        out.reverse();
        out
    }

    /// Диалог активной ветки — это и есть её история.
    pub fn path(&self) -> Vec<StoredMessage> {
        self.path_indices()
            .into_iter()
            .map(|i| self.nodes[i].msg.clone())
            .collect()
    }

    pub fn path_len(&self) -> usize {
        self.path_indices().len()
    }

    /// Дописать реплику в активную ветку.
    pub fn push(&mut self, msg: StoredMessage) -> usize {
        let parent = self.active().leaf;
        self.nodes.push(Node { parent, msg });
        let idx = self.nodes.len() - 1;
        self.active_mut().leaf = Some(idx);
        idx
    }

    /// Переписать путь активной ветки под новый список сообщений.
    ///
    /// Нужен ровно для одного случая: front-end правит `Session::messages`
    /// напрямую (откат последнего хода после сетевой ошибки). Общий префикс
    /// переиспользуется, так что соседние ветки, висящие на нём, не рвутся.
    pub fn resync(&mut self, messages: &[StoredMessage]) {
        let current = self.path_indices();
        let mut common = 0;
        while common < current.len()
            && common < messages.len()
            && self.nodes[current[common]].msg.role == messages[common].role
            && self.nodes[current[common]].msg.content == messages[common].content
        {
            common += 1;
        }
        if common == current.len() && common == messages.len() {
            return;
        }
        self.active_mut().leaf = if common == 0 {
            None
        } else {
            Some(current[common - 1])
        };
        for m in &messages[common..] {
            self.push(m.clone());
        }
    }

    /// Записать текущую память агента в активную ветку.
    pub fn store_memory(&mut self, compressor: Compressor, facts: FactStore) {
        let b = self.active_mut();
        b.compressor = compressor;
        b.facts = facts;
    }

    /// Поставить чекпойнт на текущий лист активной ветки.
    ///
    /// Сдвигает точку вверх до ближайшего ответа ассистента — форк от
    /// реплики пользователя дал бы два user-сообщения подряд.
    pub fn checkpoint(&mut self, name: Option<&str>) -> Result<CheckpointResult, String> {
        let Some(leaf) = self.active().leaf else {
            return Err("в этой ветке ещё нет сообщений — нечего отмечать".into());
        };
        let snapped_node = self
            .snap_to_assistant(leaf)
            .ok_or_else(|| "до ближайшего ответа ассистента идти некуда".to_string())?;
        let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => n.to_string(),
            None => self.next_checkpoint_name(),
        };
        self.checkpoints.retain(|c| c.name != name);
        self.checkpoints.push(Checkpoint {
            name: name.clone(),
            node: snapped_node,
        });
        Ok(CheckpointResult {
            name,
            snapped: snapped_node != leaf,
            depth: self.path_indices_from(Some(snapped_node)).len(),
        })
    }

    fn snap_to_assistant(&self, node: usize) -> Option<usize> {
        let mut cur = Some(node);
        while let Some(i) = cur {
            let n = self.nodes.get(i)?;
            if n.msg.role == "assistant" {
                return Some(i);
            }
            cur = n.parent;
        }
        None
    }

    fn next_checkpoint_name(&self) -> String {
        (1..).map(|i| format!("cp{i}")).find(|n| !self.checkpoints.iter().any(|c| &c.name == n)).unwrap_or_else(|| "cp".into())
    }

    pub fn find_checkpoint(&self, name: &str) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.name == name.trim())
    }

    /// Форк новой ветки. `from` — имя чекпойнта; без него берём последний
    /// поставленный, а если чекпойнтов нет — текущий конец ветки.
    pub fn fork(&mut self, name: &str, from: Option<&str>) -> Result<usize, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("у ветки должно быть имя".into());
        }
        if self.branches.iter().any(|b| b.name == name) {
            return Err(format!("ветка `{name}` уже есть"));
        }
        let node = match from {
            Some(cp) => {
                self.find_checkpoint(cp)
                    .ok_or_else(|| format!("нет чекпойнта `{cp}`"))?
                    .node
            }
            None => match self.checkpoints.last() {
                Some(cp) => cp.node,
                None => self
                    .active()
                    .leaf
                    .and_then(|leaf| self.snap_to_assistant(leaf))
                    .ok_or_else(|| "не от чего форкать: в ветке нет ответов ассистента".to_string())?,
            },
        };
        let prefix_len = self.path_indices_from(Some(node)).len();
        let parent = self.active();
        // Summary, описывающее больше сообщений, чем есть в общем префиксе,
        // говорит о репликах, которых в новой ветке нет — такое не копируем.
        let compressor = if parent.compressor.covered() <= prefix_len {
            parent.compressor.clone()
        } else {
            Compressor::new()
        };
        let facts = parent.facts.clone();
        self.branches.push(Branch {
            name: name.to_string(),
            leaf: Some(node),
            forked_at: Some(node),
            compressor,
            facts,
        });
        Ok(self.branches.len() - 1)
    }

    /// Найти ветку по имени или по номеру из `/branch` (1-based).
    pub fn resolve(&self, selector: &str) -> Result<usize, String> {
        let sel = selector.trim();
        if sel.is_empty() {
            return Err("нужно имя или номер ветки".into());
        }
        if let Some(i) = self.branches.iter().position(|b| b.name == sel) {
            return Ok(i);
        }
        if let Ok(n) = sel.parse::<usize>() {
            if n >= 1 && n <= self.branches.len() {
                return Ok(n - 1);
            }
        }
        Err(format!("нет ветки `{sel}`"))
    }

    /// Переключиться на ветку. Память вызывающего (агента) передаётся сюда и
    /// возвращается уже от новой ветки — так соседние ветки не протекают.
    pub fn switch_memory(
        &mut self,
        selector: &str,
        current: (Compressor, FactStore),
    ) -> Result<(Compressor, FactStore), String> {
        let target = self.resolve(selector)?;
        self.store_memory(current.0, current.1);
        self.active = target;
        let b = self.active();
        Ok((b.compressor.clone(), b.facts.clone()))
    }

    pub fn rename(&mut self, selector: &str, new_name: &str) -> Result<String, String> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err("новое имя не должно быть пустым".into());
        }
        let i = self.resolve(selector)?;
        if self.branches.iter().enumerate().any(|(j, b)| j != i && b.name == new_name) {
            return Err(format!("ветка `{new_name}` уже есть"));
        }
        let old = std::mem::replace(&mut self.branches[i].name, new_name.to_string());
        Ok(old)
    }

    /// Удалить ветку. Узлы не чистим: они дёшевы и могут быть общими с
    /// соседями. Последнюю ветку удалить нельзя.
    pub fn delete(&mut self, selector: &str) -> Result<String, String> {
        if self.branches.len() <= 1 {
            return Err("нельзя удалить последнюю ветку".into());
        }
        let i = self.resolve(selector)?;
        let removed = self.branches.remove(i);
        if self.active >= self.branches.len() {
            self.active = self.branches.len() - 1;
        } else if self.active > i {
            self.active -= 1;
        } else if self.active == i {
            self.active = 0;
        }
        Ok(removed.name)
    }

    /// Короткая строка про активную ветку — для футера.
    pub fn line(&self) -> String {
        let b = self.active();
        let fork = match b.forked_at {
            Some(node) => format!(", форк от узла глубины {}", self.path_indices_from(Some(node)).len()),
            None => String::new(),
        };
        format!(
            "ветка {} ({}/{}), сообщений {}{fork}",
            b.name,
            self.active_index() + 1,
            self.branches.len(),
            self.path_len()
        )
    }

    /// Многострочный список для `/branch`.
    pub fn listing(&self) -> String {
        let active = self.active_index();
        let mut lines = Vec::new();
        for (i, b) in self.branches().iter().enumerate() {
            let marker = if i == active { "*" } else { " " };
            let depth = self.path_indices_from(b.leaf).len();
            let fork = match b.forked_at {
                Some(node) => format!("  форк от узла глубины {}", self.path_indices_from(Some(node)).len()),
                None => "  корневая".to_string(),
            };
            lines.push(format!(
                "{marker} {}. {:<16} сообщений {depth}{fork}",
                i + 1,
                b.name
            ));
        }
        if self.checkpoints.is_empty() {
            lines.push("чекпойнтов нет — /checkpoint [имя]".into());
        } else {
            let cps = self
                .checkpoints
                .iter()
                .map(|c| format!("{} (глубина {})", c.name, self.path_indices_from(Some(c.node)).len()))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("чекпойнты: {cps}"));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> StoredMessage {
        StoredMessage {
            role: role.into(),
            content: content.into(),
            interrupted: false,
        }
    }

    fn linear(store: &mut BranchStore, pairs: &[(&str, &str)]) {
        for (u, a) in pairs {
            store.push(msg("user", u));
            store.push(msg("assistant", a));
        }
    }

    fn texts(store: &BranchStore) -> Vec<String> {
        store.path().into_iter().map(|m| m.content).collect()
    }

    #[test]
    fn a_single_branch_is_just_the_flat_history() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1"), ("u2", "a2")]);
        assert_eq!(texts(&store), ["u1", "a1", "u2", "a2"]);
        assert_eq!(store.active_name(), MAIN_BRANCH);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn migration_from_a_flat_session_keeps_the_same_history() {
        let flat = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        let store = BranchStore::from_messages(&flat);
        assert_eq!(store.path(), flat);
        assert_eq!(store.len(), 1);
        assert!(!store.needs_migration());
        // Пустое дерево из serde(default) — повод мигрировать.
        assert!(BranchStore::default().needs_migration());
    }

    #[test]
    fn two_branches_from_one_checkpoint_are_independent() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("общее", "принято")]);
        let cp = store.checkpoint(None).unwrap();
        assert_eq!(cp.name, "cp1");
        assert!(!cp.snapped, "лист уже был ответом ассистента");
        store.fork("alpha", None).unwrap();
        store.fork("beta", None).unwrap();

        let empty = (Compressor::new(), FactStore::new());
        store.switch_memory("alpha", empty.clone()).unwrap();
        linear(&mut store, &[("ALPHA?", "ALPHA!")]);
        store.switch_memory("beta", empty.clone()).unwrap();
        linear(&mut store, &[("BETA?", "BETA!")]);

        assert_eq!(texts(&store), ["общее", "принято", "BETA?", "BETA!"]);
        store.switch_memory("alpha", empty.clone()).unwrap();
        assert_eq!(texts(&store), ["общее", "принято", "ALPHA?", "ALPHA!"]);
        // Ни одна ветка не видит реплик соседней — в этом вся стратегия.
        assert!(!texts(&store).iter().any(|t| t.contains("BETA")));
        store.switch_memory("main", empty).unwrap();
        assert_eq!(texts(&store), ["общее", "принято"]);
    }

    #[test]
    fn branch_memory_does_not_leak_between_branches() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("общее", "принято")]);
        store.checkpoint(None).unwrap();
        store.fork("alpha", None).unwrap();

        // Уходим из main с пустой памятью — её и запомнит main.
        let empty = (Compressor::new(), FactStore::new());
        let (_, on_alpha) = store.switch_memory("alpha", empty).unwrap();
        assert!(on_alpha.is_empty());

        // В alpha завели факт и ушли обратно: факт остаётся в alpha.
        let mut facts_alpha = FactStore::new();
        facts_alpha.set("секрет", "ALPHA");
        let (_, on_main) = store
            .switch_memory("main", (Compressor::new(), facts_alpha))
            .unwrap();
        assert!(on_main.is_empty(), "в main не должно быть фактов alpha");

        let (_, back_on_alpha) = store
            .switch_memory("alpha", (Compressor::new(), on_main))
            .unwrap();
        assert_eq!(back_on_alpha.get("секрет"), Some("ALPHA"));
    }

    #[test]
    fn forking_snaps_the_checkpoint_up_to_an_assistant_turn() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1")]);
        store.push(msg("user", "u2-без-ответа"));
        let cp = store.checkpoint(Some("после-вопроса")).unwrap();
        assert!(cp.snapped, "чекпойнт на реплике пользователя должен сдвинуться");
        assert_eq!(cp.depth, 2);
        store.fork("alt", Some("после-вопроса")).unwrap();
        let (c, f) = (Compressor::new(), FactStore::new());
        store.switch_memory("alt", (c, f)).unwrap();
        assert_eq!(texts(&store), ["u1", "a1"], "ветка стартует после ответа");
    }

    #[test]
    fn checkpoint_needs_an_assistant_answer_to_exist() {
        let mut store = BranchStore::new();
        assert!(store.checkpoint(None).is_err(), "пустая ветка");
        store.push(msg("user", "u1"));
        assert!(store.checkpoint(None).is_err(), "ответа ассистента ещё не было");
        assert!(store.fork("alt", None).is_err());
    }

    #[test]
    fn checkpoint_names_do_not_collide() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1")]);
        assert_eq!(store.checkpoint(None).unwrap().name, "cp1");
        linear(&mut store, &[("u2", "a2")]);
        assert_eq!(store.checkpoint(None).unwrap().name, "cp2");
        // Повтор имени переставляет чекпойнт, а не плодит второй.
        linear(&mut store, &[("u3", "a3")]);
        store.checkpoint(Some("cp1")).unwrap();
        assert_eq!(store.checkpoints().len(), 2);
        assert_eq!(store.find_checkpoint("cp1").unwrap().node, 5);
    }

    #[test]
    fn resolve_takes_a_name_or_a_one_based_number() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1")]);
        store.checkpoint(None).unwrap();
        store.fork("alpha", None).unwrap();
        assert_eq!(store.resolve("main").unwrap(), 0);
        assert_eq!(store.resolve("2").unwrap(), 1);
        assert!(store.resolve("0").is_err());
        assert!(store.resolve("3").is_err());
        assert!(store.resolve("нет").is_err());
        assert!(store.fork("alpha", None).is_err(), "дубликат имени");
    }

    #[test]
    fn deleting_the_active_branch_falls_back_and_the_last_one_stays() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1")]);
        store.checkpoint(None).unwrap();
        store.fork("alpha", None).unwrap();
        let empty = (Compressor::new(), FactStore::new());
        store.switch_memory("alpha", empty.clone()).unwrap();
        assert_eq!(store.active_name(), "alpha");
        assert_eq!(store.delete("alpha").unwrap(), "alpha");
        assert_eq!(store.active_name(), MAIN_BRANCH);
        assert_eq!(store.len(), 1);
        // История main цела: удаление ветки не чистит общие узлы.
        assert_eq!(texts(&store), ["u1", "a1"]);
        assert!(store.delete("main").is_err(), "последнюю ветку удалять нельзя");
    }

    #[test]
    fn rename_rejects_empty_and_duplicate_names() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1")]);
        store.checkpoint(None).unwrap();
        store.fork("alpha", None).unwrap();
        assert!(store.rename("alpha", "").is_err());
        assert!(store.rename("alpha", "main").is_err());
        assert_eq!(store.rename("alpha", "beta").unwrap(), "alpha");
        assert_eq!(store.branches()[1].name, "beta");
        // Переименование в себя же разрешено.
        assert!(store.rename("beta", "beta").is_ok());
    }

    #[test]
    fn fork_drops_a_summary_that_describes_messages_the_branch_does_not_have() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1"), ("u2", "a2"), ("u3", "a3")]);
        // Чекпойнт на глубине 2, а summary родителя описывает 4 сообщения.
        let mut c = Compressor::new();
        c.apply("сводка про u2/a2".into(), 4, 10);
        store.store_memory(c, FactStore::new());
        store.checkpoints.push(Checkpoint { name: "ранний".into(), node: 1 });
        store.fork("alt", Some("ранний")).unwrap();
        assert!(store.branches()[1].compressor.is_empty());
        // А если summary укладывается в общий префикс — копируется.
        let mut c = Compressor::new();
        c.apply("сводка".into(), 2, 10);
        store.store_memory(c, FactStore::new());
        store.fork("alt2", Some("ранний")).unwrap();
        assert!(!store.branches()[2].compressor.is_empty());
    }

    #[test]
    fn resync_reuses_the_common_prefix_and_keeps_siblings_intact() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1"), ("u2", "a2")]);
        store.checkpoints.push(Checkpoint { name: "cp".into(), node: 1 });
        store.fork("alpha", Some("cp")).unwrap();
        let nodes_before = store.nodes.len();
        // Откат последнего хода: front-end убрал две последние реплики.
        let rolled_back = vec![msg("user", "u1"), msg("assistant", "a1")];
        store.resync(&rolled_back);
        assert_eq!(texts(&store), ["u1", "a1"]);
        assert_eq!(store.nodes.len(), nodes_before, "префикс переиспользован");
        // Соседняя ветка по-прежнему указывает на живой узел.
        let (c, f) = (Compressor::new(), FactStore::new());
        store.switch_memory("alpha", (c, f)).unwrap();
        assert_eq!(texts(&store), ["u1", "a1"]);
        // Дописали новое — путь продолжается, узлы добавились.
        store.resync(&[msg("user", "u1"), msg("assistant", "a1"), msg("user", "новое")]);
        assert_eq!(texts(&store).last().unwrap(), "новое");
        // Полная замена с нуля тоже работает.
        store.resync(&[]);
        assert!(store.path().is_empty());
    }

    #[test]
    fn line_and_listing_mark_the_active_branch() {
        let mut store = BranchStore::new();
        linear(&mut store, &[("u1", "a1")]);
        store.checkpoint(None).unwrap();
        store.fork("alpha", None).unwrap();
        let listing = store.listing();
        assert!(listing.contains("* 1. main"));
        assert!(listing.contains("  2. alpha"));
        assert!(listing.contains("cp1"));
        assert!(store.line().contains("ветка main (1/2)"));
    }
}
