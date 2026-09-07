# Рабочий агент — чат-TUI поверх z.ai

Терминальный ChatGPT-стиль клиент на Rust/ratatui. Это **база для следующих
тасок**: не одноразовый HTTP-вызов, а сущность `Agent` с настройками, историей
и инструкциями из AGENTS.md, говорящая с z.ai по обычному OpenAI-совместимому
API. Папка — рабочая копия задачи 6, сделана из дословной копии корневого
чат-TUI; транспорт, каталог моделей и политика разговора переписаны под z.ai.

Живые вызовы разрешены **только** для `glm-5.3-flash`. Остальные десять id из
каталога можно выбрать в TUI, но на отправке клиент отказывается — они стоят
денег и намеренно не гонялись.

## Зачем отдельная сущность `Agent`

CLI и TUI **не** ходят в HTTP сами. Они держат `Agent` и зовут его методы.
`api.rs` — тонкий транспорт: собрать тело, POST/stream, разобрать ответ.

`Agent` владеет всем, что составляет один разговор:

* runtime-настройки (`Settings`: модель, effort, sampling, JSON, стоп, лимиты);
* системный промпт;
* история user/assistant;
* снимок загруженных AGENTS.md / CLAUDE.md (`ContextBundle`).

Политика, которой нет у «голого» `chat()`:

* `ask()` дописывает реплику пользователя, зовёт провайдера и кладёт ответ;
  если транспорт упал — пользовательская реплика откатывается, история не
  остаётся кривой.
* `complete()` / `stream()` отвечают по явной истории и **не** мутируют
  собственную. Так работают TUI-сессия и `/personas`.
* Системный промпт и AGENTS.md собираются в `messages[0]` с `role: system` и
  **никогда** не попадают в `history` / сохранённую сессию. Иначе длинный чат
  размножил бы файл N раз и показал бы фейковый user-пузырь.
* `resume()` восстанавливает настройки и историю из файла, а instruction-файлы
  перечитывает с диска (пути в сессии — подсказка, не кэш содержимого).
* Перед живым вызовом стоит `guard_live_model`: всё, что не `glm-5.3-flash`,
  отсекается до сокета.

Это и есть graded-требование: агент — объект разговора, а не обёртка над
одним POST.

## z.ai

База — **plain** API, не coding-plan:

```
https://api.z.ai/api/paas/v4
```

Completions: `POST …/chat/completions`. Endpoint `/api/coding/paas/v4` клиент
никогда не использует.

Ключ (значение нигде не печатается, ни в логах, ни в ошибках):

1. `$ZAI_API_KEY`
2. `~/.pi/agent/auth.json` → `["zai-coding-cn"]["key"]`
3. `~/.omp/agent/auth.json` — то же поле

Ключ лежит в слоте coding-плана, но ходит на обычный paas/v4. Если ничего не
нашлось, ошибка перечисляет эти три места и напоминает про plain URL.

## Модели

Каталог с `GET https://api.z.ai/api/paas/v4/models` на 2026-09-07 (10 id):

`glm-4.5`, `glm-4.5-air`, `glm-4.6`, `glm-4.7`, `glm-5`, `glm-5-turbo`,
`glm-5.1`, `glm-5.2`, `glm-5.3`, **`glm-5.3-flash`**.

Дефолт и единственная live-модель — `glm-5.3-flash`. Остальные выбираются
через `/model`, пикер в `/settings` и `--model`, но `Agent::complete` /
stream / CLI one-shot отказываются до HTTP: «other catalog models are
expensive».

## Запуск

```sh
cargo build --release --manifest-path tree/task-6/Cargo.toml
# бинарник: tree/task-6/target/release/ask
# target/ в gitignore — артефакт сборки в снапшот не коммитится
```

```sh
./tree/task-6/target/release/ask                         # чат-TUI
./tree/task-6/target/release/ask "вопрос"                # one-shot
./tree/task-6/target/release/ask --verify                # живой self-test рычагов
./tree/task-6/target/release/ask --sessions              # список сессий
./tree/task-6/target/release/ask --continue              # последняя сессия
./tree/task-6/target/release/ask --resume ID             # сессия по id
```

`--verify-stop` — видимый алиас `--verify`. Нужен `$ZAI_API_KEY` (или ключ в
auth.json, см. выше).

## Runtime-настройки

Всё, что видит один ход генерации (`Settings`). Клампится в документированные
диапазоны z.ai до сборки тела, чтобы цикл в TUI не протащил `temp=2.0`.

| рычаг | где | на проводе | диапазон / заметка |
|---|---|---|---|
| `model` | `/model`, settings, `--model` | `model` | каталог; live только `glm-5.3-flash` |
| `system_prompt` | `/system`, settings | `role: system` | дефолт: «Ты — полезный ассистент…» |
| `context_enabled` | `/context on\|off` | файлы в system | по умолчанию on |
| `effort` | `/effort`, settings, `--effort` | `reasoning_effort` | на проводе только `low`/`high`/`max` |
| `json_mode` | `/json …` | `response_format: json_object` + подсказка в system | schema — hint, не гарантия |
| `max_chars` | settings, `--max-chars` | hint + клиентский truncate | гарантия на видимый ответ |
| `budget_tokens` | `/max-tokens`, settings, `--budget-tokens` / `--max-tokens` | `max_tokens` | 1…131072; reasoning + visible |
| `stop` | `/stop add\|clear`, `--stop` | `stop` | максимум 4 строки |
| `temperature` | `/temp`, settings, `--temperature` | `temperature` | 0.0…**1.0** (дока z.ai) |
| `top_p` | `/top-p`, settings, `--top-p` | `top_p` | 0.01…1.0; unset = дефолт площадки |
| `top_k` | `/top-k`, settings, `--top-k` | `top_k` | >0 или `-1` (full); в публичной схеме нет |

Плюс всегда: `thinking: { type: "enabled", clear_thinking: false }`. У
`glm-5.3-flash` thinking выключить нельзя.

Панель `/settings` (Tab): model, effort, json mode, max_chars, budget_tokens,
stop, temperature, top_p, top_k, system_prompt. Контекст туда не вынесен —
только `/context`.

## Сессии

Каждый чат — JSON под `~/.ask6/sessions/<id>.json` (не `~/.ask/sessions/`
старого TUI). Переопределение: `$ASK_SESSIONS_DIR`. Запись атомарная
(`.tmp` + rename). В файле: id, title, timestamps, messages, settings,
пути instruction-файлов на момент сохранения.

`/new` и Ctrl-N начинают новый чат; `/sessions` — список и переключение;
`/rename <title>` переименовывает. CLI: `--sessions`, `--resume ID`,
`--continue`. Удаление в панели: `d` / Delete, подтверждение `y`/`n`.

## AGENTS.md

Складывается в **system**, не в историю. На каждый HTTP-запрос — один раз,
как `messages[0]`.

Глобальный файл, первый существующий обычный файл побеждает (от `$HOME`):

1. `~/.pi/agent/AGENTS.md`
2. `~/.claude/CLAUDE.md`
3. `~/.config/ask/AGENTS.md`

Локально: от cwd вверх до git-корня (каталог с `.git`). В каждой директории
`AGENTS.md` бьёт `CLAUDE.md`. Несколько попаданий идут **снаружи внутрь**
(корень репо → дети → cwd). Нет `.git` — ищется только cwd, никогда `/`.

Бинарные (NUL), каталоги, нечитаемые и отсутствующие пропускаются. Потолок
файла — 32 768 символов, хвост отрезается с маркером. `/context show|on|off|reload`
показывает список, гасит ingest или перечитывает диск.

## Клавиши

```
Enter          отправить сообщение или выполнить /команду
Shift+Enter    перевод строки (поле растёт до 4 строк)
←→ ↑↓          курсор по тексту; на первой/последней строке ↑↓ скроллят чат
Tab            фокус на панель настроек
↑↓ / j k       (settings / sessions / model) выбор
←→ / h l       (settings) изменить выбранный рычаг
Enter / Space  (settings) пикер модели или редактор system prompt
d / Delete     (sessions) удалить чат, с подтверждением y/n
Ctrl-N         новый чат
PgUp / PgDn    скролл разговора (по 10)
колёсико       скролл разговора (по 3)
Home / End     начало / конец разговора
Esc            во время генерации — стоп (частичный ответ остаётся);
               иначе — закрыть панель / выйти из ввода
Ctrl-Q         выход, в том числе во время генерации
```

Shift+Enter требует kitty keyboard protocol; без него деградирует в Enter.
Вставка многострочного текста — одним bracketed-paste, не построчной отправкой.
Скролл вверх отцепляет follow за хвостом стрима; возврат вниз прицепляет снова.
Любой расчёт offset климпуется через `App::max_scroll` — иначе
`Paragraph::scroll` рисует пустую панель.

Попап команд (ввод начинается с `/`): ↑↓ выбор, → применить, ← закрыть.

## Slash-команды

```
/new                              новый чат
/rename <title>                   переименовать текущий
/sessions                         список и переключение
/effort [none|low|medium|high|max]  reasoning effort (none→low, medium→high)
/model [id]                       текущая / сменить / открыть пикер
/system [text|edit|clear]         системный промпт
/json on|off                      structured JSON
/json fields a,b,c                плоская string-схема
/json schema <json>               произвольный JSON Schema
/json edit <instruction>          модель переписывает схему
/json show                        показать схему
/temp [off|0.0-1.0]               температура
/top-p [off|0.01-1.0]             nucleus
/top-k [off|full|N]               top-k
/max-tokens [off|1-131072]        потолок токенов
/stop add <seq>                   стоп-последовательность (макс. 4)
/stop clear                       сбросить стопы
/verify                           живой self-test рычагов z.ai
/personas <question>              физик / философ / математик, по одному вызову
/personas a,b,c: <question>       свой состав
/context [show|on|off|reload]     AGENTS.md в system
/settings                         панель настроек (то же, что Tab)
/help                             эта шпаргалка
/quit                             выход
```

`/personas` никогда не параллелит вызовы. Каждая персона видит только исходный
вопрос, не чужие ответы.

## Что показал `--verify`

Живой прогон на `glm-5.3-flash` (коммит `8845ea8`). Вердикт — причинная
сигнатура, не «тексты различаются». `Flat` / `Unsupported` — честный итог,
если площадка рычаг игнорирует или глушит thinking'ом.

endpoint: `https://api.z.ai/api/paas/v4/chat/completions`

| проверка | наблюдение | вердикт |
|---|---|---|
| reachability | model=`glm-5.3-flash` finish=`stop` prompt=20 completion=10 reasoning=5 total=30 **2022 ms** | **Confirmed** |
| max_tokens 16 vs 96 | capped finish=`length` completion=16 reasoning=6 **1637 ms**; control finish=`length` completion=96 reasoning=53 **4610 ms** | **Confirmed** |
| system prompt `QUINCE` | on=`QUINCE`, off=`NONE` | **Confirmed** |
| AGENTS.md `NIGHTJAR` | on=`NIGHTJAR` (prompt_tokens=**684**), off=`NONE` | **Confirmed** |
| temperature 0.0 vs 1.0, n=4 | cold `[7,7,7,7]` distinct=1; hot `[7,14,7,17]` distinct=3 | **Confirmed** |
| top_p 0.01 vs 1.0 | cold distinct=2 `[17,7,7,7]`; hot distinct=1 `[7,7,7,7]` | **Flat** |
| top_k 1 vs full | cold distinct=2 `[7,14,7,7]`; hot distinct=2 `[14,7,7,7]` | **Unsupported** |

`cargo test` — 111/111, без сети. `cargo clippy --all-targets -- -D warnings` — чисто.

Confirmed для max_tokens: `finish_reason=length` **и** `completion_tokens`
ровно равен капу, а контрольный прогон длиннее капа. Control тоже упёрся в
свой кап (96), это нормально: сравнивается не «дописал до конца», а «кап
режет раньше».

Temperature Confirmed только потому, что холодная сторона схлопнулась в одно
число, а горячая разошлась. top_p не дал этой картинки (узкий nucleus даже
шире, чем широкий). top_k=1 не схлопнул выдачу — параметр либо не дошёл до
семплера, либо его перебил обязательный thinking.

## Подводные камни

* **Coding-plan URL — ловушка.** Ключ живёт в `zai-coding-cn`, но completions
  надо слать на `https://api.z.ai/api/paas/v4`, не на `/api/coding/paas/v4`.
* **Thinking не выключается.** `thinking.type: disabled`, `reasoning_effort:
  none` и `medium` — HTTP 400, код 1210: «This model always engages in
  thinking and cannot be disabled; please use low, high, or max». `none` и
  `medium` остаются в enum, чтобы старые сессии десериализовались, и мапятся
  в `low` / `high` на проводе.
* **Температура в доке — `[0.0, 1.0]`.** Live 1.5 и 2.0 на flash дали HTTP 200
  (в отличие от yolo-auto, который 400 на 2.01). Клиент всё равно клампит по
  доке. В `/help` ещё торчит текст «0.0–2.0» от старого TUI — врать ему не
  стоит, рычаг режет на 1.0.
* **`json_schema` / `strict` не держат форму.** Live: HTTP 200, но в ответе
  markdown-забор и лишние поля. На проводе `response_format: json_object` плюс
  схема в system; flatten на клиенте. Это hint, не гарантия провайдера.
* **`top_k` в публичной Chat Completions схеме нет**, но Java-класс запроса
  поле знает: `top_k: "nope"` вернул HTTP 400 с именем
  `ChatCompletionRequest["top_k"]`. Поле шлётся, когда задано. Self-test:
  `Unsupported` — на flash оно не ведёт себя как greedy cutoff.
* **`top_p` на flash с thinking почти не семплит.** 0.01 vs 1.0 → `Flat`.
  Смотреть температуру лучше при выключенных top_p/top_k (как сделал verify).
* **Сессии — `~/.ask6/`, не `~/.ask/`.** Намеренно другой каталог, чтобы не
  затереть чаты корневого TUI.
* **AGENTS.md не в историю.** Иначе чередование user/assistant ломается,
  файл размножается каждым ходом и всплывает пузырём в TUI.
* **Дорогие модели выбираются, но не вызываются.** Пикер показывает все 10 id;
  отказ — в `guard_live_model`, до сокета, с явной ошибкой.
* **Скролл за конец контента.** `Paragraph::scroll` клипует, не клампит:
  offset больше высоты wrapped-текста → пустая панель. Offset всегда через
  `App::max_scroll`.

## Состав

```
src/main.rs     диспетчер
src/cli.rs      clap, one-shot, --verify, --sessions/--resume/--continue
src/agent.rs    сущность разговора: settings, history, context, ask/complete/stream
src/api.rs      ключ, тело, POST/SSE, parse; без политики разговора
src/config.rs   Settings, Effort, каталог моделей, клампы
src/context.rs  discovery + сборка AGENTS.md / CLAUDE.md в system
src/session.rs  ~/.ask6/sessions/*.json
src/render.rs   flatten JSON-ответа («Key: value»), общий для CLI и TUI
src/tui.rs      чат, панели, slash-команды, пикер модели
src/verify.rs   живой self-test рычагов (glm-5.3-flash only)
```
