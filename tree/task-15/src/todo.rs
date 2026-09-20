//! Состояние задачи как конечный автомат (задача 13).
//!
//! # Что это
//!
//! Обычный ход агента — «вопрос → ответ». Здесь ход превращается в
//! **тудушку**: прежде чем отвечать по существу, агент проходит лестницу
//! этапов, и на каждом видно три вещи, которых требовала постановка задачи:
//!
//! | что | где живёт | пример |
//! |-----|-----------|--------|
//! | этап задачи | [`TaskState::stage`] | `execute` — «Сделать» |
//! | текущий шаг | [`TaskState::step`] | шаг 3 из 5 |
//! | ожидаемое действие | [`Stage::expected`] | «выполнить план и выдать результат» |
//!
//! Лестница: `study → plan → execute → validate → report → done`
//! («изучить → запланировать → сделать → валидировать → отписаться»).
//! Постановка называла четыре состояния (`planning → execution →
//! validation → done`); здесь их пять, потому что «изучить» и
//! «запланировать» — разные ожидаемые действия, а «отписаться» — то, что
//! видит человек, и сливать его с `done` значит терять этап, на котором
//! ответ ещё можно поправить.
//!
//! # Почему это автомат, а не счётчик
//!
//! Переходы разрешены не любые: [`TaskState::advance`] работает только из
//! `Running`, [`TaskState::resume`] — только из `Paused`, а `Done`
//! терминален. Незаконный переход — это `Err`, а не молчаливое «ну ладно»:
//! состояние задачи, которое можно сдвинуть случайно, доказательством
//! ничему не служит.
//!
//! Этап **не закрывается сам по себе**. Агент обязан закончить свой ответ
//! строкой [`DONE_MARK`] с id этапа и строкой [`RESULT_MARK`] с итогом;
//! [`TaskState::confirm`] сверяет заявленный id с текущим этапом. Если
//! модель не подтвердила этап или подтвердила чужой — автомат не двигается,
//! а встаёт на паузу с объяснением. «Текст выглядит законченным» решением
//! не считается: это ровно тот способ самообмана, от которого в этом
//! проекте отказывались и в `verify.rs`.
//!
//! # Пауза и продолжение без повторных объяснений
//!
//! Пауза разрешена на любом этапе и сохраняет `stage`/`step`. Продолжение
//! ничего не переобъясняет: всё, что уже выяснено, лежит в
//! [`TaskState::log`] и уезжает в `system` внутри одного блока
//! [`BLOCK_HEAD`]. Поэтому после паузы (и даже после перезапуска — состояние
//! пишется в файл сессии) достаточно сказать «продолжай»: задача, пройденные
//! этапы и их итоги уже в запросе. Это и проверяет
//! `ask --verify-todo resume`.

use serde::{Deserialize, Serialize};

use crate::config::Res;

/// Открывающий тег блока состояния в `system`. Блок обрамлён тегами по той
/// же причине, что и профиль в `profile.rs`: проверке нужно вырезать ровно
/// его и сравнить всё остальное побайтно.
pub const BLOCK_HEAD: &str = "<task-state";

/// Закрывающий тег блока.
pub const BLOCK_END: &str = "</task-state>";

/// Строка, которой агент закрывает этап: `ЭТАП-ГОТОВ: <id этапа>`.
pub const DONE_MARK: &str = "ЭТАП-ГОТОВ:";

/// Строка с итогом этапа: `ИТОГ: <одна строка>`. Именно она переезжает в
/// следующий этап, поэтому просят её отдельно от рассуждений.
pub const RESULT_MARK: &str = "ИТОГ:";

/// Этап задачи. `Done` терминален: из него переходов нет.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Изучить: что вообще просят и как поймём, что сделано.
    #[default]
    Study,
    /// Запланировать: шаги с проверяемым результатом.
    Plan,
    /// Сделать: выполнить план.
    Execute,
    /// Валидировать: проверить результат независимым способом.
    Validate,
    /// Отписаться: короткий ответ человеку.
    Report,
    /// Готово. Терминальное состояние.
    Done,
}

impl Stage {
    /// Рабочие этапы по порядку — то, из чего состоит лестница. `Done` сюда
    /// не входит: это не работа, а её конец.
    pub const LADDER: [Stage; 5] = [
        Stage::Study,
        Stage::Plan,
        Stage::Execute,
        Stage::Validate,
        Stage::Report,
    ];

    /// Машинный id: он же уезжает в `system`, он же ожидается в
    /// [`DONE_MARK`].
    pub fn id(self) -> &'static str {
        match self {
            Stage::Study => "study",
            Stage::Plan => "plan",
            Stage::Execute => "execute",
            Stage::Validate => "validate",
            Stage::Report => "report",
            Stage::Done => "done",
        }
    }

    /// Человеческое имя для чеклиста.
    pub fn title(self) -> &'static str {
        match self {
            Stage::Study => "Изучить",
            Stage::Plan => "Запланировать",
            Stage::Execute => "Сделать",
            Stage::Validate => "Валидировать",
            Stage::Report => "Отписаться",
            Stage::Done => "Готово",
        }
    }

    /// Номер этапа в лестнице, 1-based. У `Done` — длина лестницы + 1.
    pub fn index(self) -> usize {
        Stage::LADDER
            .iter()
            .position(|s| *s == self)
            .map(|i| i + 1)
            .unwrap_or(Stage::LADDER.len() + 1)
    }

    /// Следующий этап. У последнего рабочего — `Done`, у `Done` — `None`.
    pub fn next(self) -> Option<Stage> {
        match self {
            Stage::Done => None,
            other => Some(
                Stage::LADDER
                    .get(other.index())
                    .copied()
                    .unwrap_or(Stage::Done),
            ),
        }
    }

    /// Ожидаемое действие — третья из трёх вещей, которых требовала
    /// постановка. Одной строкой, потому что она уезжает и в чеклист, и в
    /// статус.
    pub fn expected(self) -> &'static str {
        match self {
            Stage::Study => "разобрать задачу и сформулировать критерий готовности",
            Stage::Plan => "выдать план из 2–5 шагов с проверяемым результатом каждого",
            Stage::Execute => "выполнить план и выдать результат по существу",
            Stage::Validate => "проверить результат независимым способом и вынести вердикт",
            Stage::Report => "выдать короткий финальный ответ человеку",
            Stage::Done => "ничего: задача закрыта",
        }
    }

    /// Что агент делает на этом этапе. Это уезжает в запрос дословно —
    /// отсюда императив и запреты: без «не решай» этап `study` превращается
    /// в обычный ответ, и вся лестница теряет смысл.
    pub fn brief(self) -> &'static str {
        match self {
            Stage::Study => {
                "Изучи задачу. Выпиши: (1) что именно просят получить на выходе, \
                 (2) что известно из условия, (3) чего не хватает или что \
                 неоднозначно, (4) критерий готовности — по какому признаку \
                 будет видно, что задача решена правильно. Решать задачу \
                 сейчас нельзя: ответа по существу на этом этапе быть не \
                 должно."
            }
            Stage::Plan => {
                "Составь план. От 2 до 5 пронумерованных шагов, у каждого — \
                 проверяемый результат (что появится, когда шаг сделан). \
                 Отдельной строкой укажи, каким независимым способом результат \
                 будет проверен на этапе валидации. Выполнять шаги сейчас \
                 нельзя."
            }
            Stage::Execute => {
                "Выполни план и дай результат по существу. Иди по пунктам плана, \
                 не добавляя новых. Если по ходу выяснилось, что план неверен — \
                 скажи об этом прямо в итоге, не переписывая план молча."
            }
            Stage::Validate => {
                "Проверь результат по критерию готовности из этапа `study` — \
                 независимым способом: пересчитай другим методом, подставь \
                 обратно, поищи контрпример. Пересказ уже сделанного проверкой \
                 не считается. Вынеси вердикт `ok` или `не ok`; если `не ok` — \
                 назови конкретный дефект."
            }
            Stage::Report => {
                "Отпишись человеку. Коротко и самодостаточно: результат, каким \
                 способом он проверен, что осталось неясным. Без пересказа \
                 процесса и без повторения плана."
            }
            Stage::Done => "Задача закрыта, новых действий по ней нет.",
        }
    }

    /// Когда этап считается завершённым. Печатается агенту рядом с
    /// [`Stage::brief`] и человеку — в `/todo show`.
    pub fn done_when(self) -> &'static str {
        match self {
            Stage::Study => "перечислены требования, пробелы и назван критерий готовности",
            Stage::Plan => "есть 2–5 шагов с проверяемым результатом и назван способ проверки",
            Stage::Execute => "все пункты плана закрыты и получен результат",
            Stage::Validate => "вынесен вердикт ok / не ok, и при `не ok` назван дефект",
            Stage::Report => "человеку выдан короткий самодостаточный ответ",
            Stage::Done => "уже завершён",
        }
    }

    /// Разобрать id этапа (для [`DONE_MARK`] и `/todo`).
    pub fn parse(s: &str) -> Option<Stage> {
        let s = s.trim().trim_end_matches(['.', ',', '!', ')']).trim();
        [
            Stage::Study,
            Stage::Plan,
            Stage::Execute,
            Stage::Validate,
            Stage::Report,
            Stage::Done,
        ]
        .into_iter()
        .find(|st| st.id().eq_ignore_ascii_case(s))
    }
}

/// Состояние автомата. `Idle` — задачи нет; блок в `system` не уезжает.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Idle,
    Running,
    Paused,
    Finished,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Running => "running",
            Status::Paused => "paused",
            Status::Finished => "finished",
        }
    }
}

/// Закрытый этап: что это был за этап, на каком шаге и с каким итогом. Из
/// этих записей и собирается «продолжение без повторных объяснений».
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRecord {
    pub stage: Stage,
    pub step: usize,
    pub summary: String,
}

/// Состояние задачи целиком. Сериализуется в файл сессии, поэтому пауза
/// переживает не только смену темы, но и перезапуск приложения.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskState {
    /// Формулировка задачи — ровно то, что человек написал.
    pub goal: String,
    pub stage: Stage,
    pub status: Status,
    /// Номер шага, 1-based. `0` — задача ещё не начата.
    pub step: usize,
    /// Закрытые этапы с итогами.
    pub log: Vec<StageRecord>,
    /// Почему стоим (пауза) или что пошло не так. Пустая строка — нечего
    /// сказать.
    pub note: String,
}

impl TaskState {
    pub fn new() -> TaskState {
        TaskState::default()
    }

    /// Идёт ли работа прямо сейчас.
    pub fn running(&self) -> bool {
        self.status == Status::Running
    }

    pub fn paused(&self) -> bool {
        self.status == Status::Paused
    }

    pub fn finished(&self) -> bool {
        self.status == Status::Finished
    }

    /// Есть ли вообще задача. Только в этом случае блок уезжает в `system`:
    /// пустое состояние в запросе — лишние токены и лишний шум.
    pub fn active(&self) -> bool {
        self.status != Status::Idle
    }

    /// Начать задачу. Поверх идущей задачи начать нельзя — иначе тудушка
    /// молча теряет то, что уже сделано.
    pub fn start(&mut self, goal: &str) -> Res<()> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err("пустая формулировка задачи".into());
        }
        if self.status == Status::Running || self.status == Status::Paused {
            return Err(format!(
                "задача уже идёт (этап {}, шаг {}); /todo reset — начать заново",
                self.stage.id(),
                self.step
            ));
        }
        self.goal = goal.to_string();
        self.stage = Stage::Study;
        self.status = Status::Running;
        self.step = 1;
        self.log.clear();
        self.note.clear();
        Ok(())
    }

    /// Закрыть текущий этап и перейти к следующему. Возвращает новый этап.
    ///
    /// Законен только из `Running`: на паузе двигаться нельзя (в этом и
    /// смысл паузы), из `Done` — некуда.
    pub fn advance(&mut self, summary: &str) -> Res<Stage> {
        match self.status {
            Status::Running => {}
            Status::Paused => {
                return Err("задача на паузе — сначала /todo resume".into());
            }
            Status::Finished => return Err("задача уже завершена (done)".into()),
            Status::Idle => return Err("задачи нет — /todo start <что сделать>".into()),
        }
        let closed = self.stage;
        self.log.push(StageRecord {
            stage: closed,
            step: self.step,
            summary: summary.trim().to_string(),
        });
        self.note.clear();
        match closed.next() {
            Some(Stage::Done) | None => {
                self.stage = Stage::Done;
                self.status = Status::Finished;
                Ok(Stage::Done)
            }
            Some(next) => {
                self.stage = next;
                self.step += 1;
                Ok(next)
            }
        }
    }

    /// Пауза на текущем этапе. Разрешена на любом рабочем этапе — это
    /// требование постановки, поэтому `stage`/`step` не трогаем вовсе.
    pub fn pause(&mut self, why: &str) -> Res<()> {
        match self.status {
            Status::Running => {
                self.status = Status::Paused;
                self.note = why.trim().to_string();
                Ok(())
            }
            Status::Paused => Err("задача уже на паузе".into()),
            Status::Finished => Err("задача завершена — паузу ставить не на чем".into()),
            Status::Idle => Err("задачи нет — /todo start <что сделать>".into()),
        }
    }

    /// Продолжить с того же этапа и шага. Ничего не переобъясняем: всё
    /// уже в `log` и уедет в `system` блоком.
    pub fn resume(&mut self) -> Res<()> {
        match self.status {
            Status::Paused => {
                self.status = Status::Running;
                self.note.clear();
                Ok(())
            }
            Status::Running => Err("задача и так идёт".into()),
            Status::Finished => Err("задача завершена — продолжать нечего".into()),
            Status::Idle => Err("задачи нет — /todo start <что сделать>".into()),
        }
    }

    /// Снести состояние. Единственный способ уйти из `Done` и единственный
    /// способ начать другую задачу, не потеряв это молча.
    pub fn reset(&mut self) {
        *self = TaskState::new();
    }

    /// Заявленное закрытие этапа из ответа модели. Возвращает итог этапа
    /// или объяснение, почему этап не закрыт.
    ///
    /// Здесь и стоит защита от самообмана: подтверждение — это именно
    /// маркер с id **текущего** этапа. Чужой id или его отсутствие —
    /// ошибка, а не повод двинуться дальше.
    pub fn confirm(&self, text: &str) -> Res<String> {
        let Some((claimed, summary)) = parse_completion(text) else {
            return Err(format!(
                "этап `{}` не подтверждён: в ответе нет строки `{} {}`",
                self.stage.id(),
                DONE_MARK,
                self.stage.id()
            ));
        };
        if claimed != self.stage {
            return Err(format!(
                "ответ закрывает этап `{}`, а текущий — `{}`",
                claimed.id(),
                self.stage.id()
            ));
        }
        if summary.trim().is_empty() {
            return Err(format!(
                "этап `{}` подтверждён без строки `{RESULT_MARK}` — переносить в следующий этап нечего",
                self.stage.id()
            ));
        }
        Ok(summary)
    }

    /// Что отправляется модели на текущем этапе. Формулировку задачи сюда
    /// не дублируем: она уже в блоке `system`.
    pub fn stage_prompt(&self) -> String {
        let s = self.stage;
        format!(
            "Работай по этапу `{}` ({}), шаг {}.\n\n{}\n\nЭтап завершён, когда {}.\n\n\
             Закончи ответ ровно двумя строками:\n{} {}\n{} <итог этапа одной строкой>",
            s.id(),
            s.title(),
            self.step,
            s.brief(),
            s.done_when(),
            DONE_MARK,
            s.id(),
            RESULT_MARK
        )
    }

    /// Блок состояния для `system`. Один блок на запрос, с тремя обязательными
    /// строками (этап / шаг / ожидаемое действие) и историей закрытых этапов.
    pub fn block(&self) -> String {
        let mut out = format!(
            "{BLOCK_HEAD} stage=\"{}\" step=\"{}\" status=\"{}\">\n",
            self.stage.id(),
            self.step,
            self.status.as_str()
        );
        out.push_str(
            "Состояние задачи (конечный автомат). Оно ведётся клиентом и уже \
             учтено: переспрашивать и пересказывать его не нужно.\n",
        );
        out.push_str(&format!("- Задача: {}\n", self.goal.trim()));
        out.push_str(&format!(
            "- Этап задачи: {} из {} — `{}` ({})\n",
            self.stage.index(),
            Stage::LADDER.len(),
            self.stage.id(),
            self.stage.title()
        ));
        out.push_str(&format!("- Текущий шаг: {}\n", self.step));
        out.push_str(&format!(
            "- Ожидаемое действие: {}\n",
            self.stage.expected()
        ));
        if self.log.is_empty() {
            out.push_str("- Пройденные этапы: пока ни одного\n");
        } else {
            out.push_str("- Пройденные этапы (итоги; повторять их не нужно):\n");
            for r in &self.log {
                out.push_str(&format!(
                    "  - шаг {}, `{}` ({}): {}\n",
                    r.step,
                    r.stage.id(),
                    r.stage.title(),
                    r.summary.trim()
                ));
            }
        }
        if self.status == Status::Paused {
            out.push_str(
                "- Статус: пауза. Ничего не делай по задаче, пока человек не скажет \
                 продолжать; когда скажет — продолжай с этого этапа, ничего не \
                 переобъясняя заново.\n",
            );
            if !self.note.trim().is_empty() {
                out.push_str(&format!("  - причина паузы: {}\n", self.note.trim()));
            }
        }
        if self.status == Status::Finished {
            out.push_str("- Статус: задача закрыта; новых действий по ней нет.\n");
        }
        format!("{}\n{BLOCK_END}", out.trim_end())
    }

    /// Чеклист для панели в чате: (значок, строка). Значок — состояние
    /// этапа: сделан / текущий / ещё не начат.
    pub fn checklist(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for s in Stage::LADDER {
            let done = self.log.iter().find(|r| r.stage == s);
            let mark = match (done.is_some(), s == self.stage) {
                (true, _) => "\u{2714}",
                (false, true) => "\u{1f449}",
                _ => "\u{00b7}",
            };
            let mut line = format!("{} {}", s.index(), s.title());
            if let Some(r) = done {
                line.push_str(&format!(" — {}", clip(&r.summary, 72)));
            } else if s == self.stage {
                line.push_str(&format!(
                    " (шаг {}{}) — ожидается: {}",
                    self.step,
                    if self.paused() { ", пауза" } else { "" },
                    s.expected()
                ));
            }
            out.push((mark, line));
        }
        if self.finished() {
            out.push(("\u{2714}", "готово".to_string()));
        }
        out
    }

    /// Однострочный статус для футера и `/todo`.
    pub fn line(&self) -> String {
        match self.status {
            Status::Idle => "тудушка: задачи нет".into(),
            Status::Finished => format!("тудушка: done — {}", clip(&self.goal, 48)),
            _ => format!(
                "тудушка: этап {}/{} `{}`{} · шаг {} · ожидается: {}",
                self.stage.index(),
                Stage::LADDER.len(),
                self.stage.id(),
                if self.paused() { " (пауза)" } else { "" },
                self.step,
                self.stage.expected()
            ),
        }
    }

    /// Развёрнутая карточка для `/todo show`.
    pub fn card(&self) -> String {
        if !self.active() {
            return "задачи нет — напишите её текстом (тудушка включена) или /todo start <что сделать>".into();
        }
        let mut out = format!("задача: {}\n", self.goal.trim());
        out.push_str(&format!("статус: {}\n", self.status.as_str()));
        for (mark, line) in self.checklist() {
            out.push_str(&format!("  {mark} {line}\n"));
        }
        if !self.finished() {
            out.push_str(&format!(
                "\nчто делать сейчас: {}\nэтап закроется, когда {}\n",
                self.stage.brief(),
                self.stage.done_when()
            ));
        }
        if !self.note.trim().is_empty() {
            out.push_str(&format!("заметка: {}\n", self.note.trim()));
        }
        out.trim_end().to_string()
    }

    /// Строка, которую агент печатает в чат при входе в этап — то самое
    /// «явно показывает, когда переходит на следующий шаг».
    pub fn enter_line(&self) -> String {
        format!(
            "\u{1f449} этап {}/{}: {} (`{}`), шаг {} — ожидается: {}",
            self.stage.index(),
            Stage::LADDER.len(),
            self.stage.title(),
            self.stage.id(),
            self.step,
            self.stage.expected()
        )
    }
}

/// Строка, которую агент печатает при закрытии этапа.
pub fn leave_line(stage: Stage, summary: &str) -> String {
    format!(
        "\u{2714} этап {}/{} `{}` ({}) завершён — итог: {}",
        stage.index(),
        Stage::LADDER.len(),
        stage.id(),
        stage.title(),
        clip(summary, 120)
    )
}

/// Найти в ответе подтверждение этапа: id из [`DONE_MARK`] и итог из
/// [`RESULT_MARK`]. Порядок строк не важен, регистр маркеров — тоже.
pub fn parse_completion(text: &str) -> Option<(Stage, String)> {
    let mut stage = None;
    let mut summary = String::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['*', '#', '-', ' ']).trim();
        if let Some(rest) = strip_mark(line, DONE_MARK) {
            if let Some(s) = Stage::parse(rest.trim_matches(['`', '"', '*', ' '])) {
                stage = Some(s);
            }
        } else if let Some(rest) = strip_mark(line, RESULT_MARK) {
            if summary.is_empty() {
                summary = rest.trim().to_string();
            }
        }
    }
    stage.map(|s| (s, summary))
}

/// Маркер в начале строки, без учёта регистра.
fn strip_mark<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    let head = line.get(..mark.len())?;
    head.eq_ignore_ascii_case(mark)
        .then(|| &line[mark.len()..])
}

/// Вырезать блок состояния из системного сообщения тем же способом, каким
/// его собрали. Возвращает (блок, остальное) — это и есть инструмент
/// проверки «поменялся только блок состояния».
pub fn split_block(system: &str) -> (Option<String>, String) {
    let Some(start) = system.find(BLOCK_HEAD) else {
        return (None, system.to_string());
    };
    let end = match system[start..].find(BLOCK_END) {
        Some(i) => start + i + BLOCK_END.len(),
        None => system.len(),
    };
    let block = system[start..end].to_string();
    let mut rest = String::with_capacity(system.len());
    rest.push_str(&system[..start]);
    rest.push_str(&system[end..]);
    (Some(block), rest.trim().to_string())
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walked() -> TaskState {
        let mut st = TaskState::new();
        st.start("посчитать буквы").unwrap();
        st.advance("требования выписаны").unwrap();
        st
    }

    #[test]
    fn ladder_walks_to_done_and_stops_there() {
        let mut st = TaskState::new();
        st.start("задача").unwrap();
        assert_eq!((st.stage, st.step, st.status), (Stage::Study, 1, Status::Running));

        let order = [
            Stage::Plan,
            Stage::Execute,
            Stage::Validate,
            Stage::Report,
            Stage::Done,
        ];
        for (i, want) in order.into_iter().enumerate() {
            assert_eq!(st.advance(&format!("итог {i}")).unwrap(), want);
            assert_eq!(st.stage, want);
        }
        assert!(st.finished());
        // Done терминален: дальше только reset.
        assert!(st.advance("ещё").is_err());
        assert!(st.pause("хочу").is_err());
        assert!(st.resume().is_err());
        assert_eq!(st.log.len(), Stage::LADDER.len());
        st.reset();
        assert_eq!(st.status, Status::Idle);
        assert!(!st.active());
    }

    #[test]
    fn pause_holds_the_stage_and_blocks_advancing() {
        let mut st = walked();
        assert_eq!((st.stage, st.step), (Stage::Plan, 2));
        st.pause("человек ушёл за чаем").unwrap();
        assert!(st.paused());
        // На паузе автомат не двигается — иначе пауза ничего не значит.
        assert!(st.advance("тайком").is_err());
        assert!(st.pause("ещё раз").is_err());
        assert_eq!((st.stage, st.step), (Stage::Plan, 2));
        st.resume().unwrap();
        // Продолжили ровно с того же места, а не с начала.
        assert_eq!((st.stage, st.step, st.status), (Stage::Plan, 2, Status::Running));
        assert!(st.note.is_empty());
        assert!(st.resume().is_err());
    }

    #[test]
    fn illegal_starts_are_refused() {
        let mut st = TaskState::new();
        assert!(st.start("  ").is_err());
        assert!(st.advance("нечего").is_err());
        assert!(st.pause("нечего").is_err());
        st.start("первая").unwrap();
        // Поверх идущей задачи вторую молча не начинаем.
        assert!(st.start("вторая").is_err());
        st.pause("пауза").unwrap();
        assert!(st.start("вторая").is_err());
        st.reset();
        st.start("вторая").unwrap();
        assert_eq!(st.goal, "вторая");
    }

    #[test]
    fn stage_closes_only_on_its_own_marker() {
        let st = walked(); // текущий этап — plan
        assert!(st.confirm("просто текст без маркера").is_err());
        assert!(st
            .confirm("ЭТАП-ГОТОВ: execute\nИТОГ: сделал")
            .is_err(), "чужой этап закрывать нельзя");
        assert!(st
            .confirm("ЭТАП-ГОТОВ: plan")
            .is_err(), "без ИТОГ переносить в следующий этап нечего");
        assert_eq!(
            st.confirm("бла-бла\nЭТАП-ГОТОВ: plan\nИТОГ: три шага, проверка пересчётом")
                .unwrap(),
            "три шага, проверка пересчётом"
        );
    }

    #[test]
    fn markers_survive_markdown_dressing() {
        let text = "**ЭТАП-ГОТОВ:** `study`\n- ИТОГ:  критерий — совпадение с пересчётом ";
        let (stage, summary) = parse_completion(text).unwrap();
        assert_eq!(stage, Stage::Study);
        assert_eq!(summary, "критерий — совпадение с пересчётом");
        assert!(parse_completion("ничего такого").is_none());
    }

    #[test]
    fn block_carries_stage_step_expected_and_the_log() {
        let mut st = walked();
        let block = st.block();
        assert!(block.starts_with(BLOCK_HEAD) && block.ends_with(BLOCK_END));
        assert!(block.contains("stage=\"plan\""));
        assert!(block.contains("Этап задачи: 2 из 5"));
        assert!(block.contains("Текущий шаг: 2"));
        assert!(block.contains("Ожидаемое действие:"));
        // Итог прошлого этапа едет с собой — это и есть «без повторных
        // объяснений».
        assert!(block.contains("требования выписаны"));
        assert!(!block.contains("пауза"));

        st.pause("обед").unwrap();
        let paused = st.block();
        assert!(paused.contains("status=\"paused\""));
        assert!(paused.contains("причина паузы: обед"));
    }

    #[test]
    fn split_block_cuts_exactly_the_state_block() {
        let st = walked();
        let system = format!("базовый промпт\n\n{}\n\n## Память", st.block());
        let (block, rest) = split_block(&system);
        assert!(block.unwrap().contains("stage=\"plan\""));
        assert_eq!(rest, "базовый промпт\n\n\n\n## Память".trim());
        assert!(!rest.contains(BLOCK_HEAD));
    }

    #[test]
    fn state_survives_a_round_trip_through_json() {
        let mut st = walked();
        st.pause("перезапуск").unwrap();
        let raw = serde_json::to_string(&st).unwrap();
        let back: TaskState = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, st);
        // Старая сессия без поля читается как «задачи нет».
        let empty: TaskState = serde_json::from_str("{}").unwrap();
        assert!(!empty.active());
    }

    #[test]
    fn checklist_marks_done_current_and_pending() {
        let st = walked();
        let list = st.checklist();
        assert_eq!(list.len(), Stage::LADDER.len());
        assert_eq!(list[0].0, "\u{2714}");
        assert!(list[0].1.contains("требования выписаны"));
        assert_eq!(list[1].0, "\u{1f449}");
        assert!(list[1].1.contains("шаг 2"));
        assert_eq!(list[2].0, "\u{00b7}");
    }

    #[test]
    fn stage_prompt_names_the_stage_and_asks_for_the_marker() {
        let st = walked();
        let p = st.stage_prompt();
        assert!(p.contains("`plan`"));
        assert!(p.contains(DONE_MARK) && p.contains("ЭТАП-ГОТОВ: plan"));
        assert!(p.contains(RESULT_MARK));
        // Формулировку задачи в запрос не дублируем — она в блоке system.
        assert!(!p.contains("посчитать буквы"));
    }
}
