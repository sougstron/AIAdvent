# RAG-чат с памятью задачи (задача 25)

Модель `glm-5.3-flash`, индекс `rag/index.sqlite` (стратегия `fixed`), память задачи on. Каждый ход: вопрос (+ строка состояния в запросе эмбеддинга) → retrieval → ответ с обязательными источниками и дословными цитатами → экстрактор обновляет память задачи (цель / уточнено / ограничения / термины), которая блоком `<task-state>` едет в каждый следующий запрос.

## Сводка

| сценарий | ходов | ответов с источниками | цитаты дословно | числа в цитатах | судья: да | «не знаю» правильно | ожидание must | выдумал | память: проверок прошло | цель поймана |
|---|---|---|---|---|---|---|---|---|---|---|
| 1. Сервис конфигураций на Raft (raft-consensus.pdf) | 14 | 13/13 | 13/13 | 12/13 | 9/13 | 1/1 | 20/23 | 0 | 9/9 | 2/2 |
| 2. Production-чат по документации компании (rag-survey-2312.10997.pdf) | 13 | 9/10 | 9/10 | 9/10 | 8/10 | 1/1 | 18/21 | 0 | 8/8 | 2/2 |

## Сценарий 1: Сервис конфигураций на Raft (raft-consensus.pdf)

Ожидаемая цель: спроектировать по статье о Raft внутренний сервис конфигураций: кластер из 5 узлов, строго актуальные чтения, смена состава без даунтайма

Цель в памяти после диалога: «Спроектировать внутренний распределённый сервис конфигураций на основе Raft, отказоустойчивый к отказам узлов, начиная с изучения устройства Raft.» (2/2)

Память в конце:

```json
{
  "goal": "Спроектировать внутренний распределённый сервис конфигураций на основе Raft, отказоустойчивый к отказам узлов, начиная с изучения устройства Raft.",
  "clarified": [
    "Проектируют внутренний сервис конфигураций",
    "Сервис распределённый и должен переживать отказ узлов",
    "Основан на статье о Raft",
    "Изучают Raft с нуля, просят краткое описание",
    "Уточняют детали выбора таймаутов",
    "Договорились использовать термин quorum = 3",
    "Требование: чтения строго актуальные, без устаревших значений",
    "Запросили итоговую сводку цели, ограничений и терминов"
  ],
  "constraints": [
    "Кластер ровно из 5 узлов",
    "quorum = 3 (согласованное значение для проекта)",
    "Чтения должны быть строго актуальными (линеаризуемыми), без устаревших значений",
    "Смена состава кластера без даунтайма"
  ],
  "terms": [
    {
      "term": "Raft",
      "meaning": "Алгоритм консенсуса для реплицированного лога; сначала выбирает выделенного лидера, управляющего логом"
    },
    {
      "term": "Лидер",
      "meaning": "Принимает записи от клиентов, реплицирует их на другие серверы и сообщает, когда записи можно применять к state machine"
    },
    {
      "term": "Heartbeat",
      "meaning": "Пустые AppendEntries RPC, которые лидер периодически шлёт followers"
    },
    {
      "term": "Election timeout",
      "meaning": "Выбирается случайно из фиксированного интервала; по истечении без сообщений сервер начинает выборы"
    },
    {
      "term": "Term",
      "meaning": "Номер, который candidate увеличивает при старте выборов"
    },
    {
      "term": "Follower/Candidate",
      "meaning": "Серверы стартуют как followers; не получив heartbeat, становятся candidates, голосуют за себя и рассылают RequestVote RPC"
    },
    {
      "term": "Рандомизированные таймеры выборов",
      "meaning": "Предотвращают и быстро разрешают split votes; candidate перезапускает таймаут перед новой попыткой"
    },
    {
      "term": "Кворум (quorum)",
      "meaning": "Для нас quorum = 3: большинство кластера из 5 узлов; committed запись — реплицированная на это большинство"
    },
    {
      "term": "Committed запись",
      "meaning": "Запись, реплицированная лидером на большинство серверов (quorum); гарантированно durable и в итоге исполняется всеми доступными state machines"
    },
    {
      "term": "Joint consensus (Cold,new)",
      "meaning": "Переходная конфигурация при смене состава кластера без даунтайма; новые узлы сначала входят как non-voting members"
    }
  ]
}
```

| # | вопрос | статус | источники | ожидание | память |
|---|---|---|---|---|---|
| 1 | Мы проектируем внутренний сервис конфигураций по статье о Raft: распределённый, переживает отказ узлов. С чего начать — как Raft вообще устроен, коротко? | answer (ждали answer) | 3/3 | 2/2 | ✓ |
| 2 | Ограничение фиксируем: кластер ровно из 5 узлов. Сколько узлов нужно для кворума и почему? | answer (ждали answer) | 2/2 | 1/2 | ✓ |
| 3 | Как выбирается лидер? Кто начинает выборы и как побеждает? | answer (ждали answer) | 3/3 | 2/2 | — |
| 4 | А таймауты? | answer (ждали answer) | 3/3 | 2/2 | — |
| 5 | Зачем таймауты выборов рандомизированы? | answer (ждали answer) | 2/2 | 1/1 | — |
| 6 | Согласуем термин: для нас «quorum = 3». Как лидер надёжно фиксирует запись в логе? | answer (ждали answer) | 2/2 | 2/2 | ✓ |
| 7 | Ещё требование: чтения должны быть строго актуальными, никаких устаревших значений. Что это меняет для нас по статье? | answer (ждали answer) | 2/2 | 2/2 | ✓ |
| 8 | Состав кластера менять придётся, но без даунтайма. Как Raft меняет конфигурацию? | answer (ждали answer) | 3/3 | 1/1 | ✓ |
| 9 | А новый узел, который только поднялся, — он сразу голосует? | answer (ждали answer) | 1/1 | 1/1 | — |
| 10 | Если узел отстал на очень много записей, как его наверстать? | answer (ждали answer) | 1/1 | 1/1 | — |
| 11 | Напомни, сколько у нас узлов в кластере и какой кворум? | answer (ждали answer) | 1/1 | 0/2 | — |
| 12 | Что статья о Raft говорит про шардирование PostgreSQL? | unknown (ждали unknown) | 0/0 | — | — |
| 13 | Как называется переходная конфигурация при смене состава кластера? | answer (ждали answer) | 2/2 | 1/1 | — |
| 14 | Собери итог списком: цель проекта и все зафиксированные ограничения и термины. | answer (ждали answer) | 2/2 | 4/4 | ✓ |

### Ответы

#### 1.01 Мы проектируем внутренний сервис конфигураций по статье о Raft: распределённый, переживает отказ узлов. С чего начать — как Raft вообще устроен, коротко?

_контекст:_
1. #3 cos 0.9920 z 3.10 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 3 · 994 симв.
2. #8 cos 0.9912 z 2.83 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 5–6 · 999 симв.
3. #10 cos 0.9911 z 2.79 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 1000 симв.
4. #12 cos 0.9909 z 2.74 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 998 симв.

> Raft — алгоритм консенсуса для реплицированного лога: он сначала выбирает выделенного лидера, который полностью отвечает за управление логом — принимает записи от клиентов, реплицирует их на другие серверы и сообщает, когда записи можно применять к state machine. Выборы лидера запускаются механизм heartbeat: серверы стартуют как followers; лидер периодически шлёт heartbeats (пустые AppendEntries RPC). Если follower не получает сообщений в течение election timeout, он начинает выборы — увеличивает term, становится candidate, голосует за себя и рассылает RequestVote RPC. Используются рандомизированные таймеры выборов, а лог-записи идут только от лидера к остальным серверам [1][2][3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 3 · fixed-raft-consensus-0017
>   [2] raft-consensus.pdf — Front matter, стр. 5–6 · fixed-raft-consensus-0031
>   [3] raft-consensus.pdf — Front matter, стр. 1 · fixed-raft-consensus-0001
> 
> Цитаты:
>   [1] «Raft implements consensus by first electing a distinguished leader, then giving the leader complete responsibility for managing the replicated log. The leader accepts log entries from clients, replicates them on other servers, and tells servers when it is safe to apply log entries to their state machines.»
>   [2] «If a follower receives no communication over a period of time called the election timeout, then it assumes there is no viable leader and begins an election to choose a new leader. To begin an election, a follower increments its current term and transitions to candidate state.»
>   [3] «Raft uses randomized timers to elect leaders.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 3/3 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично
#### 1.02 Ограничение фиксируем: кластер ровно из 5 узлов. Сколько узлов нужно для кворума и почему?

_контекст:_
1. #13 cos 0.9937 z 2.93 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.
2. #9 cos 0.9939 z 2.99 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 995 симв.
3. #17 cos 0.9934 z 2.84 llm 5 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 997 симв.

> Для кворума нужно большинство кластера из 5 узлов; такой кластер типичен для Raft и позволяет пережить отказ двух узлов — candidate обязан связаться с большинством кластера для избрания. [1][2]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 5 · fixed-raft-consensus-0029
>   [2] raft-consensus.pdf — Front matter, стр. 8 · fixed-raft-consensus-0047
> 
> Цитаты:
>   [1] «A Raft cluster contains several servers; five is a typical number, which allows the system to tolerate two failures.»
>   [2] «A candidate must contact a majority of the cluster in order to be elected»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 2/2 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да

#### 1.03 Как выбирается лидер? Кто начинает выборы и как побеждает?

_контекст:_
1. #3 cos 0.9956 z 3.46 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 5–6 · 999 симв.
2. #12 cos 0.9942 z 3.03 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
3. #7 cos 0.9946 z 3.13 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.
4. #19 cos 0.9937 z 2.84 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.

> Лидер выбирается механизмом heartbeat: серверы стартуют как followers; если follower не получает сообщений в течение election timeout, он начинает выборы — увеличивает term, становится candidate, голосует за себя и рассылает RequestVote RPC каждому серверу [1]. Candidate побеждает, если получает голоса большинства кластера (split vote возникает, когда ни один candidate не получает majority) [3]. Для предотвращения повторяющихся split votes используются рандомизированные election timeouts, выбираемые из фиксированного интервала [2].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 5–6 · fixed-raft-consensus-0031
>   [2] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
>   [4] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0033
> 
> Цитаты:
>   [1] «If a follower receives no communication over a period of time called the election timeout, then it assumes there is no viable leader and begins an election to choose a new leader.»
>   [1] «To begin an election, a follower increments its current term and transitions to candidate state. It then votes for itself and issues RequestVote RPCs in parallel to each of the other servers in the cluster.»
>   [3] «votes could be split so that no candidate obtains a majority»
>   [2] «election timeouts are chosen randomly from a fixed interval»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 4/4 дословно (1 — из другого фрагмента) · ✓ смысл по цитатам: да

#### 1.04 А таймауты?

_контекст:_
1. #1 cos 0.9953 z 3.31 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
2. #9 cos 0.9942 z 2.98 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 15 · 996 симв.
3. #11 cos 0.9938 z 2.87 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 1000 симв.
4. #4 cos 0.9950 z 3.22 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 5–6 · 999 симв.

> Election timeouts выбираются случайно из фиксированного интервала (например, 150–300ms) — это предотвращает split votes, поскольку застаёт только один сервер, который выигрывает выборы и шлёт heartbeats раньше других [1]. Рекомендуется консервативный интервал 150–300ms [2]. На практике election timeout likely 10ms–500ms [3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
>   [2] raft-consensus.pdf — Front matter, стр. 15 · fixed-raft-consensus-0092
>   [3] raft-consensus.pdf — Front matter, стр. 10–11 · fixed-raft-consensus-0063
> 
> Цитаты:
>   [1] «election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms). This spreads out the servers so that in most cases only a single server will time out; it wins the election and sends heartbeats before any other servers time out.»
>   [2] «We recommend using a conservative election timeout such as 150–300ms; such timeouts are unlikely to cause unnecessary leader changes and will still provide good availability.»
>   [3] «the election timeout is likely to be somewhere between 10ms and 500ms»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 3/3 дословно · ✓ числа 4/4 в цитатах · ✓ смысл по цитатам: да

#### 1.05 Зачем таймауты выборов рандомизированы?

_контекст:_
1. #1 cos 0.9952 z 3.68 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
2. #10 cos 0.9878 z 3.03 llm 9 · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
3. #20 cos 0.9920 z 2.75 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 15 · 994 симв.
4. #17 cos 0.9922 z 2.82 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 1000 симв.

> Рандомизация предотвращает и быстро разрешает split votes: таймауты выбираются случайно из фиксированного интервала, серверы «разносятся» так, что обычно только один успевает выйти из таймаута, выигрывает выборы и шлёт heartbeats раньше остальных [1]. Каждый candidate также перезапускает свой рандомизированный таймаут перед новой попыткой [1]. Без рандомизации выборы из-за split votes стабильно длились >10 секунд, а уже 5ms случайности давали медиану 287ms [3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
>   [3] raft-consensus.pdf — Front matter, стр. 15 · fixed-raft-consensus-0091
> 
> Цитаты:
>   [1] «Raft uses randomized election timeouts to ensure that split votes are rare and that they are resolved quickly.»
>   [1] «Each candidate restarts its randomized election timeout at the start of an election»
>   [3] «In the absence of randomness, leader election consistently took longer than 10 seconds in our tests due to many split votes. Adding just 5ms of randomness helps significantly, resulting in a median downtime of 287ms.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ числа 3/3 в цитатах · ✓ смысл по цитатам: да

#### 1.06 Согласуем термин: для нас «quorum = 3». Как лидер надёжно фиксирует запись в логе?

_контекст:_
1. #3 cos 0.9941 z 3.36 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 7 · 994 симв.
2. #6 cos 0.9933 z 3.11 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 6–7 · 997 симв.
3. #2 cos 0.9943 z 3.40 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 1000 симв.
4. #4 cos 0.9939 z 3.29 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 9 · 1000 симв.

> Запись считается committed, когда лидер реплицировал её на большинство серверов — для нашего кластера это наш quorum; Raft гарантирует, что committed записи durable и в итоге исполнятся всеми доступными state machines [1]. Лидер решает, когда запись безопасно применять к state machine, и отслеживает наивысший committed index, включая его в будущие AppendEntries RPC и heartbeats, а follower, узнав о committed записи, применяет её к локальной state machine в порядке лога [1]. Если followers падают, медленны или пакеты теряются, лидер бесконечно повторяет AppendEntries RPC, пока все followers не сохранят все записи [2].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 7 · fixed-raft-consensus-0038
>   [2] raft-consensus.pdf — Front matter, стр. 6–7 · fixed-raft-consensus-0037
> 
> Цитаты:
>   [1] «A log entry is committed once the leader that created the entry has replicated it on a majority of the servers (e.g., entry 7 in Figure 6).»
>   [1] «Raft guarantees that committed entries are durable and will eventually be executed by all of the available state machines.»
>   [1] «The leader keeps track of the highest index it knows to be committed, and it includes that index in future AppendEntries RPCs (including heartbeats) so that the other servers eventually find out.»
>   [1] «Once a follower learns that a log entry is committed, it applies the entry to its local state machine (in log order).»
>   [2] «If followers crash or run slowly, or if network packets are lost, the leader retries AppendEntries RPCs indefinitely (even after it has responded to the client) until all followers eventually store all log entries.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 5/5 дословно · ✓ смысл по цитатам: да

#### 1.07 Ещё требование: чтения должны быть строго актуальными, никаких устаревших значений. Что это меняет для нас по статье?

_контекст:_
1. #1 cos 0.9940 z 2.96 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 13 · 996 симв.
2. #2 cos 0.9937 z 2.87 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 13 · 998 симв.

> Чтения без записи в лог могут вернуть устаревшие данные: лидер, отвечающий на запрос, мог быть заменён более новым лидером. Raft требует двух предосторожностей для линеаризуемых чтений без записи в лог. Во-первых, лидер должен знать, какие записи committed, — новый лидер в начале срока коммитит пустую no-op запись, чтобы выяснить это. Во-вторых, перед обработкой read-only запроса лидер обязан проверить, что не был смещён: он обменивается heartbeat-сообщениями с большинством кластера (наш quorum) перед ответом. Альтернативно можно использовать heartbeat-механизм как lease, но это опирается на тайминги и предполагает ограниченный clock skew. [1][2]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 13 · fixed-raft-consensus-0081
>   [2] raft-consensus.pdf — Front matter, стр. 13 · fixed-raft-consensus-0080
> 
> Цитаты:
>   [2] «Read-only operations can be handled without writing anything into the log. However, with no additional measures, this would run the risk of returning stale data, since the leader responding to the request might have been superseded by a newer leader of which it is unaware.»
>   [1] «Raft handles this by having each leader commit a blank no-op entry into the log at the start of its term.»
>   [1] «Raft handles this by having the leader exchange heartbeat messages with a majority of the cluster before responding to read-only requests. Alternatively, the leader could rely on the heartbeat mechanism to provide a form of lease [9], but this would rely on timing for safety (it assumes bounded clock skew).»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 1.08 Состав кластера менять придётся, но без даунтайма. Как Raft меняет конфигурацию?

_контекст:_
1. #5 cos 0.9932 z 3.12 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.
2. #18 cos 0.9918 z 2.71 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.
3. #10 cos 0.9925 z 2.92 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 992 симв.
4. #20 cos 0.9915 z 2.63 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 993 симв.

> Raft автоматизирует смену конфигурации и использует двухфазный подход с joint consensus: лидер получает запрос перехода с Cold на Cnew, сохраняет конфигурацию joint consensus (Cold,new) как запись лога и реплицирует её обычными механизмами. Для коммита Cold,new требуются отдельные большинство старой и новой конфигураций; затем создаётся и коммитится запись Cnew большинством Cnew. Сервер всегда использует последнюю конфигурацию в своём логе, joint consensus позволяет серверам переходить в разное время без ущерба для безопасности, и нет момента, когда Cold и Cnew могли бы принимать решения независимо — кластер продолжает обслуживать клиентов на протяжении всей смены. [1][2][3]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 11 · fixed-raft-consensus-0068
>   [2] raft-consensus.pdf — Front matter, стр. 11 · fixed-raft-consensus-0067
>   [3] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0061
> 
> Цитаты:
>   [1] «When the leader receives a request to change the configuration from Cold to Cnew , it stores the configuration for joint consensus (Cold,new in the figure) as a log entry and replicates that entry»
>   [1] «Once a given server adds the new configuration entry to its log, it uses that configuration for all future decisions»
>   [1] «joint consensus allows the cluster to continue servicing client requests throughout the configuration change»
>   [2] «Agreement (for elections and entry commitment) requires separate majorities from both the old and new configurations.»
>   [2] «Then it creates the Cnew entry and commits it to a majority of Cnew . There is no point in time in which Cold and Cnew can both make decisions independently.»
>   [3] «configuration changes must use a two-phase approach»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 6/6 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 3/3 · ✓ цитаты 6/6 дословно · ✗ смысл по цитатам: частично
#### 1.09 А новый узел, который только поднялся, — он сразу голосует?

_контекст:_
1. #6 cos 0.9927 z 3.08 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 993 симв.

> Нет — чтобы избежать availability gaps, Raft вводит дополнительную фазу: новый сервер входит в кластер как non-voting member, лидер реплицирует ему записи лога, но он не учитывается в majorities; только когда он догонит кластер, начинается смена конфигурации [1].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 10–11 · fixed-raft-consensus-0064
> 
> Цитаты:
>   [1] «Raft introduces an additional phase before the configuration change, in which the new servers join the cluster as non-voting members»
>   [1] «the leader replicates log entries to them, but they are not considered for majorities»
>   [1] «Once the new servers have caught up with the rest of the cluster, the reconfiguration can proceed as described above.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 1.10 Если узел отстал на очень много записей, как его наверстать?

_контекст:_
1. #5 cos 0.9939 z 2.83 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 12 · 992 симв.
2. #2 cos 0.9944 z 3.00 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 1000 симв.
3. #8 cos 0.9937 z 2.80 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 7 · 998 симв.

> Если лидер уже discard-нул следующую запись лога, которую нужно отправить отставшему follower, он наверстывает разрыв, отправляя follower снапшот по сети через RPC InstallSnapshot [1].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 12 · fixed-raft-consensus-0075
> 
> Цитаты:
>   [1] «This happens when the leader has already discarded the next log entry that it needs to send to a follower.»
>   [1] «The way to bring such a follower up-to-date is for the leader to send it a snapshot over the network.»
>   [1] «The leader uses a new RPC called InstallSnapshot to send snapshots to followers that are too far behind»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 1.11 Напомни, сколько у нас узлов в кластере и какой кворум?

_контекст:_
1. #11 cos 0.9908 z 3.19 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.

> В нашем кластере пять узлов — типичный размер для Raft, позволяющий пережить отказ двух узлов [1]. Точное значение кворума в процитированном фрагменте статьи не указано — для проекта оно было согласовано отдельно.
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 5 · fixed-raft-consensus-0029
> 
> Цитаты:
>   [1] «A Raft cluster contains several servers; five is a typical number, which allows the system to tolerate two failures.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✗ смысл по цитатам: частично
#### 1.12 Что статья о Raft говорит про шардирование PostgreSQL?

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 0))
> Уточните, пожалуйста: о каком документе или системе вопрос «Что статья о Raft говорит про шардирование PostgreSQL?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: raft-consensus.pdf — Front matter, стр. 3 (llm 0)
>   ближе всего: raft-consensus.pdf — Front matter, стр. 9 (llm 0)
>   ближе всего: raft-consensus.pdf — Front matter, стр. 1 (llm 0)

#### 1.13 Как называется переходная конфигурация при смене состава кластера?

_контекст:_
1. #2 cos 0.9939 z 3.98 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 997 симв.
2. #3 cos 0.9931 z 3.73 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 1000 симв.
3. #5 cos 0.9925 z 3.55 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.
4. #14 cos 0.9901 z 2.78 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.

> Переходная конфигурация называется joint consensus (Cold,new) [1][3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0062
>   [3] raft-consensus.pdf — Front matter, стр. 11 · fixed-raft-consensus-0068
> 
> Цитаты:
>   [1] «In Raft the cluster first switches to a transitional configuration we call joint consensus; once the joint consensus has been committed, the system then transitions to the new configuration.»
>   [2] «When the leader receives a request to change the configuration from Cold to Cnew , it stores the configuration for joint consensus (Cold,new in the figure) as a log entry and replicates that entry using the mechanisms described previously.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 2/2 дословно (1 — из другого фрагмента) · ✓ смысл по цитатам: да

#### 1.14 Собери итог списком: цель проекта и все зафиксированные ограничения и термины.

_контекст:_
1. #1 cos 0.9935 z 2.96 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 1000 симв.
2. #4 cos 0.9933 z 2.90 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 16 · 997 симв.
3. #8 cos 0.9929 z 2.77 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 13 · 996 симв.
4. #9 cos 0.9929 z 2.77 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 13 · 984 симв.

> Итог проекта (сервис конфигураций на Raft):
> 
> Цель: спроектировать внутренний распределённый отказоустойчивый сервис конфигураций на основе статьи о Raft.
> 
> Зафиксированные ограничения:
> • Чтения строго актуальные (линеаризуемые), без устаревших значений — лидер обменивается heartbeat с большинством кластера перед read-only ответом, no-op запись в начале срока [3].
> • Смена состава кластера без даунтайма — joint consensus, кластер продолжает нормально работать во время смены конфигурации [1].
> • Кворум согласован проектом как 3 (большинство кластера из 5 узлов) — вне приведённых фрагментов.
> 
> Согласованные термины:
> • Raft — алгоритм консенсуса для реплицированного лога; выбирает выделенного лидера, полностью управляющего логом.
> • Лидер — принимает записи от клиентов, реплицирует их на другие серверы, сообщает, когда записи можно применять к state machine.
> • Heartbeat — пустые AppendEntries RPC, периодически шлёмые лидером.
> • Election timeout — случайно из фиксированного интервала; по истечении без сообщений сервер начинает выборы (конкретные интервалы — вне приведённых фрагментов).
> • Term — номер, который candidate увеличивает при старте выборов.
> • Follower/Candidate — стартуют как followers; без heartbeat становятся candidates, голосуют за себя, рассылают RequestVote RPC.
> • Рандомизированные таймеры выборов — предотвращают и быстро разрешают split votes; candidate перезапускает таймаут перед новой попыткой.
> • Кворум (quorum = 3) — committed запись реплицирована на большинство серверов.
> • Committed запись — durable и в итоге исполняется всеми доступными state machines.
> • InstallSnapshot — лидер отправляет снапшот сильно отставшему follower.
> • Joint consensus (Cold,new) — переходная конфигурация при смене состава; новые узлы сначала входят как non-voting members.
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 1 · fixed-raft-consensus-0001
>   [3] raft-consensus.pdf — Front matter, стр. 13 · fixed-raft-consensus-0081
> 
> Цитаты:
>   [1] «Raft’s mechanism for
> changing the set of servers in the cluster uses a new
> joint consensus approach»
>   [1] «This allows the cluster to continue operating
> normally during configuration changes.»
>   [3] «Raft handles this by having the leader exchange heartbeat messages with a majority of the cluster before responding to read-only requests.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✗ числа 0/2 в цитатах · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✗ числа 0/2 в цитатах · ✗ смысл по цитатам: частично
## Сценарий 2: Production-чат по документации компании (rag-survey-2312.10997.pdf)

Ожидаемая цель: собрать по обзору RAG production-чат, отвечающий по документации компании с обязательными источниками: 10k чанков, обязательный реранкер

Цель в памяти после диалога: «Собрать production-чат, отвечающий по внутренней документации компании, на основе RAG; получить финальный план из 5 шагов (индекс, поиск, реранк, генерация, источники).» (2/2)

Память в конце:

```json
{
  "goal": "Собрать production-чат, отвечающий по внутренней документации компании, на основе RAG; получить финальный план из 5 шагов (индекс, поиск, реранк, генерация, источники).",
  "clarified": [
    "Строим production-чат по внутренней документации компании с использованием RAG",
    "Объём базы знаний небольшой: порядка 10k чанков",
    "Интересуется сравнением RAG и fine-tuning: когда что применять",
    "Уточняет, как по обзору оптимизировать индекс при чанковании",
    "Спрашивает про эмбеддинг-модели, рассматриваемые обзором",
    "Решение зафиксировано: реранкер обязателен в пайплайне",
    "Спрашивает про оптимизацию запроса до поиска (pre-retrieval)",
    "Запросил финальный план в 5 шагов: индекс, поиск, реранк, генерация, источники"
  ],
  "constraints": [
    "Объём индекса ~10k чанков",
    "Каждый ответ должен сопровождаться источниками",
    "Реранкер обязателен в пайплайне"
  ],
  "terms": [
    {
      "term": "Naive RAG",
      "meaning": "Базовая парадигма «Retrieve-Read»: indexing, retrieval, generation; чанки кодируются эмбеддинг-моделью и хранятся в векторной базе для similarity search"
    },
    {
      "term": "Advanced RAG",
      "meaning": "Улучшает качество поиска за счёт pre-retrieval и post-retrieval стратегий и улучшенного индексирования"
    },
    {
      "term": "Modular RAG",
      "meaning": "Парадигма с большей гибкостью за счёт функциональных модулей"
    },
    {
      "term": "Классический RAG-пайплайн",
      "meaning": "Три этапа: Indexing (чанкование, векторизация, хранение), Retrieval (Top-k по семантическому сходству), Generation (вопрос + чанки подаются в LLM)"
    },
    {
      "term": "RAG vs fine-tuning",
      "meaning": "RAG даёт обновление знаний в реальном времени и интерпретируемость и превосходит unsupervised fine-tuning; fine-tuning статичен, но даёт кастомизацию поведения/стиля и может снижать галлюцинации"
    },
    {
      "term": "Оптимизация индексации",
      "meaning": "Sliding window, fine-grained сегментация, метаданные (в т.ч. искусственные: сумаризации, Reverse HyDE), иерархические структуры (hierarchical index, parent-child)"
    },
    {
      "term": "Embedding-модели в обзоре",
      "meaning": "Retrieval основан на cosine similarity; sparse encoder (BM25) и dense retriever (BERT-архитектуры); современные модели — AngIE, Voyage, BGE — выигрывают за счёт multi-task instruct tuning"
    },
    {
      "term": "Reranking",
      "meaning": "Реранкер помещает наиболее релевантные результаты в начало, сокращая пул документов; работает как усилитель и фильтр. Rule-based (Diversity, Relevance, MRR) или model-based: BERT (SpanBERT), Cohere re"
    },
    {
      "term": "Метрики качества RAG",
      "meaning": "Retrieval: Hit Rate, MRR, NDCG. Generation: для неразмеченного контента — faithfulness, relevance, non-harmfulness; для размеченного — accuracy"
    },
    {
      "term": "Финальный план 5 шагов",
      "meaning": "Индекс (чанкование, эмбеддинги, метаданные, parent-child) → поиск Top-k → реранк → генерация (question + chunks в LLM) → источники и метрики"
    }
  ]
}
```

| # | вопрос | статус | источники | ожидание | память |
|---|---|---|---|---|---|
| 1 | Собираем production-чат, который отвечает по внутренней документации компании, через RAG. Какие парадигмы RAG выделяет обзор? | answer (ждали answer) | 3/3 | 3/3 | ✓ |
| 2 | Уточнение: объём небольшой, порядка 10k чанков. С какой парадигмы разумно начать? | answer (ждали answer) | 1/1 | 1/1 | ✓ |
| 3 | Фиксируем требование: каждый ответ — с источниками. Из каких трёх этапов состоит классический RAG-пайплайн? | answer (ждали answer) | 2/2 | 3/3 | ✓ |
| 4 | Чем RAG лучше дообучения модели, и когда всё-таки нужен fine-tuning? | answer (ждали answer) | 3/3 | 1/2 | — |
| 5 | Чанкование: как оптимизировать индекс по обзору? | answer (ждали answer) | 3/3 | 2/2 | — |
| 6 | Какие эмбеддинг-модели рассматривает обзор? | answer (ждали answer) | 1/1 | 1/1 | — |
| 7 | Решение зафиксировано: реранкер обязателен. Что обзор говорит про reranking? | answer (ждали answer) | 1/1 | 1/1 | ✓ |
| 8 | Как можно улучшить сам запрос до поиска? | unknown (ждали answer) | 0/0 | 0/1 | — |
| 9 | Что обзор говорит про цены на GPU у AWS в 2026 году? | unknown (ждали unknown) | 0/0 | — | — |
| 10 | Метрики: по каким оценивать качество нашего чата? | answer (ждали answer) | 1/1 | 2/2 | — |
| 11 | На каких фреймворках можно собрать прототип? | unknown (ждали answer) | 0/0 | 0/1 | — |
| 12 | Напомни, какой реранкер мы зафиксировали и каков объём базы? | answer (ждали answer) | 0/0 | 2/2 | — |
| 13 | Дай финальный план в 5 шагов: индекс, поиск, реранк, генерация, источники — с учётом всего, что записано. | answer (ждали answer) | 3/3 | 2/2 | ✓ |

### Ответы

#### 2.01 Собираем production-чат, который отвечает по внутренней документации компании, через RAG. Какие парадигмы RAG выделяет обзор?

_контекст:_
1. #4 cos 0.9917 z 3.23 llm 9 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · 995 симв.
2. #9 cos 0.9910 z 2.98 llm 9 (rewrite) · rag-survey-2312.10997.pdf — Front matter, стр. 1 · 998 симв.
3. #3 cos 0.9920 z 3.34 llm 8 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · 1000 симв.
4. #8 cos 0.9913 z 3.08 llm 7 (rewrite) · rag-survey-2312.10997.pdf — I. INTRODUCTION, стр. 1–2 · 999 симв.

> Обзор выделяет три парадигмы RAG: Naive RAG, Advanced RAG и Modular RAG [1], [2]. Advanced RAG улучшает качество поиска за счёт pre-retrieval и post-retrieval стратегий и улучшенного индексирования, а Modular RAG отличается большей гибкостью за счёт функциональных модулей [3].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · fixed-rag-survey-2312.10997-0010
>   [2] rag-survey-2312.10997.pdf — Front matter, стр. 1 · fixed-rag-survey-2312.10997-0001
>   [3] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · fixed-rag-survey-2312.10997-0015
> 
> Цитаты:
>   [1] «we categorize it into three stages: Naive RAG, Advanced
> RAG, and Modular RAG»
>   [2] «encompassing
> the Naive RAG, the Advanced RAG, and the Modular RAG.»
>   [3] «Focusing on enhancing retrieval quality, it employs pre-retrieval and post-retrieval strategies. To tackle the indexing issues, Advanced RAG refines
> its indexing techniques»
>   [3] «Modular RAG inherits and develops from the previous paradigm, showcasing greater flexibility overall. This is evident in the
> introduction of multiple specific functional modules»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 4/4 дословно · ✓ смысл по цитатам: да

#### 2.02 Уточнение: объём небольшой, порядка 10k чанков. С какой парадигмы разумно начать?

_контекст:_
1. #13 cos 0.9899 z 2.93 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.

> При объёме ~10k чанков разумно начать с Naive RAG — базовой парадигмы «Retrieve-Read» с процессом indexing, retrieval и generation, где чанки кодируются эмбеддинг-моделью и хранятся в векторной базе, обеспечивая эффективный similarity search [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · fixed-rag-survey-2312.10997-0011
> 
> Цитаты:
>   [1] «The Naive RAG follows a traditional process that includes indexing, retrieval, and generation, which is also characterized as a “Retrieve-Read” framework [7].»
>   [1] «Chunks are then encoded into vector representations using an embedding model and stored in vector database.»
>   [1] «This step is crucial for enabling efficient similarity searches in the subsequent retrieval phase.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да

#### 2.03 Фиксируем требование: каждый ответ — с источниками. Из каких трёх этапов состоит классический RAG-пайплайн?

_контекст:_
1. #1 cos 0.9954 z 3.34 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · 995 симв.
2. #5 cos 0.9940 z 2.85 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.
3. #2 cos 0.9945 z 3.03 llm 7 (rewrite) · rag-survey-2312.10997.pdf — I. INTRODUCTION, стр. 2 · 998 симв.
4. #4 cos 0.9942 z 2.92 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · 1000 симв.

> Классический RAG-пайплайн состоит из трёх этапов: 1) Indexing — документы разбиваются на чанки, кодируются в векторы и хранятся в векторной базе; 2) Retrieval — извлекаются Top-k чанков, наиболее релевантных вопросу по семантическому сходству; 3) Generation — исходный вопрос и извлечённые чанки подаются в LLM для генерации финального ответа [1][2].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · fixed-rag-survey-2312.10997-0010
>   [2] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · fixed-rag-survey-2312.10997-0011
> 
> Цитаты:
>   [1] «It mainly consists of 3 steps. 1) Indexing. Documents are split into chunks, encoded into vectors, and stored in a vector database. 2) Retrieval. Retrieve the Top k chunks most relevant to the question based on semantic similarity.»
>   [2] «The Naive RAG follows a traditional process that includes indexing, retrieval, and generation, which is also characterized as a “Retrieve-Read” framework [7].»
>   [2] «Chunks are then encoded into vector representations using an embedding model and stored in vector database.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ числа 3/3 в цитатах · ✓ смысл по цитатам: да

#### 2.04 Чем RAG лучше дообучения модели, и когда всё-таки нужен fine-tuning?

_контекст:_
1. #18 cos 0.9920 z 2.52 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · 998 симв.
2. #5 cos 0.9928 z 2.82 llm 9 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · 995 симв.
3. #3 cos 0.9892 z 2.84 llm 8 · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · 994 симв.
4. #4 cos 0.9929 z 2.83 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > C. Modular RAG, стр. 5 · 992 симв.

> RAG лучше дообучения тем, что обеспечивает обновление знаний в реальном времени и эффективное использование внешних источников с высокой интерпретируемостью, а по результатам сравнений стабильно превосходит unsupervised fine-tuning на knowledge-intensive задачах — как по знаниям из обучения, так и по новым [1][2]. Fine-tuning статичен и требует переобучения при обновлениях, но даёт глубокую кастомизацию поведения и стиля модели и подходит для воспроизведения специфических структур, стилей или форматов; он может снижать галлюцинации [2][3]. Подходы не взаимоисключающие: в некоторых случаях их совместное использование может привести к оптимальной производительности [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · fixed-rag-survey-2312.10997-0026
>   [2] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · fixed-rag-survey-2312.10997-0025
>   [3] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · fixed-rag-survey-2312.10997-0024
> 
> Цитаты:
>   [2] «RAG excels in dynamic environments by offering realtime knowledge updates and effective utilization of external knowledge sources with high interpretability.»
>   [2] «while unsupervised fine-tuning shows some improvement, RAG consistently outperforms it, for both existing knowledge encountered during training and entirely new knowledge.»
>   [2] «FT is more static, requiring retraining for updates but enabling deep customization of the model’s behavior and style.»
>   [2] «while it can reduce hallucinations, it may face challenges with unfamiliar data.»
>   [3] «FT is comparable to a student internalizing knowledge over time, suitable for scenarios requiring replication of specific structures, styles, or formats.»
>   [1] «RAG and FT are not mutually exclusive and can complement each other, enhancing a model’s capabilities at different levels.»
>   [1] «In some instances, their combined use may lead to optimal performance.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 7/7 дословно · ✓ смысл по цитатам: да

#### 2.05 Чанкование: как оптимизировать индекс по обзору?

_контекст:_
1. #3 cos 0.9934 z 3.22 llm 8 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · 1000 симв.
2. #13 cos 0.9922 z 2.83 llm 8 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 4 · 997 симв.
3. #18 cos 0.9920 z 2.76 llm 8 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 · 1000 симв.
4. #7 cos 0.9927 z 2.98 llm 6 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.

> По обзору индекс при чанковании оптимизируется следующими способами: sliding window подход, fine-grained сегментация и добавление метаданных [1]; в pre-retrieval-процессе — повышение гранулярности данных, оптимизация структуры индекса, добавление метаданных, alignment optimization и mixed retrieval [2]; метаданные можно строить искусственно (суммаризации параграфов, гипотетические вопросы / Reverse HyDE), а также строить иерархическую структуру документов (hierarchical index, parent-child) для ускорения поиска [3].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · fixed-rag-survey-2312.10997-0015
>   [2] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 4 · fixed-rag-survey-2312.10997-0016
>   [3] rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 · fixed-rag-survey-2312.10997-0041
> 
> Цитаты:
>   [1] «Advanced RAG refines its indexing techniques through the use of a sliding window approach, fine-grained segmentation, and the incorporation of metadata.»
>   [2] «enhancing data granularity, optimizing index structures, adding metadata, alignment optimization, and mixed retrieval»
>   [3] «metadata can also be artificially constructed. For example, adding summaries of paragraph, as well as introducing hypothetical questions. This method is also known as Reverse HyDE.»
>   [3] «establish a hierarchical structure for the documents. By constructing In structure, RAG system can expedite the retrieval and processing of pertinent data.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 4/4 дословно · ✓ смысл по цитатам: да

#### 2.06 Какие эмбеддинг-модели рассматривает обзор?

_контекст:_
1. #1 cos 0.9947 z 3.08 llm 10 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · 1000 симв.

> Обзор отмечает, что retrieval основан на cosine similarity между эмбеддингами вопроса и чанков, и рассматривает sparse encoder (BM25) и dense retriever (BERT-архитектуры), а среди современных prominent embedding-моделей — AngIE, Voyage, BGE и др., выигрывающие за счёт multi-task instruct tuning; универсального ответа «какую модель выбрать» нет — конкретные модели лучше подходят под конкретные задачи [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · fixed-rag-survey-2312.10997-0051
> 
> Цитаты:
>   [1] «This mainly includes a sparse encoder (BM25) and a dense retriever (BERT architecture Pre-training language models).»
>   [1] «Recent research has introduced prominent embedding models such as AngIE, Voyage, BGE,etc [94]–[96], which are benefit from multi-task instruct tuning.»
>   [1] «There is no one-size-fits-all answer to “which embedding model to use.” However, some specific models are better suited for particular use cases.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 2.07 Решение зафиксировано: реранкер обязателен. Что обзор говорит про reranking?

_контекст:_
1. #2 cos 0.9938 z 3.18 llm 10 (rewrite) · rag-survey-2312.10997.pdf — IV. GENERATION > A. Context Curation, стр. 10 · 999 симв.

> Обзор говорит про reranking следующее: реранкер помещает наиболее релевантные результаты в начало, сокращая общий пул документов и работая одновременно как усилитель и фильтр, подавая уточнённые входы для более точной обработки языковой моделью. Реранкер может быть rule-based (на метриках Diversity, Relevance, MRR) или model-based: энкодер-декодеры серии BERT (например, SpanBERT), специализированные модели вроде Cohere rerank или bge-raranker-large, а также общие LLM вроде GPT [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — IV. GENERATION > A. Context Curation, стр. 10 · fixed-rag-survey-2312.10997-0056
> 
> Цитаты:
>   [1] «most pertinent results first, effectively reducing the overall document pool, severing a dual purpose in information retrieval, acting as both an enhancer and a filter, delivering refined inputs for more precise language model processing»
>   [1] «Reranking can be performed using rule-based methods that depend on predefined metrics like Diversity, Relevance, and MRR, or model-based approaches like Encoder-Decoder models from the BERT series (e.g., SpanBERT), specialized reranking models such as Cohere rerank or bge-raranker-large, and general large language models like GPT»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 2/2 дословно · ✓ смысл по цитатам: да

#### 2.08 Как можно улучшить сам запрос до поиска?

_контекст:_
1. #2 cos 0.9944 z 2.90 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · 1000 симв.
2. #6 cos 0.9939 z 2.74 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 994 симв.

> Не знаю: в найденных фрагментах нет ответа на этот вопрос.
> Уточните, пожалуйста: Приведите, пожалуйста, фрагменты обзора с разделом Query Optimization / pre-retrieval стратегиями (например, query rewriting, expansion, HyDE), чтобы я мог точно перечислить способы улучшения запроса до поиска.

#### 2.09 Что обзор говорит про цены на GPU у AWS в 2026 году?

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 0))
> Уточните, пожалуйста: о каком документе или системе вопрос «Что обзор говорит про цены на GPU у AWS в 2026 году?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 (llm 0)
>   ближе всего: rag-survey-2312.10997.pdf — Front matter, стр. 1 (llm 0)
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 (llm 0)

#### 2.10 Метрики: по каким оценивать качество нашего чата?

_контекст:_
1. #1 cos 0.9946 z 2.91 llm 9 (rewrite) · rag-survey-2312.10997.pdf — VI. TASK AND EVALUATION > B. Evaluation Target, стр. 12 · 993 симв.
2. #4 cos 0.9938 z 2.64 llm 8 (rewrite) · rag-survey-2312.10997.pdf — VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness, стр. 14–15 · 992 симв.
3. #5 cos 0.9937 z 2.61 llm 7 (rewrite) · rag-survey-2312.10997.pdf — VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches, стр. 15 · 998 симв.

> Для оценки качества RAG-чата используются стандартные метрики поисковых и информационно-поисковых систем: Hit Rate, MRR и NDCG [1]. Оценка качества генерации делится по типу контента: для неразмеченного контента — faithfulness, relevance, non-harmfulness; для размеченного — accuracy [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — VI. TASK AND EVALUATION > B. Evaluation Target, стр. 12 · fixed-rag-survey-2312.10997-0070
> 
> Цитаты:
>   [1] «Metrics such as Hit Rate, MRR, and NDCG are commonly utilized for this purpose [161], [162].»
>   [1] «the evaluation encompasses the faithfulness, relevance, and non-harmfulness of the generated answers. In contrast, for labeled content, the focus is on the accuracy of the information produced by the model [161].»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 2/2 дословно · ✓ смысл по цитатам: да

#### 2.11 На каких фреймворках можно собрать прототип?

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 2))
> Уточните, пожалуйста: о каком документе или системе вопрос «На каких фреймворках можно собрать прототип?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 (llm 2)
>   ближе всего: rag-survey-2312.10997.pdf — III. RETRIEVAL > A. Retrieval Source, стр. 7 (llm 1)
>   ближе всего: rag-survey-2312.10997.pdf — I. INTRODUCTION, стр. 2 (llm 1)

#### 2.12 Напомни, какой реранкер мы зафиксировали и каков объём базы?

> Зафиксировано следующее:
> - **Реранкер**: решение, что реранкер **обязателен в пайплайне**. Конкретная модель реранкера не фиксировалась — лишь обсуждались варианты из обзора (rule-based, BERT-серии типа SpanBERT, Cohere rerank, bge-reranker-large, общие LLM).
> - **Объём базы**: порядка **10k чанков** — на основании этого было решено начинать с Naive RAG.
> 
> Из памяти задачи: ответ дан из цели и договорённостей этого диалога, а не из фрагментов корпуса — источников нет.

- ✗ проверки: 
#### 2.13 Дай финальный план в 5 шагов: индекс, поиск, реранк, генерация, источники — с учётом всего, что записано.

_контекст:_
1. #12 cos 0.9903 z 2.52 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.
2. #1 cos 0.9911 z 2.78 llm 7 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · 1000 симв.
3. #4 cos 0.9909 z 2.72 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > C. Modular RAG, стр. 5 · 992 симв.
4. #7 cos 0.9906 z 2.61 llm 7 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 · 1000 симв.

> Финальный план из пяти шагов для production-чата: Первый шаг — индекс: чанкование со sliding window / fine-grained сегментацией, кодирование эмбеддинг-моделью (sparse BM25 или dense BERT-архитектуры; современные модели AngIE, Voyage, BGE с multi-task instruct tuning; универсального ответа нет — модели подбираются под задачу), хранение в векторной базе; обогащение метаданными (суммаризации параграфов, гипотетические вопросы / Reverse HyDE) и иерархической структурой parent-child для ускорения поиска. Второй шаг — поиск: по user query тем же кодировщиком извлекать Top-k чанков по semantic similarity. Третий шаг — реранк (обязателен): реранкер помещает наиболее релевантные результаты в начало, работает как усилитель и фильтр — rule-based (Diversity, Relevance, MRR) или model-based (BERT/SpanBERT, Cohere rerank, bge-reranker-large, GPT). Четвёртый шаг — генерация: подать original question и retrieved chunks вместе в LLM для финального ответа. Пятый шаг — источники: каждый ответ сопровождать источниками; качество измерять метриками retrieval (Hit Rate, MRR, NDCG) и generation (faithfulness, relevance, non-harmfulness; для размеченного контента — accuracy).
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · fixed-rag-survey-2312.10997-0011
>   [2] rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · fixed-rag-survey-2312.10997-0051
>   [4] rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 · fixed-rag-survey-2312.10997-0041
> 
> Цитаты:
>   [1] «most relevant to the question based on semantic similarity. 3) Generation. Input the original question and the retrieved chunks together into LLM to generate the final answer.»
>   [2] «This mainly includes a sparse encoder (BM25) and a dense retriever (BERT architecture Pre-training language models). Recent research has introduced prominent embedding models such as AngIE, Voyage, BGE,etc [94]–[96], which are benefit from multi-task instruct tuning.»
>   [3] «metadata can also be artificially constructed. For example, adding summaries of paragraph, as well as introducing hypothetical questions. This method is also known as Reverse HyDE.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 3/3 дословно (1 — из другого фрагмента) · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 3/3 · ✓ цитаты 3/3 дословно (1 — из другого фрагмента) · ✗ смысл по цитатам: частично