//! Персонализация: профиль пользователя поверх модели памяти.
//!
//! # Что это
//!
//! Профиль — это **отдельная настройка** (`profile` в `/settings`,
//! `--profile`, `/profile`), а не ещё один текст в системном промпте. Он
//! описывает, *как* ассистент разговаривает с этим человеком:
//!
//! | поле | что задаёт | пример |
//! |------|------------|--------|
//! | [`Profile::style`] | манера речи, роль | «дипломированный химик, строго научный стиль» |
//! | [`Profile::format`] | оформление ответа | «начинай с `Гипотеза:`, заканчивай `Вывод:`» |
//! | [`Profile::limits`] | ограничения | «без эмодзи», «не длиннее 60 слов» |
//! | [`Profile::max_chars`] | жёсткий потолок ответа | 600 символов |
//!
//! Первые три уезжают в `system` одним блоком `## Профиль пользователя`,
//! четвёртое — не просьба, а клиентское усечение (`api::enforce_max_chars`),
//! как и `max_chars` в настройках: лимит, о котором модель только попросили,
//! гарантией не является.
//!
//! # Поверх памяти, а не вместо неё
//!
//! Долговременный слой памяти (`memory/long/profile.json`) уже хранит записи
//! с префиксом `профиль.` — имя, язык, предпочтения, которые агент выделил из
//! разговора сам. [`Profile::block`] подклеивает их в тот же блок под
//! заголовком «известно о пользователе». Поэтому персонализация — это два
//! источника в одном блоке: что человек выбрал руками (профиль) и что агент
//! про него запомнил (долговременная память). Отсюда и «учитывает
//! автоматически»: ни то, ни другое в запросе повторять не нужно.
//!
//! # Где лежит
//!
//! `<корень памяти>/long/profiles.json` — тот же корень, что и у слоёв
//! памяти (`ASK_MEMORY_DIR` / `--memory-dir`). Файл хранит выбранный профиль
//! и профили пользователя; встроенные ([`BUILTINS`]) подмешиваются при
//! загрузке, а одноимённый профиль из файла встроенный перекрывает. Так
//! каталог не приходится копировать на диск, чтобы поправить одну строчку.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Res;

/// Имя файла с профилями внутри `<корень памяти>/long/`.
pub const PROFILES_FILE: &str = "profiles.json";

/// Значение настройки «профиль выключен».
pub const OFF: &str = "off";

/// Открывающий тег блока в `system`. Блок обрамлён тегами — как AGENTS.md в
/// `context.rs` — именно ради проверки: границу надо знать точно, чтобы
/// вырезать блок и сравнить всё остальное побайтно. Заголовком в markdown
/// границу не задать: следующий кусок системного сообщения заголовком быть
/// не обязан.
pub const BLOCK_HEAD: &str = "<user-profile";

/// Закрывающий тег блока.
pub const BLOCK_END: &str = "</user-profile>";

/// Один профиль: стиль, формат, ограничения.
///
/// `marker` — не украшение: это машинно-проверяемая подпись профиля
/// (см. [`Marker`]), по которой `--verify-profile voice` отличает «ответ
/// действительно в этом голосе» от «два ответа просто разные».
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    /// Человеческое имя для списка и статуса.
    pub title: String,
    /// Манера речи и роль.
    pub style: String,
    /// Как оформлять ответ.
    #[serde(default)]
    pub format: String,
    /// Ограничения: чего не делать, чего держаться.
    #[serde(default)]
    pub limits: Vec<String>,
    /// Жёсткий потолок видимого ответа в символах. Применяется клиентски,
    /// если в настройках свой `max_chars` не задан.
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// Подпись профиля для проверки.
    #[serde(default)]
    pub marker: Option<Marker>,
}

/// Машинно-проверяемая подпись голоса: что в ответе обязано быть и чего в нём
/// быть не должно. Слова берутся из самих ограничений профиля — то есть
/// проверяется ровно то, о чём профиль просил, а не вкус проверяющего.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// Хотя бы одно из этих слов обязано встретиться (регистр не важен).
    #[serde(default)]
    pub must: Vec<String>,
    /// Ни одного из этих встретиться не должно.
    #[serde(default)]
    pub forbid: Vec<String>,
}

impl Marker {
    /// Есть ли в тексте хотя бы одно обязательное слово.
    pub fn hits(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.must.iter().any(|w| lower.contains(&w.to_lowercase()))
    }

    /// Нет ли в тексте запрещённых слов.
    pub fn clean(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        !self.forbid.iter().any(|w| lower.contains(&w.to_lowercase()))
    }

    /// Ответ в этом голосе: обязательное есть, запрещённого нет.
    pub fn matches(&self, text: &str) -> bool {
        self.hits(text) && self.clean(text)
    }
}

impl Profile {
    /// Блок для `system`: профиль плюс то, что про пользователя уже знает
    /// долговременная память (`профиль.*`). Пустой список фактов — просто нет
    /// второй половины блока.
    pub fn block(&self, known: &[(String, String)]) -> String {
        let mut out = format!("{BLOCK_HEAD} id=\"{}\">\n", self.id);
        out.push_str(
            "Профиль пользователя: постоянные предпочтения человека, с которым ты \
             говоришь. Они действуют на каждый ответ, пока профиль активен; повторять \
             их в запросе не нужно.\n",
        );
        out.push_str(&format!("- Роль и стиль: {}\n", self.style.trim()));
        if !self.format.trim().is_empty() {
            out.push_str(&format!("- Формат ответа: {}\n", self.format.trim()));
        }
        if !self.limits.is_empty() {
            out.push_str("- Ограничения:\n");
            for l in &self.limits {
                out.push_str(&format!("  - {}\n", l.trim()));
            }
        }
        if let Some(n) = self.max_chars {
            out.push_str(&format!(
                "  - жёсткий потолок ответа: {n} символов (лишнее будет обрезано клиентом)\n"
            ));
        }
        if !known.is_empty() {
            out.push_str("- Известно о пользователе (долговременная память):\n");
            for (k, v) in known {
                out.push_str(&format!("  - {}: {}\n", k.trim(), v.trim()));
            }
        }
        format!("{}\n{BLOCK_END}", out.trim_end())
    }

    /// Строка для списка профилей и статуса.
    pub fn summary(&self) -> String {
        format!("{} — {}", self.id, self.title)
    }

    /// Развёрнутая карточка для `/profile show`.
    pub fn card(&self) -> String {
        let mut out = format!("[{}] {}\n", self.id, self.title);
        out.push_str(&format!("стиль:   {}\n", self.style.trim()));
        if !self.format.trim().is_empty() {
            out.push_str(&format!("формат:  {}\n", self.format.trim()));
        }
        for (i, l) in self.limits.iter().enumerate() {
            let head = if i == 0 { "лимиты:  " } else { "         " };
            out.push_str(&format!("{head}{}\n", l.trim()));
        }
        if let Some(n) = self.max_chars {
            out.push_str(&format!("потолок: {n} символов (клиентское усечение)\n"));
        }
        if let Some(m) = &self.marker {
            out.push_str(&format!(
                "подпись: должно быть [{}]{}\n",
                m.must.join(", "),
                if m.forbid.is_empty() {
                    String::new()
                } else {
                    format!(", не должно быть [{}]", m.forbid.join(", "))
                }
            ));
        }
        out.trim_end().to_string()
    }
}

/// Встроенный каталог. Три голоса, намеренно далёких друг от друга: по ним
/// разницу видно и человеку, и проверке.
pub fn builtins() -> Vec<Profile> {
    vec![
        Profile {
            id: "chemist".into(),
            title: "Дипломированный химик".into(),
            style: "Ты — дипломированный химик. Говори строго научно, как будто \
                    доказываешь гипотезу перед учёным советом: термины, механизмы, \
                    причинно-следственные связи, оценка достоверности."
                .into(),
            format: "начинай ответ словом «Гипотеза:», дальше «Обоснование:» \
                     (2–4 пункта), заканчивай строкой «Вывод:»"
                .into(),
            limits: vec![
                "никакого сленга и панибратства".into(),
                "без эмодзи".into(),
                "если утверждение спорное — прямо называй его гипотезой, а не фактом".into(),
            ],
            max_chars: Some(1200),
            marker: Some(Marker {
                must: vec!["гипотеза".into()],
                forbid: vec!["братан".into(), "кореш".into()],
            }),
        },
        Profile {
            id: "gopnik".into(),
            title: "Гопник с района".into(),
            style: "Ты — гопник с района, говоришь со своим корешем. Уличная речь, \
                    короткие рубленые фразы, простые житейские сравнения."
                .into(),
            format: "обращайся к собеседнику «братан» хотя бы раз, никаких списков \
                     и заголовков — сплошной разговорной речью"
                .into(),
            limits: vec![
                "без мата и без угроз".into(),
                "не длиннее 60 слов".into(),
                "никаких научных терминов — объясняй на пальцах".into(),
            ],
            max_chars: Some(600),
            marker: Some(Marker {
                must: vec!["братан".into()],
                forbid: vec!["гипотеза".into()],
            }),
        },
        Profile {
            id: "tutor".into(),
            title: "Терпеливый преподаватель".into(),
            style: "Ты — терпеливый преподаватель, который объясняет новичку. \
                    Никакого снисхождения, сначала суть, потом бытовая аналогия."
                .into(),
            format: "начинай строкой «Коротко:» с одним предложением, дальше \
                     «Подробнее:» списком из 2–3 пунктов, в конце «Аналогия:»"
                .into(),
            limits: vec![
                "не использовать термин, который тут же не объяснён".into(),
                "без эмодзи".into(),
            ],
            max_chars: Some(900),
            marker: Some(Marker {
                must: vec!["коротко:".into()],
                forbid: vec!["братан".into()],
            }),
        },
    ]
}

/// На диске: что выбрано и какие профили добавил человек.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    active: String,
    #[serde(default)]
    profiles: Vec<Profile>,
}

/// Каталог профилей плюс выбранный. Один на агента.
#[derive(Clone, Debug)]
pub struct ProfileSet {
    profiles: Vec<Profile>,
    active: String,
    path: Option<PathBuf>,
    /// Профили, приехавшие из файла (не встроенные) — для `/profile list`.
    custom: Vec<String>,
}

impl Default for ProfileSet {
    fn default() -> Self {
        ProfileSet {
            profiles: builtins(),
            active: OFF.to_string(),
            path: None,
            custom: Vec::new(),
        }
    }
}

impl ProfileSet {
    /// Только встроенный каталог, без диска (тесты, `--verify-profile wire`).
    pub fn in_memory() -> ProfileSet {
        ProfileSet::default()
    }

    /// Путь к файлу профилей внутри корня памяти.
    pub fn file_in(root: impl AsRef<Path>) -> PathBuf {
        root.as_ref().join("long").join(PROFILES_FILE)
    }

    /// Загрузить каталог: встроенные профили плюс файл. Одноимённый профиль
    /// из файла перекрывает встроенный целиком. Битый файл не роняет
    /// приложение — остаётся встроенный каталог, а `active` берётся из
    /// настроек.
    pub fn open(root: impl AsRef<Path>) -> ProfileSet {
        let path = ProfileSet::file_in(&root);
        let mut set = ProfileSet {
            profiles: builtins(),
            active: OFF.to_string(),
            path: Some(path.clone()),
            custom: Vec::new(),
        };
        let Ok(data) = fs::read_to_string(&path) else {
            return set;
        };
        let file: ProfilesFile = match serde_json::from_str(&data) {
            Ok(f) => f,
            Err(_) => return set,
        };
        for p in file.profiles {
            if p.id.trim().is_empty() {
                continue;
            }
            set.custom.push(p.id.clone());
            match set.profiles.iter_mut().find(|b| b.id == p.id) {
                Some(slot) => *slot = p,
                None => set.profiles.push(p),
            }
        }
        if !file.active.trim().is_empty() {
            set.active = file.active.trim().to_string();
        }
        set
    }

    pub fn all(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn is_custom(&self, id: &str) -> bool {
        self.custom.iter().any(|c| c == id)
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Идентификаторы для переключалки в настройках: `off` плюс каталог.
    pub fn ids(&self) -> Vec<String> {
        std::iter::once(OFF.to_string())
            .chain(self.profiles.iter().map(|p| p.id.clone()))
            .collect()
    }

    pub fn active_id(&self) -> &str {
        &self.active
    }

    /// Выбранный профиль. `off` или неизвестный id — `None`: неизвестное имя
    /// не должно молча подставлять чужой голос.
    pub fn active(&self) -> Option<&Profile> {
        if self.active == OFF || self.active.is_empty() {
            return None;
        }
        self.get(&self.active)
    }

    /// Выбрать профиль по имени. `off` выключает персонализацию.
    pub fn set_active(&mut self, id: &str) -> Res<()> {
        self.active = self.resolve(id)?;
        self.persist();
        Ok(())
    }

    /// Проверить имя по каталогу и привести к каноническому виду, ничего не
    /// меняя. Этим живёт `--profile <id>`: флаг на один запуск не должен
    /// переписывать файл с тем, что человек выбрал в TUI.
    pub fn resolve(&self, id: &str) -> Res<String> {
        let id = id.trim();
        if id.is_empty() || id.eq_ignore_ascii_case(OFF) || id.eq_ignore_ascii_case("none") {
            return Ok(OFF.to_string());
        }
        if self.get(id).is_none() {
            return Err(format!(
                "неизвестный профиль `{id}`; есть: {}",
                self.ids().join(", ")
            ));
        }
        Ok(id.to_string())
    }

    /// Принять выбор, не записывая файл: так `--profile <id>` на один
    /// запуск не переписывает то, что человек выбрал в TUI. Неизвестное имя
    /// остаётся неизвестным (`active()` вернёт `None`), а не подменяется.
    pub fn adopt_active(&mut self, id: &str) {
        self.active = id.trim().to_string();
    }

    /// Что печатает `/profile list` и `--profiles`.
    pub fn listing(&self) -> String {
        let mut lines = vec![format!(
            "профиль: {}{}",
            self.active,
            match self.active() {
                Some(p) => format!(" ({})", p.title),
                None => " (персонализация выключена)".into(),
            }
        )];
        if let Some(path) = &self.path {
            lines.push(format!("файл: {}", path.display()));
        }
        lines.push(String::new());
        for p in &self.profiles {
            let mark = if p.id == self.active { "*" } else { " " };
            let origin = if self.is_custom(&p.id) {
                " [свой]"
            } else {
                ""
            };
            lines.push(format!("{mark} {}{origin}", p.summary()));
        }
        lines.push(String::new());
        lines.push(format!("{} off — обычный ассистент без профиля", {
            if self.active == OFF {
                "*"
            } else {
                " "
            }
        }));
        lines.join("\n")
    }

    /// Записать выбор и свои профили. Встроенные в файл не уезжают: иначе
    /// правка каталога в коде не доезжала бы до тех, у кого файл уже есть.
    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let file = ProfilesFile {
            active: self.active.clone(),
            profiles: self
                .profiles
                .iter()
                .filter(|p| self.is_custom(&p.id))
                .cloned()
                .collect(),
        };
        let Ok(data) = serde_json::to_string_pretty(&file) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, data).is_ok() && fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// Вырезать блок профиля из системного сообщения — тем же способом, каким его
/// собрали. Возвращает (блок, остальное). Нужно проверке: «сменился только
/// блок профиля» — это побайтное сравнение остального.
pub fn split_block(system: &str) -> (Option<String>, String) {
    let Some(start) = system.find(BLOCK_HEAD) else {
        return (None, system.to_string());
    };
    let end = match system[start..].find(BLOCK_END) {
        Some(i) => start + i + BLOCK_END.len(),
        // Закрывающего тега нет — это баг сборки, и молчать о нём нельзя:
        // отдаём всё остальное как есть, проверка увидит пустой остаток.
        None => system.len(),
    };
    let block = system[start..end].to_string();
    let mut rest = String::with_capacity(system.len());
    rest.push_str(&system[..start]);
    rest.push_str(&system[end..]);
    (Some(block), rest.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chemist() -> Profile {
        builtins().into_iter().find(|p| p.id == "chemist").unwrap()
    }

    #[test]
    fn block_carries_style_format_and_limits() {
        let block = chemist().block(&[]);
        assert!(block.starts_with(BLOCK_HEAD) && block.ends_with(BLOCK_END));
        assert!(block.contains("дипломированный химик"));
        assert!(block.contains("Гипотеза:"));
        assert!(block.contains("без эмодзи"));
    }

    #[test]
    fn long_term_profile_facts_land_in_the_same_block() {
        let known = vec![
            ("профиль.имя".to_string(), "Евгений".to_string()),
            ("профиль.язык".to_string(), "русский".to_string()),
        ];
        let block = chemist().block(&known);
        assert!(block.contains("Известно о пользователе"));
        assert!(block.contains("профиль.имя: Евгений"));
        // Без фактов второй половины блока просто нет.
        assert!(!chemist().block(&[]).contains("Известно о пользователе"));
    }

    #[test]
    fn markers_separate_the_two_voices() {
        let chem = chemist().marker.unwrap();
        let gop = builtins()
            .into_iter()
            .find(|p| p.id == "gopnik")
            .unwrap()
            .marker
            .unwrap();
        let sci = "Гипотеза: рассеяние Рэлея. Вывод: небо голубое.";
        let street = "Слышь, братан, синее оно потому что воздух свет рассеивает.";
        assert!(chem.matches(sci) && !chem.matches(street));
        assert!(gop.matches(street) && !gop.matches(sci));
    }

    #[test]
    fn unknown_profile_is_refused_not_silently_swapped() {
        let set = ProfileSet::in_memory();
        assert!(set.resolve("нету-такого").is_err());
        assert_eq!(set.resolve("off").unwrap(), OFF);
        assert_eq!(set.resolve("gopnik").unwrap(), "gopnik");
        let mut set = set;
        assert!(set.set_active("нету-такого").is_err());
        assert_eq!(set.active_id(), OFF);
        assert!(set.active().is_none());
        set.set_active("gopnik").unwrap();
        assert_eq!(set.active().unwrap().id, "gopnik");
        set.set_active("off").unwrap();
        assert!(set.active().is_none());
    }

    /// Профиль, дописанный в файл руками (это и есть документированный
    /// способ завести свой), перекрывает одноимённый встроенный, а выбор
    /// переживает перезапуск.
    #[test]
    fn file_profile_overrides_the_builtin_and_survives_reopen() {
        let root = std::env::temp_dir().join(format!("ask-profiles-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("long")).unwrap();
        fs::write(
            ProfileSet::file_in(&root),
            r#"{"active":"chemist","profiles":[
                 {"id":"chemist","title":"Химик (мой)","style":"по-своему"},
                 {"id":"poet","title":"Поэт","style":"четверостишиями"}]}"#,
        )
        .unwrap();

        let set = ProfileSet::open(&root);
        assert_eq!(set.active_id(), "chemist");
        assert_eq!(set.get("chemist").unwrap().title, "Химик (мой)");
        assert!(set.is_custom("chemist") && set.is_custom("poet"));
        // Остальные встроенные на месте, свой добавился.
        assert!(set.get("gopnik").is_some());
        assert_eq!(set.all().len(), builtins().len() + 1);

        // Смена выбора переписывает файл, но встроенные в него не уезжают.
        let mut set = set;
        set.set_active("gopnik").unwrap();
        let raw = fs::read_to_string(ProfileSet::file_in(&root)).unwrap();
        assert!(raw.contains("\"active\": \"gopnik\""));
        assert!(!raw.contains("Гопник с района"), "встроенный в файл не пишем");
        assert_eq!(ProfileSet::open(&root).active_id(), "gopnik");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn split_block_cuts_exactly_the_profile_block() {
        let system = format!(
            "{}\n\nбазовый промпт\n\n## Долговременная память [long]\n- профиль.имя: Е",
            chemist().block(&[])
        );
        let (block, rest) = split_block(&system);
        assert!(block.unwrap().contains("Гипотеза:"));
        assert!(rest.contains("базовый промпт"));
        assert!(rest.contains("Долговременная память"));
        assert!(!rest.contains(BLOCK_HEAD));
    }
}
