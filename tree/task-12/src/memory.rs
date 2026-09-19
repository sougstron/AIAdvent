//! Модель памяти агента: три слоя, три папки, явная маршрутизация.
//!
//! # Слои
//!
//! | слой | что хранит | время жизни | папка |
//! |------|------------|-------------|-------|
//! | [`Layer::Short`] | текущий диалог: тема, о чём только что договорились | до `/new` или смены сессии | `memory/short/<session>.json` |
//! | [`Layer::Working`] | данные текущей задачи: вводные, ограничения, промежуточные результаты | до смены задачи (`/mem task <имя>`) | `memory/working/<задача>.json` |
//! | [`Layer::Long`] | профиль, решения, знания | не истекает, переживает перезапуск | `memory/long/<профиль>.json` |
//!
//! Краткосрочный слой кроме записей хранит ещё и **дословную копию диалога**
//! (`dialog` в том же файле): экстрактор мог не позваться или ничего не
//! выделить, но «текущий разговор» обязан лежать в короткой памяти целиком.
//! Копия пишется на каждом ходе, независимо от стратегии.
//!
//! Слои **физически разделены**: каждый — свой файл в своей папке. Это не
//! украшение, а то, что делает проверку возможной: «какие данные попали в
//! слой» — это `cat` файла, а не догадка про то, что модель себе думает.
//!
//! # Явная маршрутизация
//!
//! Записать что-то «в память» здесь нельзя: у каждой записи есть слой, и
//! выбирает его [`route`], а не вызывающий код и не модель.
//!
//! 1. Ручная правка (`/mem long set ...`) — слой назвал человек, он и
//!    побеждает.
//! 2. Ключ с префиксом из [`PREFIXES`] (`профиль.`, `задача.`, `диалог.`, …)
//!    — слой определяет префикс. **Даже если экстрактор попросил другой**:
//!    детерминированное правило важнее настроения модели, а расхождение
//!    видно в `/mem routes` и в отчёте проверки.
//! 3. Подсказка экстрактора (`"layer": "..."`), если префикса нет.
//! 4. Иначе — рабочий слой: незнакомая запись про текущую задачу не должна
//!    молча оседать в долговременном профиле.
//!
//! # Что уезжает на провод
//!
//! При стратегии [`ContextStrategy::Memory`](crate::config::ContextStrategy)
//! на провод идут последние `keep_recent` сообщений, а слои уезжают в
//! **system** тремя отдельными блоками (long → working → short). Блоки
//! разделены и подписаны именно для того, чтобы влияние слоя на ответ можно
//! было включить и выключить по одному.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::ChatMessage;
use crate::facts::{json_slice, strip_fences};

/// Слой памяти.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// Краткосрочная: текущий диалог.
    Short,
    /// Рабочая: данные текущей задачи.
    Working,
    /// Долговременная: профиль, решения, знания.
    Long,
}

impl Layer {
    pub const ALL: [Layer; 3] = [Layer::Long, Layer::Working, Layer::Short];

    pub fn label(self) -> &'static str {
        match self {
            Layer::Short => "short",
            Layer::Working => "working",
            Layer::Long => "long",
        }
    }

    /// Человеческое имя для блока в system и для `/mem show`.
    pub fn title(self) -> &'static str {
        match self {
            Layer::Short => "Краткосрочная память (текущий диалог)",
            Layer::Working => "Рабочая память (данные текущей задачи)",
            Layer::Long => "Долговременная память (профиль, решения, знания)",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Layer::Short => "живёт до конца диалога: тема, договорённости этого чата",
            Layer::Working => "живёт до смены задачи: вводные, ограничения, результаты",
            Layer::Long => "не истекает: профиль пользователя, принятые решения, знания",
        }
    }

    /// Имя папки внутри корня памяти.
    pub fn dir(self) -> &'static str {
        self.label()
    }

    /// Потолок записей в слое. Короткая память держится маленькой намеренно:
    /// её задача — текущий диалог, а не архив.
    pub fn capacity(self) -> usize {
        match self {
            Layer::Short => 12,
            Layer::Working => 30,
            Layer::Long => 60,
        }
    }

    pub fn parse(s: &str) -> Result<Layer, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "short" | "s" | "кратко" | "краткосрочная" | "dialog" => Ok(Layer::Short),
            "working" | "work" | "w" | "task" | "рабочая" => Ok(Layer::Working),
            "long" | "l" | "longterm" | "long-term" | "долговременная" => Ok(Layer::Long),
            other => Err(format!(
                "неизвестный слой `{other}`; ожидается short, working или long"
            )),
        }
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Префиксы ключей, которые жёстко задают слой. Их же называет экстрактору
/// [`MEMORY_SYSTEM`], поэтому обычно подсказка и префикс совпадают, а
/// расхождение — сигнал, а не тихая порча памяти.
pub const PREFIXES: &[(&str, Layer)] = &[
    ("профиль.", Layer::Long),
    ("profile.", Layer::Long),
    ("решение.", Layer::Long),
    ("decision.", Layer::Long),
    ("знание.", Layer::Long),
    ("knowledge.", Layer::Long),
    ("задача.", Layer::Working),
    ("task.", Layer::Working),
    ("данные.", Layer::Working),
    ("data.", Layer::Working),
    ("шаг.", Layer::Working),
    ("step.", Layer::Working),
    ("диалог.", Layer::Short),
    ("dialog.", Layer::Short),
    ("тема.", Layer::Short),
    ("topic.", Layer::Short),
];

/// Предел длины значения одной записи.
pub const MAX_VALUE_CHARS: usize = 200;
/// Предел длины одного слоя в system-сообщении.
pub const MAX_BLOCK_CHARS: usize = 1500;

/// Системный промпт экстрактора памяти. В отличие от `facts.rs`, модель
/// обязана назвать слой: это и есть «явно выбираем, что и куда сохраняется»
/// на стороне автоматики.
pub const MEMORY_SYSTEM: &str = "Ты ведёшь трёхслойную память агента и раскладываешь новую информацию по слоям.\n\
СЛОИ:\n\
- short — краткосрочная: только про текущий диалог (тема разговора, о чём договорились сейчас). Ключи с префиксом \"диалог.\" или \"тема.\".\n\
- working — рабочая: данные текущей задачи (вводные, ограничения, сроки, промежуточные результаты). Ключи с префиксом \"задача.\", \"данные.\" или \"шаг.\".\n\
- long — долговременная: профиль пользователя, принятые решения, устойчивые знания. Ключи с префиксом \"профиль.\", \"решение.\" или \"знание.\".\n\
Тебе дают текущее содержимое слоёв и последние реплики. Верни ТОЛЬКО JSON вида \
{\"ops\":[{\"op\":\"add|update|delete\",\"layer\":\"short|working|long\",\"key\":\"...\",\"value\":\"...\"}]}. \
op=add/update — положить значение по ключу, op=delete — запись больше не верна. Если менять нечего, верни {\"ops\":[]}. \
Ключ — префикс слоя плюс короткое существительное в нижнем регистре (\"профиль.язык\", \"задача.срок\"). \
Значение — одна строка до 200 символов. Не выдумывай, не дублируй ключи, не записывай болтовню. \
Без пояснений и без markdown-заборов.";

/// Одна запись памяти.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub value: String,
    /// Номер обновления, на котором запись трогали последний раз — по нему
    /// вытесняем при переполнении слоя.
    #[serde(default)]
    pub turn: usize,
    /// Кто положил: `user` (ручная правка) или `extractor`.
    #[serde(default)]
    pub source: String,
    /// Почему запись оказалась именно в этом слое — решение [`route`].
    #[serde(default)]
    pub reason: String,
}

/// Одна реплика диалога в дословной копии сессии.
///
/// Записи (`Record`) — это выжимка, которую сделал экстрактор; `DialogTurn` —
/// сырая реплика как она была. Краткосрочный слой хранит и то, и другое:
/// «что агент помнит про этот разговор» и «что в этом разговоре вообще было».
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogTurn {
    /// Номер реплики в сессии, с единицы.
    pub n: usize,
    /// `user` или `assistant` (system в историю сессии не попадает).
    pub role: String,
    pub text: String,
}

/// Содержимое одного файла слоя.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LayerFile {
    #[serde(default)]
    layer: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    records: Vec<Record>,
    /// Дословная копия диалога. Есть только у краткосрочного слоя: у рабочего
    /// и долговременного разговора нет, у них есть задача и профиль.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dialog: Vec<DialogTurn>,
}

/// Решение маршрутизатора по одной записи.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routed {
    pub layer: Layer,
    pub reason: String,
    /// Экстрактор просил другой слой, а префикс ключа сказал иначе.
    pub overridden: Option<Layer>,
}

/// Куда положить запись с таким ключом. Чистая функция — её проверяет
/// `--verify-memory routing` без единого запроса к сети.
pub fn route(key: &str, hint: Option<Layer>) -> Routed {
    let k = key.trim().to_lowercase();
    if let Some((prefix, layer)) = PREFIXES.iter().find(|(p, _)| k.starts_with(p)) {
        return Routed {
            layer: *layer,
            reason: format!("префикс ключа `{prefix}`"),
            overridden: hint.filter(|h| *h != *layer),
        };
    }
    match hint {
        Some(layer) => Routed {
            layer,
            reason: "слой назвал экстрактор".into(),
            overridden: None,
        },
        None => Routed {
            // Незнакомая запись — про текущую задачу, а не про человека
            // вообще: в долговременный профиль она попадёт только если её
            // туда попросили явно.
            layer: Layer::Working,
            reason: "по умолчанию — рабочий слой".into(),
            overridden: None,
        },
    }
}

/// Операция над памятью, как её вернул экстрактор.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Upsert {
        key: String,
        value: String,
        layer: Option<Layer>,
    },
    Delete {
        key: String,
        layer: Option<Layer>,
    },
}

/// Что сделало одно обновление памяти — по слоям.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub evicted: usize,
    /// Куда именно легли записи: (слой, ключ, причина).
    pub routes: Vec<(Layer, String, String)>,
    /// Ключи, где подсказка экстрактора разошлась с префиксом.
    pub overrides: Vec<String>,
}

impl Delta {
    pub fn touched(&self) -> usize {
        self.added + self.updated + self.deleted
    }

    pub fn line(&self) -> String {
        let mut s = format!("+{} ~{} -{}", self.added, self.updated, self.deleted);
        if self.evicted > 0 {
            s.push_str(&format!(" (вытеснено {})", self.evicted));
        }
        if !self.overrides.is_empty() {
            s.push_str(&format!(
                " (маршрут исправлен: {})",
                self.overrides.join(", ")
            ));
        }
        s
    }
}

/// Трёхслойная память агента, лежащая на диске.
///
/// Каждая мутация сразу пишет свой слой в файл: память, которая теряется при
/// падении процесса, — это не память.
#[derive(Clone, Debug)]
pub struct MemoryStore {
    root: PathBuf,
    /// Имя файла краткосрочного слоя (обычно id сессии).
    short_scope: String,
    /// Имя файла рабочего слоя — текущая задача.
    work_scope: String,
    /// Имя файла долговременного слоя — профиль.
    long_scope: String,
    short: Vec<Record>,
    working: Vec<Record>,
    long: Vec<Record>,
    /// Дословная копия текущего диалога — она же лежит в файле короткого слоя.
    dialog: Vec<DialogTurn>,
    updates: usize,
    extractions: usize,
    last_error: Option<String>,
    /// Последние решения маршрутизатора — для `/mem routes`.
    routes: Vec<(Layer, String, String)>,
}

pub const DEFAULT_TASK: &str = "default";
pub const DEFAULT_PROFILE: &str = "profile";

impl Default for MemoryStore {
    fn default() -> MemoryStore {
        MemoryStore::in_memory()
    }
}

impl MemoryStore {
    /// Память без диска — для тестов и для одноразовых прогонов.
    pub fn in_memory() -> MemoryStore {
        MemoryStore {
            root: PathBuf::new(),
            short_scope: "session".into(),
            work_scope: DEFAULT_TASK.into(),
            long_scope: DEFAULT_PROFILE.into(),
            short: Vec::new(),
            working: Vec::new(),
            long: Vec::new(),
            dialog: Vec::new(),
            updates: 0,
            extractions: 0,
            last_error: None,
            routes: Vec::new(),
        }
    }

    /// Открыть память на диске: три папки создаются, три файла читаются.
    pub fn open(root: impl AsRef<Path>, short_scope: &str, work_scope: &str) -> MemoryStore {
        let mut store = MemoryStore {
            root: root.as_ref().to_path_buf(),
            short_scope: safe_scope(short_scope, "session"),
            work_scope: safe_scope(work_scope, DEFAULT_TASK),
            long_scope: DEFAULT_PROFILE.into(),
            ..MemoryStore::in_memory()
        };
        store.ensure_dirs();
        for layer in Layer::ALL {
            let file = read_layer(&store.path(layer));
            *store.slot_mut(layer) = file.records;
            if layer == Layer::Short {
                store.dialog = file.dialog;
            }
            // Файл слоя создаётся сразу, ещё пустым. Иначе «в working ничего
            // нет» невозможно отличить от «working ещё не открывали», а
            // проверять надо первое.
            if !store.path(layer).exists() {
                store.persist(layer);
            }
        }
        store
    }

    fn on_disk(&self) -> bool {
        !self.root.as_os_str().is_empty()
    }

    pub fn task(&self) -> &str {
        &self.work_scope
    }

    pub fn session_scope(&self) -> &str {
        &self.short_scope
    }

    /// Файл слоя. Публичный намеренно: «проверить, что попало в слой» —
    /// это прочитать вот этот путь.
    pub fn path(&self, layer: Layer) -> PathBuf {
        let name = match layer {
            Layer::Short => &self.short_scope,
            Layer::Working => &self.work_scope,
            Layer::Long => &self.long_scope,
        };
        self.root.join(layer.dir()).join(format!("{name}.json"))
    }

    fn ensure_dirs(&self) {
        if !self.on_disk() {
            return;
        }
        for layer in Layer::ALL {
            let dir = self.root.join(layer.dir());
            if let Err(e) = fs::create_dir_all(&dir) {
                eprintln!("warning: не создать {}: {e}", dir.display());
            }
        }
    }

    fn slot(&self, layer: Layer) -> &Vec<Record> {
        match layer {
            Layer::Short => &self.short,
            Layer::Working => &self.working,
            Layer::Long => &self.long,
        }
    }

    fn slot_mut(&mut self, layer: Layer) -> &mut Vec<Record> {
        match layer {
            Layer::Short => &mut self.short,
            Layer::Working => &mut self.working,
            Layer::Long => &mut self.long,
        }
    }

    pub fn records(&self, layer: Layer) -> &[Record] {
        self.slot(layer)
    }

    pub fn len(&self, layer: Layer) -> usize {
        self.slot(layer).len()
    }

    pub fn total(&self) -> usize {
        Layer::ALL.iter().map(|l| self.len(*l)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
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

    pub fn routes(&self) -> &[(Layer, String, String)] {
        &self.routes
    }

    /// Все ключи всех слоёв — живой источник автокомплита `/mem del`.
    pub fn keys(&self) -> Vec<String> {
        Layer::ALL
            .iter()
            .flat_map(|l| self.slot(*l).iter().map(|r| r.key.clone()))
            .collect()
    }

    pub fn get(&self, layer: Layer, key: &str) -> Option<&str> {
        let k = normalize(key);
        self.slot(layer)
            .iter()
            .find(|r| normalize(&r.key) == k)
            .map(|r| r.value.as_str())
    }

    /// В каком слое лежит ключ (`/mem where`).
    pub fn find(&self, key: &str) -> Option<(Layer, &Record)> {
        let k = normalize(key);
        Layer::ALL.iter().find_map(|l| {
            self.slot(*l)
                .iter()
                .find(|r| normalize(&r.key) == k)
                .map(|r| (*l, r))
        })
    }

    /// Ручная запись: слой назвал человек, маршрутизатор не спорит.
    /// Возвращает `true`, если запись новая.
    pub fn set(&mut self, layer: Layer, key: &str, value: &str) -> bool {
        self.updates += 1;
        let turn = self.updates;
        let new = self.upsert(layer, key, value, turn, "user", "явный выбор пользователя");
        self.evict(layer);
        self.persist(layer);
        new
    }

    /// Ручное удаление из конкретного слоя.
    pub fn remove(&mut self, layer: Layer, key: &str) -> bool {
        self.updates += 1;
        let k = normalize(key);
        let slot = self.slot_mut(layer);
        match slot.iter().position(|r| normalize(&r.key) == k) {
            Some(i) => {
                slot.remove(i);
                self.persist(layer);
                true
            }
            None => false,
        }
    }

    /// Очистить один слой — и в памяти, и на диске.
    pub fn clear(&mut self, layer: Layer) -> usize {
        let n = self.slot(layer).len();
        self.slot_mut(layer).clear();
        self.updates += 1;
        self.persist(layer);
        n
    }

    /// Сменить задачу: рабочий слой уезжает в свой файл, на его место
    /// поднимается файл новой задачи. Долговременный слой не трогаем — в
    /// этом вся разница между рабочим и долговременным.
    pub fn set_task(&mut self, task: &str) {
        let task = safe_scope(task, DEFAULT_TASK);
        if task == self.work_scope {
            return;
        }
        self.persist(Layer::Working);
        self.work_scope = task;
        self.working = if self.on_disk() {
            read_layer(&self.path(Layer::Working)).records
        } else {
            Vec::new()
        };
        if self.on_disk() && !self.path(Layer::Working).exists() {
            self.persist(Layer::Working);
        }
    }

    /// Сменить диалог: краткосрочный слой всегда начинается пустым — он
    /// принадлежит текущему разговору и ничему больше.
    pub fn set_session(&mut self, scope: &str) {
        let scope = safe_scope(scope, "session");
        if scope == self.short_scope {
            return;
        }
        self.persist(Layer::Short);
        self.short_scope = scope;
        let file = if self.on_disk() {
            read_layer(&self.path(Layer::Short))
        } else {
            LayerFile::default()
        };
        self.short = file.records;
        self.dialog = file.dialog;
        if self.on_disk() && !self.path(Layer::Short).exists() {
            self.persist(Layer::Short);
        }
    }

    /// Дословная копия диалога, как она лежит в файле короткого слоя.
    pub fn dialog(&self) -> &[DialogTurn] {
        &self.dialog
    }

    /// Переписать краткосрочный слой текущей историей сессии.
    ///
    /// Это намеренный дубликат: та же история есть в `~/.ask6/sessions/`, но
    /// краткосрочная память — «текущий диалог», и проверяться она должна там,
    /// где живёт, а не по чужому файлу. В отличие от записей, диалог кладётся
    /// **всегда** — независимо от стратегии и от того, звали ли экстрактора;
    /// поэтому `memory/short/<сессия>.json` перестаёт быть пустой коробкой с
    /// одним только именем сессии.
    ///
    /// Возвращает `true`, если файл переписан.
    pub fn sync_dialog(&mut self, history: &[ChatMessage]) -> bool {
        let next: Vec<DialogTurn> = history
            .iter()
            .filter(|m| m.role != crate::api::Role::System)
            .enumerate()
            .map(|(i, m)| DialogTurn {
                n: i + 1,
                role: m.role.as_str().to_string(),
                text: m.content.clone(),
            })
            .collect();
        if next == self.dialog {
            return false;
        }
        self.dialog = next;
        self.persist(Layer::Short);
        true
    }

    /// Применить пачку операций экстрактора через маршрутизатор.
    pub fn apply_ops(&mut self, ops: &[Op]) -> Delta {
        self.updates += 1;
        self.last_error = None;
        let turn = self.updates;
        let mut delta = Delta::default();
        let mut touched: Vec<Layer> = Vec::new();
        for op in ops {
            match op {
                Op::Upsert { key, value, layer } => {
                    if key.trim().is_empty() || value.trim().is_empty() {
                        continue;
                    }
                    let routed = route(key, *layer);
                    if let Some(asked) = routed.overridden {
                        delta.overrides.push(format!(
                            "{} просили в {asked}, лёг в {}",
                            key.trim(),
                            routed.layer
                        ));
                    }
                    if self.upsert(routed.layer, key, value, turn, "extractor", &routed.reason) {
                        delta.added += 1;
                    } else {
                        delta.updated += 1;
                    }
                    delta
                        .routes
                        .push((routed.layer, key.trim().to_string(), routed.reason));
                    touched.push(routed.layer);
                }
                Op::Delete { key, layer } => {
                    // Удаление ищет ключ там, где он назван, а если слой не
                    // назвали — во всех трёх.
                    let targets: Vec<Layer> = match layer {
                        Some(l) => vec![*l],
                        None => Layer::ALL.to_vec(),
                    };
                    for l in targets {
                        let k = normalize(key);
                        let slot = self.slot_mut(l);
                        if let Some(i) = slot.iter().position(|r| normalize(&r.key) == k) {
                            slot.remove(i);
                            delta.deleted += 1;
                            touched.push(l);
                            break;
                        }
                    }
                }
            }
        }
        for layer in Layer::ALL {
            if touched.contains(&layer) {
                delta.evicted += self.evict(layer);
                self.persist(layer);
            }
        }
        self.routes = delta.routes.clone();
        delta
    }

    fn upsert(
        &mut self,
        layer: Layer,
        key: &str,
        value: &str,
        turn: usize,
        source: &str,
        reason: &str,
    ) -> bool {
        let key = key.trim().to_string();
        let value = clip(value);
        let k = normalize(&key);
        // Запись живёт ровно в одном слое: если ключ переезжает, из старого
        // слоя он уходит. Иначе «разделены» превращается в «продублированы».
        for other in Layer::ALL {
            if other == layer {
                continue;
            }
            let slot = self.slot_mut(other);
            if let Some(i) = slot.iter().position(|r| normalize(&r.key) == k) {
                slot.remove(i);
                self.persist(other);
            }
        }
        let slot = self.slot_mut(layer);
        match slot.iter().position(|r| normalize(&r.key) == k) {
            Some(i) => {
                slot[i].value = value;
                slot[i].turn = turn;
                slot[i].source = source.to_string();
                slot[i].reason = reason.to_string();
                false
            }
            None => {
                slot.push(Record {
                    key,
                    value,
                    turn,
                    source: source.to_string(),
                    reason: reason.to_string(),
                });
                true
            }
        }
    }

    /// Вытеснение «давно не трогали» по потолку слоя и по длине блока.
    fn evict(&mut self, layer: Layer) -> usize {
        let cap = layer.capacity();
        let mut evicted = 0;
        loop {
            let over_count = self.slot(layer).len() > cap;
            let over_chars =
                !self.slot(layer).is_empty() && self.body(layer).chars().count() > MAX_BLOCK_CHARS;
            if !over_count && !over_chars {
                break;
            }
            let Some(oldest) = self
                .slot(layer)
                .iter()
                .enumerate()
                .min_by_key(|(i, r)| (r.turn, *i))
                .map(|(i, _)| i)
            else {
                break;
            };
            self.slot_mut(layer).remove(oldest);
            evicted += 1;
        }
        evicted
    }

    fn persist(&self, layer: Layer) {
        if !self.on_disk() {
            return;
        }
        let path = self.path(layer);
        let file = LayerFile {
            layer: layer.label().to_string(),
            scope: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            records: self.slot(layer).clone(),
            dialog: if layer == Layer::Short {
                self.dialog.clone()
            } else {
                Vec::new()
            },
        };
        let Ok(data) = serde_json::to_string_pretty(&file) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, data).is_ok() && fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Тело слоя в детерминированном порядке (по ключу).
    fn body(&self, layer: Layer) -> String {
        let mut sorted: Vec<&Record> = self.slot(layer).iter().collect();
        sorted.sort_by(|a, b| normalize(&a.key).cmp(&normalize(&b.key)));
        sorted
            .iter()
            .map(|r| format!("- {}: {}", r.key.trim(), r.value.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Блок одного слоя для system-сообщения. Пустой слой — блока нет.
    pub fn block(&self, layer: Layer) -> Option<String> {
        if self.slot(layer).is_empty() {
            return None;
        }
        Some(format!(
            "## {} [{}]\n{}\n\n{}",
            layer.title(),
            layer.label(),
            layer.describe(),
            self.body(layer)
        ))
    }

    /// Блоки всех непустых слоёв, от долговременного к краткосрочному.
    pub fn blocks(&self) -> Vec<String> {
        Layer::ALL.iter().filter_map(|l| self.block(*l)).collect()
    }

    /// Что показывает `/mem` и отчёт проверки.
    pub fn listing(&self) -> String {
        let mut lines = vec![
            format!(
                "память: long {} · working {} · short {} (обновлений {}, вызовов экстрактора {})",
                self.len(Layer::Long),
                self.len(Layer::Working),
                self.len(Layer::Short),
                self.updates,
                self.extractions
            ),
            format!(
                "корень: {}",
                if self.on_disk() {
                    self.root.display().to_string()
                } else {
                    "(без диска)".into()
                }
            ),
            format!("задача: {} · диалог: {}", self.work_scope, self.short_scope),
            format!(
                "дословная копия диалога в коротком слое: {} реплик",
                self.dialog.len()
            ),
        ];
        if let Some(err) = self.last_error() {
            lines.push(format!("последняя ошибка разбора: {err}"));
        }
        for layer in Layer::ALL {
            lines.push(String::new());
            lines.push(format!(
                "[{}] {} — {}",
                layer.label(),
                self.path(layer).display(),
                if self.slot(layer).is_empty() {
                    "пусто".to_string()
                } else {
                    format!("{} записей", self.len(layer))
                }
            ));
            if !self.slot(layer).is_empty() {
                lines.push(self.body(layer));
            }
            if layer == Layer::Short && !self.dialog.is_empty() {
                lines.push(format!(
                    "  + дословный диалог: {} реплик ({} символов)",
                    self.dialog.len(),
                    self.dialog.iter().map(|t| t.text.chars().count()).sum::<usize>()
                ));
            }
        }
        lines.join("\n")
    }

    /// Строка для футера и `/strategy show`.
    pub fn status_line(&self) -> String {
        format!(
            "long {} · work {} · short {} (диалог {})",
            self.len(Layer::Long),
            self.len(Layer::Working),
            self.len(Layer::Short),
            self.dialog.len()
        )
    }
}

/// Текущее содержимое слоёв плюс последние реплики — вход экстрактора.
/// Как и в `facts.rs`, `recent` намеренно короткий: промпт памяти не должен
/// расти вместе с историей.
pub fn extract_prompt(store: &MemoryStore, recent: &[ChatMessage]) -> String {
    let mut current = String::from("ТЕКУЩАЯ ПАМЯТЬ:\n");
    for layer in Layer::ALL {
        current.push_str(&format!("[{}]\n", layer.label()));
        if store.records(layer).is_empty() {
            current.push_str("пусто\n");
        } else {
            current.push_str(&store.body(layer));
            current.push('\n');
        }
    }
    let transcript = recent
        .iter()
        .map(|m| format!("{}: {}", m.role.as_str(), m.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{current}\nНОВЫЕ РЕПЛИКИ:\n{transcript}\n\nВерни только JSON с операциями.")
}

/// Последняя пара реплик — вход экстрактора (см. `facts::recent_slice`).
pub fn recent_slice(history: &[ChatMessage]) -> &[ChatMessage] {
    let n = history.len().min(2);
    &history[history.len() - n..]
}

#[derive(Deserialize)]
struct RawOp {
    #[serde(default)]
    op: String,
    #[serde(default)]
    layer: String,
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

/// Толерантный разбор ответа экстрактора: неразобранный ответ оставляет
/// память как была и никогда не роняет ход.
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
            // Нераспознанный слой — не ошибка: маршрутизатор разложит по
            // префиксу ключа или по правилу умолчания.
            let layer = Layer::parse(&r.layer).ok();
            match r.op.trim().to_ascii_lowercase().as_str() {
                "delete" | "remove" | "del" => Some(Op::Delete { key, layer }),
                "noop" | "none" | "skip" => None,
                _ if !r.value.trim().is_empty() => Some(Op::Upsert {
                    key,
                    value: r.value.trim().to_string(),
                    layer,
                }),
                _ => None,
            }
        })
        .collect())
}

/// Корень памяти: `ASK_MEMORY_DIR`, иначе папка снапшота задачи рядом с
/// бинарником (`tree/task-11/memory`), иначе `~/.ask6/memory`.
///
/// Папка задачи ищется от исполняемого файла (`.../target/release/ask` →
/// `.../`), и только если рядом действительно лежит `Cargo.toml` — чтобы
/// установленный в систему бинарник не пытался писать в чужие каталоги.
pub fn memory_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("ASK_MEMORY_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    if let Some(dir) = snapshot_root() {
        return dir;
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".ask6")
        .join("memory")
}

fn snapshot_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let snapshot = exe.parent()?.parent()?.parent()?;
    snapshot
        .join("Cargo.toml")
        .is_file()
        .then(|| snapshot.join("memory"))
}

fn read_layer(path: &Path) -> LayerFile {
    let Ok(data) = fs::read_to_string(path) else {
        return LayerFile::default();
    };
    match serde_json::from_str::<LayerFile>(&data) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("warning: битый файл памяти {}: {e}", path.display());
            LayerFile::default()
        }
    }
}

/// Имя файла из произвольной строки: без разделителей пути и без сюрпризов.
fn safe_scope(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.chars().take(64).collect()
    }
}

fn normalize(key: &str) -> String {
    key.trim().to_lowercase()
}

fn clip(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= MAX_VALUE_CHARS {
        return value.to_string();
    }
    let mut out: String = value.chars().take(MAX_VALUE_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ask-memory-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn prefix_beats_the_extractor_hint() {
        // Экстрактор просит long, а ключ размечен как рабочий — побеждает
        // префикс, и расхождение видно.
        let r = route("задача.срок", Some(Layer::Long));
        assert_eq!(r.layer, Layer::Working);
        assert_eq!(r.overridden, Some(Layer::Long));

        let r = route("профиль.язык", Some(Layer::Long));
        assert_eq!(r.layer, Layer::Long);
        assert_eq!(r.overridden, None, "совпало — исправлять нечего");
    }

    #[test]
    fn hint_is_used_when_the_key_has_no_prefix() {
        assert_eq!(route("бюджет", Some(Layer::Long)).layer, Layer::Long);
        // Без подсказки и без префикса — рабочий слой, а не профиль.
        assert_eq!(route("бюджет", None).layer, Layer::Working);
    }

    #[test]
    fn layers_are_three_separate_files_on_disk() {
        let root = scratch("files");
        let mut m = MemoryStore::open(&root, "ses1", "стенд");
        m.set(Layer::Long, "профиль.язык", "русский");
        m.set(Layer::Working, "задача.срок", "две недели");
        m.set(Layer::Short, "тема.сейчас", "обсуждаем память");

        for layer in Layer::ALL {
            assert!(m.path(layer).is_file(), "нет файла слоя {layer}");
        }
        let long = fs::read_to_string(m.path(Layer::Long)).unwrap();
        assert!(long.contains("профиль.язык"));
        assert!(
            !long.contains("задача.срок") && !long.contains("тема.сейчас"),
            "слои не должны протекать друг в друга"
        );

        // Перечитали с диска — всё на месте.
        let again = MemoryStore::open(&root, "ses1", "стенд");
        assert_eq!(again.get(Layer::Long, "профиль.язык"), Some("русский"));
        assert_eq!(again.get(Layer::Working, "задача.срок"), Some("две недели"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn switching_the_task_swaps_working_but_keeps_long() {
        let root = scratch("task");
        let mut m = MemoryStore::open(&root, "ses1", "alpha");
        m.set(Layer::Long, "профиль.язык", "русский");
        m.set(Layer::Working, "задача.код", "ALPHA1");

        m.set_task("beta");
        assert!(m.get(Layer::Working, "задача.код").is_none(), "рабочая память задачи beta пуста");
        assert_eq!(
            m.get(Layer::Long, "профиль.язык"),
            Some("русский"),
            "долговременная переживает смену задачи"
        );
        m.set(Layer::Working, "задача.код", "BETA2");

        // Вернулись — данные alpha никуда не делись, они в своём файле.
        m.set_task("alpha");
        assert_eq!(m.get(Layer::Working, "задача.код"), Some("ALPHA1"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_dialog_starts_with_an_empty_short_layer() {
        let root = scratch("session");
        let mut m = MemoryStore::open(&root, "ses1", "alpha");
        m.set(Layer::Short, "тема.сейчас", "память");
        m.set_session("ses2");
        assert_eq!(m.len(Layer::Short), 0);
        assert_eq!(m.get(Layer::Long, "нет"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn short_layer_keeps_the_whole_dialog_verbatim_and_survives_reopen() {
        let root = scratch("dialog");
        let mut m = MemoryStore::open(&root, "ses1", "alpha");
        let history = vec![
            ChatMessage {
                role: crate::api::Role::System,
                content: "не должно попасть в диалог".into(),
            },
            ChatMessage::user("привет, меня зовут Евгений"),
            ChatMessage::assistant("привет, Евгений"),
        ];
        assert!(m.sync_dialog(&history), "первая синхронизация переписывает файл");
        assert!(!m.sync_dialog(&history), "повтор без изменений файл не трогает");

        // Слой пуст по записям — экстрактора не звали — но диалог на диске.
        assert_eq!(m.len(Layer::Short), 0);
        let raw = fs::read_to_string(m.path(Layer::Short)).unwrap();
        assert!(raw.contains("привет, меня зовут Евгений"), "{raw}");
        assert!(!raw.contains("не должно попасть"), "system в диалог не идёт");

        let reopened = MemoryStore::open(&root, "ses1", "alpha");
        assert_eq!(reopened.dialog().len(), 2);
        assert_eq!(reopened.dialog()[0].role, "user");
        assert_eq!(reopened.dialog()[1].n, 2);
        assert_eq!(reopened.dialog()[1].text, "привет, Евгений");

        // Новый диалог — новая копия: короткая память принадлежит разговору.
        let mut m = reopened;
        m.set_session("ses2");
        assert!(m.dialog().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn opening_memory_materialises_all_three_layer_files() {
        let root = scratch("files");
        let m = MemoryStore::open(&root, "ses1", "alpha");
        for layer in Layer::ALL {
            assert!(
                m.path(layer).is_file(),
                "{} должен существовать сразу после открытия",
                m.path(layer).display()
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_key_lives_in_exactly_one_layer() {
        let mut m = MemoryStore::in_memory();
        m.set(Layer::Short, "код", "X1");
        m.set(Layer::Long, "код", "X2");
        assert_eq!(m.len(Layer::Short), 0, "переезд, а не дубль");
        assert_eq!(m.get(Layer::Long, "код"), Some("X2"));
        assert_eq!(m.find("код").map(|(l, _)| l), Some(Layer::Long));
    }

    #[test]
    fn ops_are_routed_by_prefix_and_report_overrides() {
        let mut m = MemoryStore::in_memory();
        let ops = parse_ops(
            r#"{"ops":[
                {"op":"add","layer":"long","key":"задача.срок","value":"две недели"},
                {"op":"add","layer":"long","key":"профиль.язык","value":"русский"},
                {"op":"add","layer":"short","key":"тема.сейчас","value":"память"}
            ]}"#,
        )
        .unwrap();
        let delta = m.apply_ops(&ops);
        assert_eq!(delta.added, 3);
        assert_eq!(delta.overrides.len(), 1, "ровно одно расхождение");
        assert_eq!(m.get(Layer::Working, "задача.срок"), Some("две недели"));
        assert_eq!(m.get(Layer::Long, "профиль.язык"), Some("русский"));
        assert_eq!(m.get(Layer::Short, "тема.сейчас"), Some("память"));
    }

    #[test]
    fn parse_survives_fences_and_unknown_layers() {
        let ops = parse_ops(
            "Вот JSON:\n```json\n{\"ops\":[{\"op\":\"add\",\"layer\":\"мусор\",\"key\":\"профиль.язык\",\"value\":\"русский\"}]}\n```",
        )
        .unwrap();
        assert_eq!(ops.len(), 1);
        let mut m = MemoryStore::in_memory();
        m.apply_ops(&ops);
        // Слой не разобран — спас префикс ключа.
        assert_eq!(m.get(Layer::Long, "профиль.язык"), Some("русский"));
        assert!(parse_ops("не json").is_err());
        assert_eq!(parse_ops("{\"ops\":[]}").unwrap().len(), 0);
    }

    #[test]
    fn blocks_are_separate_per_layer_and_ordered_long_first() {
        let mut m = MemoryStore::in_memory();
        assert!(m.blocks().is_empty(), "пустая память — блоков нет");
        m.set(Layer::Short, "тема.сейчас", "память");
        m.set(Layer::Long, "профиль.язык", "русский");
        let blocks = m.blocks();
        assert_eq!(blocks.len(), 2, "пустой рабочий слой блока не даёт");
        assert!(blocks[0].contains("[long]"));
        assert!(blocks[1].contains("[short]"));
    }

    #[test]
    fn short_layer_evicts_the_least_recently_touched() {
        let mut m = MemoryStore::in_memory();
        for i in 0..Layer::Short.capacity() + 3 {
            m.set(Layer::Short, &format!("тема.{i}"), "значение");
        }
        assert_eq!(m.len(Layer::Short), Layer::Short.capacity());
        assert!(m.get(Layer::Short, "тема.0").is_none(), "вытеснено самое старое");
        assert!(m.get(Layer::Short, "тема.14").is_some());
    }

    #[test]
    fn scope_names_cannot_escape_the_memory_root() {
        let root = scratch("scope");
        let m = MemoryStore::open(&root, "../../etc/passwd", "a/b");
        assert_eq!(m.path(Layer::Short).parent().unwrap(), root.join("short"));
        assert!(!m.session_scope().contains('/'));
        assert!(!m.task().contains('/'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_without_a_layer_searches_all_three() {
        let mut m = MemoryStore::in_memory();
        m.set(Layer::Working, "данные.х", "1");
        let ops = parse_ops(r#"{"ops":[{"op":"delete","key":"данные.х"}]}"#).unwrap();
        let delta = m.apply_ops(&ops);
        assert_eq!(delta.deleted, 1);
        assert!(m.find("данные.х").is_none());
    }
}
