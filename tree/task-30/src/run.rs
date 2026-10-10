//! Контролируемый жизненный цикл задачи (задача 15).
//!
//! `todo.rs` даёт лестницу этапов — «что делаем сейчас». Здесь лежит то,
//! что задача 15 просит сверху: **прогон** (`TaskRun`) — материализованная
//! задача, которая живёт не один запрос, а от формулировки до `done`, и
//! переходы между её состояниями, которые нельзя перепрыгнуть.
//!
//! Три причины, почему прогон — отдельная сущность, а не ещё пара полей в
//! [`crate::todo::TaskState`]:
//!
//! 1. **Фаза запроса.** Этап (`execute`) переживал перезапуск и раньше, а
//!    вот «мы прямо сейчас в сети, попытка 2» — нет. Прогон пишется на диск
//!    *до* сетевого вызова, поэтому падение посреди запроса видно в файле
//!    сессии, а не только в голове у человека.
//! 2. **Гейты.** «Нельзя реализацию до утверждённого плана» — это не
//!    строчка в промпте, а состояние [`RunStatus::AwaitingApproval`], из
//!    которого в `execute` ведёт ровно одно событие: [`Event::Approve`].
//! 3. **Один источник правды.** Раньше драйвер лестницы был скопирован в
//!    `tui.rs` и `cli.rs`, и правило, выполненное в одном фронтенде, могло
//!    не выполняться в другом. Теперь оба зовут [`Engine::step`], а все
//!    переходы проходят через [`transition`] — чистую функцию с таблицей.
//!
//! Незаконный переход — не паника и не тихий no-op, а [`Refusal`]: она и
//! возвращается вызывающему, и ложится в `run.log`, и показывается человеку
//! одной и той же формулировкой (`/todo log`).

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::Reply;
use crate::config::{Res, Settings};
use crate::invariants::{InvariantSet, MAX_ATTEMPTS};
use crate::pipeline::{self, Decision};
use crate::todo::{Stage, Status, TaskState, Verdict};

/// Фаза одного запроса внутри этапа. Нужна ровно для одного: чтобы по файлу
/// сессии было видно, где нас оборвало.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Промпт этапа ещё собирается — в сеть не ходили.
    #[default]
    Prompt,
    /// Запрос ушёл модели; ответа ещё нет.
    Model,
    /// Ответ есть, идёт детерминированная проверка (инварианты + контракт).
    Validate,
    /// Проверка пройдена, решаем судьбу этапа (закрыть / переделать).
    Decide,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Prompt => "prompt",
            Phase::Model => "model",
            Phase::Validate => "validate",
            Phase::Decide => "decide",
        }
    }
}

/// Статус прогона. Это и есть «допустимые состояния задачи» из постановки.
///
/// `Interrupted` отличается от `Paused` только происхождением: паузу
/// поставил человек, а прерывание мы **обнаружили** при загрузке сессии —
/// в файле лежал идущий прогон, значит процесс умер на полушаге. Права у
/// них одинаковые: продолжить или бросить.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Прогон заведён, но задача ещё не сформулирована.
    #[default]
    Idle,
    Running,
    /// Этап закрыт и ждёт подписи человека. Дальше хода нет.
    AwaitingApproval,
    Paused,
    Interrupted,
    /// Терминал: прошли всю лестницу.
    DonePass,
    /// Терминал: бросили (abort, или исчерпали попытки).
    DoneFail,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Idle => "idle",
            RunStatus::Running => "running",
            RunStatus::AwaitingApproval => "awaiting-approval",
            RunStatus::Paused => "paused",
            RunStatus::Interrupted => "interrupted",
            RunStatus::DonePass => "done(pass)",
            RunStatus::DoneFail => "done(fail)",
        }
    }

    /// Терминальные состояния: из них выходит только `reset`.
    pub fn terminal(self) -> bool {
        matches!(self, RunStatus::DonePass | RunStatus::DoneFail)
    }

}

/// События, которыми двигается прогон. Других способов сдвинуть состояние
/// нет: [`TaskRun`] не отдаёт наружу ни `status`, ни `stage` на запись.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Сформулировать задачу и начать со `study`.
    Start(String),
    /// Этап закрыт моделью: итог этапа и (для `validate`) машинный вердикт.
    StageClosed {
        summary: String,
        verdict: Option<Verdict>,
    },
    /// Подпись под планом: кто утвердил (`человек` / `auto`) и комментарий.
    Approve(String),
    /// План отклонён с причиной — переделываем план, а не идём дальше.
    Reject(String),
    Pause(String),
    Resume,
    /// Бросить прогон: терминал `done(fail)`.
    Abort(String),
    /// Смена фазы внутри этапа. Пишется на диск до сетевого вызова.
    PhaseTo(Phase),
}

impl Event {
    /// Короткое имя для лога. Аргумент событие тащит отдельно.
    pub fn name(&self) -> &'static str {
        match self {
            Event::Start(_) => "start",
            Event::StageClosed { .. } => "stage-closed",
            Event::Approve(_) => "approve",
            Event::Reject(_) => "reject",
            Event::Pause(_) => "pause",
            Event::Resume => "resume",
            Event::Abort(_) => "abort",
            Event::PhaseTo(_) => "phase",
        }
    }

    fn detail(&self) -> String {
        match self {
            Event::Start(g) => g.trim().to_string(),
            Event::StageClosed { summary, verdict } => match verdict {
                Some(v) => format!("вердикт {} · {}", verdict_id(*v), summary.trim()),
                None => summary.trim().to_string(),
            },
            Event::Approve(by) => by.trim().to_string(),
            Event::Reject(why) | Event::Pause(why) | Event::Abort(why) => why.trim().to_string(),
            Event::Resume => String::new(),
            Event::PhaseTo(p) => p.as_str().to_string(),
        }
    }
}

pub fn verdict_id(v: Verdict) -> &'static str {
    match v {
        Verdict::Ok => "ok",
        Verdict::NotOk => "не ok",
    }
}

/// Отказ в переходе. Ровно этот текст видит человек и ровно он лежит в
/// логе — одна формулировка, чтобы «реакцию ассистента» можно было
/// проверить машинно.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// Состояние, из которого отказали: `running/plan/prompt`.
    pub from: String,
    pub event: String,
    pub why: String,
}

impl Refusal {
    pub fn line(&self) -> String {
        format!("\u{26d4} переход `{}` из {}: {}", self.event, self.from, self.why)
    }
}

/// Снимок позиции прогона — то, над чем работает [`transition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub status: RunStatus,
    pub stage: Stage,
    pub phase: Phase,
}

impl Position {
    pub fn line(&self) -> String {
        format!("{}/{}/{}", self.status.as_str(), self.stage.id(), self.phase.as_str())
    }
}

/// Что делать с лестницей после разрешённого перехода. Позиция —
/// «где стоим», действие — «что записать в `TaskState`».
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Move {
    /// Ничего не менять в лестнице (смена фазы, пауза, продолжение).
    Stay,
    /// Начать задачу.
    Begin(String),
    /// Закрыть текущий этап с итогом (в `TaskState.log`) и не двигаться:
    /// следующий этап назначает `to.stage`.
    Close(String),
    /// Переделка того же (или более раннего) этапа: запись в лог с
    /// пометкой, шаг увеличивается.
    Rework(String),
}

/// Результат разрешённого перехода.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub to: Position,
    pub act: Move,
}

/// **Единственная точка правды о переходах.** Чистая функция: ничего не
/// мутирует, поэтому незаконный переход физически не может «наполовину»
/// сдвинуть состояние.
///
/// Таблица (`from` → событие → `to`):
///
/// | из | событие | в |
/// |---|---|---|
/// | idle | start | running/study/prompt |
/// | running/plan | stage-closed | **awaiting-approval**/plan |
/// | awaiting-approval/plan | approve | running/execute/prompt |
/// | awaiting-approval/plan | reject | running/plan/prompt (переделка) |
/// | running/validate | stage-closed(ok) | running/report/prompt |
/// | running/validate | stage-closed(не ok) | running/execute/prompt (переделка) |
/// | running/validate | stage-closed(без вердикта) | **отказ** |
/// | running/report | stage-closed | done(pass) |
/// | running/* | pause / abort | paused / done(fail) |
/// | paused, interrupted | resume | running/<тот же этап>/prompt |
/// | paused, interrupted | stage-closed, approve, phase | **отказ** |
/// | awaiting-approval | stage-closed, phase, resume, pause | **отказ** |
/// | done(*) | что угодно | **отказ** (терминал) |
pub fn transition(cur: Position, ev: &Event) -> Result<Step, Refusal> {
    let no = |why: &str| {
        Err(Refusal {
            from: cur.line(),
            event: ev.name().to_string(),
            why: why.to_string(),
        })
    };
    let go = |status: RunStatus, stage: Stage, phase: Phase, act: Move| {
        Ok(Step {
            to: Position { status, stage, phase },
            act,
        })
    };

    // Терминал первым: из done не ведёт ничего, кроме /todo reset.
    if cur.status.terminal() {
        return no("прогон завершён — новое событие ничего не меняет; /todo reset начнёт заново");
    }

    match (cur.status, ev) {
        // --- старт -------------------------------------------------------
        (RunStatus::Idle, Event::Start(goal)) => {
            if goal.trim().is_empty() {
                no("пустая формулировка задачи")
            } else {
                go(
                    RunStatus::Running,
                    Stage::Study,
                    Phase::Prompt,
                    Move::Begin(goal.trim().to_string()),
                )
            }
        }
        (RunStatus::Idle, _) => no("задачи нет — сначала /todo start <что сделать>"),
        (_, Event::Start(_)) => no("прогон уже идёт — /todo reset, чтобы начать другую задачу"),

        // --- бросить можно из любого рабочего состояния -------------------
        (_, Event::Abort(_)) => go(RunStatus::DoneFail, cur.stage, cur.phase, Move::Stay),

        // --- гейт утверждения плана ---------------------------------------
        (RunStatus::AwaitingApproval, Event::Approve(_)) => {
            let next = cur.stage.next().unwrap_or(Stage::Done);
            go(RunStatus::Running, next, Phase::Prompt, Move::Stay)
        }
        (RunStatus::AwaitingApproval, Event::Reject(why)) => {
            if why.trim().is_empty() {
                no("отклонение без причины: переделывать нечего — /todo reject <что не так>")
            } else {
                go(
                    RunStatus::Running,
                    cur.stage,
                    Phase::Prompt,
                    Move::Rework(format!("план отклонён: {}", why.trim())),
                )
            }
        }
        (RunStatus::AwaitingApproval, _) => no(&format!(
            "этап `{}` ждёт утверждения: до /todo approve дальше хода нет (или /todo reject <причина>)",
            cur.stage.id()
        )),

        // --- пауза и продолжение ------------------------------------------
        (RunStatus::Running, Event::Pause(why)) => {
            let _ = why;
            go(RunStatus::Paused, cur.stage, cur.phase, Move::Stay)
        }
        (RunStatus::Running, Event::Resume) => no("прогон и так идёт"),
        (RunStatus::Paused | RunStatus::Interrupted, Event::Resume) => {
            // Оборванный ответ модели восстановить нельзя, поэтому текущий
            // запрос начинается заново (`Phase::Prompt`) — но этап, шаг и
            // итоги закрытых этапов остаются, переобъяснять нечего.
            go(RunStatus::Running, cur.stage, Phase::Prompt, Move::Stay)
        }
        (RunStatus::Paused | RunStatus::Interrupted, _) => no(&format!(
            "прогон стоит ({}) на этапе `{}` — сначала /todo resume",
            cur.status.as_str(),
            cur.stage.id()
        )),

        // --- фаза внутри этапа ---------------------------------------------
        (RunStatus::Running, Event::PhaseTo(p)) => go(RunStatus::Running, cur.stage, *p, Move::Stay),

        // --- закрытие этапа --------------------------------------------------
        (RunStatus::Running, Event::StageClosed { summary, verdict }) => {
            if summary.trim().is_empty() {
                return no("этап закрыт без итога — переносить в следующий этап нечего");
            }
            match cur.stage {
                // Гейт 1: план не закрывается в реализацию, а уходит под подпись.
                s if s.requires_approval() => {
                    go(RunStatus::AwaitingApproval, s, Phase::Decide, Move::Close(summary.clone()))
                }
                // Гейт 2: финала без вердикта валидации не бывает.
                Stage::Validate => match verdict {
                    None => no("этап `validate` не закрыт: нет машинной строки \
                                `ВЕРДИКТ: ok|не ok`, а без неё `report` недостижим"),
                    Some(Verdict::Ok) => go(
                        RunStatus::Running,
                        Stage::Report,
                        Phase::Prompt,
                        Move::Close(summary.clone()),
                    ),
                    Some(Verdict::NotOk) => go(
                        RunStatus::Running,
                        Stage::Execute,
                        Phase::Prompt,
                        Move::Rework(format!("вердикт «не ok»: {}", summary.trim())),
                    ),
                },
                Stage::Report => go(
                    RunStatus::DonePass,
                    Stage::Done,
                    Phase::Decide,
                    Move::Close(summary.clone()),
                ),
                Stage::Done => no("этап `done` ничего не закрывает"),
                other => {
                    let next = other.next().unwrap_or(Stage::Done);
                    go(RunStatus::Running, next, Phase::Prompt, Move::Close(summary.clone()))
                }
            }
        }

        (RunStatus::Running, Event::Approve(_) | Event::Reject(_)) => {
            no("никто не ждёт утверждения — утверждать нечего")
        }

        // Терминалы отсечены наверху.
        (RunStatus::DonePass | RunStatus::DoneFail, _) => unreachable!("terminal handled above"),
    }
}

/// Запись в журнале прогона: законный переход или отказ. Отказы пишутся
/// наравне с переходами — это и есть машинно проверяемая «реакция на
/// попытку перепрыгнуть этап».
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: u64,
    pub from: String,
    pub event: String,
    pub detail: String,
    /// `None` — переход не состоялся.
    pub to: Option<String>,
    pub ok: bool,
    pub why: String,
}

impl LogEntry {
    pub fn line(&self) -> String {
        let head = match &self.to {
            Some(to) => format!("\u{2713} {} --{}--> {}", self.from, self.event, to),
            None => format!("\u{26d4} {} --{}--> X", self.from, self.event),
        };
        let tail = if self.why.trim().is_empty() {
            self.detail.trim().to_string()
        } else {
            self.why.trim().to_string()
        };
        if tail.is_empty() {
            head
        } else {
            format!("{head} · {tail}")
        }
    }
}

/// Сколько записей журнала храним в файле сессии. Журнал — диагностика, а
/// не история диалога: расти без предела ему незачем.
pub const LOG_CAP: usize = 200;

/// Прогон: задача, материализованная на всё время работы над ней.
///
/// Живёт полем `Session.run`, то есть в том же файле, что и разговор.
/// Отсюда бесплатно получаются оба требования постановки: прогон
/// **привязан к сессии** и **удаляется вместе с ней**; второго источника
/// правды не заводим.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskRun {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub status: RunStatus,
    /// Лестница этапов — ровно та же, что была в задаче 13/14.
    pub state: TaskState,
    pub phase: Phase,
    /// Номер попытки текущего запроса (retry валидатора).
    pub attempt: usize,
    /// Настройки на момент старта прогона: модель, стратегия, профиль,
    /// тудушка, инварианты. Меняются в середине — молчать об этом нельзя.
    pub pinned: Pinned,
    /// Подписи под утверждёнными этапами: `plan: человек — ок`.
    pub approvals: Vec<String>,
    pub log: Vec<LogEntry>,
}

impl Default for TaskRun {
    fn default() -> Self {
        TaskRun {
            id: String::new(),
            created_at: 0,
            updated_at: 0,
            status: RunStatus::Idle,
            state: TaskState::new(),
            phase: Phase::Prompt,
            attempt: 0,
            pinned: Pinned::default(),
            approvals: Vec::new(),
            log: Vec::new(),
        }
    }
}

/// Слепок настроек на старте прогона. Полный [`Settings`] сюда не кладём:
/// в файле сессии он уже есть, а дублировать его целиком — это второй
/// источник правды о системном промпте. Пиннится ровно то, что меняет
/// смысл прогона.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pinned {
    pub model: String,
    pub strategy: String,
    pub profile: String,
    pub invariants: bool,
    pub approve: ApprovePolicy,
}

impl Pinned {
    pub fn of(s: &Settings) -> Pinned {
        Pinned {
            model: s.model.clone(),
            strategy: s.context_strategy.label().to_string(),
            profile: s.profile.clone(),
            invariants: s.invariants,
            approve: s.approve,
        }
    }

    /// Чем текущие настройки разошлись с зафиксированными. Пусто — сходятся.
    pub fn drift(&self, s: &Settings) -> Vec<String> {
        let now = Pinned::of(s);
        let mut out = Vec::new();
        let mut cmp = |what: &str, was: &str, is: &str| {
            if was != is {
                out.push(format!("{what}: прогон начат на `{was}`, сейчас `{is}`"));
            }
        };
        cmp("модель", &self.model, &now.model);
        cmp("стратегия", &self.strategy, &now.strategy);
        cmp("профиль", &self.profile, &now.profile);
        cmp(
            "инварианты",
            if self.invariants { "on" } else { "off" },
            if now.invariants { "on" } else { "off" },
        );
        cmp("утверждение", self.approve.id(), now.approve.id());
        out
    }
}

/// Кто подписывает план. `Auto` — это **подпись**, а не отключённый гейт:
/// прогон всё равно проходит через [`RunStatus::AwaitingApproval`] и
/// событие [`Event::Approve`], просто подписывает не человек, и это видно
/// в журнале (`approved-by=auto`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovePolicy {
    #[default]
    Manual,
    Auto,
}

impl ApprovePolicy {
    pub fn id(self) -> &'static str {
        match self {
            ApprovePolicy::Manual => "manual",
            ApprovePolicy::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Res<ApprovePolicy> {
        match s.trim().to_lowercase().as_str() {
            "manual" | "human" | "ручное" => Ok(ApprovePolicy::Manual),
            "auto" | "авто" => Ok(ApprovePolicy::Auto),
            other => Err(format!("не знаю политику утверждения `{other}` (manual|auto)")),
        }
    }
}

impl TaskRun {
    /// Пустой прогон, привязанный к сессии. Задачи в нём ещё нет.
    pub fn new(session_id: &str, settings: &Settings) -> TaskRun {
        TaskRun {
            id: format!("{session_id}-run"),
            created_at: now_secs(),
            updated_at: now_secs(),
            pinned: Pinned::of(settings),
            ..TaskRun::default()
        }
    }

    /// Собрать прогон вокруг уже существующей лестницы. Нужно для сессий
    /// задач 13/14: там есть `todo`, но нет прогона — перезапуск не должен
    /// стоить человеку начатой задачи.
    ///
    /// Фаза восстанавливается как `Prompt`, попытка — 1: про середину
    /// запроса старый файл ничего не знал и знать не мог, а врать об этом
    /// хуже, чем честно переспросить этап.
    pub fn adopt(&mut self, state: TaskState) {
        self.status = match state.status {
            Status::Idle => RunStatus::Idle,
            Status::Running => RunStatus::Running,
            Status::Paused => RunStatus::Paused,
            Status::Finished => RunStatus::DonePass,
        };
        self.state = state;
        self.phase = Phase::Prompt;
        self.attempt = 1;
        self.note(
            "adopt",
            &format!(
                "прогон собран из состояния задачи старой сессии: этап `{}`, шаг {}",
                self.state.stage.id(),
                self.state.step
            ),
        );
    }

    pub fn position(&self) -> Position {
        Position {
            status: self.status,
            stage: self.state.stage,
            phase: self.phase,
        }
    }

    pub fn goal(&self) -> &str {
        &self.state.goal
    }

    pub fn active(&self) -> bool {
        self.status != RunStatus::Idle
    }

    pub fn running(&self) -> bool {
        self.status == RunStatus::Running
    }

    pub fn awaiting_approval(&self) -> bool {
        self.status == RunStatus::AwaitingApproval
    }

    /// Применить событие. Возвращает новую позицию либо отказ; в обоих
    /// случаях в журнал уходит запись. Это единственный способ сдвинуть
    /// прогон — «перепрыгнуть» этап нечем, потому что мимо [`transition`]
    /// дороги нет.
    pub fn apply(&mut self, ev: Event) -> Result<Position, Refusal> {
        let from = self.position();
        let result = transition(from, &ev);
        let entry = LogEntry {
            at: now_secs(),
            from: from.line(),
            event: ev.name().to_string(),
            detail: ev.detail(),
            to: result.as_ref().ok().map(|s| s.to.line()),
            ok: result.is_ok(),
            why: match &result {
                Ok(_) => String::new(),
                Err(r) => r.why.clone(),
            },
        };
        self.push_log(entry);
        let step = result?;

        // Лестница двигается только после разрешения таблицы.
        match &step.act {
            Move::Stay => {}
            Move::Begin(goal) => {
                self.state = TaskState::new();
                self.state.goal = goal.clone();
                self.state.step = 1;
                self.state.status = Status::Running;
                self.created_at = now_secs();
            }
            Move::Close(summary) => {
                self.state.close(from.stage, summary);
            }
            Move::Rework(why) => {
                self.state.rework(from.stage, why);
            }
        }
        self.state.stage = step.to.stage;
        self.state.status = match step.to.status {
            RunStatus::Idle => Status::Idle,
            RunStatus::Running => Status::Running,
            RunStatus::DonePass | RunStatus::DoneFail => Status::Finished,
            _ => Status::Paused,
        };
        if matches!(step.act, Move::Close(_) | Move::Rework(_)) && !step.to.status.terminal() {
            self.state.step += 1;
        }
        self.state.note = match &ev {
            Event::Pause(why) | Event::Abort(why) | Event::Reject(why) => why.trim().to_string(),
            Event::Resume | Event::Approve(_) => String::new(),
            _ => self.state.note.clone(),
        };
        if let Event::Approve(by) = &ev {
            self.approvals
                .push(format!("{}: {}", from.stage.id(), approver(by)));
        }
        self.status = step.to.status;
        self.phase = step.to.phase;
        if step.to.phase == Phase::Prompt {
            self.attempt = 0;
        }
        self.updated_at = now_secs();
        Ok(step.to)
    }

    /// Пометить попытку текущего запроса. Отдельно от [`Self::apply`],
    /// потому что счётчик попыток не меняет допустимость переходов.
    pub fn set_attempt(&mut self, n: usize) {
        self.attempt = n;
        self.updated_at = now_secs();
    }

    /// Записать в журнал факт, который переходом не является (расхождение
    /// настроек, сообщение об исчерпанных попытках).
    pub fn note(&mut self, event: &str, detail: &str) {
        let from = self.position().line();
        self.push_log(LogEntry {
            at: now_secs(),
            from,
            event: event.to_string(),
            detail: detail.to_string(),
            to: None,
            ok: true,
            why: String::new(),
        });
    }

    fn push_log(&mut self, e: LogEntry) {
        self.log.push(e);
        if self.log.len() > LOG_CAP {
            let cut = self.log.len() - LOG_CAP;
            self.log.drain(..cut);
        }
    }

    /// Прогон поднят с диска: идущий прогон в файле означает, что процесс
    /// умер на полушаге. Молча продолжать нельзя — помечаем прерванным.
    pub fn mark_interrupted(&mut self) -> bool {
        if self.status != RunStatus::Running {
            return false;
        }
        self.status = RunStatus::Interrupted;
        self.state.status = Status::Paused;
        self.note(
            "interrupted",
            &format!(
                "прогон восстановлен с диска: этап `{}`, шаг {}, фаза {}, попытка {}",
                self.state.stage.id(),
                self.state.step,
                self.phase.as_str(),
                self.attempt.max(1)
            ),
        );
        true
    }

    /// Строка для футера и `/todo`: этап, фаза, попытка, статус.
    pub fn line(&self) -> String {
        match self.status {
            RunStatus::Idle => "прогон: задачи нет".into(),
            RunStatus::DonePass => format!("прогон: done(pass) — {}", clip(self.goal(), 48)),
            RunStatus::DoneFail => format!("прогон: done(fail) — {}", self.state.note.trim()),
            RunStatus::AwaitingApproval => format!(
                "прогон: этап {}/{} `{}` закрыт и ждёт утверждения — /todo approve | /todo reject <причина>",
                self.state.stage.index(),
                Stage::LADDER.len(),
                self.state.stage.id()
            ),
            st => format!(
                "прогон: {} · этап {}/{} `{}` · шаг {} · фаза {}{}",
                st.as_str(),
                self.state.stage.index(),
                Stage::LADDER.len(),
                self.state.stage.id(),
                self.state.step,
                self.phase.as_str(),
                if self.attempt > 1 {
                    format!(" · попытка {}", self.attempt)
                } else {
                    String::new()
                }
            ),
        }
    }

    /// Последние `n` записей журнала.
    pub fn log_tail(&self, n: usize) -> String {
        if self.log.is_empty() {
            return "журнал прогона пуст".into();
        }
        let from = self.log.len().saturating_sub(n);
        self.log[from..]
            .iter()
            .map(|e| e.line())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Карточка прогона для `/todo show`.
    pub fn card(&self) -> String {
        let mut out = self.state.card();
        out.push_str(&format!(
            "\nпрогон: {} · фаза {} · попытка {}\n",
            self.status.as_str(),
            self.phase.as_str(),
            self.attempt.max(1)
        ));
        out.push_str(&format!(
            "утверждение плана: {}\n",
            if self.approvals.is_empty() {
                format!("нет ({})", self.pinned.approve.id())
            } else {
                self.approvals.join("; ")
            }
        ));
        out.push_str(&format!(
            "зафиксировано на старте: модель {}, стратегия {}, профиль {}, инварианты {}\n",
            self.pinned.model,
            self.pinned.strategy,
            self.pinned.profile,
            if self.pinned.invariants { "on" } else { "off" }
        ));
        out
    }
}

fn approver(by: &str) -> String {
    let by = by.trim();
    if by.is_empty() {
        "человек".into()
    } else {
        by.to_string()
    }
}

/// Что вышло из одного шага лестницы.
#[derive(Clone, Debug)]
pub enum StepOutcome {
    /// Этап закрыт, прогон перешёл дальше.
    Advanced {
        closed: Stage,
        summary: String,
        next: Stage,
        text: String,
    },
    /// Этап закрыт и ждёт подписи человека.
    AwaitingApproval { stage: Stage, summary: String, text: String },
    /// Прогон встал: этап не закрыт, попытки исчерпаны, ход не дошёл.
    Paused { why: String, text: Option<String> },
    /// Терминал.
    Done { pass: bool, why: String, text: Option<String> },
    /// Переход отклонён таблицей.
    Refused(Refusal),
}

/// Драйвер лестницы: один шаг, одинаковый в TUI и в CLI.
///
/// Именно тут живёт «ассистент не может перепрыгнуть этап»: ответ модели
/// проходит `confirm` (маркер закрытия именно текущего этапа), контракт
/// этапа (`check_stage_output`, тот же retry, что у инвариантов) и только
/// потом — [`transition`]. Ни один из трёх шагов пропустить нельзя.
pub struct Engine;

impl Engine {
    /// `call(prompt, attempt, retry_note) -> Reply` — весь ввод-вывод.
    /// `persist(&TaskRun)` зовётся на каждой смене фазы, **в том числе до
    /// сетевого вызова**: иначе падение посреди запроса не видно на диске.
    pub fn step<C, P>(
        run: &mut TaskRun,
        set: &InvariantSet,
        inv_on: bool,
        extra: Option<String>,
        mut call: C,
        persist: &mut P,
    ) -> Res<StepOutcome>
    where
        C: FnMut(&str, usize, Option<&str>) -> Res<Reply>,
        P: FnMut(&TaskRun),
    {
        if !run.running() {
            return Ok(StepOutcome::Refused(Refusal {
                from: run.position().line(),
                event: "step".into(),
                why: format!("шаг лестницы не запускается из состояния `{}`", run.status.as_str()),
            }));
        }
        let stage = run.state.stage;
        let mut prompt = run.state.stage_prompt();
        if let Some(note) = extra.filter(|s| !s.trim().is_empty()) {
            prompt.push_str(&format!("\n\nУточнение от человека: {}", note.trim()));
        }

        // На диск до сети: с этого момента файл сессии знает, что мы в сети.
        let _ = run.apply(Event::PhaseTo(Phase::Model));
        run.set_attempt(1);
        persist(run);

        let outcome = pipeline::run_stage_with(set, inv_on, Some(stage), &prompt, |n, note| {
            run.set_attempt(n);
            persist(run);
            call(&prompt, n, note)
        })?;

        let _ = run.apply(Event::PhaseTo(Phase::Validate));
        persist(run);

        let text = match outcome.decision {
            Decision::Pass { text, .. } => text,
            Decision::RefusedByInvariant { explanation, .. } => {
                let _ = run.apply(Event::Pause(format!("инвариант: {explanation}")));
                persist(run);
                return Ok(StepOutcome::Paused {
                    why: explanation.clone(),
                    text: Some(explanation),
                });
            }
            Decision::GaveUp { last, violations } => {
                let why = format!(
                    "после {MAX_ATTEMPTS} попыток этап `{}` так и не сложился: {}",
                    stage.id(),
                    violations
                        .iter()
                        .map(|v| format!("{} ({})", v.id, v.evidence))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let _ = run.apply(Event::Abort(why.clone()));
                persist(run);
                return Ok(StepOutcome::Done {
                    pass: false,
                    why,
                    text: Some(last.text),
                });
            }
        };

        let _ = run.apply(Event::PhaseTo(Phase::Decide));
        persist(run);

        // Заявка на закрытие этапа: маркер именно текущего этапа и итог.
        let summary = match run.state.confirm(&text) {
            Ok(s) => s,
            Err(e) => {
                let _ = run.apply(Event::Pause(e.clone()));
                persist(run);
                return Ok(StepOutcome::Paused { why: e, text: Some(text) });
            }
        };
        let verdict = (stage == Stage::Validate)
            .then(|| crate::todo::parse_verdict(&text))
            .flatten();

        match run.apply(Event::StageClosed {
            summary: summary.clone(),
            verdict,
        }) {
            Ok(to) => {
                persist(run);
                Ok(match to.status {
                    RunStatus::AwaitingApproval => StepOutcome::AwaitingApproval {
                        stage,
                        summary,
                        text,
                    },
                    RunStatus::DonePass => StepOutcome::Done {
                        pass: true,
                        why: summary,
                        text: Some(text),
                    },
                    _ => StepOutcome::Advanced {
                        closed: stage,
                        summary,
                        next: to.stage,
                        text,
                    },
                })
            }
            Err(r) => {
                // Таблица не пустила (например, `validate` без вердикта).
                // Прогон не двигается — встаём на том же этапе.
                let _ = run.apply(Event::Pause(r.why.clone()));
                persist(run);
                Ok(StepOutcome::Refused(r))
            }
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!("{}\u{2026}", s.chars().take(n).collect::<String>())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> TaskRun {
        let mut run = TaskRun::new("ses-test", &Settings::default());
        run.apply(Event::Start("посчитать буквы".into())).unwrap();
        run
    }

    fn closed(run: &mut TaskRun, summary: &str) -> Result<Position, Refusal> {
        run.apply(Event::StageClosed {
            summary: summary.into(),
            verdict: None,
        })
    }

    #[test]
    fn start_requires_a_goal_and_lands_on_study() {
        let mut run = TaskRun::new("ses", &Settings::default());
        assert!(run.apply(Event::Start("   ".into())).is_err());
        assert_eq!(run.status, RunStatus::Idle);
        run.apply(Event::Start("задача".into())).unwrap();
        assert_eq!(run.state.stage, Stage::Study);
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn plan_cannot_reach_execute_without_approval() {
        let mut run = started();
        closed(&mut run, "изучил").unwrap();
        assert_eq!(run.state.stage, Stage::Plan);

        closed(&mut run, "план из трёх шагов").unwrap();
        assert_eq!(run.status, RunStatus::AwaitingApproval);
        assert_eq!(run.state.stage, Stage::Plan, "план не должен прыгать в execute");

        // Из-под гейта ничего не двигается.
        let before = serde_json::to_string(&run).unwrap();
        assert!(closed(&mut run, "а я всё равно сделал").is_err());
        assert!(run.apply(Event::PhaseTo(Phase::Model)).is_err());
        assert!(run.apply(Event::Resume).is_err());
        let after = serde_json::to_string(&run).unwrap();
        assert_eq!(
            strip_log(&before),
            strip_log(&after),
            "отказ не имеет права двигать состояние"
        );

        run.apply(Event::Approve("человек".into())).unwrap();
        assert_eq!(run.state.stage, Stage::Execute);
        assert_eq!(run.approvals, vec!["plan: человек".to_string()]);
    }

    #[test]
    fn reject_sends_the_plan_back_not_forward() {
        let mut run = started();
        closed(&mut run, "изучил").unwrap();
        closed(&mut run, "план").unwrap();
        assert!(run.apply(Event::Reject("  ".into())).is_err());
        run.apply(Event::Reject("шагов слишком много".into())).unwrap();
        assert_eq!(run.state.stage, Stage::Plan);
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn report_is_unreachable_without_a_verdict() {
        let mut run = started();
        closed(&mut run, "изучил").unwrap();
        closed(&mut run, "план").unwrap();
        run.apply(Event::Approve("auto".into())).unwrap();
        closed(&mut run, "сделал").unwrap();
        assert_eq!(run.state.stage, Stage::Validate);

        // Без вердикта — отказ, этап остаётся validate.
        assert!(closed(&mut run, "вроде норм").is_err());
        assert_eq!(run.state.stage, Stage::Validate);

        // «не ok» — обратно в execute, а не в report.
        run.apply(Event::StageClosed {
            summary: "нашёл дефект".into(),
            verdict: Some(Verdict::NotOk),
        })
        .unwrap();
        assert_eq!(run.state.stage, Stage::Execute);

        closed(&mut run, "починил").unwrap();
        run.apply(Event::StageClosed {
            summary: "чисто".into(),
            verdict: Some(Verdict::Ok),
        })
        .unwrap();
        assert_eq!(run.state.stage, Stage::Report);
        closed(&mut run, "отписался").unwrap();
        assert_eq!(run.status, RunStatus::DonePass);
    }

    #[test]
    fn terminal_refuses_everything() {
        let mut run = started();
        run.apply(Event::Abort("передумали".into())).unwrap();
        assert_eq!(run.status, RunStatus::DoneFail);
        for ev in [
            Event::Resume,
            Event::Pause("x".into()),
            Event::Approve("x".into()),
            Event::PhaseTo(Phase::Model),
            Event::Start("другая".into()),
        ] {
            assert!(run.apply(ev).is_err());
        }
        assert_eq!(run.status, RunStatus::DoneFail);
    }

    #[test]
    fn pause_holds_stage_and_resume_restarts_only_the_request() {
        let mut run = started();
        closed(&mut run, "изучил").unwrap();
        closed(&mut run, "план").unwrap();
        run.apply(Event::Approve("auto".into())).unwrap();
        run.apply(Event::PhaseTo(Phase::Model)).unwrap();
        run.set_attempt(3);
        let (stage, step) = (run.state.stage, run.state.step);

        run.apply(Event::Pause("Esc".into())).unwrap();
        assert!(closed(&mut run, "тайком").is_err());
        assert!(run.apply(Event::Approve("x".into())).is_err());
        assert_eq!((run.state.stage, run.state.step), (stage, step));

        run.apply(Event::Resume).unwrap();
        assert_eq!((run.state.stage, run.state.step), (stage, step));
        assert_eq!(run.phase, Phase::Prompt);
        assert_eq!(run.attempt, 0, "оборванный запрос начинается заново");
    }

    #[test]
    fn interrupted_run_is_found_on_load_and_resumes() {
        let mut run = started();
        run.apply(Event::PhaseTo(Phase::Model)).unwrap();
        let raw = serde_json::to_string(&run).unwrap();
        let mut back: TaskRun = serde_json::from_str(&raw).unwrap();
        assert!(back.mark_interrupted());
        assert_eq!(back.status, RunStatus::Interrupted);
        assert!(closed(&mut back, "тайком").is_err());
        back.apply(Event::Resume).unwrap();
        assert_eq!(back.state.stage, Stage::Study);
    }

    #[test]
    fn serde_round_trip_and_log_cap() {
        let mut run = started();
        for _ in 0..(LOG_CAP + 20) {
            let _ = run.apply(Event::Resume); // отказ, но пишется в журнал
        }
        assert_eq!(run.log.len(), LOG_CAP);
        let raw = serde_json::to_string(&run).unwrap();
        let back: TaskRun = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, run);
    }

    #[test]
    fn pinned_settings_report_drift() {
        let settings = Settings::default();
        let p = Pinned::of(&settings);
        assert!(p.drift(&settings).is_empty());
        let mut other = settings.clone();
        other.model = "другая-модель".into();
        assert_eq!(p.drift(&other).len(), 1);
    }

    /// Журнал растёт и на отказах — сравнивать состояние «до/после» нужно
    /// без него, иначе тест доказывал бы противоположное.
    fn strip_log(raw: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
        v.as_object_mut().unwrap().remove("log");
        v.as_object_mut().unwrap().remove("updated_at");
        v.to_string()
    }
}
